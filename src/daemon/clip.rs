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
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use color_eyre::eyre::{Result, WrapErr as _};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, info, warn};

use super::{backoff::Backoff, hub::Hub};
use crate::{
    clip::{
        Backend as _, Clipboard, Effect, Item, Pause, Switch,
        backend::{self, Command, Platform},
    },
    config::{Dirs, Settings, write_private},
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
    /// No compositor, or one without the data-control protocol. The daemon
    /// keeps replicating; `yank copy` and `yank paste` keep working. This
    /// is the normal state on a machine with no graphical session.
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
    /// Woken when the state changed in a way the service loop cares about:
    /// a new lifetime to wait for, or a pause that was lifted.
    wake: Notify,
}

impl ClipService {
    /// Loads the history from disk and starts the service. `identity` is
    /// this machine's endpoint id, which the log writes its entries under.
    pub fn open(
        dirs: &Dirs,
        identity: EndpointId,
        settings: Arc<Settings>,
        hub: Arc<Hub>,
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
            wake: Notify::new(),
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
    pub fn copy(
        &self,
        mime: String,
        bytes: Vec<u8>,
        secret: bool,
        ttl: Option<Duration>,
    ) -> Result<EntryId> {
        let (id, effects) = self.board.lock().unwrap().copy(mime, bytes, secret, ttl)?;
        self.perform(effects);

        Ok(id)
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

    /// The bytes of an entry: the selection when none is named.
    pub fn paste(&self, needle: Option<&str>) -> Result<(String, Vec<u8>)> {
        let board = self.board.lock().unwrap();
        let id = match needle {
            Some(needle) => board.resolve(needle)?.id,
            None => {
                board
                    .selection()
                    .ok_or_else(|| color_eyre::eyre::eyre!("the clipboard is empty"))?
                    .id
            }
        };
        let copy = board.body(id)?;

        Ok((copy.mime, copy.bytes))
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

    /// Whether the compositor side is up, and why not when it is not.
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

        for effect in effects {
            match effect {
                Effect::Store(entry) => self.writer.store(&entry),
                Effect::Forget(id) => self.writer.forget(id),
                Effect::Apply { mimes, bytes } => {
                    self.to_backend(Command::Offer { mimes, bytes });
                }
                Effect::ClearSelection => self.to_backend(Command::Clear),
            }
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
        let max_bytes = self.settings.max_entry_bytes();
        let backend = tokio::task::spawn_blocking(move || backend::connect(&events, max_bytes))
            .await
            .wrap_err("the clipboard connection task failed")?
            .wrap_err("cannot read the clipboard")?;

        *self.backend.lock().unwrap() = Some(backend);
        *self.down.lock().unwrap() = None;
        info!("clipboard connected");

        // The compositor reports what it holds as soon as it can, and that
        // report decides whether the mesh's selection is applied here (see
        // `Clipboard::captured`). Settling now, before it arrives, would
        // overwrite a selection the user made while the daemon was down.
        Ok(())
    }

    /// Handles one event from the compositor. Returns whether the backend
    /// has to be reconnected.
    fn on_backend_event(&self, event: backend::Event) -> bool {
        match event {
            backend::Event::Copied(captured) => {
                // Bound before performing: `perform` takes the same lock,
                // and a guard in a `match` scrutinee would still be held
                // while its arms run.
                let recorded = self.board.lock().unwrap().captured(captured);
                match recorded {
                    Ok(effects) => self.perform(effects),
                    Err(err) => warn!("cannot record the selection: {err:#}"),
                }

                // Whatever the compositor holds is now accounted for, so
                // an entry the mesh chose while we were down can be
                // applied without overwriting a newer local one.
                let effects = self.board.lock().unwrap().settle();
                self.perform(effects);
                false
            }
            backend::Event::Emptied => {
                // Somebody emptied the clipboard here. It is not shared:
                // applications empty it when they exit, and wiping every
                // machine for that would be worse than doing nothing.
                let mut board = self.board.lock().unwrap();
                board.emptied();
                let effects = board.settle();
                drop(board);

                self.perform(effects);
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
