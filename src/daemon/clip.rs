//! The clipboard service: the clipboard state machine, wired up.
//!
//! [`crate::clip::Clipboard`] decides *what* should happen and returns
//! effects; this is what performs them, and what feeds it from the three
//! places clipboard changes come from:
//!
//! ```text
//!   compositor ──► captured ─┐
//!   CLI (socket) ──► copy ───┼─► Clipboard ──► effects ──► disk
//!   peers ────────► accept ──┘        │                └─► compositor
//!                                     └─► announce ────────► peers
//! ```
//!
//! It also runs the two things that happen without anyone asking: dropping
//! entries whose lifetime has run out, and reconnecting to the compositor
//! after the session goes away and comes back.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use color_eyre::eyre::{Result, WrapErr as _, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, info, warn};

use super::{
    backoff::Backoff,
    files::{Job, Spool},
    hub::Hub,
};
use crate::{
    clip::{
        Backend as _, Captured, Clipboard, Effect, Item, Pause, Policy, Rep, Selection, Switch,
        backend::{self, Command, Platform},
    },
    config::{Dirs, Settings, write_private},
    files::{FileRef, Hash},
    log::{self, Checkpoint, EntryId, Log, Watermark, WireEntry},
    net::proto::Topic,
};

/// First delay before reconnecting to the compositor.
const RECONNECT_MIN: Duration = Duration::from_secs(1);

/// Ceiling of that delay. A daemon started before the graphical session
/// waits this long at worst.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// How often the service loop wakes with nothing due. It is what catches
/// the things no event announces: a wall clock that jumped over an entry's
/// lifetime, and a `yank pause --for` whose time is up.
const EXPIRY_FLOOR: Duration = Duration::from_mins(1);

/// The state kept outside the log: where our own numbering is up to, and
/// which directions the user paused.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Persisted {
    checkpoint: Checkpoint,
    pause: Pause,
}

/// Why the clipboard is not being read right now.
#[derive(Clone, Debug)]
pub enum BackendState {
    Running,
    /// No compositor or compatible data-control protocol is available.
    Down(String),
}

/// The clipboard, everything it writes to, and everyone it tells.
#[derive(Debug)]
pub struct ClipService {
    dirs: Dirs,
    settings: Arc<Settings>,
    board: Mutex<Clipboard>,
    writer: log::Writer,
    backend: Mutex<Option<Platform>>,
    down: Mutex<Option<String>>,
    hub: Arc<Hub>,
    /// Where a copied file's contents go, and where the ones an entry
    /// names are asked for.
    spool: Spool,
    /// Woken when the state changed in a way the service loop cares about:
    /// a new lifetime to wait for, or a pause that was lifted.
    wake: Notify,
    /// Woken when an entry's files have been laid out here, for whoever is
    /// waiting on one.
    arrived: Notify,
}

/// Where an entry's files are.
#[derive(Debug)]
pub struct Located {
    pub id: EntryId,
    pub files: Arc<[FileRef]>,
    /// The tree they are laid out in, `None` while they are still on their
    /// way.
    pub tree: Option<PathBuf>,
}

impl ClipService {
    /// Loads the history from disk and starts the service. `identity` is
    /// this machine's endpoint id, which the log writes its entries under.
    pub fn open(
        dirs: &Dirs,
        identity: EndpointId,
        settings: Arc<Settings>,
        hub: Arc<Hub>,
        spool: Spool,
    ) -> Result<Arc<Self>> {
        let persisted = load_persisted(dirs)?;
        let history = dirs.history_dir();

        let mut board = Clipboard::new(
            Log::new(identity, Clipboard::limits(&settings), persisted.checkpoint),
            settings.clone(),
            persisted.pause,
        );
        let dropped = board.restore(log::Writer::load(&history)?);

        let service = Arc::new(ClipService {
            dirs: dirs.clone(),
            settings,
            board: Mutex::new(board),
            writer: log::Writer::spawn(history)?,
            backend: Mutex::new(None),
            down: Mutex::new(Some("starting".to_owned())),
            hub,
            spool,
            wake: Notify::new(),
            arrived: Notify::new(),
        });
        // Files the caps or a replayed removal dropped as the history was
        // read back. They hold clipboard contents in plain text, so they
        // go now rather than at some later cleanup.
        service.perform(dropped);

        Ok(service)
    }

    /// What this machine holds, for the announcements.
    pub fn have(&self) -> Watermark {
        self.board.lock().unwrap().have()
    }

    /// The entries a peer at `want` is missing.
    pub fn since(&self, want: &Watermark) -> Vec<WireEntry> {
        self.board.lock().unwrap().since(want)
    }

    /// Takes a batch of entries from a peer.
    pub fn accept(&self, entries: Vec<WireEntry>) -> Result<()> {
        let effects = self.board.lock().unwrap().accept(entries)?;
        self.perform(effects);

        Ok(())
    }

    /// Copies something from this machine.
    pub fn copy(&self, rep: Rep, secret: bool, ttl: Option<Duration>) -> Result<EntryId> {
        let (id, effects) =
            self.board
                .lock()
                .unwrap()
                .copy(Selection::of(rep), Vec::new(), secret, ttl)?;
        self.perform(effects);

        Ok(id)
    }

    /// Copies files from this machine: their contents are spooled first,
    /// so the entry never names what this machine cannot serve.
    ///
    /// What goes on the clipboard is the paths, the way a file manager
    /// offers them. Pasting it here is pasting the originals; pasting it
    /// on another machine is pasting the copies that land there.
    pub async fn copy_files(
        self: &Arc<Self>,
        paths: Vec<PathBuf>,
        ttl: Option<Duration>,
    ) -> Result<EntryId> {
        ensure!(
            self.settings.files,
            "sharing files is turned off in config.toml",
        );

        let service = self.clone();
        let spooled = paths.clone();
        let files = tokio::task::spawn_blocking(move || {
            super::files::take_all(&service.spool.store, &spooled, &service.settings)
        })
        .await
        .wrap_err("the spool task failed")??;

        let reserved = files.clone();
        let copied =
            self.board
                .lock()
                .unwrap()
                .copy(Selection::of_files(&paths), files, false, ttl);
        self.spool.store.release(&reserved);
        let (id, effects) = copied?;
        self.perform(effects);

        Ok(id)
    }

    /// Where an entry's files are, asking for them when they are not here
    /// and waiting up to `within` for them to arrive.
    ///
    /// The wait is the point: a transfer takes as long as the files are
    /// large, and a caller polling for the answer would ask hundreds of
    /// times to be told the same thing. Past the deadline the answer says
    /// they are not here; the fetch carries on regardless.
    pub async fn located(&self, needle: Option<&str>, within: Duration) -> Result<Located> {
        let (id, files) = {
            let board = self.board.lock().unwrap();
            let id = board.named(needle)?.id;
            let files = board.files(id);
            ensure!(!files.is_empty(), "entry {id} names no file");
            ensure!(
                !board.is_local(id),
                "entry {id} was copied here; its files are where they always were",
            );

            (id, files)
        };

        let deadline = Instant::now() + within;
        loop {
            // Registered before the tree is looked for, or files landing
            // between the two would be a wake-up nobody was waiting for.
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();

            let tree = self.board.lock().unwrap().tree(id).map(Path::to_path_buf);
            if tree.is_some() {
                return Ok(Located { id, files, tree });
            }
            // Fetching happens when an entry reaches the clipboard, so an
            // older one has to be asked for; never once its files are
            // here, since a fetch lays the tree out again and rebuilding
            // one somebody is reading is how a paste finds half of it.
            self.spool.send(Job::Fetch(id));

            let left = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(left, arrived).await.is_err() {
                return Ok(Located {
                    id,
                    files,
                    tree: None,
                });
            }
        }
    }

    /// Records a selection the compositor reported, with the files it
    /// named once they are spooled.
    pub fn record(&self, captured: Captured, files: Vec<FileRef>) {
        let recorded = self.board.lock().unwrap().captured(captured, files);
        match recorded {
            Ok(effects) => self.perform(effects),
            Err(err) => warn!("cannot record the selection: {err:#}"),
        }
    }

    /// Records that an entry's files are on this machine now.
    pub fn materialized(&self, id: EntryId, tree: PathBuf) {
        let effects = self.board.lock().unwrap().materialized(id, tree);
        self.perform(effects);
        self.arrived.notify_waiters();
    }

    /// The files an entry names.
    pub fn files(&self, id: EntryId) -> Arc<[FileRef]> {
        self.board.lock().unwrap().files(id)
    }

    /// What the spool may keep: the entries the history still holds, and
    /// the content they name.
    pub fn referenced(&self) -> (BTreeSet<EntryId>, BTreeSet<Hash>) {
        self.board.lock().unwrap().referenced()
    }

    /// Makes an entry already in the history the selection again.
    pub fn pick(&self, needle: &str) -> Result<EntryId> {
        let mut board = self.board.lock().unwrap();
        let id = board.resolve(needle)?.id;
        let (picked, effects) = board.pick(id)?;
        drop(board);
        self.perform(effects);

        Ok(picked)
    }

    /// One representation of an entry: the selection when none is named,
    /// and its best type when none is asked for.
    ///
    /// Returns the type of the bytes, and the other types the entry
    /// carries.
    pub fn paste(
        &self,
        needle: Option<&str>,
        mime: Option<&str>,
    ) -> Result<(String, Vec<String>, Vec<u8>)> {
        let board = self.board.lock().unwrap();
        let id = board.named(needle)?.id;
        let copy = board.body(id)?;

        let rep = match mime {
            Some(mime) => copy.selection.rep(mime).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "entry {id} has no {} in it; it has {}",
                    crate::config::sanitize(mime),
                    copy.selection
                        .reps()
                        .map(|rep| rep.mime.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            })?,
            None => &copy.selection.primary,
        };
        // Compared by where they sit rather than by name: the type asked
        // for is matched case-insensitively, so `--type TEXT/HTML` would
        // otherwise list `text/html` as an alternative to itself.
        let alternates = copy
            .selection
            .reps()
            .filter(|other| !std::ptr::eq(*other, rep))
            .map(|other| other.mime.clone())
            .collect();

        Ok((rep.mime.clone(), alternates, rep.bytes.clone()))
    }

    /// Drops one entry from every machine.
    pub fn forget(&self, needle: &str) -> Result<EntryId> {
        let mut board = self.board.lock().unwrap();
        let id = board.resolve(needle)?.id;
        let effects = board.forget(id)?;
        drop(board);
        self.perform(effects);

        Ok(id)
    }

    /// Empties the clipboard everywhere, and the history too when asked.
    pub fn clear(&self, history: bool) -> Result<()> {
        let mut board = self.board.lock().unwrap();
        let mut effects = board.clear()?;
        if history {
            effects.extend(board.purge()?);
        }
        drop(board);
        self.perform(effects);

        Ok(())
    }

    /// Pauses or resumes a direction of the clipboard.
    pub fn set_pause(&self, capture: Option<Switch>, apply: Option<Switch>) -> Result<Pause> {
        let mut board = self.board.lock().unwrap();
        let effects = board.set_pause(capture, apply);
        let pause = board.pause();
        drop(board);

        self.perform(effects);
        self.persist()?;
        self.wake.notify_one();

        Ok(pause)
    }

    /// The history, newest first, and which entry is the selection.
    pub fn history(&self) -> (Vec<Item>, Option<EntryId>) {
        let board = self.board.lock().unwrap();
        let selected = board.selection().map(|item| item.id);
        let items = board.history().into_iter().cloned().collect();

        (items, selected)
    }

    /// Which directions are running.
    pub fn pause(&self) -> Pause {
        self.board.lock().unwrap().pause()
    }

    /// Whether the compositor side is running, and why not when it is not.
    pub fn backend_state(&self) -> BackendState {
        match self.down.lock().unwrap().clone() {
            Some(reason) => BackendState::Down(reason),
            None => BackendState::Running,
        }
    }

    /// Performs what a mutation asked for, then tells the peers.
    ///
    /// Nothing is sent for a mutation that changed nothing, which is what
    /// keeps an idle mesh silent.
    fn perform(&self, effects: Vec<Effect>) {
        if effects.is_empty() {
            return;
        }

        let mut forgot = false;
        for effect in effects {
            match effect {
                Effect::Store(entry) => self.writer.store(&entry),
                Effect::Forget(id) => {
                    self.writer.forget(id);
                    forgot = true;
                }
                Effect::Apply(serves) => self.to_backend(Command::Offer(serves)),
                Effect::ClearSelection => self.to_backend(Command::Clear),
                Effect::Fetch(id) => self.spool.send(Job::Fetch(id)),
            }
        }
        // An entry that went takes its files with it. Swept rather than
        // deleted one by one: the same content may be named by another
        // entry, and only the whole history knows.
        if forgot {
            self.spool.send(Job::Sweep);
        }

        if let Err(err) = self.persist() {
            warn!("cannot save the clipboard state: {err:#}");
        }
        self.hub.announce(Topic::Clipboard, self.have());
        self.wake.notify_one();
    }

    /// Hands a command to the compositor, if there is one.
    fn to_backend(&self, command: Command) {
        if let Some(backend) = &*self.backend.lock().unwrap() {
            backend.send(command);
        } else {
            debug!("no clipboard backend; dropping {command:?}");
        }
    }

    /// Writes the sequence, clock and pause state.
    ///
    /// A few hundred bytes, at most once per clipboard event, written and
    /// renamed into place. Losing it would make this machine reuse entry
    /// numbers it already used, which every other machine would then
    /// ignore as already seen.
    fn persist(&self) -> Result<()> {
        let board = self.board.lock().unwrap();
        let persisted = Persisted {
            checkpoint: board.checkpoint(),
            pause: board.pause(),
        };
        drop(board);

        let bytes = serde_json::to_vec_pretty(&persisted).expect("clip state must serialize");
        write_private(&self.dirs.clip_file(), &bytes)
    }
}

/// Runs the clipboard service until the daemon stops.
///
/// Holds the connection to the compositor, reconnecting when the session
/// goes away, and drops entries as their lifetimes run out.
pub async fn run(service: Arc<ClipService>) {
    let (events, mut inbox) = mpsc::unbounded_channel();
    let mut backoff = Backoff::new(RECONNECT_MIN, RECONNECT_MAX);
    let mut reconnect_at = Some(Instant::now());

    // A machine with `clipboard = false` never looks for a compositor:
    // retrying forever on a server would be noise, and the daemon is
    // useful there anyway, as somewhere to paste from.
    if !service.settings.clipboard {
        service.mark_down("turned off in config.toml");
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
                    service.mark_down(&format!("{err:#}"));
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

        let effects = service.board.lock().unwrap().expire(SystemTime::now());
        service.perform(effects);
    }
}

impl ClipService {
    /// Connects to the compositor.
    ///
    /// Connecting means a round trip to the compositor, which is a
    /// blocking call on a process that may be busy or wedged, so it does
    /// not run on the async runtime.
    async fn connect_backend(&self, events: &mpsc::UnboundedSender<backend::Event>) -> Result<()> {
        let events = events.clone();
        let policy = Policy {
            max_bytes: self.settings.max_entry_bytes(),
            ignore: self.settings.ignore_mime.clone(),
        };
        let backend = tokio::task::spawn_blocking(move || backend::connect(&events, policy))
            .await
            .wrap_err("the clipboard connection task failed")?
            .wrap_err("cannot read the clipboard")?;

        *self.backend.lock().unwrap() = Some(backend);
        *self.down.lock().unwrap() = None;
        info!("clipboard connected");

        Ok(())
    }

    /// Handles one event from the compositor. Returns whether the backend
    /// has to be reconnected.
    fn on_backend_event(&self, event: backend::Event) -> bool {
        match event {
            backend::Event::Copied(captured) => {
                // A selection that names files is recorded by the file
                // service instead, once their contents are spooled: an
                // entry must never name content this machine cannot serve.
                // A secret is the exception, since spooling is a write to
                // disk and a secret never touches one.
                let paths = if self.settings.files && !captured.secret {
                    captured.selection.file_paths()
                } else {
                    Vec::new()
                };
                if paths.is_empty() {
                    self.spool.send(Job::Record(captured));
                } else {
                    self.spool.send(Job::Snapshot {
                        captured: Box::new(captured),
                        paths,
                    });
                }
                false
            }
            backend::Event::Lost(reason) => {
                warn!("clipboard disconnected: {reason}");
                // Taken out of the lock before it is dropped: dropping a
                // backend waits for its thread.
                let backend = self.backend.lock().unwrap().take();
                drop(backend);

                self.mark_down(&reason);
                true
            }
        }
    }

    fn mark_down(&self, reason: &str) {
        *self.down.lock().unwrap() = Some(reason.to_owned());
    }
}

/// How long the service loop may sleep: until the next lifetime runs out,
/// the next reconnection attempt, or a minute, whichever comes first.
fn next_wake(service: &ClipService, reconnect_at: Option<Instant>) -> Duration {
    // A deadline already in the past means no sleep at all, not a full
    // floor's wait: the entry is due now.
    let expiry = service
        .board
        .lock()
        .unwrap()
        .next_expiry()
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

/// Reads `clip.json`, treating a missing or unreadable file as empty: it
/// costs the entry numbering a restart, not the daemon.
fn load_persisted(dirs: &Dirs) -> Result<Persisted> {
    let path = dirs.clip_file();
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(persisted) => Ok(persisted),
            Err(err) => {
                warn!("cannot read {}: {err}", path.display());
                Ok(Persisted::default())
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Persisted::default()),
        Err(err) => Err(err).wrap_err_with(|| format!("cannot read {}", path.display())),
    }
}
