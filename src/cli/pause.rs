//! `yank pause` and `yank resume`.
//!
//! The two directions are separate because the reasons to stop them are:
//! sharing what you copy here is a privacy question, and taking what
//! others copy is an interruption question. Naming neither means both.

use std::time::{Duration, SystemTime};

use clap::Args;
use color_eyre::eyre::{Result, bail};

use super::{parse_duration, ui};
use crate::{
    clip::{Pause, Switch},
    config::Dirs,
    daemon::control::{CLIENT_TIMEOUT, Request, Response, request},
};

/// Stop sharing the clipboard, in one direction or both
#[derive(Debug, Args)]
pub struct PauseArgs {
    /// Stop sharing what is copied here
    #[arg(long)]
    capture: bool,

    /// Stop taking what other machines copy
    #[arg(long)]
    apply: bool,

    /// Resume on its own after this long, for example `30m`
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    r#for: Option<Duration>,
}

/// Start sharing the clipboard again
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Resume sharing what is copied here
    #[arg(long)]
    capture: bool,

    /// Resume taking what other machines copy
    #[arg(long)]
    apply: bool,
}

pub fn pause(args: &PauseArgs, dirs: &Dirs) -> Result<()> {
    let until = args.r#for.map(|delay| SystemTime::now() + delay);
    let switch = Switch::Paused { until };
    let (capture, apply) = directions(args.capture, args.apply, switch);

    apply_pause(dirs, capture, apply)
}

pub fn resume(args: &ResumeArgs, dirs: &Dirs) -> Result<()> {
    let (capture, apply) = directions(args.capture, args.apply, Switch::On);

    apply_pause(dirs, capture, apply)
}

/// Which directions the flags name. Naming none means both, which is what
/// a bare `yank pause` should do.
fn directions(capture: bool, apply: bool, switch: Switch) -> (Option<Switch>, Option<Switch>) {
    if !capture && !apply {
        return (Some(switch), Some(switch));
    }

    (capture.then_some(switch), apply.then_some(switch))
}

fn apply_pause(dirs: &Dirs, capture: Option<Switch>, apply: Option<Switch>) -> Result<()> {
    let response = request(dirs, &Request::SetPause { capture, apply }, CLIENT_TIMEOUT)?;
    let Response::Paused(pause) = response else {
        bail!("unexpected answer from the daemon: {response:?}");
    };

    anstream::println!("{}", describe(pause));

    Ok(())
}

/// One line saying what is running now.
pub fn describe(pause: Pause) -> String {
    let now = SystemTime::now();
    match (pause.capture.is_on(now), pause.apply.is_on(now)) {
        (true, true) => ui::good("Sharing in both directions").to_string(),
        (false, false) => {
            ui::warn(format_args!("Paused{}", resumes(pause.capture, now))).to_string()
        }
        (true, false) => ui::warn(format_args!(
            "Sharing what is copied here, not taking what others copy{}",
            resumes(pause.apply, now),
        ))
        .to_string(),
        (false, true) => ui::warn(format_args!(
            "Taking what others copy, not sharing what is copied here{}",
            resumes(pause.capture, now),
        ))
        .to_string(),
    }
}

/// When a paused direction comes back on its own.
fn resumes(switch: Switch, now: SystemTime) -> String {
    match switch {
        Switch::Paused { until: Some(until) } => format!(
            " (resumes in {})",
            ui::duration(until.duration_since(now).unwrap_or_default().as_secs()),
        ),
        _ => String::new(),
    }
}
