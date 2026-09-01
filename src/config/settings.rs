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
use color_eyre::eyre::{Result, WrapErr as _};
use serde::Deserialize;

use super::Dirs;

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

# How many entries the shared history keeps. One keeps nothing but what
# is on the clipboard right now.
#history-limit = 100

# Largest single entry that gets shared. Anything above is ignored, and
# 4 MiB is as high as this goes: it is what one entry may weigh on the
# wire.
#max-entry-size = "1 MiB"

# Total size the history may occupy; the oldest entries are dropped first.
#history-budget = "64 MiB"

# Lifetime given to an entry marked secret when none was asked for.
#secret-ttl = "90s"

# Mime types to never capture, on top of the ones yank already refuses.
#ignore-mime = []
"#;

/// The parsed `config.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Settings {
    /// Whether to connect to a compositor at all.
    pub clipboard: bool,
    /// Whether entries from other machines reach this clipboard.
    pub apply: bool,
    /// Whether this machine's clipboard is shared with the others.
    pub capture: bool,
    /// How many entries the history keeps. Read it through
    /// [`Self::history_entries`], which is where the floor of one is
    /// enforced.
    pub(crate) history_limit: usize,
    /// Largest entry accepted, from the local clipboard or from a peer.
    pub max_entry_size: ByteSize,
    /// Total size the history may occupy.
    pub history_budget: ByteSize,
    /// Lifetime given to a secret entry when the caller asks for none.
    #[serde(deserialize_with = "duration")]
    pub secret_ttl: Duration,
    /// Mime types never captured, on top of [`Self::is_ignored_mime`]'s
    /// built-in refusals.
    pub ignore_mime: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            clipboard: true,
            apply: true,
            capture: true,
            history_limit: 100,
            max_entry_size: ByteSize::mib(1),
            history_budget: ByteSize::mib(64),
            secret_ttl: Duration::from_secs(90),
            ignore_mime: Vec::new(),
        }
    }
}

impl Settings {
    /// Loads `config.toml`, treating a missing file as empty. Errors on
    /// unreadable or invalid contents; the caller decides the fallback.
    pub fn load(dirs: &Dirs) -> Result<Self> {
        let path = dirs.settings_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).wrap_err_with(|| format!("cannot read {}", path.display()));
            }
        };

        toml::from_str(&text).wrap_err_with(|| format!("cannot parse {}", path.display()))
    }

    /// Writes the commented template unless the file already exists (a
    /// concurrent writer included: the creation is exclusive).
    pub fn write_template(dirs: &Dirs) -> Result<()> {
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

    /// How many entries to keep, never below one: a history of zero
    /// would drop every entry as it arrived, including the one on the
    /// clipboard, which is not what anyone means by turning history off.
    pub fn history_entries(&self) -> usize {
        self.history_limit.max(1)
    }

    /// The size cap as a plain number, for comparisons against payloads.
    ///
    /// Never above the protocol ceiling: a larger setting would let a
    /// selection past this check only to have the log refuse it, with an
    /// error naming a limit the user never wrote.
    pub fn max_entry_bytes(&self) -> usize {
        usize::try_from(self.max_entry_size.as_u64())
            .unwrap_or(usize::MAX)
            .min(crate::log::MAX_ENTRY_BYTES as usize)
    }

    /// Whether a mime type must never be captured.
    ///
    /// The password-manager hint is how KeePassXC, 1Password and others
    /// ask clipboard managers to leave an entry alone; yank honors it by
    /// marking the entry secret rather than by dropping it (see
    /// [`crate::clip`]), so it is not listed here.
    pub fn is_ignored_mime(&self, mime: &str) -> bool {
        self.ignore_mime.iter().any(|ignored| ignored == mime)
    }
}

/// Reads a duration written the way a person writes one: `90s`, `5m`,
/// `1h 30m`.
fn duration<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
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
        let settings = parse("max-entry-size = \"512 KiB\"\nsecret-ttl = \"2m\"");
        assert_eq!(settings.max_entry_bytes(), 512 * 1024);
        assert_eq!(settings.secret_ttl, Duration::from_mins(2));
    }

    #[test]
    fn an_entry_size_over_the_protocol_ceiling_is_clamped() {
        let settings = parse("max-entry-size = \"1 GiB\"");
        assert_eq!(
            settings.max_entry_bytes(),
            crate::log::MAX_ENTRY_BYTES as usize,
        );
    }

    #[test]
    fn the_history_always_keeps_the_current_entry() {
        assert_eq!(parse("history-limit = 0").history_entries(), 1);
        assert_eq!(parse("history-limit = 5").history_entries(), 5);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Settings>("histroy-limit = 10").is_err());
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
