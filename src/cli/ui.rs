//! Shared presentation for the CLI.
//!
//! Styling is `anstyle`, the vocabulary `clap` itself is built on, printed
//! through `anstream`, which strips the sequences when the output is not a
//! terminal or `NO_COLOR` is set. Commands go through the wrappers below
//! rather than styling by hand, so the palette stays in one place.
//!
//! Machine names and entry previews are cleaned up at the daemon boundary
//! and print as they are; anything else coming from a peer or the daemon
//! has to pass through [`crate::config::sanitize`] first.

use std::fmt::{self, Display};

use anstyle::{AnsiColor, Style};

/// A section heading.
pub fn heading<D: Display>(text: D) -> Painted<D> {
    paint(Style::new().bold(), text)
}

/// Healthy, or done.
pub fn good<D: Display>(text: D) -> Painted<D> {
    paint(AnsiColor::Green.on_default(), text)
}

/// Worth a look, without being broken.
pub fn warn<D: Display>(text: D) -> Painted<D> {
    paint(AnsiColor::Yellow.on_default(), text)
}

/// Broken.
pub fn bad<D: Display>(text: D) -> Painted<D> {
    paint(AnsiColor::Red.on_default(), text)
}

/// Detail, secondary to what it sits beside.
pub fn dim<D: Display>(text: D) -> Painted<D> {
    paint(Style::new().dimmed(), text)
}

/// Something to print in a style.
///
/// A wrapper rather than a pair of escape sequences around a `format!`, so
/// a caller cannot forget the reset and colour the rest of the line.
#[derive(Debug)]
pub struct Painted<D> {
    style: Style,
    value: D,
}

fn paint<D: Display>(style: Style, value: D) -> Painted<D> {
    Painted { style, value }
}

impl<D: Display> Display for Painted<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `{:#}` is how anstyle renders the reset for a style.
        write!(f, "{}{}{:#}", self.style, self.value, self.style)
    }
}

/// A duration in seconds, short enough for a column: `43s`, `12m`, `2h
/// 4m`, `3d`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d", s / 86400),
    }
}

/// The width of the longest name, for lining columns up. Counts
/// characters, to match what `{:width$}` pads.
pub fn width<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    names.map(|name| name.chars().count()).max().unwrap_or(0)
}
