//! What the CLI and the daemon say to each other.
//!
//! The daemon holds the identity key, the clipboard and the mesh state, so
//! every command is a request over the control socket rather than
//! something the CLI does itself. Shared by [`super::server`] and
//! [`super::client`], and re-exported for the CLI, which never depends on
//! the server.

use std::time::Duration;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::clip::{Pause, Switch};

/// Cap on a control message. Sized for the largest one, a clipboard entry
/// at the protocol ceiling, with room for its envelope.
pub const MAX_MESSAGE_SIZE: u32 = crate::log::MAX_ENTRY_BYTES + 64 * 1024;

/// Budget for a request the daemon answers without touching the network.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Budget for a copy of files, which the daemon answers once every one of
/// them is spooled: an entry never names content this machine cannot
/// serve, and a folder takes as long as it takes.
pub const COPY_FILES_TIMEOUT: Duration = Duration::from_mins(10);

/// How long the daemon waits for an entry's files to arrive from a peer
/// before answering that they have not. The fetch carries on either way.
pub const FILES_WAIT: Duration = Duration::from_mins(5);

/// Budget for that request, with room for the answer to come back.
pub const FILES_TIMEOUT: Duration = FILES_WAIT.saturating_add(CLIENT_TIMEOUT);

/// Budget for issuing a pairing ticket, which may first wait for a relay.
pub const TICKET_TIMEOUT: Duration = Duration::from_secs(45);

/// How long an issued pairing ticket stays redeemable. Here because the
/// CLI tells the user; the daemon is what enforces it.
pub const PAIR_TICKET_TTL: Duration = Duration::from_mins(3);

/// A request from the CLI.
///
/// Postcard encodes variants by position: existing ones keep their
/// position *and meaning* (renaming is fine), new ones are only appended,
/// so a CLI and a daemon of different versions still understand each
/// other for the commands they share.
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
        ttl_secs: Option<u64>,
    },
    /// Read an entry, the current selection when none is named, in one of
    /// the types it carries, the best one when none is named either.
    Paste {
        entry: Option<String>,
        mime: Option<String>,
    },
    /// Put files on the clipboard of every machine, contents and all.
    CopyFiles {
        paths: Vec<String>,
        ttl_secs: Option<u64>,
    },
    /// Where an entry's files are on this machine, asking for them if they
    /// are not here yet and waiting for them to arrive.
    Files { entry: Option<String> },
    /// List the history, newest first.
    History { limit: Option<usize> },
    /// Make an entry the selection again.
    Pick { entry: String },
    /// Drop one entry from every machine.
    Forget { entry: String },
    /// Empty the clipboard everywhere, and the history too when asked.
    Clear { history: bool },
    /// Stop a direction of the clipboard, or start it again.
    SetPause {
        capture: Option<Switch>,
        apply: Option<Switch>,
    },
}

/// The daemon's answer.
///
/// Same rule as [`Request`] about variant positions.
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    /// Boxed because it dwarfs every other answer, and every answer
    /// would otherwise be as big as the biggest.
    Status(Box<Status>),
    /// The ticket to carry to the other machine.
    PairTicket(String),
    /// Pairing finished and the machine is saved.
    Paired {
        name: String,
        endpoint: EndpointId,
    },
    PeerRemoved(EndpointId),
    /// An entry was written; the label names it.
    Wrote {
        label: String,
    },
    /// The contents of an entry, in one of the types it carries.
    Contents {
        /// The type `bytes` is in.
        mime: String,
        /// The other types the entry carries.
        alternates: Vec<String>,
        bytes: Vec<u8>,
    },
    History(Vec<HistoryEntry>),
    /// The files an entry names, and where they are.
    Files {
        label: String,
        /// Where they are laid out, once they are all here. `None` means
        /// they are still on their way.
        tree: Option<String>,
        files: Vec<FileInfo>,
    },
    Cleared,
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
    pub peers: Vec<PeerStatus>,
}

/// The clipboard half of the status.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClipboardStatus {
    /// Whether the compositor side is up, and why not when it is not.
    pub backend: Option<String>,
    pub pause: Pause,
    /// How many entries the history holds.
    pub entries: usize,
    /// What the clipboard currently holds, mesh-wide.
    pub selection: Option<HistoryEntry>,
}

/// One entry of the history.
#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Short name, and what commands taking an entry accept a prefix of.
    pub label: String,
    /// One line of contents, `<secret>` when it must not be shown.
    pub preview: String,
    pub mime: String,
    pub size: usize,
    pub secret: bool,
    /// How many files it names, zero when it names none.
    pub files: usize,
    /// The machine that copied it, by its paired name when we know it.
    pub origin: String,
    /// How long ago it was copied.
    pub age_secs: u64,
    /// How long until it disappears everywhere, when it has a lifetime.
    pub expires_in_secs: Option<u64>,
    /// Whether this is what the clipboard holds.
    pub selected: bool,
}

/// One file an entry names.
#[derive(Debug, Serialize, Deserialize)]
pub struct FileInfo {
    /// Where the file sits under the entry's root.
    pub path: String,
    pub size: u64,
}

/// One paired machine.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub endpoint: EndpointId,
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
