//! The control socket: what the CLI asks the daemon, and the daemon's
//! side of it.

mod protocol;
mod server;

pub use protocol::*;
pub(super) use server::{Context, Server};
