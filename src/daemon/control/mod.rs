//! Control socket protocol and client.

mod client;
mod protocol;
mod server;

pub use client::{Client, DaemonNotRunning, request, talk};
pub use protocol::*;
pub(super) use server::{Context, Server};
