//! Resolution of the directories `yank` writes to.
//!
//! Three roots, per the XDG base directory specification: config for what
//! the user edits, state for what the daemon owns, runtime for the control
//! socket. Constructing [`Dirs`] guarantees the config and state
//! directories exist, so the files inside are safe to create.
//!
//! A `--dir` override collapses all three into one directory, which keeps
//! several independent daemons (tests, a second identity) from sharing a
//! socket or a clipboard.

use std::{
    fs,
    path::{Path, PathBuf},
};

use etcetera::BaseStrategy as _;
use eyre::WrapErr as _;

/// The resolved directories of one `yank` installation.
#[derive(Clone, Debug)]
pub struct Dirs {
    config: PathBuf,
    state: PathBuf,
    /// Where the control socket goes: the state directory when there is
    /// no runtime one.
    runtime: PathBuf,
    /// The `--dir` override, which the service installer must then bake
    /// into the service definition.
    root: Option<PathBuf>,
}

impl Dirs {
    /// Resolves the XDG directories, or puts everything under `override_root`
    /// when given. An overridden root is created if missing, like the XDG
    /// ones: it exists to be pointed at an empty path.
    pub fn new(override_root: Option<PathBuf>) -> eyre::Result<Self> {
        if let Some(root) = override_root {
            create_private(&root)?;
            eyre::ensure!(root.is_dir(), "{} is not a directory", root.display());

            return Ok(Dirs {
                config: root.clone(),
                state: root.clone(),
                runtime: root.clone(),
                root: Some(root),
            });
        }

        let base = etcetera::choose_base_strategy()
            .wrap_err("cannot determine the XDG base directories")?;
        // `state_dir` is only `None` on Windows.
        let state = base
            .state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("yank");
        let config = base.config_dir().join("yank");

        create_private(&config)?;
        create_private(&state)?;

        Ok(Dirs {
            config,
            runtime: base.runtime_dir().unwrap_or_else(|| state.clone()),
            state,
            root: None,
        })
    }

    /// The directory everything is under, when overridden with `--dir`.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The hand-edited settings (`config.toml`).
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// This machine's private identity key.
    pub fn identity_file(&self) -> PathBuf {
        self.state.join("identity.key")
    }

    /// The paired machines.
    pub fn mesh_file(&self) -> PathBuf {
        self.state.join("mesh.json")
    }

    /// The clipboard, as last saved.
    pub fn clip_file(&self) -> PathBuf {
        self.state.join("clip.bin")
    }

    /// The spool of copied files: content by hash, and the tree the clip
    /// is laid out in.
    pub fn files_dir(&self) -> PathBuf {
        self.state.join("files")
    }

    /// The control socket the CLI dials.
    pub fn socket_file(&self) -> PathBuf {
        self.runtime.join("yank.sock")
    }
}

/// Reads a file with `read`, `None` when there is no file.
pub fn read_existing<'a, T>(
    path: &'a Path,
    read: impl FnOnce(&'a Path) -> std::io::Result<T>,
) -> eyre::Result<Option<T>> {
    match read(path) {
        Ok(read) => Ok(Some(read)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).wrap_err_with(|| format!("cannot read {}", path.display())),
    }
}

/// Creates a directory and every missing parent, readable by its owner
/// only: these hold the identity key and the clipboard.
pub fn create_private(path: &Path) -> eyre::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .wrap_err_with(|| format!("cannot create {}", path.display()))
}

/// Writes a file atomically and owner-only, by having `contents` write a
/// sibling and renaming it into place: a crash mid-write must never leave a
/// truncated file where a valid one was.
pub fn write_private(
    path: &Path,
    contents: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> eyre::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        contents(&mut file)?;
        file.sync_all()
    };

    write().wrap_err_with(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path).wrap_err_with(|| format!("cannot write {}", path.display()))
}
