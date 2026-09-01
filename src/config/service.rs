//! `service.toml`: which program installed the background service.

use std::{fs, io::ErrorKind};

use color_eyre::eyre::{Result, WrapErr as _};
use serde::{Deserialize, Serialize};

use super::Dirs;

/// The recorded installation of the daemon service.
///
/// `yank service install` records itself as [`Self::CLI`]; external
/// managers (the Nix module) record their own name, which is what stops
/// the CLI from replacing a declaratively managed unit. A missing file
/// means nothing was recorded.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceState {
    pub installer: String,
    pub label: String,
}

impl ServiceState {
    /// Installer recorded by `yank service install`.
    pub const CLI: &str = "cli";

    /// Reads the record; `None` when there is none.
    pub fn load(dirs: &Dirs) -> Result<Option<Self>> {
        let path = dirs.service_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).wrap_err_with(|| format!("cannot read {}", path.display()));
            }
        };

        toml::from_str(&text)
            .map(Some)
            .wrap_err_with(|| format!("cannot parse {}", path.display()))
    }

    /// Records `label` as installed by the CLI.
    pub fn record_cli(dirs: &Dirs, label: &str) -> Result<()> {
        let state = Self {
            installer: Self::CLI.to_owned(),
            label: label.to_owned(),
        };
        let path = dirs.service_file();

        fs::write(
            &path,
            toml::to_string(&state).expect("state must serialize"),
        )
        .wrap_err_with(|| format!("cannot write {}", path.display()))
    }

    /// Removes the record; a missing file is fine.
    pub fn clear(dirs: &Dirs) -> Result<()> {
        let path = dirs.service_file();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).wrap_err_with(|| format!("cannot remove {}", path.display())),
        }
    }
}
