//! `yank status`: what the daemon is doing.

use clap::Args;
use color_eyre::eyre::{Result, bail};

use super::{pause, ui};
use crate::{
    config::Dirs,
    daemon::control::{
        CLIENT_TIMEOUT, ConnectionStatus, HistoryEntry, PeerStatus, Request, Response, Route,
        Status, request,
    },
};

/// Show the machines, the clipboard and the daemon
#[derive(Debug, Args)]
pub struct StatusArgs {}

pub fn run(_args: &StatusArgs, dirs: &Dirs) -> Result<()> {
    let response = request(dirs, &Request::Status, CLIENT_TIMEOUT)?;
    let Response::Status(status) = response else {
        bail!("unexpected answer from the daemon: {response:?}");
    };

    print(&status);

    Ok(())
}

fn print(status: &Status) {
    anstream::println!(
        "{} {}",
        ui::heading("This machine"),
        ui::dim(format_args!("{}", status.endpoint)),
    );
    anstream::println!(
        "  yank {}, up for {}",
        status.version,
        ui::duration(status.uptime_secs),
    );
    match &status.clipboard.backend {
        None => anstream::println!("  {}", ui::good("clipboard connected")),
        Some(reason) => {
            anstream::println!("  {}", ui::warn(format_args!("clipboard down: {reason}")));
        }
    }
    anstream::println!("  {}", pause::describe(status.clipboard.pause));

    anstream::println!("\n{}", ui::heading("Clipboard"));
    match &status.clipboard.selection {
        Some(entry) => anstream::println!("  {}", selection(entry)),
        None => anstream::println!("  {}", ui::dim("empty")),
    }
    anstream::println!(
        "  {}",
        ui::dim(format_args!("{} entries shared", status.clipboard.entries)),
    );

    anstream::println!("\n{}", ui::heading("Machines"));
    if status.peers.is_empty() {
        anstream::println!(
            "  {}",
            ui::dim("none paired yet; run `yank peer add` to add one"),
        );
        return;
    }

    let width = ui::width(status.peers.iter().map(|peer| peer.name.as_str()));
    for peer in &status.peers {
        anstream::println!("  {:width$}  {}", peer.name, connection(peer));
    }
}

/// The entry currently on the clipboard.
fn selection(entry: &HistoryEntry) -> String {
    let preview = if entry.secret {
        ui::warn(&entry.preview).to_string()
    } else {
        entry.preview.clone()
    };
    let expiry = match entry.expires_in_secs {
        Some(secs) => format!(
            " {}",
            ui::warn(format_args!("(gone in {})", ui::duration(secs)))
        ),
        None => String::new(),
    };

    format!(
        "{preview}{expiry} {}",
        ui::dim(format_args!(
            "— {}, {} ago",
            entry.origin,
            ui::duration(entry.age_secs),
        )),
    )
}

/// One machine's connection, in a few words.
fn connection(peer: &PeerStatus) -> String {
    match &peer.connection {
        ConnectionStatus::Connecting => ui::warn("connecting").to_string(),
        ConnectionStatus::Backoff { retry_in_secs } => ui::bad(format_args!(
            "offline, retrying in {}",
            ui::duration(*retry_in_secs),
        ))
        .to_string(),
        ConnectionStatus::Connected { route, since_secs } => {
            let how = match route {
                Some(Route::Direct { addr, rtt_ms }) => format!("direct {addr}, {rtt_ms}ms"),
                Some(Route::Relay { url, rtt_ms }) => format!("relayed {url}, {rtt_ms}ms"),
                None => "connecting a path".to_owned(),
            };

            format!(
                "{} {}",
                ui::good("connected"),
                ui::dim(format_args!("{how}, for {}", ui::duration(*since_secs))),
            )
        }
    }
}
