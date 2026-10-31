//! `yank status`: what the daemon is doing.

use super::{
    client::{request, unexpected},
    ui,
};
use crate::{
    config::Dirs,
    daemon::control::{ClipInfo, ConnectionStatus, PeerInfo, Request, Response, Route, Status},
};

pub fn run(dirs: &Dirs) -> eyre::Result<()> {
    let response = request(dirs, &Request::Status)?;
    let Response::Status(status) = response else {
        return Err(unexpected(&response));
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

    anstream::println!("\n{}", ui::heading("Clipboard"));
    match &status.clipboard.current {
        Some(clip) => anstream::println!("  {}", current(clip)),
        None => anstream::println!("  {}", ui::dim("empty")),
    }

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

/// What the clipboard holds.
fn current(clip: &ClipInfo) -> String {
    let preview = if clip.secret {
        ui::warn(&clip.preview).to_string()
    } else if clip.files > 0 {
        format!(
            "{} {}",
            clip.preview,
            ui::dim(format_args!(
                "({} file{})",
                clip.files,
                if clip.files == 1 { "" } else { "s" }
            )),
        )
    } else {
        clip.preview.clone()
    };
    let expiry = match clip.expires_in_secs {
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
            clip.origin,
            ui::duration(clip.age_secs),
        )),
    )
}

/// One machine's connection, in a few words.
fn connection(peer: &PeerInfo) -> String {
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
