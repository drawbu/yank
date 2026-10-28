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
