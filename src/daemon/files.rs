//! The file service: what makes a copied file a file on the other machine.
//!
//! The clipboard state machine says *what* has to happen and knows nothing
//! about disks or peers; this is what does it, on its own task, because
//! everything here is slow in a way the clipboard must not be:
//!
//! ```text
//!   copy   uri-list ─► walk ─► spool ─► manifest ─► the clip, shared
//!   paste  Effect::Fetch ─► pull the hashes ─► lay out ─► re-offer
//!   always clip replaced ─► sweep what it no longer names
//! ```
//!
//! Copying is what holds the clip up: the files are spooled before it is
//! made, so a clip never names content this machine cannot serve. Pasting
//! is not: the clip reaches the clipboard as the text of its paths right
//! away, and becomes a file reference when the bytes land.

use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use eyre::WrapErr as _;
use iroh::endpoint::{Connection, RecvStream};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinHandle, spawn_blocking},
};

use super::{backoff::Backoff, clip::ClipService, peers::PeerSet};
use crate::{
    clip::{Captured, Stamp},
    config::Settings,
    files::{self, FileRef, Incoming, Store},
    net::{
        proto::{self, Ask, ContentReply, ContentRequest},
        wire::{read_message, write_message},
    },
};

/// Budget for one chunk of a transfer. A peer that stops sending mid-file
/// without dropping the connection would otherwise hold the queue.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Chunks held between the network and the disk. Bounded, so a fast peer
/// cannot fill memory with a file that is being written slowly.
const WRITE_QUEUE: usize = 4;

/// Delays between attempts at a fetch. A peer that has the content may be
/// asleep.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_mins(2);

/// How many times a fetch is attempted before the clip stays text.
const RETRIES: usize = 6;

/// What the clipboard asks the file service for.
///
/// Jobs that touch the spool run one at a time, in order, so a sweep never
/// sees a file taken in but not yet named by the clip.
#[derive(Debug)]
pub enum Job {
    /// Take the files a selection names into the spool, then record it,
    /// in compositor order. The clip is not made until the content is here
    /// to back it.
    Record {
        /// The clip when it was copied; see [`ClipService::record`].
        base: Option<Stamp>,
        captured: Box<Captured>,
        /// The files it names, empty when it names none.
        paths: Vec<PathBuf>,
    },
    /// Take files into the spool and copy them, from `yank copy`.
    Copy {
        paths: Vec<PathBuf>,
        ttl: Option<Duration>,
        done: oneshot::Sender<eyre::Result<()>>,
    },
    /// Bring the files of the clip with this stamp onto this machine.
    Fetch(Stamp),
    /// Drop what the clip does not name.
    Sweep,
}

/// How the clipboard reaches the file service.
#[derive(Clone, Debug)]
pub struct Jobs(mpsc::UnboundedSender<Job>);

impl Jobs {
    /// The queue, and its end for [`run`].
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Job>) {
        let (jobs, queue) = mpsc::unbounded_channel();

        (Jobs(jobs), queue)
    }

    /// Queues a job, dropping it if the service is gone: the daemon is on
    /// its way down, and files are not worth a panic.
    pub fn send(&self, job: Job) {
        if self.0.send(job).is_err() {
            tracing::debug!("the file service is gone; dropping a job");
        }
    }
}

/// Runs the file service until the daemon stops.
pub async fn run(
    clip: Arc<ClipService>,
    peers: Arc<PeerSet>,
    store: Arc<Store>,
    settings: Arc<Settings>,
    mut jobs: mpsc::UnboundedReceiver<Job>,
) {
    sweep(&clip, &store).await;

    // One fetch at a time, for the newest clip asked about: the files of a
    // replaced clip are not wanted anymore. A fetch runs off the queue, so
    // a transfer holds up nothing else.
    let mut fetch: Option<(Stamp, JoinHandle<()>)> = None;

    // The clip the last record made, which the next one was queued behind.
    let mut recorded = None;

    while let Some(job) = jobs.recv().await {
        match job {
            Job::Record {
                base,
                captured,
                paths,
            } => {
                let files = if paths.is_empty() {
                    Vec::new()
                } else {
                    // Still worth sharing: with no manifest, the clip is
                    // the paths as text.
                    spool(&store, &settings, paths).await.unwrap_or_else(|err| {
                        tracing::debug!("sharing the paths as text: {err:#}");
                        Vec::new()
                    })
                };
                recorded = clip.record(base, recorded, *captured, files);
            }
            Job::Copy { paths, ttl, done } => {
                let copied = match spool(&store, &settings, paths.clone()).await {
                    Ok(files) => clip.copy_spooled(&paths, files, ttl),
                    Err(err) => Err(err),
                };
                let _ = done.send(copied);
            }
            Job::Fetch(stamp) => {
                if fetch
                    .as_ref()
                    .is_some_and(|(asked, task)| *asked >= stamp && !task.is_finished())
                {
                    continue;
                }
                if let Some((_, task)) = fetch.take() {
                    task.abort();
                }
                let (clip, peers, store, settings) =
                    (clip.clone(), peers.clone(), store.clone(), settings.clone());
                let task = tokio::spawn(async move {
                    if let Err(err) = materialize(&clip, &peers, &store, &settings, stamp).await {
                        tracing::warn!("cannot bring the files of {stamp} here: {err:#}");
                    }
                });
                fetch = Some((stamp, task));
            }
            Job::Sweep => sweep(&clip, &store).await,
        }
    }
}

/// Takes every file a selection names into the spool, or none of them, so
/// the clip about to be made names content this machine can serve.
async fn spool(
    store: &Arc<Store>,
    settings: &Settings,
    paths: Vec<PathBuf>,
) -> eyre::Result<Vec<FileRef>> {
    let store = store.clone();
    let max_bytes = settings.max_file_size.as_u64();
    let files = spawn_blocking(move || {
        let sources = files::walk(&paths, files::MAX_FILES, max_bytes)?;
        eyre::ensure!(!sources.is_empty(), "the selection names no file");
        sources
            .into_iter()
            .map(|source| {
                let (hash, size) = store.take(&source.from)?;
                Ok(FileRef {
                    path: source.path,
                    size,
                    hash,
                })
            })
            .collect::<eyre::Result<Vec<_>>>()
    })
    .await
    .wrap_err("the spool task failed")??;
    tracing::info!("sharing {} file(s), {}", files.len(), size(&files));

    Ok(files)
}

/// Brings a clip's files here and tells the clipboard, retrying while
/// the machines that have them are away.
async fn materialize(
    clip: &ClipService,
    peers: &PeerSet,
    store: &Arc<Store>,
    settings: &Settings,
    stamp: Stamp,
) -> eyre::Result<()> {
    let files = clip.wanted(stamp);
    if files.is_empty() {
        return Ok(());
    }
    eyre::ensure!(
        files::total(&files) <= settings.max_file_size.as_u64(),
        "it is over the {} limit in config.toml",
        settings.max_file_size,
    );

    let mut backoff = Backoff::new(RETRY_MIN, RETRY_MAX);
    for attempt in 1..=RETRIES {
        match pull_all(clip, store, peers, stamp, &files).await {
            Ok(()) => break,
            Err(err) if attempt == RETRIES || clip.head() != Some(stamp) => return Err(err),
            Err(err) => {
                tracing::debug!("cannot fetch the files of {stamp} yet: {err:#}");
                tokio::time::sleep(backoff.next_delay()).await;
            }
        }
    }

    let (laid_out, store) = (files.clone(), store.clone());
    let tree = spawn_blocking(move || store.lay_out(stamp, &laid_out))
        .await
        .wrap_err("the spool task failed")??;
    tracing::info!(
        "{} file(s) of {stamp} are here, {}",
        files.len(),
        size(&files)
    );
    clip.materialized(stamp, tree);

    Ok(())
}

/// Pulls whatever of a clip is not spooled yet, asking each machine that
/// is up for everything still missing in one request, the origin first.
async fn pull_all(
    clip: &ClipService,
    store: &Arc<Store>,
    peers: &PeerSet,
    stamp: Stamp,
    files: &[FileRef],
) -> eyre::Result<()> {
    let mut seen = BTreeSet::new();
    let mut missing: Vec<&FileRef> = files
        .iter()
        .filter(|file| !store.has(file.hash) && seen.insert(file.hash))
        .collect();

    for (peer, conn) in peers.connected(stamp.origin) {
        if missing.is_empty() {
            break;
        }
        if let Err(err) = pull(clip, stamp, store, &conn, &missing).await {
            tracing::debug!("cannot pull from {peer}: {err:#}");
        }
        missing.retain(|file| !store.has(file.hash));
    }
    eyre::ensure!(
        missing.is_empty(),
        "no machine that is up has {} of the files",
        missing.len(),
    );

    Ok(())
}

/// Pulls `files` from one peer over a single stream, resuming each where
/// a previous attempt left off, and giving up once the clip is replaced:
/// its fetch would hold up the next one's. The files the peer does not
/// have are skipped.
async fn pull(
    clip: &ClipService,
    stamp: Stamp,
    store: &Arc<Store>,
    conn: &Connection,
    files: &[&FileRef],
) -> eyre::Result<()> {
    let at: Vec<u64> = files
        .iter()
        .map(|file| store.partial(file.hash, file.size))
        .collect();
    let ask = Ask::Contents(
        files
            .iter()
            .zip(&at)
            .map(|(file, &at)| ContentRequest {
                hash: file.hash,
                at,
            })
            .collect(),
    );

    let (mut send, mut recv) = conn.open_bi().await?;
    write_message(&mut send, &ask, proto::MAX_REQUEST_SIZE).await?;
    send.finish()?;

    for (file, at) in files.iter().zip(at) {
        eyre::ensure!(clip.head() == Some(stamp), "the clip was replaced");
        let reply = tokio::time::timeout(
            CHUNK_TIMEOUT,
            read_message::<ContentReply>(&mut recv, proto::MAX_REPLY_SIZE),
        )
        .await
        .wrap_err("the transfer stalled")??;
        if let ContentReply::Sending { size } = reply {
            eyre::ensure!(
                at.checked_add(size) == Some(file.size),
                "a peer offered {} bytes where the clip says {}",
                at.saturating_add(size),
                file.size,
            );
            receive(store, &mut recv, file, at).await?;
        }
    }

    Ok(())
}

/// Takes the rest of `file`, past the `at` bytes already here, off the
/// stream and into the spool.
///
/// Content that turns out not to be what it was named is only logged: the
/// stream is still in step, and the next file is worth having.
async fn receive(
    store: &Arc<Store>,
    recv: &mut RecvStream,
    file: &FileRef,
    at: u64,
) -> eyre::Result<()> {
    let hash = file.hash;
    let incoming = {
        let store = store.clone();
        spawn_blocking(move || store.receive(hash))
            .await
            .wrap_err("the spool task failed")??
    };
    eyre::ensure!(incoming.at() == at, "the partial transfer of {hash} moved");

    // The bytes go to disk on a thread of their own: a file arriving at
    // the speed of a local network must not put its writes on the runtime.
    // What it does *not* do is finish the transfer, so a stream that dies
    // halfway leaves what it wrote where the next attempt resumes from.
    let (chunks, queue) = mpsc::channel(WRITE_QUEUE);
    let writing = spawn_blocking(move || write_queued(incoming, queue));

    let mut left = file.size - at;
    while left > 0 {
        let want = usize::try_from(left).unwrap_or(usize::MAX);
        let read = tokio::time::timeout(CHUNK_TIMEOUT, recv.read_chunk(want))
            .await
            .wrap_err("the transfer stalled")??;
        let Some(chunk) = read else {
            eyre::bail!("the transfer stopped {left} bytes short");
        };
        left -= chunk.len() as u64;
        chunks
            .send(chunk)
            .await
            .map_err(|_| eyre::eyre!("cannot write the transfer"))?;
    }
    drop(chunks);
    let incoming = writing.await.wrap_err("the spool task failed")??;

    let store = store.clone();
    let finished = spawn_blocking(move || incoming.finish(&store))
        .await
        .wrap_err("the spool task failed")?;
    if let Err(err) = finished {
        tracing::warn!("{err:#}");
    }

    Ok(())
}

/// Writes what arrives on `queue` until it closes.
fn write_queued(
    mut incoming: Incoming,
    mut queue: mpsc::Receiver<impl AsRef<[u8]>>,
) -> eyre::Result<Incoming> {
    while let Some(chunk) = queue.blocking_recv() {
        incoming.write(chunk.as_ref())?;
    }

    Ok(incoming)
}

/// Drops the content and the trees the clip does not name.
async fn sweep(clip: &ClipService, store: &Arc<Store>) {
    let (live, content) = clip.referenced();
    let store = store.clone();

    let swept = spawn_blocking(move || store.sweep(live, &content))
        .await
        .wrap_err("the spool task failed")
        .and_then(|swept| swept);
    if let Err(err) = swept {
        tracing::warn!("cannot sweep the file spool: {err:#}");
    }
}

/// What a manifest weighs, for the log lines a person reads.
fn size(files: &[FileRef]) -> bytesize::ByteSize {
    bytesize::ByteSize::b(files::total(files))
}
