//! What the CLI and the daemon say to each other.
//!
//! The daemon holds the identity key, the clipboard and the mesh state, so
//! every command is a request over the control socket rather than
//! something the CLI does itself. Shared by [`super::server`] and the
//! CLI's client, which never depends on the server.

use std::time::Duration;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::{
    clip::{Pause, Switch},
    files::FileRef,
};

/// Cap on a control message. Sized for the largest one, a copy at the
/// protocol ceiling.
pub const MAX_MESSAGE_SIZE: u32 = crate::net::proto::MAX_CLIP_SIZE;

/// Budget for the answer to a request the daemon answers without
/// touching the network.
const ANSWER: Duration = Duration::from_secs(5);

/// How long the daemon waits for the clip's files to arrive from a peer
/// before answering that they have not. The fetch carries on either way.
pub const FILES_WAIT: Duration = Duration::from_mins(5);

/// Budget for a whole pairing, from dialing to the confirmation.
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(90);

/// How long an issued pairing ticket stays redeemable. Here because the
/// CLI tells the user; the daemon is what enforces it.
pub const PAIR_TICKET_TTL: Duration = Duration::from_mins(3);

/// A request from the CLI.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Report what the daemon is doing.
    Status,
    /// Issue a one-time pairing ticket, revoking any outstanding one.
    PairHost { name: String },
    /// Redeem a ticket issued by another machine.
    PairJoin { ticket: String, name: String },
    /// Remove a machine from the mesh, by name or endpoint id.
    RemovePeer { peer: String },
    /// Put something on the clipboard of every machine.
    Copy {
        mime: String,
        bytes: Vec<u8>,
        secret: bool,
        ttl: Option<Duration>,
    },
    /// Read the clipboard in one of the types it carries, the best one
    /// when none is named.
    Paste { mime: Option<String> },
    /// Put files on the clipboard of every machine, contents and all.
    CopyFiles {
        paths: Vec<String>,
        ttl: Option<Duration>,
    },
    /// Where the clipboard's files are on this machine, asking for them if
    /// they are not here yet and waiting for them to arrive.
    Files,
    /// Empty the clipboard everywhere.
    Clear,
    /// Stop a direction of the clipboard, or start it again.
    SetPause {
        capture: Option<Switch>,
        apply: Option<Switch>,
    },
}

impl Request {
    /// How long the CLI waits for the answer.
    pub fn timeout(&self) -> Duration {
        match self {
            // Answered once every file is spooled: a clip never names
            // content this machine cannot serve, and a folder takes as
            // long as it takes.
            Request::CopyFiles { .. } => Duration::from_mins(10),
            Request::Files => FILES_WAIT + ANSWER,
            // Issuing a ticket may first wait for a relay.
            Request::PairHost { .. } => Duration::from_secs(45),
            Request::PairJoin { .. } => PAIRING_TIMEOUT + ANSWER,
            _ => ANSWER,
        }
    }
}

/// The daemon's answer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    /// Done, with nothing to say.
    Done,
    /// Boxed because it dwarfs every other answer, and every answer
    /// would otherwise be as big as the biggest.
    Status(Box<Status>),
    /// The ticket to carry to the other machine.
    PairTicket(String),
    /// Pairing finished and the machine is saved under this name.
    Paired(String),
    PeerRemoved(EndpointId),
    /// The contents of the clipboard, in one of the types it carries.
    Contents {
        /// The type `bytes` is in.
        mime: String,
        /// The other types the clipboard carries.
        alternates: Vec<String>,
        bytes: Vec<u8>,
    },
    /// The files the clipboard names, and where they are.
    Files {
        /// Where they are laid out, once they are all here. `None` means
        /// they are still on their way.
        tree: Option<String>,
        files: Vec<FileRef>,
    },
    Paused(Pause),
    /// The request failed. The message is already safe to print.
    Error(String),
}

/// What the daemon is doing, for `yank status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    /// This machine's identity on the mesh.
    pub endpoint: EndpointId,
    pub uptime_secs: u64,
    pub version: String,
    pub clipboard: ClipboardStatus,
    /// Every paired machine, connected or not.
    pub peers: Vec<PeerInfo>,
}

/// The clipboard half of the status.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClipboardStatus {
    /// Whether the compositor side is up, and why not when it is not.
    pub backend: Option<String>,
    pub pause: Pause,
    /// What the clipboard holds, mesh-wide.
    pub current: Option<ClipInfo>,
}

/// What the clipboard holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClipInfo {
    /// One line of contents, `<secret>` when it must not be shown.
    pub preview: String,
    pub secret: bool,
    /// How many files it names, zero when it names none.
    pub files: usize,
    /// The machine that copied it, by its paired name when we know it.
    pub origin: String,
    /// How long ago it was copied.
    pub age_secs: u64,
    /// How long until it disappears everywhere, when it has a lifetime.
    pub expires_in_secs: Option<u64>,
}

/// One paired machine.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub connection: ConnectionStatus,
}

/// Where a machine's connection is at.
#[derive(Debug, Serialize, Deserialize)]
pub enum ConnectionStatus {
    /// Dialing, or waiting for it to dial us.
    Connecting,
    Connected {
        /// How traffic gets there, once a path is chosen.
        route: Option<Route>,
        since_secs: u64,
    },
    /// The last attempt failed; waiting before the next.
    Backoff { retry_in_secs: u64 },
}

/// How traffic reaches a machine.
#[derive(Debug, Serialize, Deserialize)]
pub enum Route {
    /// A hole-punched path straight to it.
    Direct { addr: String, rtt_ms: u64 },
    /// Through a relay, because a direct path could not be made.
    Relay { url: String, rtt_ms: u64 },
}
