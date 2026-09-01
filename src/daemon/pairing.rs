//! Pairing, from the daemon's side.
//!
//! The daemon is the only holder of the identity key, so pairing happens
//! here and the CLI drives it over the control socket. The pairing ALPN is
//! always served; what gates it is the *ticket*. At most one is valid at a
//! time, hosting again revokes the previous one, and redeeming is atomic:
//! the ticket is consumed under the same lock that saves the peer, so a
//! revoked, expired or already-used ticket can never register anything.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, eyre};
use iroh::{Endpoint, endpoint::Connection};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use super::{control::PAIR_TICKET_TTL, store::MeshStore};
use crate::net::pair;

/// How long to wait for a relay connection before issuing a ticket, so the
/// ticket carries an address that works before hole punching does.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(30);

/// Budget for one connection to complete the whole exchange.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Concurrent inbound exchanges. The pairing ALPN accepts connections from
/// machines we know nothing about, so surplus is dropped, not queued.
const MAX_EXCHANGES: usize = 4;

/// The daemon's pairing state: the outstanding ticket, if any.
#[derive(Debug)]
pub struct Pairing {
    endpoint: Endpoint,
    /// Whether the endpoint uses relays. Without them there is no relay
    /// connection to wait for, and waiting would never return.
    uses_relays: bool,
    store: Arc<MeshStore>,
    issued: Mutex<Option<IssuedTicket>>,
    exchanges: Semaphore,
}

/// A ticket handed to the user, redeemable until it is revoked or expires.
#[derive(Debug)]
struct IssuedTicket {
    ticket: pair::PairTicket,
    expires: Instant,
    /// The name this machine announces to whoever redeems it.
    local_name: String,
}

impl Pairing {
    pub fn new(endpoint: Endpoint, uses_relays: bool, store: Arc<MeshStore>) -> Self {
        Pairing {
            endpoint,
            uses_relays,
            store,
            issued: Mutex::new(None),
            exchanges: Semaphore::new(MAX_EXCHANGES),
        }
    }

    /// Issues a fresh ticket, revoking any outstanding one.
    pub async fn host(&self, local_name: String) -> Result<pair::PairTicket> {
        // Revoked before the wait below: someone re-hosting to kill a
        // leaked ticket must not need the network for the old one to die.
        if self.issued.lock().unwrap().take().is_some() {
            info!("pairing ticket revoked");
        }

        if self.uses_relays {
            tokio::time::timeout(ONLINE_TIMEOUT, self.endpoint.online())
                .await
                .map_err(|_| eyre!("cannot reach an iroh relay; check the network"))?;
        }

        let ticket = pair::PairTicket::generate(self.endpoint.addr());
        *self.issued.lock().unwrap() = Some(IssuedTicket {
            ticket: ticket.clone(),
            expires: Instant::now() + PAIR_TICKET_TTL,
            local_name,
        });
        info!("pairing ticket issued");

        Ok(ticket)
    }

    /// Serves one connection on the pairing ALPN.
    ///
    /// Refused outright unless a ticket is outstanding. A failed attempt
    /// leaves the ticket valid, since the joiner was told why and can try
    /// again; only a completed one redeems it.
    pub async fn serve_inbound(&self, conn: Connection) {
        let Ok(_permit) = self.exchanges.try_acquire() else {
            debug!("dropping a surplus pairing connection");
            conn.close(0u32.into(), b"busy");
            return;
        };

        let Some((ticket, local_name)) = self.outstanding() else {
            debug!("refusing a pairing connection: no ticket outstanding");
            pair::reject_attempt(&conn, "no pairing in progress on this machine").await;
            return;
        };

        let snapshot = self.store.snapshot();
        let exchange = pair::pair_with(&conn, &ticket, &local_name, &snapshot);
        let peer = match tokio::time::timeout(EXCHANGE_TIMEOUT, exchange).await {
            Ok(Ok(pair::Outcome::Paired(peer))) => peer,
            Ok(Ok(pair::Outcome::Dismissed)) => return,
            Ok(Err(err)) => return warn!("pairing attempt failed: {err:#}"),
            Err(_timeout) => {
                conn.close(0u32.into(), b"timeout");
                return;
            }
        };

        // Consuming the ticket and saving the peer happen under one lock,
        // so a ticket revoked mid-exchange cannot still register a machine
        // and a ticket is redeemed at most once.
        let saved = {
            let mut issued = self.issued.lock().unwrap();
            let live = issued
                .as_ref()
                .is_some_and(|held| held.ticket.matches(&ticket) && Instant::now() < held.expires);
            if !live {
                warn!("pairing discarded: the ticket is no longer valid");
                conn.close(0u32.into(), b"cancelled");
                return;
            }
            *issued = None;

            self.store.add_paired(&peer)
        };

        match saved {
            Ok(()) => {
                pair::confirm_paired(&conn);
                info!(peer = %peer.name, "paired");
            }
            Err(err) => {
                conn.close(0u32.into(), b"failed");
                warn!("pairing failed: cannot save the machine: {err:#}");
            }
        }
    }

    /// The outstanding ticket and the name to announce, dropping the
    /// ticket when it turns out to have expired.
    fn outstanding(&self) -> Option<(pair::PairTicket, String)> {
        let mut issued = self.issued.lock().unwrap();
        if issued
            .as_ref()
            .is_some_and(|held| held.expires <= Instant::now())
        {
            *issued = None;
            info!("pairing ticket expired");
        }

        issued
            .as_ref()
            .map(|held| (held.ticket.clone(), held.local_name.clone()))
    }
}
