//! Resolution of the directories `yank` writes to.
//!
//! Three roots, per the XDG base directory specification: config for what
//! the user edits, state for what the daemon owns, runtime for the control
//! socket. Constructing [`Dirs`] guarantees the config and state
//! directories exist, so the files inside are safe to create.
//!
//! A `--dir` override collapses all three into one directory, which keeps
//! several independent daemons (tests, a second identity) from sharing a
//! socket or a history.

use std::{
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr as _, ensure};
use etcetera::BaseStrategy as _;

/// The resolved directories of one `yank` installation.
#[derive(Clone, Debug)]
pub struct Dirs {
    config: PathBuf,
    state: PathBuf,
    /// Where the control socket goes. `None` falls back to the state
    /// directory, which is also what an overridden root does.
    runtime: Option<PathBuf>,
    /// Whether the roots came from `--dir`, which the service installer
    /// must then bake into the service definition.
    custom: bool,
}

impl Dirs {
    /// Resolves the XDG directories, or puts everything under `override_root`
    /// when given. An overridden root is created if missing, like the XDG
    /// ones: it exists to be pointed at an empty path.
    pub fn new(override_root: Option<PathBuf>) -> Result<Self> {
        if let Some(root) = override_root {
            create_private(&root)?;
            ensure!(root.is_dir(), "{} is not a directory", root.display());

            return Ok(Dirs {
                config: root.clone(),
                state: root,
                runtime: None,
                custom: true,
            });
        }

        let base = etcetera::choose_base_strategy()
            .wrap_err("cannot determine the XDG base directories")?;
        // `state_dir` is only `None` on platforms without the notion; on
        // Linux, the one platform we support, it is always set.
        let state = base
            .state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("yank");
        let config = base.config_dir().join("yank");

        create_private(&config)?;
        create_private(&state)?;

        Ok(Dirs {
            config,
            state,
            runtime: base.runtime_dir(),
            custom: false,
        })
    }

    /// Whether the roots were overridden on the command line.
    pub fn is_custom(&self) -> bool {
        self.custom
    }

    /// The root the CLI reports, for messages pointing the user at files.
    pub fn config_root(&self) -> &Path {
        &self.config
    }

    /// The hand-edited settings (`config.toml`).
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// The record of who installed the background service.
    pub fn service_file(&self) -> PathBuf {
        self.config.join("service.toml")
    }

    /// This machine's private identity key.
    pub fn identity_file(&self) -> PathBuf {
        self.state.join("identity.key")
    }

    /// The paired machines.
    pub fn mesh_file(&self) -> PathBuf {
        self.state.join("mesh.json")
    }

    /// Clipboard state that is not an entry: sequence, clock, pause.
    pub fn clip_file(&self) -> PathBuf {
        self.state.join("clip.json")
    }

    /// The directory holding one file per persisted log entry.
    pub fn history_dir(&self) -> PathBuf {
        self.state.join("history")
    }

    /// The control socket the CLI dials.
    pub fn socket_file(&self) -> PathBuf {
        match &self.runtime {
            Some(runtime) => runtime.join("yank.sock"),
            None => self.state.join("yank.sock"),
        }
    }
}

/// Creates a directory and every missing parent, readable by its owner
/// only: these hold the identity key and the clipboard history.
pub fn create_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .wrap_err_with(|| format!("cannot create {}", path.display()))
}

/// Writes a file atomically and owner-only, by writing a sibling and
/// renaming it into place: a crash mid-write must never leave a truncated
/// file where a valid one was.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};

    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };

    write().wrap_err_with(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path).wrap_err_with(|| format!("cannot write {}", path.display()))
}
