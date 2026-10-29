//! Peer-to-peer clipboard replication over [iroh](https://www.iroh.computer/).
//!
//! Every machine holds the latest copy made anywhere on the mesh, and
//! replicates it directly to its paired peers. The daemon owns the mesh and
//! clipboard state; the CLI talks to it through a Unix socket.

#![warn(clippy::pedantic)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

use std::sync::LazyLock;

pub mod cli;
pub mod clip;
pub mod config;
pub mod daemon;
pub mod files;
pub mod net;

/// The Cargo version, with the Nix source revision when available.
pub static VERSION: LazyLock<String> = LazyLock::new(|| {
    let version = env!("CARGO_PKG_VERSION");
    match option_env!("NIX_YANK_GIT_HASH") {
        Some(revision) => format!("{version}-{revision}"),
        None => version.to_owned(),
    }
});
