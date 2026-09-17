//! Command-line interface.

mod clip;
mod daemon;
mod pause;
mod peer;
mod service;
mod status;
mod ui;

use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::{CommandFactory as _, Parser, Subcommand, ValueHint};

use crate::config::Dirs;

/// A peer-to-peer clipboard daemon
///
/// Replicates the clipboard, and a bounded history of it, across the
/// machines you own. They pair once and connect directly from then on,
/// with no server in between.
///
/// Getting started:
///   1. Install the background service:  yank service install
///   2. Pair with another machine:       yank peer add
///   3. Run the same two there, redeeming the ticket it printed.
///
/// Use `yank status` to see the machines and what the clipboard holds.
#[derive(Debug, Parser)]
#[command(name = "yank", version = crate::VERSION.as_str(), verbatim_doc_comment)]
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
    Get(clip::GetArgs),
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
pub fn run() -> eyre::Result<()> {
    // Answers completion requests (`COMPLETE=<shell> yank ...`) and exits;
    // does nothing on a normal invocation. Has to come before anything is
    // parsed or printed.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();
    let dirs = Dirs::new(cli.dir)?;

    match cli.command {
        Command::Copy(args) => clip::copy(args, &dirs),
        Command::Paste(args) => clip::paste(&args, &dirs),
        Command::Get(args) => clip::get(&args, &dirs),
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

/// Outcomes that are situations to act on, not failures to debug.
///
/// The CLI prints them as a plain sentence, without the error dressing,
/// and exits with the sysexits code that names the situation.
#[derive(Debug, thiserror::Error)]
pub enum Situation {
    #[error("The yank daemon is not running. Start it with `yank service start`.")]
    DaemonNotRunning,
    #[error("The files of {label} have not arrived; the daemon is still trying.")]
    FilesPending { label: String },
}

impl Situation {
    fn exit_code(&self) -> ExitCode {
        const EX_UNAVAILABLE: u8 = 69;
        const EX_TEMPFAIL: u8 = 75;

        match self {
            Situation::DaemonNotRunning => ExitCode::from(EX_UNAVAILABLE),
            Situation::FilesPending { .. } => ExitCode::from(EX_TEMPFAIL),
        }
    }
}

/// Prints a failure and returns the exit code it deserves.
pub fn report_error(err: &eyre::Report) -> ExitCode {
    let (message, code) = match err.downcast_ref::<Situation>() {
        Some(situation) => (format!("{err:#}"), situation.exit_code()),
        None => (format!("Error: {err:#}"), ExitCode::FAILURE),
    };

    anstream::eprintln!("{}", ui::bad(message));
    code
}

fn parse_duration(text: &str) -> eyre::Result<Duration, String> {
    humantime::parse_duration(text).map_err(|err| err.to_string())
}

/// This machine's hostname: the name it offers the mesh by default.
fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}
