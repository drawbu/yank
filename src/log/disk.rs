//! Persistence of the log: one file per entry, under `history/`.
//!
//! A file per entry rather than one index: adding and dropping entries are
//! then a create and an unlink, with no file to rewrite and nothing to
//! repair after a crash. A file that fails to decode is one that a crash
//! caught mid-write; it is deleted rather than mourned.
//!
//! Writes run on their own thread and in order, so an unlink can never
//! overtake the write it undoes, and a megabyte hitting a slow disk never
//! stalls the async runtime.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

use color_eyre::eyre::{Result, WrapErr as _, ensure};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::{Entry, EntryId, WireEntry};
use crate::config::write_private;

/// Extension of a persisted entry, also the filter used when reading the
/// directory back.
const EXTENSION: &str = "entry";

/// Schema of the files in this directory. A missing marker is the pre-schema
/// history, which this version intentionally discards.
const VERSION: u32 = 1;
const META: &str = "meta.json";

#[derive(Debug, Deserialize, Serialize)]
struct Meta {
    version: u32,
}

/// The disk side of the log.
///
/// Dropping it drains what is queued: the daemon may exit right after
/// writing an entry, and the entry should still be there next time.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    queue: Option<mpsc::Sender<Op>>,
    thread: Option<thread::JoinHandle<()>>,
}

/// One queued disk operation.
#[derive(Debug)]
enum Op {
    Store(PathBuf, Vec<u8>),
    Forget(PathBuf),
}

impl Writer {
    /// Starts the writer for `dir`, creating the directory if needed.
    pub fn spawn(dir: PathBuf) -> Result<Self> {
        prepare(&dir)?;

        let (queue, ops) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("yank-history".to_owned())
            .spawn(move || run(&ops))
            .wrap_err("cannot start the history writer")?;

        Ok(Writer {
            dir,
            queue: Some(queue),
            thread: Some(thread),
        })
    }

    /// Reads back every entry in `dir`, discarding the unreadable ones.
    pub fn load(dir: &Path) -> Result<Vec<WireEntry>> {
        prepare(dir)?;
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).wrap_err_with(|| format!("cannot read {}", dir.display())),
        };

        let mut entries = Vec::new();
        for file in listing {
            let path = file
                .wrap_err_with(|| format!("cannot read {}", dir.display()))?
                .path();
            if path.extension().is_none_or(|ext| ext != EXTENSION) {
                continue;
            }

            match fs::read(&path)
                .map_err(color_eyre::Report::from)
                .and_then(|bytes| postcard::from_bytes(&bytes).wrap_err("cannot decode the entry"))
            {
                Ok(entry) => entries.push(entry),
                // A file written by a crashed daemon, or by another
                // version. It is one clipboard entry: drop it and move on
                // rather than refusing to start.
                Err(err) => {
                    warn!("discarding {}: {err:#}", path.display());
                    let _ = fs::remove_file(&path);
                }
            }
        }

        Ok(entries)
    }

    /// Queues an entry to be written. Entries the caller marked
    /// non-durable are dropped here, which is the single point where
    /// secrecy turns into "never touches the disk".
    pub fn store(&self, entry: &Entry) {
        if !entry.durable {
            return;
        }

        let bytes = postcard::to_stdvec(&WireEntry::from(entry)).expect("entry must serialize");
        self.send(Op::Store(self.dir.join(entry.id.file_name()), bytes));
    }

    /// Queues an entry's file for deletion. Harmless for an entry that was
    /// never persisted.
    pub fn forget(&self, id: EntryId) {
        self.send(Op::Forget(self.dir.join(id.file_name())));
    }

    fn send(&self, op: Op) {
        if let Some(queue) = &self.queue
            && queue.send(op).is_err()
        {
            warn!("the history writer stopped; entries are no longer persisted");
        }
    }
}

/// Makes the history directory match this build's schema.
fn prepare(dir: &Path) -> Result<()> {
    let meta = dir.join(META);
    match fs::read(&meta) {
        Ok(bytes) => {
            let meta: Meta = serde_json::from_slice(&bytes)
                .wrap_err_with(|| format!("cannot read {}", meta.display()))?;
            ensure!(
                meta.version == VERSION,
                "history schema {} is newer than this yank",
                meta.version,
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if dir.exists() {
                fs::remove_dir_all(dir)
                    .wrap_err_with(|| format!("cannot clear {}", dir.display()))?;
            }
            crate::config::create_private(dir)?;
            crate::config::write_private(
                &meta,
                &serde_json::to_vec_pretty(&Meta { version: VERSION })?,
            )?;
        }
        Err(err) => return Err(err).wrap_err_with(|| format!("cannot read {}", meta.display())),
    }

    Ok(())
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the queue is what ends the loop; joining then waits for
        // the writes already queued.
        self.queue = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Applies queued operations until the writer is dropped.
fn run(ops: &mpsc::Receiver<Op>) {
    while let Ok(op) = ops.recv() {
        let outcome = match &op {
            Op::Store(path, bytes) => write_private(path, bytes),
            Op::Forget(path) => match fs::remove_file(path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => {
                    Err(err).wrap_err_with(|| format!("cannot remove {}", path.display()))
                }
                _ => Ok(()),
            },
        };

        // The history is a convenience; losing a file is not worth taking
        // the daemon down for.
        if let Err(err) = outcome {
            debug!("history write failed: {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{EntryId, Hlc, payload};

    fn entry(durable: bool) -> Entry {
        Entry {
            id: EntryId {
                origin: iroh::SecretKey::generate().public(),
                seq: 3,
            },
            clock: Hlc {
                millis: 42,
                counter: 1,
            },
            payload: payload(b"hello".to_vec()),
            durable,
        }
    }

    #[test]
    fn entries_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        let entry = entry(true);

        {
            let writer = Writer::spawn(path.clone()).unwrap();
            writer.store(&entry);
        }

        let loaded = Writer::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, entry.id);
        assert_eq!(loaded[0].payload, b"hello");

        {
            let writer = Writer::spawn(path.clone()).unwrap();
            writer.forget(entry.id);
        }
        assert!(Writer::load(&path).unwrap().is_empty());
    }

    #[test]
    fn a_secret_entry_never_reaches_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");

        {
            let writer = Writer::spawn(path.clone()).unwrap();
            writer.store(&entry(false));
        }

        assert!(Writer::load(&path).unwrap().is_empty());
    }

    #[test]
    fn a_truncated_file_is_discarded_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("broken.entry"), b"\xff\xff\xff").unwrap();

        assert!(Writer::load(&path).unwrap().is_empty());
        assert!(!path.join("broken.entry").exists());
    }

    #[test]
    fn a_history_without_a_schema_marker_is_started_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("old.entry"), b"old").unwrap();

        assert!(Writer::load(&path).unwrap().is_empty());
        let meta: Meta = serde_json::from_slice(&fs::read(path.join(META)).unwrap()).unwrap();
        assert_eq!(meta.version, VERSION);
        assert!(!path.join("old.entry").exists());
    }
}
