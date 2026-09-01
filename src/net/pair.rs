//! Pairing: how a machine joins the mesh.
//!
//! Pairing runs on its own ALPN because it is the one protocol that
//! accepts a connection from a machine we have never seen. What authorizes
//! it is not the peer list but a one-time secret, carried out of band in a
//! *ticket* the user copies from one machine to the other. The identities
//! themselves are authenticated by iroh's handshake, so the exchange only
//! has to carry the human-readable names.
//!
//! Over a single bidirectional stream:
//!
//! 1. joiner → host: `Hello`, the ticket secret and its name
//! 2. host → joiner: `Welcome` with its own name, or `Reject`
//! 3. joiner → host: `Done`, or `Reject`
//!
//! Each side checks the other against its own mesh state before answering,
//! so neither ends up holding a peer the other refused. The host saves the
//! peer *before* confirming with the `paired` close, so it can never
//! confirm one it failed to save; the joiner saves after seeing that
//! close. A failure in that last step leaves the host paired one-sidedly,
//! which pairing again repairs: registering a machine already registered
//! is a no-op, since the handshake proves the identity either way.

use std::{fmt, str::FromStr, time::Duration};

use color_eyre::eyre::{Result, WrapErr as _, bail, ensure, eyre};
use data_encoding::BASE32_NOPAD;
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, ConnectionError, SendStream, VarInt},
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;

use super::wire::{read_message, write_message};
use crate::config::{MeshState, sanitize_bounded};

/// ALPN of the pairing protocol.
pub const ALPN: &[u8] = b"yank/pair/0";

/// Prefix of a serialized ticket, so a mangled paste is caught before it
/// is decoded.
const TICKET_PREFIX: &str = "yank-pair-";

/// Cap on a pairing message.
const MAX_MESSAGE_SIZE: u32 = 4096;

/// How long the host waits for a connection to open its stream and present
/// its `Hello`. This is the only phase someone without the ticket can
/// stretch, and a stalled attempt occupies one of the few exchange slots,
/// so it gets a much shorter deadline than the exchange itself.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a refusing side waits for the other to read the refusal.
const REJECT_LINGER: Duration = Duration::from_secs(5);

/// The same, for refusing an attempt nobody authenticated: anyone can
/// trigger those, so the courtesy has to stay cheap.
const REJECT_LINGER_BRIEF: Duration = Duration::from_secs(1);

/// Close reason that confirms a pairing to the joiner.
const PAIRED_REASON: &[u8] = b"paired";

/// What the host prints and the joiner redeems.
///
/// Carries everything needed to reach the host directly, bypassing
/// discovery, which may not have propagated yet, plus the one-time secret
/// that authorizes the join.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairTicket {
    addr: EndpointAddr,
    secret: PairSecret,
}

impl PairTicket {
    /// Issues a ticket, with a fresh secret, for a host reachable at
    /// `addr`.
    pub fn generate(addr: EndpointAddr) -> Self {
        PairTicket {
            addr,
            secret: PairSecret::generate(),
        }
    }

    /// Whether `other` is this same issued ticket.
    pub fn matches(&self, other: &PairTicket) -> bool {
        self.secret.verify(&other.secret)
    }
}

impl fmt::Display for PairTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = postcard::to_stdvec(self).expect("a ticket must serialize");
        let encoded = BASE32_NOPAD.encode(&bytes).to_ascii_lowercase();
        write!(f, "{TICKET_PREFIX}{encoded}")
    }
}

impl FromStr for PairTicket {
    type Err = color_eyre::eyre::Error;

    fn from_str(s: &str) -> Result<Self> {
        let encoded = s
            .trim()
            .strip_prefix(TICKET_PREFIX)
            .ok_or_else(|| eyre!("not a yank pairing ticket"))?;
        let bytes = BASE32_NOPAD
            .decode(encoded.to_ascii_uppercase().as_bytes())
            .wrap_err("invalid pairing ticket")?;

        let (ticket, rest) =
            postcard::take_from_bytes(&bytes).wrap_err("invalid pairing ticket")?;
        ensure!(rest.is_empty(), "invalid pairing ticket: trailing bytes");

        Ok(ticket)
    }
}

/// The one-time secret authorizing a join.
#[derive(Clone, Serialize, Deserialize)]
struct PairSecret([u8; 16]);

impl PairSecret {
    fn generate() -> Self {
        PairSecret(rand::random())
    }

    /// Compared in constant time, so a wrong guess leaks nothing through
    /// how long the answer took.
    fn verify(&self, candidate: &PairSecret) -> bool {
        self.0.ct_eq(&candidate.0).into()
    }
}

impl fmt::Debug for PairSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairSecret(..)")
    }
}

/// The machine a successful pairing registers.
#[derive(Debug, Clone)]
pub struct PairedPeer {
    /// The name it announced.
    pub name: String,
    /// Its identity, authenticated by the connection handshake.
    pub endpoint: EndpointId,
}

/// What hosting one pairing attempt produced.
#[derive(Debug)]
pub enum Outcome {
    /// The exchange completed. The caller must save the peer and then call
    /// [`confirm_paired`].
    Paired(PairedPeer),
    /// The attempt failed without needing the user (a wrong ticket,
    /// connection trouble); the ticket stays redeemable.
    Dismissed,
}

/// A message of the exchange. Each is sent by one side only; they share an
/// enum because they share a stream.
#[derive(Debug, Serialize, Deserialize)]
enum Message {
    /// The joiner's opening request, proving it holds the ticket.
    Hello { secret: PairSecret, name: String },
    /// The host's acceptance, announcing its own name.
    Welcome { name: String },
    /// The joiner's confirmation.
    Done,
    /// A refusal, from either side.
    Reject { reason: String },
}

/// Runs the host side on one connection accepted with `ticket`
/// outstanding.
///
/// An error means the ticket holder's own attempt failed, and it was told
/// why; the caller decides whether the ticket stays valid. On
/// [`Outcome::Paired`] the connection is left open: closing it any other
/// way than [`confirm_paired`], dropping it included, makes the joiner
/// treat the pairing as failed.
pub async fn pair_with(
    conn: &Connection,
    ticket: &PairTicket,
    local_name: &str,
    state: &MeshState,
) -> Result<Outcome> {
    let opening = tokio::time::timeout(HELLO_TIMEOUT, async {
        let (send, mut recv) = conn.accept_bi().await.ok()?;
        let hello = read_message(&mut recv, MAX_MESSAGE_SIZE).await;
        Some((send, recv, hello))
    })
    .await;
    let (mut send, mut recv, hello) = match opening {
        Ok(Some(opened)) => opened,
        Ok(None) => return Ok(Outcome::Dismissed),
        Err(_timeout) => {
            conn.close(0u32.into(), b"timeout");
            return Ok(Outcome::Dismissed);
        }
    };

    // A caller that does not speak the protocol, or holds the wrong
    // secret, is dismissed without ending the host's wait.
    let announced = match hello {
        Ok(Message::Hello { secret, name }) if ticket.secret.verify(&secret) => name,
        Ok(_) | Err(_) => {
            send_reject(
                conn,
                &mut send,
                "invalid pairing ticket",
                REJECT_LINGER_BRIEF,
            )
            .await;
            return Ok(Outcome::Dismissed);
        }
    };

    let endpoint = conn.remote_id();
    let name = resolve_peer_name(conn, &mut send, announced, &endpoint, state).await?;

    let welcome = Message::Welcome {
        name: local_name.to_owned(),
    };
    if write_message(&mut send, &welcome, MAX_MESSAGE_SIZE)
        .await
        .is_err()
    {
        return Ok(Outcome::Dismissed);
    }

    match read_message(&mut recv, MAX_MESSAGE_SIZE).await {
        Ok(Message::Done) => {}
        Ok(Message::Reject { reason }) => {
            bail!("the other machine refused: {}", sanitize_bounded(&reason))
        }
        Ok(msg) => bail!("unexpected message from the other machine: {msg:?}"),
        // The connection died mid-exchange: let the joiner retry with the
        // same ticket rather than burning it.
        Err(_) => return Ok(Outcome::Dismissed),
    }

    Ok(Outcome::Paired(PairedPeer { name, endpoint }))
}

/// Answers a pairing connection arriving while no ticket is outstanding,
/// so the joiner reads a reason instead of a transport error.
pub async fn reject_attempt(conn: &Connection, reason: &str) {
    // The joiner's `Hello` is left unread, but its stream stays open until
    // the linger expires, so its write cannot fail before it reads this.
    match tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi()).await {
        Ok(Ok((mut send, _recv))) => {
            send_reject(conn, &mut send, reason, REJECT_LINGER_BRIEF).await;
        }
        Ok(Err(_)) | Err(_) => conn.close(0u32.into(), b"timeout"),
    }
}

/// Confirms a pairing to the joiner. The peer must already be saved.
pub fn confirm_paired(conn: &Connection) {
    conn.close(0u32.into(), PAIRED_REASON);
}

/// The joiner side: dials the ticket's host with `endpoint`, which stays
/// open and belongs to the caller.
pub async fn join(
    endpoint: &Endpoint,
    ticket: &PairTicket,
    local_name: &str,
    state: &MeshState,
) -> Result<PairedPeer> {
    ensure!(
        ticket.addr.id != endpoint.secret_key().public(),
        "cannot pair a machine with itself",
    );

    let conn = endpoint
        .connect(ticket.addr.clone(), ALPN)
        .await
        .wrap_err("cannot reach the other machine")?;
    let (mut send, mut recv) = conn.open_bi().await?;

    let hello = Message::Hello {
        secret: ticket.secret.clone(),
        name: local_name.to_owned(),
    };
    write_message(&mut send, &hello, MAX_MESSAGE_SIZE).await?;

    let host = conn.remote_id();
    let announced = match read_message(&mut recv, MAX_MESSAGE_SIZE).await? {
        Message::Welcome { name } => name,
        Message::Reject { reason } => {
            conn.close(0u32.into(), b"rejected");
            bail!("pairing refused: {}", sanitize_bounded(&reason));
        }
        msg => bail!("unexpected message from the other machine: {msg:?}"),
    };

    let name = resolve_peer_name(&conn, &mut send, announced, &host, state).await?;

    write_message(&mut send, &Message::Done, MAX_MESSAGE_SIZE).await?;
    let _ = send.finish();

    // Any close other than the confirmation means the host did not save
    // us; see the module docs.
    match conn.closed().await {
        ConnectionError::ApplicationClosed(close)
            if close.error_code == VarInt::from_u32(0)
                && close.reason.as_ref() == PAIRED_REASON => {}
        reason => bail!(
            "the connection dropped before pairing completed ({reason}); \
             check `yank status` on the other machine before trying again",
        ),
    }

    Ok(PairedPeer {
        name,
        endpoint: host,
    })
}

/// Decides the name to register for the authenticated `endpoint`, which
/// announced itself as `announced`.
///
/// A machine we already know keeps the name we have for it. A new one is
/// checked against the mesh state, and told why when refused.
async fn resolve_peer_name(
    conn: &Connection,
    send: &mut SendStream,
    announced: String,
    endpoint: &EndpointId,
    state: &MeshState,
) -> Result<String> {
    if let Some(existing) = state.peer_name(endpoint) {
        return Ok(existing.to_owned());
    }
    if let Err(err) = state.validate_new_peer(&announced, endpoint) {
        send_reject(conn, send, &err.to_string(), REJECT_LINGER).await;
        return Err(err.wrap_err("cannot pair"));
    }

    Ok(announced)
}

/// Sends a refusal and waits up to `linger` for the other side to read it:
/// closing right after the write would discard the queued message.
async fn send_reject(conn: &Connection, send: &mut SendStream, reason: &str, linger: Duration) {
    let reject = Message::Reject {
        reason: reason.to_owned(),
    };

    if write_message(send, &reject, MAX_MESSAGE_SIZE).await.is_ok() && send.finish().is_ok() {
        let _ = tokio::time::timeout(linger, conn.closed()).await;
    }
    conn.close(0u32.into(), b"rejected");
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn ticket() -> PairTicket {
        PairTicket {
            addr: EndpointAddr::from(SecretKey::generate().public()),
            secret: PairSecret::generate(),
        }
    }

    #[test]
    fn tickets_round_trip() {
        let ticket = ticket();
        let parsed: PairTicket = ticket.to_string().parse().unwrap();

        assert_eq!(parsed.addr, ticket.addr);
        assert!(ticket.matches(&parsed));
    }

    #[test]
    fn tickets_reject_garbage() {
        assert!("yank-pair-notbase32!".parse::<PairTicket>().is_err());
        assert!("some-other-ticket".parse::<PairTicket>().is_err());

        // Trailing data must be refused, not quietly ignored.
        let padded = format!("{}aaaaaaaa", ticket());
        assert!(padded.parse::<PairTicket>().is_err());
    }

    #[test]
    fn a_secret_only_matches_itself() {
        let secret = PairSecret::generate();
        assert!(secret.verify(&secret.clone()));
        assert!(!secret.verify(&PairSecret::generate()));
    }
}
