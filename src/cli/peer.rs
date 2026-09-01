//! `yank peer`: the machines the clipboard is shared with.
//!
//! Pairing runs in the daemon, which holds the identity key; hosting only
//! asks it for a ticket and returns. The daemon finishes the pairing on
//! its own once the other machine redeems it.

use clap::{Args, Subcommand};
use color_eyre::eyre::{Result, bail};

use super::{hostname, ui};
use crate::{
    config::Dirs,
    daemon::control::{
        CLIENT_TIMEOUT, PAIR_TICKET_TTL, Request, Response, TICKET_TIMEOUT, request,
    },
    net::pair::PairTicket,
};

/// Budget for redeeming a ticket: the whole exchange with the other
/// machine, which may still be waiting for its relay.
const JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// Manage the machines you share the clipboard with
#[derive(Debug, Args)]
pub struct PeerArgs {
    #[command(subcommand)]
    command: PeerCommand,
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    /// Pair with another machine
    ///
    /// Run this on one machine to print a ticket, then run it again on the
    /// other with the ticket. A ticket works once and expires after a few
    /// minutes.
    ///
    /// A paired machine sees everything you copy, and can pair further
    /// machines itself. Pair only machines you own.
    Add {
        /// Ticket printed by `yank peer add` on the other machine
        ticket: Option<String>,

        /// Name to announce; the hostname by default
        #[arg(long)]
        name: Option<String>,
    },
    /// Stop sharing with a machine
    ///
    /// It stops being told what you copy, and stops being able to reach
    /// this one. Whatever it already has, it keeps.
    #[command(visible_alias = "remove")]
    Rm {
        /// Machine to remove, by name or by the start of its id
        peer: String,
    },
}

pub fn run(args: PeerArgs, dirs: &Dirs) -> Result<()> {
    match args.command {
        PeerCommand::Add { ticket, name } => add(dirs, ticket, name.unwrap_or_else(hostname)),
        PeerCommand::Rm { peer } => rm(dirs, peer),
    }
}

fn add(dirs: &Dirs, ticket: Option<String>, name: String) -> Result<()> {
    match ticket {
        Some(ticket) => join(dirs, ticket, name),
        None => host(dirs, name),
    }
}

/// Asks the daemon for a ticket and prints it. The pairing itself finishes
/// in the daemon, so there is nothing to wait for here.
fn host(dirs: &Dirs, name: String) -> Result<()> {
    let response = request(dirs, &Request::PairHost { name }, TICKET_TIMEOUT)?;
    let Response::PairTicket(ticket) = response else {
        bail!("unexpected answer from the daemon: {response:?}");
    };

    anstream::println!("Run this on the other machine:\n");
    anstream::println!(
        "    {}\n",
        ui::heading(format_args!("yank peer add {ticket}"))
    );
    anstream::println!(
        "{}",
        ui::dim(format_args!(
            "It will see everything you copy. The ticket works once, \
             and expires in {} minutes.",
            PAIR_TICKET_TTL.as_secs() / 60,
        )),
    );

    Ok(())
}

/// Redeems a ticket printed by another machine.
fn join(dirs: &Dirs, ticket: String, name: String) -> Result<()> {
    // Parsed here first, so a mangled paste fails at once instead of after
    // a round trip.
    let _: PairTicket = ticket.parse()?;

    anstream::println!("Reaching the other machine...");
    let response = request(dirs, &Request::PairJoin { ticket, name }, JOIN_TIMEOUT)?;
    let Response::Paired { name, .. } = response else {
        bail!("unexpected answer from the daemon: {response:?}");
    };

    anstream::println!("{}", ui::good(format_args!("Paired with `{name}`")));

    Ok(())
}

fn rm(dirs: &Dirs, peer: String) -> Result<()> {
    let response = request(dirs, &Request::RemovePeer { peer }, CLIENT_TIMEOUT)?;
    let Response::PeerRemoved(endpoint) = response else {
        bail!("unexpected answer from the daemon: {response:?}");
    };

    anstream::println!("{}", ui::good(format_args!("Removed {endpoint}")));
    anstream::println!(
        "{}",
        ui::dim("It will be removed on the other machines as they hear about it."),
    );

    Ok(())
}
