//! The file service: what makes a copied file a file on the other machine.
//!
//! The clipboard state machine says *what* has to happen and knows nothing
//! about disks or peers; this is what does it, on its own task, because
//! everything here is slow in a way the clipboard must not be:
//!
//! ```text
//!   copy   uri-list ─► walk ─► spool ─► manifest ─► the entry, shared
//!   paste  Effect::Fetch ─► pull the hashes ─► lay out ─► re-offer
//!   always entries dropped ─► sweep what nothing names any more
//! ```
//!
//! Copying is what holds the entry up: the files are spooled before it is
//! written, so an entry never names content this machine cannot serve.
//! Pasting is not: the entry reaches the clipboard as the text of its
//! paths right away, and becomes a file reference when the bytes land.

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr as _, bail, ensure};
use iroh::{EndpointId, endpoint::Connection};
use tokio::{
    sync::{Semaphore, mpsc},
    task::spawn_blocking,
};
use tracing::{debug, info, warn};

use super::{backoff::Backoff, clip::ClipService, peers::PeerSet};
use crate::{
    clip::Captured,
    config::Settings,
    files::{self, FileRef, Store},
    log::EntryId,
    net::{
        proto::{self, Ask, ContentReply, ContentRequest},
        wire::{read_message, write_message},
    },
};

/// How many files one selection may name. A folder is walked, and a deep
/// one is a manifest the log cannot carry.
const MAX_FILES: usize = 4096;

/// Budget for one chunk of a transfer. A peer that stops sending mid-file
/// without dropping the connection would otherwise hold the queue.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Chunks held between the network and the disk. Bounded, so a fast peer
/// cannot fill memory with a file that is being written slowly.
const WRITE_QUEUE: usize = 4;

/// Delays between attempts at a fetch. A peer that has the content may be
/// asleep, and the clipboard entry outlives its absence.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_mins(2);

/// How many times a fetch is attempted before the entry stays text.
const RETRIES: usize = 6;

/// What the clipboard asks the spool for.
#[derive(Debug)]
pub enum Job {
    /// Record a non-file selection in compositor order.
    Record(Captured),
    /// Take the files this selection names into the spool, then record
    /// it. The entry is not written until the content is here to back it.
    Snapshot {
        captured: Box<Captured>,
        /// What it names, read off it once by the caller that decided this
        /// was a file selection at all.
        paths: Vec<PathBuf>,
    },
    /// Bring this entry's files onto this machine.
    Fetch(EntryId),
    /// Drop what no entry names any more.
    Sweep,
}

/// How the clipboard reaches the spool.
#[derive(Clone, Debug)]
pub struct Spool {
    pub store: Arc<Store>,
    pub jobs: mpsc::UnboundedSender<Job>,
}

impl Spool {
    /// Queues a job, dropping it if the service is gone: the daemon is on
    /// its way down, and files are not worth a panic.
    pub fn send(&self, job: Job) {
        if self.jobs.send(job).is_err() {
            debug!("the file service is gone; dropping a job");
        }
    }
}

/// Runs the file service until the daemon stops.
pub async fn run(
    clip: Arc<ClipService>,
    peers: Arc<PeerSet>,
    settings: Arc<Settings>,
    spool: Spool,
    mut jobs: mpsc::UnboundedReceiver<Job>,
) {
    sweep(&clip, &spool.store);

    // One fetch at a time: they are whole files, and a machine catching up
    // on a history of them should not open one connection per entry. The
    // permit is taken in the task rather than here, so a transfer holds up
    // the next transfer and nothing else.
    let fetching = Arc::new(Semaphore::new(1));
    // The clipboard and a caller waiting on the files both ask, and
    // running the fetch twice would have the second lay the tree out again
    // under the first.
    let asked: Arc<Mutex<BTreeSet<EntryId>>> = Arc::default();

    while let Some(job) = jobs.recv().await {
        match job {
            Job::Record(captured) => clip.record(captured, Vec::new()),
            Job::Snapshot { captured, paths } => {
                let files = snapshot(&spool.store, &settings, paths).await;
                clip.record(*captured, files.clone());
                spool.store.release(&files);
            }
            Job::Fetch(id) => {
                if !asked.lock().unwrap().insert(id) {
                    continue;
                }
                let (clip, peers, settings, spool) =
                    (clip.clone(), peers.clone(), settings.clone(), spool.clone());
                let (fetching, asked) = (fetching.clone(), asked.clone());
                tokio::spawn(async move {
                    let Ok(_permit) = fetching.acquire().await else {
                        return;
                    };
                    if let Err(err) = materialize(&clip, &peers, &settings, &spool, id).await {
                        warn!("cannot bring the files of {id} here: {err:#}");
                    }
                    asked.lock().unwrap().remove(&id);
                });
            }
            Job::Sweep => sweep(&clip, &spool.store),
        }
    }
}

/// Takes a selection's files into the spool, so the entry about to be
/// written names content this machine can serve.
///
/// A selection that cannot be spooled is still worth sharing: the entry
/// goes out with no manifest, which is the paths as text, exactly what it
/// was before files were carried at all.
async fn snapshot(
    store: &Arc<Store>,
    settings: &Arc<Settings>,
    paths: Vec<PathBuf>,
) -> Vec<FileRef> {
    let (store, settings) = (store.clone(), settings.clone());
    let taken = spawn_blocking(move || take_all(&store, &paths, &settings)).await;

    match taken {
        Ok(Ok(files)) => {
            info!("sharing {} file(s), {}", files.len(), size(&files));
            files
        }
        Ok(Err(err)) => {
            debug!("sharing the paths as text: {err:#}");
            Vec::new()
        }
        Err(err) => {
            warn!("the spool task failed: {err}");
            Vec::new()
        }
    }
}

/// Spools every file a selection names, or none of them.
pub fn take_all(store: &Store, paths: &[PathBuf], settings: &Settings) -> Result<Vec<FileRef>> {
    let budget = settings.file_budget.as_u64();
    let sources = files::walk(paths, MAX_FILES, settings.max_file_bytes())?;
    ensure!(!sources.is_empty(), "the selection names no file");

    let wanted = sources.iter().try_fold(0u64, |total, source| {
        total
            .checked_add(source.size)
            .ok_or_else(|| color_eyre::eyre::eyre!("the selection is too large to share"))
    })?;
    room(store, wanted, budget)?;

    let mut spooled = Vec::with_capacity(sources.len());
    for source in sources {
        let (hash, size) = match store.take_reserved(&source.from) {
            Ok(taken) => taken,
            Err(err) => {
                store.release(&spooled);
                return Err(err);
            }
        };
        spooled.push(FileRef {
            path: source.path,
            size,
            hash,
        });
    }

    Ok(spooled)
}

/// Refuses what the spool has no room for. Counted once, since walking the
/// spool is a directory listing and a stat per file in it.
fn room(store: &Store, wanted: u64, budget: u64) -> Result<()> {
    ensure!(
        store.size() + wanted <= budget,
        "the spool has no room for another {} under its {} budget in config.toml",
        bytesize::ByteSize::b(wanted),
        bytesize::ByteSize::b(budget),
    );

    Ok(())
}

/// Brings an entry's files here and tells the clipboard, retrying while
/// the machines that have them are away.
async fn materialize(
    clip: &Arc<ClipService>,
    peers: &PeerSet,
    settings: &Settings,
    spool: &Spool,
    id: EntryId,
) -> Result<()> {
    let files = clip.files(id);
    if files.is_empty() {
        return Ok(());
    }
    ensure!(
        files::total(&files)? <= settings.max_file_bytes(),
        "it is over the {} limit in config.toml",
        settings.max_file_size,
    );
    files::validate(&files)?;
    room(
        &spool.store,
        files::total(&files)?,
        settings.file_budget.as_u64(),
    )?;

    let mut backoff = Backoff::new(RETRY_MIN, RETRY_MAX);
    for attempt in 1..=RETRIES {
        match pull_all(&spool.store, peers, id.origin, &files).await {
            Ok(()) => break,
            Err(err) if attempt == RETRIES => return Err(err),
            Err(err) => {
                debug!("cannot fetch the files of {id} yet: {err:#}");
                tokio::time::sleep(backoff.next_delay()).await;
            }
        }
    }

    let (store, laid_out) = (spool.store.clone(), files.clone());
    let tree = spawn_blocking(move || store.lay_out(id, &laid_out))
        .await
        .wrap_err("the spool task failed")??;
    info!("{} file(s) of {id} are here, {}", files.len(), size(&files));
    clip.materialized(id, tree);

    Ok(())
}

/// Pulls whatever of an entry is not spooled yet.
async fn pull_all(
    store: &Arc<Store>,
    peers: &PeerSet,
    origin: EndpointId,
    files: &[FileRef],
) -> Result<()> {
    for file in files {
        if store.has(file.hash) {
            continue;
        }

        let mut found = false;
        for (peer, conn) in peers.connected(origin) {
            match pull(store, &conn, file).await {
                Ok(true) => {
                    found = true;
                    break;
                }
                Ok(false) => debug!("{peer} does not have {}", file.hash),
                Err(err) => debug!("cannot pull {} from {peer}: {err:#}", file.hash),
            }
        }
        ensure!(found, "no machine that is up has {}", file.hash);
    }

    Ok(())
}

/// Pulls one file from one peer, resuming where a previous attempt left
/// off. Answers whether that peer had it at all.
async fn pull(store: &Arc<Store>, conn: &Connection, file: &FileRef) -> Result<bool> {
    let mut incoming = {
        let (store, hash) = (store.clone(), file.hash);
        spawn_blocking(move || store.receive(hash))
            .await
            .wrap_err("the spool task failed")??
    };
    let at = incoming.at().min(file.size);

    let (mut send, mut recv) = conn.open_bi().await?;
    let ask = Ask::Content(ContentRequest {
        hash: file.hash,
        at,
    });
    write_message(&mut send, &ask, proto::MAX_REQUEST_SIZE).await?;
    send.finish()?;

    let reply = read_message::<ContentReply>(&mut recv, proto::MAX_REQUEST_SIZE).await?;
    let size = match reply {
        ContentReply::Missing => return Ok(false),
        ContentReply::Sending { size } => size,
    };
    ensure!(
        at.checked_add(size) == Some(file.size),
        "a peer offered {} bytes where the entry says {}",
        at.saturating_add(size),
        file.size,
    );

    // The bytes go to disk on a thread of their own: a file arriving at
    // the speed of a local network must not put its writes on the runtime.
    // What it does *not* do is finish the transfer, so a stream that dies
    // halfway leaves what it wrote where the next attempt resumes from.
    let (chunks, mut queue) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
    let writing = spawn_blocking(move || {
        while let Some(chunk) = queue.blocking_recv() {
            incoming.write(&chunk)?;
        }

        Ok::<_, color_eyre::Report>(incoming)
    });

    let mut left = size;
    let mut chunk = vec![0u8; proto::CONTENT_CHUNK];
    while left > 0 {
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(chunk.len());
        let read = tokio::time::timeout(CHUNK_TIMEOUT, recv.read(&mut chunk[..want]))
            .await
            .wrap_err("the transfer stalled")??;
        match read {
            Some(0) | None => bail!("the transfer stopped {left} bytes short"),
            Some(read) => {
                left -= read as u64;
                chunks
                    .send(chunk[..read].to_vec())
                    .await
                    .map_err(|_| color_eyre::eyre::eyre!("cannot write the transfer"))?;
            }
        }
    }
    drop(chunks);
    let incoming = writing.await.wrap_err("the spool task failed")??;

    let store = store.clone();
    spawn_blocking(move || incoming.finish(&store))
        .await
        .wrap_err("the spool task failed")??;

    Ok(true)
}

/// Drops the content and the trees no entry names any more.
///
/// The history is what decides: an entry that aged out, was forgotten or
/// expired takes its files with it, the way its log file goes.
fn sweep(clip: &ClipService, store: &Arc<Store>) {
    let (entries, content) = clip.referenced();
    let store = store.clone();

    tokio::task::spawn_blocking(move || {
        if let Err(err) = store.sweep(&entries, &content) {
            warn!("cannot sweep the file spool: {err:#}");
        }
    });
}

/// What a manifest weighs, for the log lines a person reads.
fn size(files: &[FileRef]) -> bytesize::ByteSize {
    bytesize::ByteSize::b(files::total(files).unwrap_or(u64::MAX))
}
