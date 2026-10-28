//! `yank daemon`: run the daemon in the foreground.
//!
//! What the installed service runs, and what to run by hand with
//! `RUST_LOG=yank=debug` when something needs looking at.

use std::io::IsTerminal as _;

use crate::{config::Dirs, daemon};

pub fn run(dirs: &Dirs) -> eyre::Result<()> {
    tracing_subscriber::fmt()
        // Under the service, stdout is the journal: no colour codes there.
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive("yank=info".parse()?)
                .from_env_lossy(),
        )
        .init();

    tokio::runtime::Runtime::new()?.block_on(daemon::run(dirs))
}
