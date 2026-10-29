//! The spool: content by hash, and the tree the clip is laid out in.
//!
//! ```text
//!   files/blobs/<hash>          the bytes, named by what they are
//!   files/blobs/<hash>.part     a transfer that has not finished
//!   files/trees/<stamp>/<path>  hard links, under the names of the copy
//! ```
//!
//! Two views of one thing. Content is addressed by hash so a file already
//! here is never fetched twice and a transfer that ends with the wrong
//! bytes is caught by the name it was asked for; the tree is what a
//! `file://` URI can point at, since a pasting application wants
//! `photos/a.png` and not a hash.
//!
//! The tree costs nothing but a directory entry: a hard link, on the same
//! filesystem, to the one copy of the content. Nothing writes to those
//! links, and nothing should: a paste reads them.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Write as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

use eyre::WrapErr as _;
use walkdir::WalkDir;

use super::{FileRef, Hash};
use crate::{clip::Stamp, config::create_private};

/// What a transfer that has not finished is named after the content it
/// will become.
const PARTIAL: &str = ".part";

/// A file to take into the spool: where it is now, and the name it keeps.
#[derive(Clone, Debug)]
pub struct Source {
    pub from: PathBuf,
    pub path: String,
}

/// The spool of one machine.
///
/// Taking a file in and sweeping are not safe against each other: a sweep
/// drops what no clip names yet. The file service runs both, one at a
/// time.
#[derive(Debug)]
pub struct Store {
    blobs: PathBuf,
    trees: PathBuf,
}

/// A transfer being written, verified as it goes.
#[derive(Debug)]
pub struct Incoming {
    hash: Hash,
    path: PathBuf,
    file: File,
    /// Everything written so far, the part resumed included.
    hasher: blake3::Hasher,
}

impl Store {
    /// Opens the spool under `root`, creating what is missing.
    pub fn open(root: &Path) -> eyre::Result<Self> {
        let store = Store {
            blobs: root.join("blobs"),
            trees: root.join("trees"),
        };
        create_private(&store.blobs)?;
        create_private(&store.trees)?;

        Ok(store)
    }

    /// Where the content of `hash` is, whether or not it is there.
    pub fn blob(&self, hash: Hash) -> PathBuf {
        self.blobs.join(hash.file_name())
    }

    /// Whether the content is here in full.
    pub fn has(&self, hash: Hash) -> bool {
        self.blob(hash).is_file()
    }

    /// Where the files of the clip stamped `stamp` are laid out.
    pub fn tree(&self, stamp: Stamp) -> PathBuf {
        self.trees.join(stamp.to_string())
    }

    /// Takes a snapshot of a file into the spool.
    pub fn take(&self, source: &Path) -> eyre::Result<(Hash, u64)> {
        let tmp = self
            .blobs
            .join(format!("taking-{:016x}", rand::random::<u64>()));
        let taken = clone_file(source, &tmp).and_then(|()| hash_file(&tmp));
        let (hash, size) = match taken {
            Ok(taken) => taken,
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
        };

        let blob = self.blob(hash);
        if blob.is_file() {
            // Some other copy already named these bytes.
            let _ = fs::remove_file(&tmp);
        } else {
            fs::rename(&tmp, &blob)
                .wrap_err_with(|| format!("cannot spool {}", source.display()))?;
        }

        Ok((hash, size))
    }

    /// How much of a transfer for `hash`, a file of `size` bytes, is
    /// already here. One that grew past it holds the wrong bytes, and goes.
    pub fn partial(&self, hash: Hash, size: u64) -> u64 {
        let path = self.partial_path(hash);
        match fs::metadata(&path) {
            Ok(meta) if meta.len() <= size => meta.len(),
            Ok(_) => {
                let _ = fs::remove_file(&path);
                0
            }
            Err(_) => 0,
        }
    }

    fn partial_path(&self, hash: Hash) -> PathBuf {
        self.blobs.join(format!("{}{PARTIAL}", hash.file_name()))
    }

    /// Opens a transfer for `hash`, resuming a partial one.
    ///
    /// What is already on disk is hashed back in, so the verification at
    /// the end covers the whole file and not only what this run wrote.
    pub fn receive(&self, hash: Hash) -> eyre::Result<Incoming> {
        let path = self.partial_path(hash);
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .wrap_err_with(|| format!("cannot open {}", path.display()))?;
        let mut hasher = blake3::Hasher::new();
        hasher
            .update_reader(&mut file)
            .wrap_err_with(|| format!("cannot read {}", path.display()))?;

        Ok(Incoming {
            hash,
            path,
            file,
            hasher,
        })
    }

    /// Lays a clip's files out as a tree of links into the spool.
    ///
    /// The tree is rebuilt rather than patched: it belongs to one clip,
    /// and a clip is written once.
    pub fn lay_out(&self, stamp: Stamp, files: &[FileRef]) -> eyre::Result<PathBuf> {
        let tree = self.tree(stamp);
        remove_tree(&tree);
        create_private(&tree)?;

        for file in files {
            eyre::ensure!(file.is_safe(), "a peer sent an unusable path");
            let link = tree.join(&file.path);
            if let Some(parent) = link.parent() {
                create_private(parent)?;
            }
            fs::hard_link(self.blob(file.hash), &link)
                .wrap_err_with(|| format!("cannot lay out {}", file.path))?;
        }

        Ok(tree)
    }

    /// Drops everything the clip stamped `live` does not name: the trees
    /// of other clips, the content nothing points at, and transfers
    /// abandoned when the clip that wanted them was replaced.
    pub fn sweep(&self, live: Option<Stamp>, content: &BTreeSet<Hash>) -> eyre::Result<()> {
        let live = live.map(|stamp| self.tree(stamp));
        for tree in read_dir(&self.trees)? {
            if live.as_ref() != Some(&tree) {
                tracing::debug!("dropping the files of a gone clip: {}", tree.display());
                remove_tree(&tree);
            }
        }

        for blob in read_dir(&self.blobs)? {
            let name = blob.file_name().and_then(|name| name.to_str());
            // Read back as the hash the name says it is, so a partial
            // transfer is swept by the same rule as the content it will
            // become, and a name that is neither goes as the junk it is.
            let named = name
                .map(|name| name.strip_suffix(PARTIAL).unwrap_or(name))
                .and_then(Hash::parse);
            if !named.is_some_and(|hash| content.contains(&hash)) {
                tracing::debug!("dropping unreferenced content: {}", blob.display());
                let _ = fs::remove_file(&blob);
            }
        }

        Ok(())
    }
}

impl Incoming {
    /// How much of the content is already here, and where a peer should
    /// carry on from.
    pub fn at(&self) -> u64 {
        self.hasher.count()
    }

    /// Appends what arrived.
    pub fn write(&mut self, bytes: &[u8]) -> eyre::Result<()> {
        self.file
            .write_all(bytes)
            .wrap_err_with(|| format!("cannot write {}", self.path.display()))?;
        self.hasher.update(bytes);

        Ok(())
    }

    /// Checks the file against the hash it was asked for and puts it in
    /// the spool. A mismatch takes the file with it: what is left is not
    /// the content, and keeping it would have every later transfer resume
    /// on top of the wrong bytes.
    pub fn finish(self, store: &Store) -> eyre::Result<()> {
        drop(self.file);

        if Hash::from(self.hasher.finalize()) != self.hash {
            let _ = fs::remove_file(&self.path);
            eyre::bail!("the content of {} is not what it was named", self.hash);
        }

        fs::rename(&self.path, store.blob(self.hash))
            .wrap_err_with(|| format!("cannot spool {}", self.hash))
    }
}

/// Expands a selection's paths into the files it names, each under the
/// name it keeps on the other machine.
///
/// Directories are walked. Symlinks and everything that is not a plain
/// file are skipped: what a paste elsewhere can do is write bytes, and a
/// link to a path that machine does not have is the very thing being
/// fixed here.
///
/// Refuses rather than truncates when the selection is over `max_files` or
/// `max_bytes`: half a folder is not the folder, and a paste that quietly
/// dropped files would be worse than one that did not happen.
pub fn walk(roots: &[PathBuf], max_files: usize, max_bytes: u64) -> eyre::Result<Vec<Source>> {
    let mut found = Vec::new();
    let mut total = 0u64;

    for root in roots {
        // What the files are named after on the other machine: the file
        // itself, or the folder everything under it sits in.
        let base = root.parent().unwrap_or(Path::new(""));

        for entry in WalkDir::new(root).sort_by_file_name() {
            let entry = entry.wrap_err_with(|| format!("cannot read {}", root.display()))?;
            // Links are not followed, so this is false for them as well.
            if !entry.file_type().is_file() {
                if !entry.file_type().is_dir() {
                    let path = entry.path().display();
                    tracing::debug!("skipping {path}, which is not a plain file");
                }
                continue;
            }

            let path = entry
                .path()
                .strip_prefix(base)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "{} has a name that cannot be shared",
                        entry.path().display(),
                    )
                })?;
            let size = entry
                .metadata()
                .wrap_err_with(|| format!("cannot read {}", entry.path().display()))?
                .size();

            total = total
                .checked_add(size)
                .ok_or_else(|| eyre::eyre!("the selection is too large to share"))?;
            eyre::ensure!(found.len() < max_files, "the selection has too many files");
            eyre::ensure!(total <= max_bytes, "the selection is too large to share");
            found.push(Source {
                from: entry.path().to_owned(),
                path: path.to_owned(),
            });
        }
    }
    found.sort_by(|left, right| left.path.cmp(&right.path));
    // Two files under one name is a selection that cannot be laid out on
    // the other machine, and there is no name to give the second that the
    // person copying would recognize.
    eyre::ensure!(
        found.windows(2).all(|pair| pair[0].path != pair[1].path),
        "two of the files would land under the same name",
    );

    Ok(found)
}

/// Copies a file, sharing the blocks with the original where the
/// filesystem can (`copy_file_range` reflinks on btrfs, XFS and bcachefs,
/// `fclonefileat` on APFS): spooling a four gigabyte image costs a
/// directory entry rather than four gigabytes.
fn clone_file(source: &Path, dest: &Path) -> eyre::Result<()> {
    fs::copy(source, dest).wrap_err_with(|| format!("cannot copy {}", source.display()))?;

    Ok(())
}

/// The hash and size of a file on disk.
fn hash_file(path: &Path) -> eyre::Result<(Hash, u64)> {
    let mut hasher = blake3::Hasher::new();
    File::open(path)
        .and_then(|file| hasher.update_reader(file).map(drop))
        .wrap_err_with(|| format!("cannot read {}", path.display()))?;

    Ok((hasher.finalize().into(), hasher.count()))
}

/// Every path in a directory, an absent directory counting as empty.
fn read_dir(dir: &Path) -> eyre::Result<Vec<PathBuf>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).wrap_err_with(|| format!("cannot read {}", dir.display())),
    };

    let mut paths = Vec::new();
    for entry in listing {
        paths.push(
            entry
                .wrap_err_with(|| format!("cannot read {}", dir.display()))?
                .path(),
        );
    }

    Ok(paths)
}

/// Removes a tree of links, saying so rather than failing: a tree left
/// behind costs directory entries, not content.
fn remove_tree(tree: &Path) {
    match fs::remove_dir_all(tree) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!("cannot remove {}: {err}", tree.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        (dir, store)
    }

    fn stamp(millis: u64) -> Stamp {
        Stamp {
            millis,
            origin: iroh::SecretKey::generate().public(),
        }
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();

        path
    }

    #[test]
    fn taking_a_file_names_it_by_its_contents() {
        let (dir, store) = store();
        let path = write(dir.path(), "a.txt", b"hello");

        let (hash, size) = store.take(&path).unwrap();
        assert_eq!(hash, Hash::of(b"hello"));
        assert_eq!(size, 5);
        assert!(store.has(hash));
        assert_eq!(fs::read(store.blob(hash)).unwrap(), b"hello");

        // The same bytes under another name cost nothing twice.
        let again = write(dir.path(), "b.txt", b"hello");
        assert_eq!(store.take(&again).unwrap().0, hash);
        assert_eq!(read_dir(&store.blobs).unwrap().len(), 1);
    }

    #[test]
    fn a_transfer_resumes_and_is_checked_against_what_it_claims() {
        let (_dir, store) = store();
        let hash = Hash::of(b"hello world");

        let mut incoming = store.receive(hash).unwrap();
        assert_eq!(incoming.at(), 0);
        incoming.write(b"hello ").unwrap();
        drop(incoming);

        let mut incoming = store.receive(hash).unwrap();
        assert_eq!(incoming.at(), 6, "the transfer picks up where it stopped");
        incoming.write(b"world").unwrap();
        incoming.finish(&store).unwrap();

        assert!(store.has(hash));
    }

    #[test]
    fn content_that_is_not_what_it_claims_is_thrown_away() {
        let (_dir, store) = store();
        let hash = Hash::of(b"hello");

        let mut incoming = store.receive(hash).unwrap();
        incoming.write(b"goodbye").unwrap();

        assert!(incoming.finish(&store).is_err());
        assert!(!store.has(hash));
        assert!(
            read_dir(&store.blobs).unwrap().is_empty(),
            "resuming on top of the wrong bytes would never terminate",
        );
    }

    #[test]
    fn sweeping_keeps_what_the_clip_names() {
        let (dir, store) = store();
        let kept = store.take(&write(dir.path(), "kept", b"kept")).unwrap().0;
        let gone = store.take(&write(dir.path(), "gone", b"gone")).unwrap().0;

        let live = stamp(1);
        store
            .lay_out(
                live,
                &[FileRef {
                    path: "kept".to_owned(),
                    size: 4,
                    hash: kept,
                }],
            )
            .unwrap();
        store.lay_out(stamp(2), &[]).unwrap();

        store.sweep(Some(live), &BTreeSet::from([kept])).unwrap();

        assert!(store.has(kept));
        assert!(!store.has(gone));
        assert!(store.tree(live).is_dir());
        assert!(!store.tree(stamp(2)).exists());
    }

    #[test]
    fn two_files_under_one_name_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "one/notes.txt", b"one");
        write(dir.path(), "two/notes.txt", b"two");

        let roots = [
            dir.path().join("one/notes.txt"),
            dir.path().join("two/notes.txt"),
        ];
        assert!(walk(&roots, 16, 1024).is_err());
    }

    #[test]
    fn a_selection_over_the_caps_is_refused_rather_than_halved() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "folder/one.txt", b"one");
        write(dir.path(), "folder/two.txt", b"two");

        assert!(walk(&[dir.path().join("folder")], 1, 1024).is_err());
        assert!(walk(&[dir.path().join("folder")], 16, 4).is_err());
    }
}
