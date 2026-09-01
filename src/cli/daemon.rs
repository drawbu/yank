//! `yank daemon`: run the daemon in the foreground.
//!
//! What the installed service runs, and what to run by hand with
//! `RUST_LOG=yank=debug` when something needs looking at.

use clap::Args;
use color_eyre::eyre::Result;

use crate::{config::Dirs, daemon};

/// Run the daemon in the foreground
#[derive(Debug, Args)]
pub struct DaemonArgs {}

pub fn run(_args: &DaemonArgs, dirs: &Dirs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive("yank=info".parse()?)
                .from_env_lossy(),
        )
        .init();

    tokio::runtime::Runtime::new()?.block_on(daemon::run(dirs))
}
