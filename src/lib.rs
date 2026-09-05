//! `yank` replicates the clipboard, and a bounded history of it, across
//! the machines you own.
//!
//! They form a *mesh*: they pair once, then hold direct connections to
//! each other over [iroh](https://www.iroh.computer/), with no server ever
//! holding the data. What they replicate is a bounded, append-only **log**
//! of events, keyed by the machine that wrote them; the clipboard is the
//! first topic carried over it.
//!
//! ```text
//! ┌─────┐  control  ┌────────┐    iroh (QUIC)    ┌──────────────┐
//! │ CLI │──socket──►│ daemon │◄─────────────────►│ peer daemons │
//! └─────┘           └───┬────┘                   └──────────────┘
//!                       │ ext/wlr-data-control
//!                       ▼
//!                 Wayland compositor
//! ```
//!
//! The layers, from the wire up:
//!
//! - [`config`] resolves the XDG directories and owns everything written
//!   to them: the machine identity key, the mesh state, the user settings.
//! - [`net`] is the transport: the iroh endpoint, the message framing, the
//!   pairing protocol and the replication vocabulary.
//! - [`log`] is the replication engine: a per-origin append-only log,
//!   ordered by a hybrid logical clock, exchanged by announcing watermarks
//!   and pulling what is missing. It knows nothing about clipboards.
//! - [`clip`] is the clipboard topic: the entry model (mime types,
//!   lifetimes, secrecy), the Wayland data-control backend, and the state
//!   machine tying them to the log.
//! - [`files`] is what a copied *file* means across machines: the log
//!   carries the manifest, and the bytes are spooled here and pulled from
//!   whoever has them.
//! - [`daemon`] runs it all: peer connections, routing, the control socket
//!   the CLI talks to.
//! - [`cli`] is the `yank` command itself.
//!
//! Adding a feature that is not the clipboard means adding a variant to
//! [`net::proto::Topic`] and a service alongside [`clip`]; the log, the
//! connections and the control socket are already topic-agnostic.

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
pub mod log;
pub mod net;

/// The Cargo version, with the Nix source revision when available.
pub static VERSION: LazyLock<String> =
    LazyLock::new(|| format_version(env!("CARGO_PKG_VERSION"), option_env!("NIX_YANK_GIT_HASH")));

fn format_version(cargo_version: &str, revision: Option<&str>) -> String {
    match revision {
        Some(revision) => format!("{cargo_version}-{revision}"),
        None => cargo_version.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::format_version;

    #[test]
    fn unversioned_builds_omit_the_source_revision() {
        assert_eq!(format_version("0.1.0", None), "0.1.0");
    }

    #[test]
    fn packaged_builds_include_the_source_revision() {
        assert_eq!(format_version("0.1.0", Some("abc123")), "0.1.0-abc123");
    }
}
