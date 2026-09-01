//! The control socket.
//!
//! The daemon holds the identity key, the clipboard and the mesh state, so
//! the CLI does none of that itself: it connects to a unix socket in the
//! runtime directory and asks. One request, one answer, using the same
//! framing as the peer protocols.
//!
//! Split along the line the CLI depends on: `protocol` is the vocabulary
//! both sides share, `client` is what the CLI dials, `server` is what
//! the daemon runs. The CLI never touches `server`.

mod client;
mod protocol;
mod server;

pub use client::{Client, DaemonNotRunning, request, talk};
pub use protocol::*;
pub(super) use server::{Context, Server};
