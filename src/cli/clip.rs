//! The clipboard commands: copying, pasting, and the shared history.

use std::{
    io::{IsTerminal as _, Read as _, Write as _},
    time::Duration,
};

use clap::Args;
use color_eyre::eyre::{Result, bail};

use super::ui;
use crate::{
    clip::mime::PLAIN_TEXT,
    config::Dirs,
    daemon::control::{CLIENT_TIMEOUT, HistoryEntry, Request, Response, request},
};

/// Copy something to every machine
///
/// Reads standard input when no text is given, so it works in a pipe the
/// way `wl-copy` does.
#[derive(Debug, Args)]
pub struct CopyArgs {
    /// Text to copy; standard input is read when this is left out
    #[arg(trailing_var_arg = true)]
    text: Vec<String>,

    /// Drop it everywhere after this long, for example `90s` or `5m`
    ///
    /// Every machine removes it on its own when the time is up, network or
    /// no network, and empties the clipboard if it is still what it holds.
    #[arg(long, short = 't', value_name = "DURATION", value_parser = parse_duration)]
    ttl: Option<Duration>,

    /// Treat it as a password
    ///
    /// It is shared, but never written to disk on any machine, never shown
    /// in the history, and given a lifetime (`secret-ttl` in config.toml)
    /// unless `--ttl` says otherwise.
    #[arg(long, short = 's')]
    secret: bool,

    /// Type to copy it as
    #[arg(long, value_name = "MIME", default_value = PLAIN_TEXT)]
    mime: String,
}

/// Print what is on the clipboard
#[derive(Debug, Args)]
pub struct PasteArgs {
    /// Entry to print, from `yank list`; the current one by default
    entry: Option<String>,

    /// Print the types the entry carries instead of the contents
    #[arg(long)]
    mime: bool,

    /// Type to print it in, when the entry carries several
    #[arg(long = "type", value_name = "MIME")]
    mime_type: Option<String>,
}

/// List the shared history, newest first
#[derive(Debug, Args)]
pub struct ListArgs {
    /// How many entries to show
    #[arg(long, short = 'n', value_name = "COUNT")]
    limit: Option<usize>,

    /// Print only the entry names, one per line, for scripts and pickers
    #[arg(long)]
    plain: bool,
}

/// Put an entry from the history back on the clipboard
#[derive(Debug, Args)]
pub struct PickArgs {
    /// Entry to pick, from `yank list`
    entry: String,
}

/// Remove an entry from every machine
#[derive(Debug, Args)]
pub struct RmArgs {
    /// Entry to remove, from `yank list`
    entry: String,
}

/// Empty the clipboard on every machine
#[derive(Debug, Args)]
pub struct ClearArgs {
    /// Drop the whole shared history as well
    #[arg(long)]
    history: bool,
}

pub fn copy(args: CopyArgs, dirs: &Dirs) -> Result<()> {
    let bytes = if args.text.is_empty() {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes)?;
        bytes
    } else {
        args.text.join(" ").into_bytes()
    };

    let response = request(
        dirs,
        &Request::Copy {
            mime: args.mime,
            bytes,
            secret: args.secret,
            ttl_secs: args.ttl.map(|ttl| ttl.as_secs()),
        },
        CLIENT_TIMEOUT,
    )?;

    match response {
        Response::Wrote { label } => {
            anstream::println!("{}", ui::good(format_args!("Copied as {label}")));
            Ok(())
        }
        other => unexpected(&other),
    }
}

pub fn paste(args: &PasteArgs, dirs: &Dirs) -> Result<()> {
    let response = request(
        dirs,
        &Request::Paste {
            entry: args.entry.clone(),
            mime: args.mime_type.clone(),
        },
        CLIENT_TIMEOUT,
    )?;

    match response {
        Response::Contents {
            mime,
            alternates,
            bytes,
        } => {
            if args.mime {
                for mime in std::iter::once(mime).chain(alternates) {
                    anstream::println!("{mime}");
                }
                return Ok(());
            }

            let mut out = std::io::stdout().lock();
            out.write_all(&bytes)?;
            // A terminal wants the prompt back on its own line; a pipe
            // wants exactly the bytes that were copied.
            if out.is_terminal() && !bytes.ends_with(b"\n") {
                out.write_all(b"\n")?;
            }
            out.flush()?;

            Ok(())
        }
        other => unexpected(&other),
    }
}

pub fn list(args: &ListArgs, dirs: &Dirs) -> Result<()> {
    let response = request(
        dirs,
        &Request::History { limit: args.limit },
        CLIENT_TIMEOUT,
    )?;
    let Response::History(entries) = response else {
        return unexpected(&response);
    };

    if entries.is_empty() {
        anstream::println!("{}", ui::dim("The shared history is empty."));
        return Ok(());
    }
    if args.plain {
        for entry in &entries {
            anstream::println!("{}", entry.label);
        }
        return Ok(());
    }

    let labels = ui::width(entries.iter().map(|entry| entry.label.as_str()));
    let origins = ui::width(entries.iter().map(|entry| entry.origin.as_str()));
    for entry in &entries {
        anstream::println!(
            "{} {:labels$} {:origins$} {} {}",
            if entry.selected { "*" } else { " " },
            ui::heading(&entry.label),
            ui::dim(&entry.origin),
            ui::dim(format_args!("{:>4}", ui::duration(entry.age_secs))),
            preview(entry),
        );
    }

    Ok(())
}

pub fn pick(args: PickArgs, dirs: &Dirs) -> Result<()> {
    let response = request(dirs, &Request::Pick { entry: args.entry }, CLIENT_TIMEOUT)?;

    match response {
        Response::Wrote { label } => {
            anstream::println!("{}", ui::good(format_args!("Copied as {label}")));
            Ok(())
        }
        other => unexpected(&other),
    }
}

pub fn rm(args: RmArgs, dirs: &Dirs) -> Result<()> {
    let response = request(dirs, &Request::Forget { entry: args.entry }, CLIENT_TIMEOUT)?;

    match response {
        Response::Wrote { label } => {
            anstream::println!("{}", ui::good(format_args!("Removed {label} everywhere")));
            Ok(())
        }
        other => unexpected(&other),
    }
}

pub fn clear(args: &ClearArgs, dirs: &Dirs) -> Result<()> {
    let response = request(
        dirs,
        &Request::Clear {
            history: args.history,
        },
        CLIENT_TIMEOUT,
    )?;

    match response {
        Response::Cleared if args.history => {
            anstream::println!(
                "{}",
                ui::good("Cleared the clipboard and history everywhere")
            );
            Ok(())
        }
        Response::Cleared => {
            anstream::println!("{}", ui::good("Cleared the clipboard everywhere"));
            Ok(())
        }
        other => unexpected(&other),
    }
}

/// One entry's contents, with its lifetime when it has one.
fn preview(entry: &HistoryEntry) -> String {
    let preview = if entry.secret {
        ui::warn(&entry.preview).to_string()
    } else {
        entry.preview.clone()
    };

    match entry.expires_in_secs {
        Some(secs) => format!(
            "{preview} {}",
            ui::warn(format_args!("(gone in {})", ui::duration(secs))),
        ),
        None => preview,
    }
}

/// Parses `--ttl`, accepting what `humantime` accepts: `90s`, `5m`, `1h`.
fn parse_duration(text: &str) -> Result<Duration, String> {
    humantime::parse_duration(text).map_err(|err| err.to_string())
}

fn unexpected(response: &Response) -> Result<()> {
    bail!("unexpected answer from the daemon: {response:?}")
}
