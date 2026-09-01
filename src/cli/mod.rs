//! The `yank` command.
//!
//! Every command is a request to the daemon over the control socket: the
//! daemon is the one holding the clipboard, the identity key and the mesh
//! state, and having the CLI touch any of them behind its back is how two
//! writers of the same file start disagreeing. The only exceptions are
//! `yank service`, which manages the daemon itself, and `yank daemon`,
//! which *is* the daemon.

mod clip;
mod daemon;
mod pause;
mod peer;
mod service;
mod status;
mod ui;

use std::path::PathBuf;

use clap::{CommandFactory as _, Parser, Subcommand, ValueHint};
use color_eyre::eyre::Result;

use crate::config::Dirs;

/// One clipboard, across your machines
///
/// Machines pair once and then connect straight to each other, with no
/// server in between. What one copies, the others can paste.
///
/// Getting started:
///   1. Install the background service:  yank service install
///   2. Pair with another machine:       yank peer add
///   3. Run the same two there, redeeming the ticket it printed.
///
/// Use `yank status` to see the machines and what the clipboard holds.
#[derive(Debug, Parser)]
#[command(name = "yank", version, verbatim_doc_comment)]
pub struct Cli {
    /// Directory holding the configuration, state and socket
    ///
    /// Only useful to run a second, independent yank on one machine.
    /// Everything normally follows the XDG base directories.
    #[arg(long, short = 'D', global = true, value_name = "DIR", value_hint = ValueHint::DirPath)]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Copy(clip::CopyArgs),
    Paste(clip::PasteArgs),
    #[command(visible_alias = "history")]
    List(clip::ListArgs),
    Pick(clip::PickArgs),
    #[command(visible_alias = "remove")]
    Rm(clip::RmArgs),
    Clear(clip::ClearArgs),
    Pause(pause::PauseArgs),
    Resume(pause::ResumeArgs),
    Status(status::StatusArgs),
    Peer(peer::PeerArgs),
    Service(service::ServiceArgs),
    // Hidden: this is what the installed service runs. Users manage the
    // daemon through `yank service`.
    #[command(hide = true)]
    Daemon(daemon::DaemonArgs),
}

/// Runs the command line.
pub fn run() -> Result<()> {
    // Answers completion requests (`COMPLETE=<shell> yank ...`) and exits;
    // does nothing on a normal invocation. Has to come before anything is
    // parsed or printed.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();
    let dirs = Dirs::new(cli.dir)?;

    match cli.command {
        Command::Copy(args) => clip::copy(args, &dirs),
        Command::Paste(args) => clip::paste(&args, &dirs),
        Command::List(args) => clip::list(&args, &dirs),
        Command::Pick(args) => clip::pick(args, &dirs),
        Command::Rm(args) => clip::rm(args, &dirs),
        Command::Clear(args) => clip::clear(&args, &dirs),
        Command::Pause(args) => pause::pause(&args, &dirs),
        Command::Resume(args) => pause::resume(&args, &dirs),
        Command::Status(args) => status::run(&args, &dirs),
        Command::Peer(args) => peer::run(args, &dirs),
        Command::Service(args) => service::run(args, &dirs),
        Command::Daemon(args) => daemon::run(&args, &dirs),
    }
}

/// Prints a failure. The expected "no daemon" case prints as a plain
/// sentence: not having started it yet is not a bug to report.
pub fn report_error(err: &color_eyre::Report) {
    let message = if err.is::<crate::daemon::control::DaemonNotRunning>() {
        format!("{err:#}")
    } else {
        format!("Error: {err:#}")
    };

    anstream::eprintln!("{}", ui::bad(message));
}

/// This machine's hostname: the name it offers the mesh by default.
fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}
