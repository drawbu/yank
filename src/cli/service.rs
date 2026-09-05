//! `yank service`: the daemon as a systemd user service.
//!
//! A user service, not a system one: the daemon needs the session to reach
//! the compositor, and the clipboard belongs to the session.
//!
//! It is deliberately not tied to `graphical-session.target`. The daemon
//! keeps replicating with no compositor in sight and reconnects when one
//! appears, so binding it to the session would only make it miss the
//! entries copied elsewhere while the screen was locked or the session was
//! restarting.
//!
//! The unit is written here rather than by a service-manager crate: yank
//! runs on systemd only, the file is a dozen lines, and writing it
//! ourselves is what lets the paths in it be quoted properly.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use clap::{Args, Subcommand};
use color_eyre::eyre::{Result, WrapErr as _, bail, ensure};

use super::ui;
use crate::config::{Dirs, ServiceState};

/// Name of the unit this command writes.
const UNIT: &str = "yank.service";

/// Recorded as the installer, so a unit written here is recognizable as
/// ours later (see [`ensure_ours`]).
const LABEL: &str = "yank";

/// Where systemd looks for user units that are not the user's own, in the
/// order it searches them. A unit there belongs to whoever configured the
/// system, and ours would shadow it rather than replace it.
const SYSTEM_UNIT_DIRS: &[&str] = &[
    "/etc/systemd/user",
    "/run/systemd/user",
    "/usr/local/lib/systemd/user",
    "/usr/lib/systemd/user",
];

/// How long to let a restarted daemon settle before checking it is still
/// there. `systemd` calls a `Type=simple` unit started as soon as the
/// process exists, so a daemon that exits at once — a bad binary, or
/// another instance holding the lock — would otherwise look fine.
const SETTLE: Duration = Duration::from_secs(1);

/// Install and manage the background service
///
/// yank needs a daemon running to hold the connections to the other
/// machines and watch the clipboard. These commands install it as a
/// systemd user service.
///
/// To run it yourself instead, run `yank daemon`.
#[derive(Debug, Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    command: ServiceCommand,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Write the unit and start it
    Install {
        /// Path written into the unit; the running binary by default
        ///
        /// Give a stable path when the binary moves between updates, for
        /// example `~/.nix-profile/bin/yank`.
        #[arg(long, value_name = "PATH")]
        program: Option<PathBuf>,
    },
    /// Stop the service and remove the unit
    Uninstall,
    /// Start the service
    Start,
    /// Stop the service
    Stop,
    /// Restart the service, to pick up a changed config.toml
    Restart,
}

pub fn run(args: ServiceArgs, dirs: &Dirs) -> Result<()> {
    let state = ServiceState::load(dirs)?;

    match args.command {
        ServiceCommand::Install { program } => {
            ensure_ours(state.as_ref())?;
            install(dirs, program)
        }
        ServiceCommand::Uninstall => {
            ensure_ours(state.as_ref())?;
            uninstall(dirs)
        }
        ServiceCommand::Start => {
            systemctl(&["start", UNIT])?;
            anstream::println!("{}", ui::good("Started"));
            Ok(())
        }
        ServiceCommand::Stop => {
            systemctl(&["stop", UNIT])?;
            anstream::println!("{}", ui::good("Stopped"));
            Ok(())
        }
        ServiceCommand::Restart => restart(),
    }
}

/// Refuses to touch a service this command does not own.
///
/// Three ways to not own one: the record names another installer, our own
/// unit file is a symlink (how Nix installs one into the user's
/// directory), or the system already provides a unit under the same name,
/// which ours would shadow rather than replace.
fn ensure_ours(state: Option<&ServiceState>) -> Result<()> {
    if let Some(state) = state {
        ensure!(
            state.installer == ServiceState::CLI,
            "the service is managed by {}: change that configuration instead",
            state.installer,
        );

        return Ok(());
    }

    let path = unit_path()?;
    ensure!(
        !path.is_symlink(),
        "{} is a symlink, so something else manages the service: \
         refusing to replace it",
        path.display(),
    );

    if let Some(system) = SYSTEM_UNIT_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(UNIT))
        .find(|path| path.exists())
    {
        bail!(
            "{} already provides this service: installing would shadow it, \
             not replace it. Change that configuration instead.",
            system.display(),
        );
    }

    Ok(())
}

/// Writes the unit and starts it.
fn install(dirs: &Dirs, program: Option<PathBuf>) -> Result<()> {
    let program = match program {
        Some(program) => program,
        None => std::env::current_exe().wrap_err("cannot find the yank binary")?,
    };
    ensure!(
        program.is_absolute(),
        "the program path must be absolute: {program:?}",
    );

    // A custom directory has to be baked into the unit; the default one is
    // resolved by the daemon itself.
    let mut command = vec![quote(program.as_os_str())?];
    if dirs.is_custom() {
        command.push("--dir".to_owned());
        command.push(quote(dirs.config_root().as_os_str())?);
    }
    let path = unit_path()?;
    let parent = path.parent().expect("the unit path has a directory");
    std::fs::create_dir_all(parent)
        .wrap_err_with(|| format!("cannot create {}", parent.display()))?;
    std::fs::write(&path, unit(&command.join(" ")))
        .wrap_err_with(|| format!("cannot write {}", path.display()))?;

    // systemd keeps serving the unit it cached until told otherwise, so a
    // reinstall would otherwise still run the old command line.
    systemctl(&["daemon-reload"])?;
    ServiceState::record_cli(dirs, LABEL)?;
    systemctl(&["enable", "--now", UNIT])
        .wrap_err("wrote the unit, but cannot start the service")?;

    anstream::println!("Wrote {}", path.display());
    anstream::println!("{}", ui::good("Service installed and started"));

    Ok(())
}

/// Stops the service and removes the unit.
fn uninstall(dirs: &Dirs) -> Result<()> {
    // Ignored: disabling a service that is not installed fails, and the
    // point here is to end up with it gone either way.
    let _ = systemctl(&["disable", "--now", UNIT]);

    let path = unit_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).wrap_err_with(|| format!("cannot remove {}", path.display()));
        }
    }
    systemctl(&["daemon-reload"])?;
    ServiceState::clear(dirs)?;

    anstream::println!("{}", ui::good(format_args!("Removed {}", path.display())));
    anstream::println!(
        "{}",
        ui::dim("The clipboard history and the paired machines are left alone."),
    );

    Ok(())
}

/// Restarts the service and checks it is still running afterwards.
fn restart() -> Result<()> {
    systemctl(&["restart", UNIT])?;
    std::thread::sleep(SETTLE);

    ensure!(
        Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", UNIT])
            .status()
            .wrap_err("cannot run systemctl")?
            .success(),
        "the service started and then died; see `journalctl --user -u {UNIT}`",
    );
    anstream::println!("{}", ui::good("Restarted"));

    Ok(())
}

/// The unit file, with `command` as its `ExecStart`.
fn unit(command: &str) -> String {
    format!(
        "# Written by `yank service install`. Anything you change here is\n\
         # lost on the next install; there is `yank service install --program`\n\
         # for the one thing worth changing.\n\
         \n\
         [Unit]\n\
         Description=yank clipboard daemon\n\
         # Wanted, not required: with no compositor the daemon still\n\
         # replicates, and it connects to one as soon as there is one.\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={command} daemon\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         Environment=RUST_LOG=yank=info\n\
         # The daemon holds clipboard contents, which may be passwords.\n\
         LimitCORE=0\n\
         NoNewPrivileges=true\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    )
}

/// Quotes one argument of an `ExecStart` line.
///
/// systemd expands `%` specifiers over the whole value before splitting it
/// into arguments, and splits on unquoted whitespace, so a path with a
/// space or a percent sign would otherwise become two arguments or an
/// entirely different path. A newline cannot be represented at all, since
/// it would end the directive.
fn quote(arg: &OsStr) -> Result<String> {
    let text = arg
        .to_str()
        .ok_or_else(|| color_eyre::eyre::eyre!("{arg:?} is not valid UTF-8"))?;
    ensure!(
        !text.chars().any(char::is_control),
        "{arg:?} contains control characters, which a unit file cannot hold",
    );

    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');

    Ok(quoted)
}

/// Runs one `systemctl --user` command, reporting what it said when it
/// fails.
fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .wrap_err("cannot run systemctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            crate::config::sanitize_bounded(stderr.trim()),
        );
    }

    Ok(())
}

/// Where the user's own units live.
fn unit_path() -> Result<PathBuf> {
    use etcetera::BaseStrategy as _;

    Ok(etcetera::choose_base_strategy()
        .wrap_err("cannot determine the XDG base directories")?
        .config_dir()
        .join("systemd")
        .join("user")
        .join(UNIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_survive_the_unit_file() {
        assert_eq!(
            quote(OsStr::new("/usr/bin/yank")).unwrap(),
            "\"/usr/bin/yank\""
        );
        // A space would split the argument in two, and a percent sign is
        // where systemd substitutes something else entirely.
        assert_eq!(
            quote(OsStr::new("/opt/my apps/yank")).unwrap(),
            "\"/opt/my apps/yank\"",
        );
        assert_eq!(quote(OsStr::new("/o%h/yank")).unwrap(), "\"/o%%h/yank\"");
        assert_eq!(quote(OsStr::new(r#"/a"b\c"#)).unwrap(), r#""/a\"b\\c""#);

        assert!(quote(OsStr::new("/a\nb")).is_err());
    }

    /// The generated unit has to be a unit: one directive per line, and
    /// every section systemd needs to enable and run it.
    #[test]
    fn the_unit_is_well_formed() {
        let unit = unit(&quote(OsStr::new("/usr/bin/yank")).unwrap());

        assert!(unit.contains("[Unit]"));
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains("ExecStart=\"/usr/bin/yank\" daemon"));
        assert!(unit.lines().all(|line| !line.contains('\r')));
    }
}
