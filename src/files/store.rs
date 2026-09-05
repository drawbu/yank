//! The spool: content by hash, and the trees entries are laid out in.
//!
//! ```text
//!   files/blobs/<hash>          the bytes, named by what they are
//!   files/blobs/<hash>.part     a transfer that has not finished
//!   files/trees/<entry>/<path>  hard links, under the names of the copy
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
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

use color_eyre::eyre::{Result, WrapErr as _, bail, ensure};
use tracing::{debug, warn};
use walkdir::WalkDir;

use super::{FileRef, Hash};
use crate::{config::create_private, log::EntryId};

/// What a transfer that has not finished is named after the content it
/// will become.
const PARTIAL: &str = ".part";

/// Prefix of a file being copied into the spool, before it is named by
/// what it turned out to be. A sweep leaves these alone: one of them may
/// be a copy in progress, and unlinking it would fail the copy.
const TAKING: &str = "taking-";

/// How much is read at a time when hashing or serving a file.
const CHUNK: usize = 256 * 1024;

/// A file to take into the spool: where it is now, the name it keeps, and
/// what it weighed when the selection was walked.
#[derive(Clone, Debug)]
pub struct Source {
    pub from: PathBuf,
    pub path: String,
    pub size: u64,
}

/// The spool of one machine.
#[derive(Debug)]
pub struct Store {
    blobs: PathBuf,
    trees: PathBuf,
    /// Hashes copied into the spool before their manifest reaches the log.
    pending: Mutex<BTreeMap<Hash, usize>>,
}

/// A transfer being written, verified as it goes.
#[derive(Debug)]
pub struct Incoming {
    hash: Hash,
    path: PathBuf,
    file: File,
    written: u64,
}

impl Store {
    /// Opens the spool under `root`, creating what is missing.
    pub fn open(root: &Path) -> Result<Self> {
        let store = Store {
            blobs: root.join("blobs"),
            trees: root.join("trees"),
            pending: Mutex::default(),
        };
        create_private(&store.blobs)?;
        create_private(&store.trees)?;
        // Nothing can be in the middle of being taken before the daemon
        // starts, so whatever is left is from one that died.
        for path in read_dir(&store.blobs)? {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(TAKING))
            {
                let _ = fs::remove_file(&path);
            }
        }

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

    /// Where an entry's files are laid out.
    pub fn tree(&self, id: EntryId) -> PathBuf {
        self.trees.join(id.label())
    }

    /// Takes a snapshot of a file into the spool without reserving it against a sweep.
    pub fn take(&self, source: &Path) -> Result<(Hash, u64)> {
        self.take_inner(source, false)
    }

    /// Takes a file into the spool and keeps it safe from a concurrent
    /// sweep until [`Self::release`] publishes its manifest.
    pub fn take_reserved(&self, source: &Path) -> Result<(Hash, u64)> {
        self.take_inner(source, true)
    }

    fn take_inner(&self, source: &Path, reserve: bool) -> Result<(Hash, u64)> {
        let tmp = self
            .blobs
            .join(format!("{TAKING}{:016x}", rand::random::<u64>()));
        let taken = clone_file(source, &tmp).and_then(|()| hash_file(&tmp));
        let (hash, size) = match taken {
            Ok(taken) => taken,
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
        };

        // A sweep holds this lock while deciding what to remove. Reserving
        // before the blob is made visible closes the gap before its manifest
        // enters the history.
        let mut pending = self.pending.lock().unwrap();
        let blob = self.blob(hash);
        if blob.is_file() {
            // Some other entry already named these bytes.
            let _ = fs::remove_file(&tmp);
        } else {
            fs::rename(&tmp, &blob)
                .wrap_err_with(|| format!("cannot spool {}", source.display()))?;
        }
        if reserve {
            *pending.entry(hash).or_default() += 1;
        }

        Ok((hash, size))
    }

    /// Allows blobs named by a newly recorded manifest to be swept again.
    pub fn release(&self, files: &[FileRef]) {
        let mut pending = self.pending.lock().unwrap();
        for file in files {
            let count = pending
                .get_mut(&file.hash)
                .expect("only reserved blobs are released");
            *count -= 1;
            if *count == 0 {
                pending.remove(&file.hash);
            }
        }
    }

    /// Opens a transfer for `hash`, resuming a partial one.
    ///
    /// What is already on disk is hashed back in, so the verification at
    /// the end covers the whole file and not only what this run wrote.
    pub fn receive(&self, hash: Hash) -> Result<Incoming> {
        let path = self.blobs.join(format!("{}{PARTIAL}", hash.file_name()));
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .wrap_err_with(|| format!("cannot open {}", path.display()))?;
        let written = file.seek(SeekFrom::End(0))?;

        Ok(Incoming {
            hash,
            path,
            file,
            written,
        })
    }

    /// Lays an entry's files out as a tree of links into the spool.
    ///
    /// The tree is rebuilt rather than patched: it belongs to one entry,
    /// and an entry is written once.
    pub fn lay_out(&self, id: EntryId, files: &[FileRef]) -> Result<PathBuf> {
        let tree = self.tree(id);
        remove_tree(&tree);
        create_private(&tree)?;

        for file in files {
            ensure!(file.is_safe(), "a peer sent an unusable path");
            let link = tree.join(&file.path);
            if let Some(parent) = link.parent() {
                create_private(parent)?;
            }
            fs::hard_link(self.blob(file.hash), &link)
                .wrap_err_with(|| format!("cannot lay out {}", file.path))?;
        }

        Ok(tree)
    }

    /// Drops everything no live entry names: the trees of entries that are
    /// gone, the content nothing points at, and transfers abandoned when
    /// the entry that wanted them went away.
    pub fn sweep(&self, entries: &BTreeSet<EntryId>, content: &BTreeSet<Hash>) -> Result<()> {
        let live: BTreeSet<String> = entries.iter().map(EntryId::label).collect();
        for tree in read_dir(&self.trees)? {
            if !tree
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| live.contains(name))
            {
                debug!("dropping the files of a gone entry: {}", tree.display());
                remove_tree(&tree);
            }
        }

        let pending = self.pending.lock().unwrap();
        for blob in read_dir(&self.blobs)? {
            let name = blob.file_name().and_then(|name| name.to_str());
            if name.is_some_and(|name| name.starts_with(TAKING)) {
                continue;
            }
            // Read back as the hash the name says it is, so a partial
            // transfer is swept by the same rule as the content it will
            // become, and a name that is neither goes as the junk it is.
            let named = name
                .map(|name| name.strip_suffix(PARTIAL).unwrap_or(name))
                .and_then(Hash::parse);
            if !named.is_some_and(|hash| content.contains(&hash) || pending.contains_key(&hash)) {
                debug!("dropping unreferenced content: {}", blob.display());
                let _ = fs::remove_file(&blob);
            }
        }

        Ok(())
    }

    /// What the spool weighs, transfers in progress included.
    pub fn size(&self) -> u64 {
        read_dir(&self.blobs)
            .unwrap_or_default()
            .iter()
            .filter_map(|blob| fs::metadata(blob).ok())
            .map(|meta| meta.len())
            .sum()
    }
}

impl Incoming {
    /// How much of the content is already here, and where a peer should
    /// carry on from.
    pub fn at(&self) -> u64 {
        self.written
    }

    /// Appends what arrived.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.file
            .write_all(bytes)
            .wrap_err_with(|| format!("cannot write {}", self.path.display()))?;
        self.written += bytes.len() as u64;

        Ok(())
    }

    /// Checks the file against the hash it was asked for and puts it in
    /// the spool. A mismatch takes the file with it: what is left is not
    /// the content, and keeping it would have every later transfer resume
    /// on top of the wrong bytes.
    pub fn finish(self, store: &Store) -> Result<()> {
        drop(self.file);

        let (hash, _) = hash_file(&self.path)?;
        if hash != self.hash {
            let _ = fs::remove_file(&self.path);
            bail!("the content of {} is not what it was named", self.hash);
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
pub fn walk(roots: &[PathBuf], max_files: usize, max_bytes: u64) -> Result<Vec<Source>> {
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
                    debug!("skipping {path}, which is not a plain file");
                }
                continue;
            }

            let path = entry
                .path()
                .strip_prefix(base)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| {
                    color_eyre::eyre::eyre!(
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
                .ok_or_else(|| color_eyre::eyre::eyre!("the selection is too large to share"))?;
            ensure!(found.len() < max_files, "the selection has too many files");
            ensure!(total <= max_bytes, "the selection is too large to share");
            found.push(Source {
                from: entry.path().to_owned(),
                path: path.to_owned(),
                size,
            });
        }
    }
    found.sort_by(|left, right| left.path.cmp(&right.path));
    // Two files under one name is a selection that cannot be laid out on
    // the other machine, and there is no name to give the second that the
    // person copying would recognize.
    ensure!(
        found.windows(2).all(|pair| pair[0].path != pair[1].path),
        "two of the files would land under the same name",
    );

    Ok(found)
}

/// Copies a file, sharing the blocks with the original where the
/// filesystem can: on btrfs, XFS, bcachefs and APFS, spooling a four
/// gigabyte image costs a directory entry rather than four gigabytes.
fn clone_file(source: &Path, dest: &Path) -> Result<()> {
    reflink_copy::reflink_or_copy(source, dest)
        .wrap_err_with(|| format!("cannot copy {}", source.display()))?;

    Ok(())
}

/// The hash and size of a file on disk.
fn hash_file(path: &Path) -> Result<(Hash, u64)> {
    let mut file = File::open(path).wrap_err_with(|| format!("cannot read {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut size = 0u64;

    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(err).wrap_err_with(|| format!("cannot read {}", path.display()));
            }
        };
        hasher.update(&chunk[..read]);
        size += read as u64;
    }

    Ok((hasher.finalize().into(), size))
}

/// Every path in a directory, an absent directory counting as empty.
fn read_dir(dir: &Path) -> Result<Vec<PathBuf>> {
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
        Err(err) => warn!("cannot remove {}: {err}", tree.display()),
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
    fn a_tree_is_the_names_the_copy_had() {
        let (dir, store) = store();
        let one = store.take(&write(dir.path(), "one", b"first")).unwrap();
        let two = store.take(&write(dir.path(), "two", b"second")).unwrap();

        let files = vec![
            FileRef {
                path: "a.txt".to_owned(),
                size: one.1,
                hash: one.0,
            },
            FileRef {
                path: "photos/b.txt".to_owned(),
                size: two.1,
                hash: two.0,
            },
        ];
        let id = EntryId {
            origin: iroh::SecretKey::generate().public(),
            seq: 1,
        };
        let tree = store.lay_out(id, &files).unwrap();

        assert_eq!(fs::read(tree.join("a.txt")).unwrap(), b"first");
        assert_eq!(fs::read(tree.join("photos/b.txt")).unwrap(), b"second");
    }

    #[test]
    fn a_path_from_a_peer_is_never_followed_out_of_the_tree() {
        let (_dir, store) = store();
        let id = EntryId {
            origin: iroh::SecretKey::generate().public(),
            seq: 1,
        };
        let files = vec![FileRef {
            path: "../escaped".to_owned(),
            size: 0,
            hash: Hash::of(b""),
        }];

        assert!(store.lay_out(id, &files).is_err());
    }

    #[test]
    fn sweeping_keeps_what_the_history_still_names() {
        let (dir, store) = store();
        let origin = iroh::SecretKey::generate().public();
        let kept = store.take(&write(dir.path(), "kept", b"kept")).unwrap().0;
        let gone = store.take(&write(dir.path(), "gone", b"gone")).unwrap().0;

        let live = EntryId { origin, seq: 1 };
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
        store.lay_out(EntryId { origin, seq: 2 }, &[]).unwrap();

        store
            .sweep(&BTreeSet::from([live]), &BTreeSet::from([kept]))
            .unwrap();

        assert!(store.has(kept));
        assert!(!store.has(gone));
        assert!(store.tree(live).is_dir());
        assert!(!store.tree(EntryId { origin, seq: 2 }).exists());
    }

    #[test]
    fn a_reserved_blob_survives_until_its_manifest_is_published() {
        let (dir, store) = store();
        let (hash, size) = store
            .take_reserved(&write(dir.path(), "kept", b"kept"))
            .unwrap();
        let files = [FileRef {
            path: "kept".to_owned(),
            size,
            hash,
        }];

        store.sweep(&BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert!(store.has(hash));

        store.release(&files);
        store.sweep(&BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert!(!store.has(hash));
    }

    #[test]
    fn walking_a_selection_finds_the_files_under_the_names_they_keep() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "folder/one.txt", b"one");
        write(dir.path(), "folder/deep/two.txt", b"two");
        let loose = write(dir.path(), "three.txt", b"three");

        let found = walk(&[dir.path().join("folder"), loose], 16, 1024).unwrap();

        let names: Vec<&str> = found.iter().map(|source| source.path.as_str()).collect();
        assert_eq!(
            names,
            vec!["folder/deep/two.txt", "folder/one.txt", "three.txt"],
        );
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
