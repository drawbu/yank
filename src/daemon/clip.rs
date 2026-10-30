//! The clipboard service: the clipboard state machine, wired up.
//!
//! [`crate::clip::Clipboard`] decides *what* should happen and returns
//! effects; this is what performs them, and what feeds it from the three
//! places clipboard changes come from:
//!
//! ```text
//!   compositor ──► captured ─┐
//!   CLI (socket) ──► copy ───┼─► Clipboard ──► effects ──► disk
//!   peers ────────► accept ──┘       │                 ├─► compositor
//!                                    │                 └─► file service
//!                                    └─► head ──► peers
//! ```
//!
//! It also runs the two things that happen without anyone asking: dropping
//! a copy whose lifetime has run out, and reconnecting to the compositor
//! after the session goes away and comes back.

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use eyre::WrapErr as _;
use iroh::EndpointId;
use tokio::sync::{Notify, mpsc, oneshot, watch};

use super::{
    backoff::Backoff,
    control::ClipInfo,
    files::{Job, Jobs},
};
use crate::{
    clip::{
        Captured, Clip, Clipboard, Effect, Pause, Payload, Policy, Rep, Saved, Selection, Stamp,
        Switch,
        backend::{self, Command, Platform},
    },
    config::{Dirs, Settings, read_existing, write_private},
    files::{FileRef, Hash},
};

/// First delay before reconnecting to the compositor.
const RECONNECT_MIN: Duration = Duration::from_secs(1);

/// Ceiling of that delay. A daemon started before the graphical session
/// waits this long at worst.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// How often the service loop wakes with nothing due. It is what catches
/// the things no event announces: a wall clock that jumped over a
/// lifetime, and a `yank pause --for` whose time is up.
const EXPIRY_FLOOR: Duration = Duration::from_mins(1);

/// The clipboard, everything it writes to, and everyone it tells.
#[derive(Debug)]
pub struct ClipService {
    dirs: Dirs,
    settings: Arc<Settings>,
    board: Mutex<Clipboard>,
    /// The compositor connection, or why there is none.
    backend: Mutex<Result<Platform, String>>,
    /// The stamp of the clip, for the peers to announce.
    head: watch::Sender<Option<Stamp>>,
    /// The file service: where a copied file's contents go, and where the
    /// ones a clip names are asked for.
    jobs: Jobs,
    /// Woken when the state changed in a way the service loop cares about:
    /// a new lifetime to wait for, or a pause that was lifted.
    wake: Notify,
    /// Woken when a clip's files have been laid out here, for whoever is
    /// waiting on them.
    arrived: Notify,
}

/// Where the clip's files are.
#[derive(Debug)]
pub struct Located {
    pub files: Vec<FileRef>,
    /// The tree they are laid out in, `None` while they are still on their
    /// way.
    pub tree: Option<PathBuf>,
}

impl ClipService {
    /// Loads the saved clip and starts the service. `identity` is this
    /// machine's endpoint id, which its copies are stamped with.
    pub fn open(
        dirs: &Dirs,
        identity: EndpointId,
        settings: Arc<Settings>,
        jobs: Jobs,
    ) -> eyre::Result<Arc<Self>> {
        let board = Clipboard::new(identity, settings.clone(), load(dirs)?);
        let head = watch::Sender::new(board.head());

        let service = Arc::new(ClipService {
            dirs: dirs.clone(),
            settings,
            board: Mutex::new(board),
            backend: Mutex::new(Err("starting".to_owned())),
            head,
            jobs,
            wake: Notify::new(),
            arrived: Notify::new(),
        });
        // What expired while the daemon was down leaves the disk now.
        service.change(|_| vec![Effect::Save]);

        Ok(service)
    }

    /// The stamp of the clip.
    pub fn head(&self) -> Option<Stamp> {
        self.board.lock().unwrap().head()
    }

    /// The stamp of the clip, now and on every change.
    pub fn subscribe(&self) -> watch::Receiver<Option<Stamp>> {
        self.head.subscribe()
    }

    /// The clip, for a peer that asked for it.
    pub fn clip(&self) -> Option<Arc<Clip>> {
        self.board.lock().unwrap().clip().cloned()
    }

    /// Takes a clip from a peer.
    pub fn accept(&self, clip: Clip) -> eyre::Result<()> {
        self.try_change(|board| board.accept(clip))
    }

    /// Copies something from this machine.
    pub fn copy(&self, rep: Rep, secret: bool, ttl: Option<Duration>) -> eyre::Result<()> {
        self.try_change(|board| board.copy(Selection::of(rep), Vec::new(), secret, ttl))
    }

    /// Copies files from this machine: their contents are spooled first,
    /// so the clip never names what this machine cannot serve.
    ///
    /// What goes on the clipboard is the paths, the way a file manager
    /// offers them. Pasting it here is pasting the originals; pasting it
    /// on another machine is pasting the copies that land there.
    pub async fn copy_files(&self, paths: Vec<PathBuf>, ttl: Option<Duration>) -> eyre::Result<()> {
        eyre::ensure!(
            self.settings.files,
            "sharing files is turned off in config.toml",
        );

        let (done, copied) = oneshot::channel();
        self.jobs.send(Job::Copy { paths, ttl, done });
        copied.await.wrap_err("the file service is gone")?
    }

    /// Copies files the file service took into the spool.
    pub fn copy_spooled(
        &self,
        paths: &[PathBuf],
        files: Vec<FileRef>,
        ttl: Option<Duration>,
    ) -> eyre::Result<()> {
        self.try_change(|board| board.copy(Selection::of_files(paths), files, false, ttl))
    }

    /// Where the clip's files are, asking for them when they are not here
    /// and waiting up to `within` for them to arrive.
    ///
    /// The wait is the point: a transfer takes as long as the files are
    /// large, and a caller polling for the answer would ask hundreds of
    /// times to be told the same thing. Past the deadline the answer says
    /// they are not here; the fetch carries on regardless.
    pub async fn located(&self, within: Duration) -> eyre::Result<Located> {
        let (stamp, files) = {
            let board = self.board.lock().unwrap();
            let (stamp, copy) = board
                .current()
                .ok_or_else(|| eyre::eyre!("the clipboard is empty"))?;
            eyre::ensure!(copy.is_files(), "the clipboard names no file");
            eyre::ensure!(
                !board.is_ours(),
                "the files were copied here; they are where they always were",
            );

            (stamp, copy.files.clone())
        };

        let deadline = Instant::now() + within;
        loop {
            // Registered before the tree is looked for, or files landing
            // between the two would be a wake-up nobody was waiting for.
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();

            let tree = {
                let board = self.board.lock().unwrap();
                eyre::ensure!(
                    board.head() == Some(stamp),
                    "the clipboard changed meanwhile"
                );
                board.tree().map(PathBuf::from)
            };
            if tree.is_some() {
                return Ok(Located { files, tree });
            }
            // Asked again in case an earlier fetch gave up; never once the
            // files are here, since a fetch lays the tree out again and
            // rebuilding one somebody is reading is how a paste finds half
            // of it.
            self.jobs.send(Job::Fetch(stamp));

            let left = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(left, arrived).await.is_err() {
                return Ok(Located { files, tree: None });
            }
        }
    }

    /// Records a selection the compositor reported, with the files it
    /// named once they are spooled. Dropped when the clip moved on since
    /// `base` was read: spooling takes time, and a copy made elsewhere
    /// meanwhile is the later one.
    pub fn record(&self, base: Option<Stamp>, captured: Captured, files: Vec<FileRef>) {
        self.change(|board| {
            if board.head() == base {
                board.captured(captured, files)
            } else {
                Vec::new()
            }
        });
    }

    /// Records that the files of the clip stamped `stamp` are here now.
    pub fn materialized(&self, stamp: Stamp, tree: PathBuf) {
        self.change(|board| board.materialized(stamp, tree));
        self.arrived.notify_waiters();
    }

    /// The files the clip stamped `stamp` names, while it is the clip and
    /// they are not laid out here: laying them out again would pull the
    /// tree from under a paste reading it.
    pub fn wanted(&self, stamp: Stamp) -> Vec<FileRef> {
        let board = self.board.lock().unwrap();
        match board.current() {
            Some((current, copy)) if current == stamp && board.tree().is_none() => {
                copy.files.clone()
            }
            _ => Vec::new(),
        }
    }

    /// What the spool may keep: the clip's tree, and the content it names.
    pub fn referenced(&self) -> (Option<Stamp>, BTreeSet<Hash>) {
        let board = self.board.lock().unwrap();
        match board.current() {
            Some((stamp, copy)) => (
                Some(stamp),
                copy.files.iter().map(|file| file.hash).collect(),
            ),
            None => (None, BTreeSet::new()),
        }
    }

    /// One representation of the clipboard, its best type when none is
    /// asked for.
    ///
    /// Returns the type of the bytes, and the other types it carries.
    pub fn paste(&self, mime: Option<&str>) -> eyre::Result<(String, Vec<String>, Payload)> {
        let board = self.board.lock().unwrap();
        let selection = &board
            .current()
            .ok_or_else(|| eyre::eyre!("the clipboard is empty"))?
            .1
            .selection;

        let rep = match mime {
            Some(mime) => selection.rep(mime).ok_or_else(|| {
                eyre::eyre!(
                    "the clipboard has no {} in it; it has {}",
                    crate::config::sanitize(mime),
                    selection
                        .reps()
                        .map(|rep| rep.mime.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            })?,
            None => &selection.primary,
        };
        // Compared by where they sit rather than by name: the type asked
        // for is matched case-insensitively, so `--type TEXT/HTML` would
        // otherwise list `text/html` as an alternative to itself.
        let alternates = selection
            .reps()
            .filter(|other| !std::ptr::eq(*other, rep))
            .map(|other| other.mime.clone())
            .collect();

        Ok((rep.mime.clone(), alternates, rep.bytes.clone()))
    }

    /// Empties the clipboard everywhere.
    pub fn clear(&self) {
        self.change(Clipboard::clear);
    }

    /// Pauses or resumes a direction of the clipboard.
    pub fn set_pause(&self, capture: Option<Switch>, apply: Option<Switch>) -> Pause {
        self.change(|board| board.set_pause(capture, apply));

        self.pause()
    }

    /// What the clipboard holds, for `yank status`. `name` names the
    /// machine it was copied on.
    pub fn describe(&self, name: impl Fn(EndpointId) -> String) -> Option<ClipInfo> {
        let board = self.board.lock().unwrap();
        let (stamp, copy) = board.current()?;
        let now = SystemTime::now();

        Some(ClipInfo {
            preview: copy.preview(),
            secret: copy.secret,
            files: copy.files.len(),
            origin: name(stamp.origin),
            age_secs: now
                .duration_since(stamp.time())
                .unwrap_or_default()
                .as_secs(),
            expires_in_secs: board
                .expires_at()
                .map(|at| at.duration_since(now).unwrap_or_default().as_secs()),
        })
    }

    pub fn pause(&self) -> Pause {
        self.board.lock().unwrap().pause()
    }

    /// Why the compositor side is not running, `None` when it is.
    pub fn backend_down(&self) -> Option<String> {
        self.backend.lock().unwrap().as_ref().err().cloned()
    }

    /// Runs a mutation and performs what it asked for.
    fn change(&self, mutate: impl FnOnce(&mut Clipboard) -> Vec<Effect>) {
        let _ = self.try_change(|board| Ok(mutate(board)));
    }

    /// The same, for a mutation that may refuse.
    ///
    /// The effects are performed under the lock, so two mutations racing
    /// reach the compositor, the disk and the peers in the order they
    /// happened.
    fn try_change(
        &self,
        mutate: impl FnOnce(&mut Clipboard) -> eyre::Result<Vec<Effect>>,
    ) -> eyre::Result<()> {
        let mut board = self.board.lock().unwrap();
        let effects = mutate(&mut board)?;
        if effects.is_empty() {
            return Ok(());
        }

        for effect in effects {
            match effect {
                Effect::Save => {
                    if let Err(err) = self.save(&board) {
                        tracing::warn!("cannot save the clipboard: {err:#}");
                    }
                    // The files of a clip that was replaced go with it.
                    self.jobs.send(Job::Sweep);
                }
                Effect::Backend(command) => self.to_backend(command),
                Effect::Fetch(stamp) => self.jobs.send(Job::Fetch(stamp)),
            }
        }
        self.head.send_if_modified(|head| {
            let changed = *head != board.head();
            *head = board.head();
            changed
        });
        self.wake.notify_one();

        Ok(())
    }

    /// Hands a command to the compositor, if there is one.
    fn to_backend(&self, command: Command) {
        if let Ok(backend) = &*self.backend.lock().unwrap() {
            backend.send(command);
        } else {
            tracing::debug!("no clipboard backend; dropping a command");
        }
    }

    fn save(&self, board: &Clipboard) -> eyre::Result<()> {
        let bytes = postcard::to_stdvec(&board.saved()).expect("the clipboard must serialize");

        write_private(&self.dirs.clip_file(), &bytes)
    }

    /// Connects to the compositor.
    ///
    /// Connecting means a round trip to the compositor, which is a
    /// blocking call on a process that may be busy or wedged, so it does
    /// not run on the async runtime.
    async fn connect_backend(
        &self,
        events: &mpsc::UnboundedSender<backend::Event>,
    ) -> eyre::Result<()> {
        let events = events.clone();
        let policy = Policy {
            max_bytes: self.settings.max_bytes(),
            ignore: self.settings.ignore_mime.clone(),
        };
        let socket = backend::locate().await;
        let backend =
            tokio::task::spawn_blocking(move || backend::connect(&events, policy, socket))
                .await
                .wrap_err("the clipboard connection task failed")?
                .wrap_err("cannot read the clipboard")?;

        self.set_backend(Ok(backend));
        tracing::info!("clipboard connected");

        Ok(())
    }

    /// Handles one event from the compositor. Returns whether the backend
    /// has to be reconnected.
    fn on_backend_event(&self, event: backend::Event) -> bool {
        match event {
            backend::Event::Copied(captured) => {
                // A selection that names files is recorded by the file
                // service instead, once their contents are spooled: a
                // clip must never name content this machine cannot serve.
                // A secret is the exception, since spooling is a write to
                // disk and a secret never touches one.
                let paths = if self.settings.files && !captured.secret {
                    captured.selection.file_paths()
                } else {
                    Vec::new()
                };
                self.jobs.send(Job::Record {
                    base: self.head(),
                    captured: Box::new(captured),
                    paths,
                });
                false
            }
            backend::Event::Lost(reason) => {
                tracing::warn!("clipboard disconnected: {reason}");
                self.set_backend(Err(reason));
                true
            }
        }
    }

    fn set_backend(&self, backend: Result<Platform, String>) {
        let previous = std::mem::replace(&mut *self.backend.lock().unwrap(), backend);
        // Out of the lock: dropping a backend waits for its thread.
        drop(previous);
    }
}

/// Runs the clipboard service until the daemon stops.
///
/// Holds the connection to the compositor, reconnecting when the session
/// goes away, and drops the copy when its lifetime runs out.
pub async fn run(service: Arc<ClipService>) {
    let (events, mut inbox) = mpsc::unbounded_channel();
    let mut backoff = Backoff::new(RECONNECT_MIN, RECONNECT_MAX);
    let mut reconnect_at = Some(Instant::now());

    // A machine with `clipboard = false` never looks for a compositor:
    // retrying forever on a server would be noise, and the daemon is
    // useful there anyway, as somewhere to paste from.
    if !service.settings.clipboard {
        service.set_backend(Err("turned off in config.toml".to_owned()));
        reconnect_at = None;
    }

    loop {
        if reconnect_at.is_some_and(|due| due <= Instant::now()) {
            reconnect_at = match service.connect_backend(&events).await {
                Ok(()) => {
                    backoff.reset();
                    None
                }
                Err(err) => {
                    service.set_backend(Err(format!("{err:#}")));
                    Some(Instant::now() + backoff.next_delay())
                }
            };
        }

        let sleep = tokio::time::sleep(next_wake(&service, reconnect_at));
        tokio::select! {
            event = inbox.recv() => match event {
                Some(event) => {
                    if service.on_backend_event(event) {
                        reconnect_at = Some(Instant::now() + backoff.next_delay());
                    }
                }
                // The sender is held by the service loop itself, so this
                // cannot happen while the loop runs.
                None => return,
            },
            () = sleep => {}
            () = service.wake.notified() => {}
        }

        service.change(|board| board.expire(SystemTime::now()));
    }
}

/// How long the service loop may sleep: until the lifetime runs out, the
/// next reconnection attempt, or a minute, whichever comes first.
fn next_wake(service: &ClipService, reconnect_at: Option<Instant>) -> Duration {
    // A deadline already in the past means no sleep at all, not a full
    // floor's wait: the copy is due now.
    let expiry = service
        .board
        .lock()
        .unwrap()
        .expires_at()
        .map_or(EXPIRY_FLOOR, |deadline| {
            deadline
                .duration_since(SystemTime::now())
                .unwrap_or_default()
        });
    let reconnect = reconnect_at.map_or(EXPIRY_FLOOR, |due| {
        due.saturating_duration_since(Instant::now())
    });

    expiry.min(reconnect).min(EXPIRY_FLOOR)
}

/// Reads the saved clipboard. An unreadable one costs the clip, not the
/// daemon.
fn load(dirs: &Dirs) -> eyre::Result<Saved> {
    let path = dirs.clip_file();
    let Some(bytes) = read_existing(&path, std::fs::read)? else {
        return Ok(Saved::default());
    };

    // A file another version wrote may decode into something else
    // entirely; leftover bytes are what gives it away.
    match postcard::take_from_bytes::<Saved>(&bytes) {
        Ok((saved, [])) => Ok(saved),
        Ok(_) => {
            tracing::warn!("{} has trailing bytes; starting empty", path.display());
            Ok(Saved::default())
        }
        Err(err) => {
            tracing::warn!("cannot read {}: {err}; starting empty", path.display());
            Ok(Saved::default())
        }
    }
}
