//! `config.toml`: the only file the user edits.
//!
//! Every key is optional and defaults in code, so a missing or empty file
//! is valid; a commented template documenting the defaults is written on
//! first start. Parsing is strict (an unknown key is an error), because a
//! typoed key silently doing nothing is worse than a load failure, which
//! the daemon reports and survives by falling back to the defaults.
//!
//! The daemon reads this once at startup: edits apply on the next restart.

use std::{fs, io::ErrorKind, time::Duration};

use bytesize::ByteSize;
use eyre::WrapErr as _;
use serde::Deserialize;

use super::{Dirs, read_existing};

/// Template written to a missing `config.toml`. Keys stay commented out:
/// the file documents the defaults without freezing them.
const TEMPLATE: &str = r#"# yank configuration
#
# The commented values below are the defaults. Restart the background
# service after editing this file.

# Talk to the compositor at all. Turn this off on a machine with no
# graphical session: yank still replicates, and `yank copy` and
# `yank paste` still work.
#clipboard = true

# Put what other machines copy on this machine's clipboard.
#apply = true

# Share what is copied on this machine with the others.
#capture = true

# Largest selection that gets shared, every type it is carried in
# together. The types that do not fit are dropped, best kept first, and a
# selection whose first type is over on its own is not shared at all.
# 4 MiB is as high as this goes: it is what a copy may weigh on the wire.
#max-size = "1 MiB"

# Lifetime given to a copy marked secret when none was asked for.
#secret-ttl = "90s"

# Mime types to never capture, on top of the ones yank already refuses.
#ignore-mime = []

# Whether copying files copies the files themselves. Turned off, a file
# copied here reaches the other machines as the text of its path, which is
# a path they do not have.
#files = true

# Largest file copy that gets shared, every file of it together. A copy
# over it is shared as text.
#max-file-size = "1 GiB"
"#;

/// The parsed `config.toml`.
///
/// Every switch here is a direction something may or may not go in, and
/// they are independent of each other; there is no state machine hiding
/// behind them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Settings {
    /// Whether to connect to a compositor at all.
    pub clipboard: bool,
    /// Whether copies from other machines reach this clipboard.
    pub apply: bool,
    /// Whether this machine's clipboard is shared with the others.
    pub capture: bool,
    /// Largest selection shared from this machine.
    pub max_size: ByteSize,
    /// Lifetime given to a secret copy when the caller asks for none.
    #[serde(deserialize_with = "duration")]
    pub secret_ttl: Duration,
    /// Mime types never captured, on top of what
    /// [`crate::clip::mime`] refuses on its own. A selection whose best
    /// type is one of these is dropped whole, and the backend reads none
    /// of it (see [`crate::clip::Policy`]).
    ///
    /// The password-manager hint is how KeePassXC, 1Password and others
    /// ask clipboard managers to leave a selection alone; yank honors it by
    /// marking the copy secret rather than by dropping it, so it is not
    /// listed here.
    pub ignore_mime: Vec<String>,
    /// Whether a copied file is shared as its contents rather than as the
    /// text of its path.
    pub files: bool,
    /// Largest file copy shared, every file of it together.
    pub max_file_size: ByteSize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            clipboard: true,
            apply: true,
            capture: true,
            max_size: ByteSize::mib(1),
            secret_ttl: Duration::from_secs(90),
            ignore_mime: Vec::new(),
            files: true,
            max_file_size: ByteSize::gib(1),
        }
    }
}

impl Settings {
    /// Loads `config.toml`, treating a missing file as empty. Errors on
    /// unreadable or invalid contents; the caller decides the fallback.
    pub fn load(dirs: &Dirs) -> eyre::Result<Self> {
        let path = dirs.settings_file();
        let Some(text) = read_existing(&path, fs::read_to_string)? else {
            return Ok(Self::default());
        };

        toml::from_str(&text).wrap_err_with(|| format!("cannot parse {}", path.display()))
    }

    /// Writes the commented template unless the file already exists (a
    /// concurrent writer included: the creation is exclusive).
    pub fn write_template(dirs: &Dirs) -> eyre::Result<()> {
        use std::io::Write as _;

        let path = dirs.settings_file();
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => file
                .write_all(TEMPLATE.as_bytes())
                .wrap_err_with(|| format!("cannot write {}", path.display())),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(err) => Err(err).wrap_err_with(|| format!("cannot create {}", path.display())),
        }
    }

    /// The size cap as a plain number, never above the protocol ceiling:
    /// a larger setting would let a selection past this check only to have
    /// every peer refuse it.
    pub fn max_bytes(&self) -> usize {
        usize::try_from(self.max_size.as_u64())
            .unwrap_or(usize::MAX)
            .min(crate::clip::MAX_SIZE as usize)
    }
}

/// Reads a duration written the way a person writes one: `90s`, `5m`,
/// `1h 30m`.
fn duration<'de, D: serde::Deserializer<'de>>(deserializer: D) -> eyre::Result<Duration, D::Error> {
    use serde::de::Error as _;

    let text = String::deserialize(deserializer)?;

    humantime::parse_duration(&text).map_err(D::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Settings {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn an_empty_file_yields_the_defaults() {
        assert_eq!(parse(""), Settings::default());
    }

    #[test]
    fn units_are_spelled_out() {
        let settings = parse("max-size = \"512 KiB\"\nsecret-ttl = \"2m\"");
        assert_eq!(settings.max_bytes(), 512 * 1024);
        assert_eq!(settings.secret_ttl, Duration::from_mins(2));
    }

    #[test]
    fn a_size_over_the_protocol_ceiling_is_clamped() {
        let settings = parse("max-size = \"1 GiB\"");
        assert_eq!(settings.max_bytes(), crate::clip::MAX_SIZE as usize);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Settings>("max-szie = 10").is_err());
        assert!(toml::from_str::<Settings>("apply = \"yes\"").is_err());
    }

    /// The template must parse as-is (everything commented, so all
    /// defaults) and must still parse once its keys are uncommented: it
    /// cannot drift away from the schema unnoticed.
    #[test]
    fn the_template_matches_the_schema() {
        assert_eq!(parse(TEMPLATE), Settings::default());

        let mut uncommented = String::new();
        for line in TEMPLATE.lines() {
            let key = line
                .strip_prefix('#')
                .filter(|rest| !rest.is_empty() && !rest.starts_with([' ', '#']));
            uncommented.push_str(key.unwrap_or(line));
            uncommented.push('\n');
        }
        assert_eq!(parse(&uncommented), Settings::default());
    }
}
