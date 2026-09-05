//! The clipboard as a state machine.
//!
//! Everything the clipboard *is* on this machine is derived from the log,
//! by folding entries in whatever order they arrive:
//!
//! - the **history** is every [copied selection](struct@Copy) not
//!   forgotten, purged or expired, ordered by the mesh-wide clock;
//! - the **selection**, the thing actually on the clipboard, is the newest
//!   history entry written after the last [`Event::Clear`].
//!
//! Deriving rather than tracking is what makes catching up safe. A machine
//! that was off for a day receives a day of entries in one go and folds
//! them exactly as if it had seen them live: the history lands in the
//! right order, the same one entry wins, and the clipboard is touched
//! once, not fifty times. Every machine folding the same entries reaches
//! the same state, so there is no reconciliation step and nothing to
//! resolve when a machine comes back.
//!
//! An entry reaches the local clipboard once, when it becomes the
//! selection, and nothing puts it back afterwards. An application that
//! takes the selection and exits leaves this clipboard empty, the way any
//! Wayland clipboard is left empty; the entry is still in the history, and
//! `yank pick` is what asks for it again. Re-asserting it would mean
//! racing the application that took the selection, and the request sent
//! last is the one the compositor keeps, so what the race costs is
//! whatever the user copied a moment ago.
//!
//! No I/O happens here. Mutations return [`Effect`]s and the caller
//! performs them, which is what lets the whole thing be tested without a
//! compositor, a disk or a peer.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use color_eyre::eyre::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::{
    backend::{Captured, Serve},
    event::{Copy, Event, Rep, Selection},
    mime,
};
use crate::{
    config::Settings,
    files::{self, FileRef, Hash},
    log::{Change, Entry, EntryId, Hlc, Limits, Log, Watermark, WireEntry, payload},
    net::proto,
};

/// How much of an entry is kept for the preview shown by `yank history`.
const PREVIEW_CHARS: usize = 72;

/// What a mutation asks the caller to do.
#[derive(Debug)]
pub enum Effect {
    /// Write this entry to the history directory.
    Store(Entry),
    /// Remove this entry's file.
    Forget(EntryId),
    /// Put these representations of a selection on the local clipboard.
    Apply(Vec<Serve>),
    /// Empty the local clipboard.
    ClearSelection,
    /// Bring the files this entry names onto this machine, from whoever
    /// has them. Until they are here it is on the clipboard as text.
    Fetch(EntryId),
}

/// One entry as the history shows it. The bytes stay in the log; this is
/// what the daemon and the CLI pass around.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: EntryId,
    pub clock: Hlc,
    /// The type the entry is named by; the rest are in the log.
    pub mime: String,
    /// What the entry weighs: every representation, and the files it
    /// names.
    pub size: usize,
    pub secret: bool,
    /// When this entry disappears everywhere, if it was given a lifetime.
    pub expires_at: Option<SystemTime>,
    /// One line describing the contents; `<secret>` when it must not be
    /// shown.
    pub preview: String,
    /// The files the entry names, empty when it names none. Shared rather
    /// than owned: a manifest runs to thousands of files, and `yank list`
    /// has no use for one beyond its length.
    pub files: Arc<[FileRef]>,
    /// Hash of the copied bytes, to recognize the same content coming back
    /// from the compositor.
    hash: [u8; 32],
}

/// Whether a direction of the clipboard is running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Switch {
    #[default]
    On,
    /// Off, until the given time or until `yank resume`.
    Paused { until: Option<SystemTime> },
}

impl Switch {
    /// Whether the direction is running at `now`, a paused-until having
    /// elapsed counting as running.
    pub fn is_on(self, now: SystemTime) -> bool {
        match self {
            Switch::On => true,
            Switch::Paused { until } => until.is_some_and(|until| now >= until),
        }
    }
}

/// Which directions of the clipboard are running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pause {
    /// Sharing what is copied here.
    pub capture: Switch,
    /// Putting what others copied on this clipboard.
    pub apply: Switch,
}

/// The clipboard of this machine.
#[derive(Debug)]
pub struct Clipboard {
    log: Log,
    settings: Arc<Settings>,
    /// The visible history, keyed by entry.
    items: BTreeMap<EntryId, Item>,
    /// Entries dropped mesh-wide, remembered so a `Forget` that arrives
    /// before the entry it names still takes effect.
    forgotten: BTreeSet<EntryId>,
    /// Nothing written at or before this point is kept.
    purged_through: Hlc,
    /// Nothing written at or before this point can be the selection.
    cleared_through: Hlc,
    /// The entry we last put on the local clipboard, whether or not it is
    /// still there, so we neither put it on again nor mistake it for
    /// something the user copied.
    applied: Option<EntryId>,
    /// Entries whose files are on this machine, and where they were laid
    /// out. Until an entry is in here its paths are another machine's, and
    /// are offered as text rather than as files.
    ready: BTreeMap<EntryId, PathBuf>,
    pause: Pause,
}

impl Clipboard {
    /// Builds the clipboard over `log`.
    pub fn new(log: Log, settings: Arc<Settings>, pause: Pause) -> Self {
        Clipboard {
            log,
            settings,
            items: BTreeMap::new(),
            forgotten: BTreeSet::new(),
            purged_through: Hlc::default(),
            cleared_through: Hlc::default(),
            applied: None,
            ready: BTreeMap::new(),
            pause,
        }
    }

    /// The limits a log serving this clipboard should be built with.
    ///
    /// Both are clamped to what the protocol can carry, because a history
    /// bigger than one fetch response is a history that can never be
    /// replicated: the receiver would refuse every over-long response and
    /// the two machines would stop syncing altogether, quietly.
    ///
    /// The payload cap is the protocol ceiling rather than the configured
    /// one, on purpose: a peer with a larger setting must still have its
    /// entries accepted, or the two would disagree about the history. The
    /// configured cap is what bounds this machine's own copies.
    pub fn limits(settings: &Settings) -> Limits {
        Limits {
            entries: settings.history_entries().min(proto::MAX_FETCH_ENTRIES),
            bytes: usize::try_from(settings.history_budget.as_u64()).unwrap_or(usize::MAX),
            payload: crate::log::MAX_ENTRY_BYTES as usize,
        }
    }

    /// What this machine holds, to announce to peers.
    pub fn have(&self) -> Watermark {
        self.log.have()
    }

    /// The entries a peer at `want` is missing.
    pub fn since(&self, want: &Watermark) -> Vec<WireEntry> {
        self.log.since(want).map(WireEntry::from).collect()
    }

    /// The sequence and clock state to persist.
    pub fn checkpoint(&self) -> crate::log::Checkpoint {
        self.log.checkpoint()
    }

    /// Which directions are running.
    pub fn pause(&self) -> Pause {
        self.pause
    }

    /// The history, newest first.
    pub fn history(&self) -> Vec<&Item> {
        let mut items: Vec<&Item> = self.items.values().collect();
        items.sort_by_key(|item| std::cmp::Reverse((item.clock, item.id)));
        items
    }

    /// The entry currently on the clipboard, as far as the mesh is
    /// concerned.
    pub fn selection(&self) -> Option<&Item> {
        self.items
            .values()
            .filter(|item| item.clock > self.cleared_through)
            .max_by_key(|item| (item.clock, item.id))
    }

    /// Resolves what a user typed to one entry: a full or partial
    /// [`EntryId::label`].
    pub fn resolve(&self, needle: &str) -> Result<&Item> {
        let matched: Vec<&Item> = self
            .items
            .values()
            .filter(|item| item.id.label().starts_with(needle))
            .collect();

        match matched.as_slice() {
            [item] => Ok(item),
            [] => bail!("no entry matching `{}`", crate::config::sanitize(needle)),
            items => bail!(
                "`{}` matches {} entries",
                crate::config::sanitize(needle),
                items.len()
            ),
        }
    }

    /// The entry a caller named, or the selection when it named none.
    pub fn named(&self, needle: Option<&str>) -> Result<&Item> {
        match needle {
            Some(needle) => self.resolve(needle),
            None => self
                .selection()
                .ok_or_else(|| color_eyre::eyre::eyre!("the clipboard is empty")),
        }
    }

    /// The copied bytes of an entry, decoded from the log.
    pub fn body(&self, id: EntryId) -> Result<Copy> {
        let entry = self
            .log
            .get(id)
            .ok_or_else(|| color_eyre::eyre::eyre!("entry {id} is gone"))?;
        match Event::decode(&entry.payload)? {
            Event::Copy(copy) => Ok(copy),
            _ => bail!("entry {id} is not a copied selection"),
        }
    }

    /// Records something copied on this machine, from `yank copy`.
    ///
    /// Unlike [`Self::captured`] the bytes are not on the clipboard yet,
    /// so this both shares them and puts them there. `reps` is the
    /// selection in every type it is being shared in, best first.
    pub fn copy(
        &mut self,
        selection: Selection,
        files: Vec<FileRef>,
        secret: bool,
        ttl: Option<Duration>,
    ) -> Result<(EntryId, Vec<Effect>)> {
        let size = selection.size();
        files::validate(&files)?;
        ensure!(size > 0, "nothing to copy");
        ensure!(
            size <= self.settings.max_entry_bytes(),
            "the selection is larger than the {} limit in config.toml",
            self.settings.max_entry_size,
        );

        let event = Event::Copy(Copy {
            selection,
            secret,
            ttl: self.lifetime(secret, ttl),
            files,
        });
        let (id, changes) = self.log.append(event.encode(), !secret)?;

        Ok((id, self.ingest(changes)))
    }

    /// Records something the compositor says was copied.
    ///
    /// The bytes are already on the clipboard, so the new entry is marked
    /// as applied: putting them back would take the selection away from
    /// the application that owns it, for nothing.
    pub fn captured(&mut self, captured: Captured, files: Vec<FileRef>) -> Result<Vec<Effect>> {
        if !self.pause.capture.is_on(SystemTime::now()) {
            return Ok(Vec::new());
        }
        let Some(selection) = self.fit(captured.selection) else {
            tracing::debug!("ignoring a selection over the configured limit");
            return Ok(Vec::new());
        };

        // The clipboard already holding what the mesh selected is the
        // normal case after a restart, or after we applied an entry an
        // application then re-announced. Recording it again would put a
        // duplicate in everyone's history on every restart. The first
        // representation is enough to tell: it is the one an application
        // that re-announces our selection announces it under.
        let hash = hash(&selection.primary.bytes);
        if let Some(current) = self.selection()
            && current.hash == hash
        {
            self.applied = Some(current.id);
            return Ok(Vec::new());
        }

        let event = Event::Copy(Copy {
            selection,
            secret: captured.secret,
            ttl: self.lifetime(captured.secret, None),
            files,
        });
        let (id, changes) = self.log.append(event.encode(), !captured.secret)?;
        let mut effects = self.fold(changes);
        // Recorded before reconciling: the bytes are on the clipboard
        // already, and reconciling without this would take the selection
        // away from the application that owns it, to put back what it just
        // put there.
        self.applied = Some(id);
        effects.extend(self.reconcile());

        Ok(effects)
    }

    /// Takes a batch of entries from a peer.
    ///
    /// A batch, not one entry at a time, and this is the whole point: a
    /// machine catching up on a day of history folds all of it before the
    /// clipboard is touched, so it is set once, to the entry that actually
    /// won, instead of flickering through every entry in turn.
    ///
    /// One unusable entry fails the batch, and the batch is checked
    /// before any of it is taken, so a bad entry halfway through leaves
    /// nothing behind. A peer that sends those is broken or hostile, and
    /// dropping the batch costs nothing: its next announcement makes us
    /// ask again.
    pub fn accept(&mut self, entries: Vec<WireEntry>) -> Result<Vec<Effect>> {
        for wire in &entries {
            self.log.validate(wire)?;
            if let Ok(Event::Copy(copy)) = Event::decode(&wire.payload) {
                files::validate(&copy.files)?;
            }
        }

        let mut changes = Vec::new();
        for wire in entries {
            // Whether an entry may be persisted is decided by what it
            // says, since the machine that copied it is the one that knew
            // it was a password.
            let durable = match Event::decode(&wire.payload) {
                Ok(Event::Copy(copy)) => !copy.secret,
                Ok(_) => true,
                Err(_) => false,
            };
            changes.extend(self.log.accept(wire, durable)?);
        }

        Ok(self.ingest(changes))
    }

    /// Re-inserts the entries read back from disk.
    ///
    /// Returns only the removals, since everything else read back is
    /// already where it belongs. Those removals matter: a history limit
    /// the user lowered, or a `Forget` replayed from the log, drops
    /// entries whose files would otherwise sit there in plain text for
    /// good, invisible to `yank list` and out of reach of
    /// `yank clear --history`.
    ///
    /// Nothing reaches the clipboard here, and nothing ever will for what
    /// was read back: those entries have had their turn on it.
    pub fn restore(&mut self, entries: Vec<WireEntry>) -> Vec<Effect> {
        let restored: Vec<EntryId> = entries.iter().map(|wire| wire.id).collect();

        let mut changes = Vec::new();
        for wire in entries {
            changes.extend(self.log.restore(wire));
        }
        // The effects of the fold are of no use here: every entry it adds
        // is one that was just read off the disk. What matters is what did
        // *not* survive, which is asked of the log rather than read off
        // the changes, since an entry the caps drop the moment it lands is
        // reported as never having landed at all.
        let _ = self.fold(changes);
        // A restart is not a reason to put an entry on the clipboard. What
        // the session holds is the compositor's to report, and it may be
        // something the user copied while the daemon was down.
        self.applied = self.selection().map(|item| item.id);

        restored
            .into_iter()
            .filter(|id| self.log.get(*id).is_none())
            .map(Effect::Forget)
            .collect()
    }

    /// Makes an entry already in the history the selection again, by
    /// copying it anew: history is append-only, so promoting an entry
    /// means writing it, not moving it.
    pub fn pick(&mut self, id: EntryId) -> Result<(EntryId, Vec<Effect>)> {
        let copy = self.body(id)?;
        let ttl = copy.ttl.map(u64::from).map(Duration::from_secs);
        let secret = copy.secret;
        let local = self.is_local(id);
        // The new entry is ours, written under this machine's identity, so
        // what it may claim is what this machine can back up: the files of
        // a copy made elsewhere are ours to name only while we hold their
        // contents.
        let held = local || self.ready.contains_key(&id);
        let files = if held { copy.files.clone() } else { Vec::new() };
        // Its paths are not carried over even when they are here: a tree
        // belongs to the entry that named it, and the new entry has none
        // until its own fetch lands.
        let selection = if local {
            copy.selection
        } else {
            stripped(copy)
        };

        let (picked, mut effects) = self.copy(selection, files, secret, ttl)?;
        if held && !local {
            effects.push(Effect::Fetch(picked));
        }

        Ok((picked, effects))
    }

    /// Empties the clipboard on every machine.
    pub fn clear(&mut self) -> Result<Vec<Effect>> {
        let (_, changes) = self.log.append(Event::Clear.encode(), true)?;

        Ok(self.ingest(changes))
    }

    /// Drops one entry from every machine.
    pub fn forget(&mut self, id: EntryId) -> Result<Vec<Effect>> {
        let (_, changes) = self.log.append(Event::Forget(id).encode(), true)?;

        Ok(self.ingest(changes))
    }

    /// Drops the whole history from every machine.
    ///
    /// What it covers is its own timestamp, not the newest entry this
    /// machine happens to hold: a machine that has not caught up yet holds
    /// little or nothing, and a purge derived from that would drop nothing
    /// anywhere while reporting success.
    pub fn purge(&mut self) -> Result<Vec<Effect>> {
        let (_, changes) = self.log.append(Event::Purge.encode(), true)?;

        Ok(self.ingest(changes))
    }

    /// Pauses a direction, or both when neither is named.
    pub fn set_pause(&mut self, capture: Option<Switch>, apply: Option<Switch>) -> Vec<Effect> {
        if let Some(capture) = capture {
            self.pause.capture = capture;
        }
        if let Some(apply) = apply {
            self.pause.apply = apply;
        }

        self.reconcile()
    }

    /// Drops everything whose lifetime has run out.
    ///
    /// Expiry needs no agreement between machines: they all hold the same
    /// entry with the same lifetime, so they all drop it at the same
    /// moment without saying a word. It works with the network down, which
    /// is the point of a lifetime on a password.
    pub fn expire(&mut self, now: SystemTime) -> Vec<Effect> {
        let expired: Vec<EntryId> = self
            .items
            .values()
            .filter(|item| item.expires_at.is_some_and(|deadline| now >= deadline))
            .map(|item| item.id)
            .collect();

        let mut effects = Vec::new();
        for id in expired {
            // An entry that was the selection empties the clipboard
            // instead of falling back to an older one: what a lifetime
            // promises is that the thing is gone, not that the previous
            // password comes back.
            if self.selection().is_some_and(|item| item.id == id) {
                let clock = self.items[&id].clock;
                self.cleared_through = self.cleared_through.max(clock);
            }
            self.drop_item(id, &mut effects);
        }
        effects.extend(self.reconcile());

        effects
    }

    /// When [`Self::expire`] next has something to do.
    pub fn next_expiry(&self) -> Option<SystemTime> {
        self.items.values().filter_map(|item| item.expires_at).min()
    }

    /// Folds log changes into the history and reconciles the clipboard.
    fn ingest(&mut self, changes: Vec<Change>) -> Vec<Effect> {
        let mut effects = self.fold(changes);
        effects.extend(self.reconcile());

        effects
    }

    /// Folds log changes into the history, without touching the clipboard.
    fn fold(&mut self, changes: Vec<Change>) -> Vec<Effect> {
        let mut effects = Vec::new();

        for change in changes {
            match change {
                Change::Added(entry) => self.add(&entry, &mut effects),
                Change::Dropped(id) => {
                    self.items.remove(&id);
                    effects.push(Effect::Forget(id));
                }
            }
        }

        effects
    }

    /// Applies one new log entry.
    fn add(&mut self, entry: &Entry, effects: &mut Vec<Effect>) {
        let event = match Event::decode(&entry.payload) {
            Ok(event) => event,
            // An entry a newer version of yank wrote. It stays in the log
            // so it keeps propagating, but it means nothing here.
            Err(err) => {
                tracing::debug!("ignoring entry {}: {err:#}", entry.id);
                return;
            }
        };

        match event {
            Event::Copy(copy) => {
                if self.forgotten.contains(&entry.id) || entry.clock <= self.purged_through {
                    self.log.remove(entry.id);
                    effects.push(Effect::Forget(entry.id));
                    return;
                }
                if let Some(item) = Self::item(entry, copy) {
                    self.items.insert(entry.id, item);
                    Self::store(entry, effects);
                } else {
                    // Its lifetime ran out before it reached us.
                    self.log.remove(entry.id);
                    effects.push(Effect::Forget(entry.id));
                }
            }
            Event::Clear => {
                self.cleared_through = self.cleared_through.max(entry.clock);
                Self::store(entry, effects);
            }
            Event::Forget(target) => {
                self.forgotten.insert(target);
                self.drop_item(target, effects);
                Self::store(entry, effects);
            }
            Event::Purge => {
                let through = entry.clock;
                self.purged_through = self.purged_through.max(through);
                let stale: Vec<EntryId> = self
                    .items
                    .values()
                    .filter(|item| item.clock <= through)
                    .map(|item| item.id)
                    .collect();
                for id in stale {
                    self.drop_item(id, effects);
                }
                Self::store(entry, effects);
            }
        }
    }

    /// Queues an entry for the disk, unless it must never touch one.
    fn store(entry: &Entry, effects: &mut Vec<Effect>) {
        if entry.durable {
            effects.push(Effect::Store(entry.clone()));
        }
    }

    /// Removes an entry from the history and from the log.
    fn drop_item(&mut self, id: EntryId, effects: &mut Vec<Effect>) {
        self.items.remove(&id);
        self.ready.remove(&id);
        self.log.remove(id);
        effects.push(Effect::Forget(id));
    }

    /// Brings the local clipboard in line with the selection.
    fn reconcile(&mut self) -> Vec<Effect> {
        if !self.pause.apply.is_on(SystemTime::now()) {
            return Vec::new();
        }

        match self.selection().map(|item| item.id) {
            Some(id) if self.applied != Some(id) => {
                let Ok(copy) = self.body(id) else {
                    return Vec::new();
                };
                self.applied = Some(id);

                // Asked for once, as the entry reaches the clipboard, and
                // answered by [`Self::materialized`] whenever it lands.
                // The clipboard is not held up for it: what it holds until
                // then is the paths as text.
                let mut effects = Vec::new();
                if copy.is_files()
                    && !copy.secret
                    && !self.is_local(id)
                    && !self.ready.contains_key(&id)
                {
                    effects.push(Effect::Fetch(id));
                }
                effects.push(Effect::Apply(serves(&self.carried(copy, id))));

                effects
            }
            None if self.applied.is_some() => {
                self.applied = None;

                vec![Effect::ClearSelection]
            }
            _ => Vec::new(),
        }
    }

    /// Builds the history entry for a copy, or `None` when its lifetime
    /// has already run out and when it carries nothing at all.
    fn item(entry: &Entry, copy: Copy) -> Option<Item> {
        files::validate(&copy.files).ok()?;
        let expires_at = copy.ttl.map(|ttl| {
            let ttl = Duration::from_secs(u64::from(ttl));
            // Counted from when it was written, but never further out than
            // a full lifetime from now: a machine whose clock is in the
            // future must not be able to keep a password around longer
            // than it promised.
            (entry.clock.as_system_time() + ttl).min(SystemTime::now() + ttl)
        });
        if expires_at.is_some_and(|deadline| SystemTime::now() >= deadline) {
            return None;
        }

        let named = usize::try_from(files::total(&copy.files).ok()?).unwrap_or(usize::MAX);
        let primary = &copy.selection.primary;

        Some(Item {
            id: entry.id,
            clock: entry.clock,
            mime: primary.mime.clone(),
            size: copy.selection.size().saturating_add(named),
            secret: copy.secret,
            expires_at,
            preview: preview(&primary.mime, &primary.bytes, copy.secret),
            files: copy.files.into(),
            hash: hash(&primary.bytes),
        })
    }

    /// Whether an entry was copied on this machine.
    pub fn is_local(&self, id: EntryId) -> bool {
        id.origin == self.log.origin()
    }

    /// Records that an entry's files are on this machine, laid out under
    /// `tree`, and offers them as files if it is what the clipboard holds.
    pub fn materialized(&mut self, id: EntryId, tree: PathBuf) -> Vec<Effect> {
        self.ready.insert(id, tree);
        // Only the entry on the clipboard has anything new to offer, and
        // it has to be applied again for the file types to appear.
        if self.applied == Some(id) {
            self.applied = None;
        }

        self.reconcile()
    }

    /// Where an entry's files are laid out here, if they are.
    pub fn tree(&self, id: EntryId) -> Option<&Path> {
        self.ready.get(&id).map(PathBuf::as_path)
    }

    /// The files an entry names, for the fetch it asked for.
    pub fn files(&self, id: EntryId) -> Arc<[FileRef]> {
        self.items
            .get(&id)
            .map_or_else(|| Arc::from([]), |item| item.files.clone())
    }

    /// Every entry the history still holds, and the content those entries
    /// name: what the spool may keep, and nothing else.
    pub fn referenced(&self) -> (BTreeSet<EntryId>, BTreeSet<Hash>) {
        let entries = self.items.keys().copied().collect();
        let content = self
            .items
            .values()
            .flat_map(|item| item.files.iter().map(|file| file.hash))
            .collect();

        (entries, content)
    }

    /// The representations of an entry that mean anything on this machine.
    ///
    /// A path is only true where the files are: on the machine that copied
    /// them, and on any machine that has fetched them since, one that
    /// picked the entry included. Until then what is left of it is the
    /// text of those paths, which is at least honest.
    fn carried(&self, copy: Copy, id: EntryId) -> Selection {
        match self.ready.get(&id) {
            Some(tree) => rehomed(&copy, tree),
            None if self.is_local(id) => copy.selection,
            None => stripped(copy),
        }
    }

    /// Drops the alternates that do not fit the entry cap, from the back,
    /// since they arrive best first. Nothing fits when the primary alone
    /// is over it: that is a selection too big to share, not one to share
    /// in part.
    fn fit(&self, mut selection: Selection) -> Option<Selection> {
        let cap = self.settings.max_entry_bytes();
        let mut size = selection.size();
        while size > cap {
            size -= selection.alternates.pop()?.bytes.len();
        }

        Some(selection)
    }

    /// The lifetime an entry gets: what was asked for, or the configured
    /// default when it is a secret and nothing was asked.
    fn lifetime(&self, secret: bool, ttl: Option<Duration>) -> Option<u32> {
        let ttl = ttl.or_else(|| secret.then_some(self.settings.secret_ttl))?;

        Some(u32::try_from(ttl.as_secs()).unwrap_or(u32::MAX))
    }
}

/// The same selection with its file references taken out: the paths in
/// them are another machine's, and this one has nothing to resolve them
/// against.
///
/// An entry that was nothing but paths keeps them as text, since a path is
/// something a person can read, and `text/uri-list` is the one of those
/// types that is nothing but paths.
fn stripped(copy: Copy) -> Selection {
    let (kept, paths): (Vec<Rep>, Vec<Rep>) = copy
        .selection
        .into_reps()
        .partition(|rep| !mime::is_local(&rep.mime));
    if let Some(selection) = Selection::new(kept) {
        return selection;
    }

    let bytes = paths
        .iter()
        .find(|rep| rep.mime.eq_ignore_ascii_case(mime::URI_LIST))
        .or(paths.first())
        .expect("a selection carries at least one representation")
        .bytes
        .clone();

    Selection::of(Rep::new(mime::PLAIN_TEXT, bytes))
}

/// The same selection, naming the copies of its files that are on this
/// machine.
///
/// Every representation that names where the files are is rebuilt rather
/// than carried across, since what it has to say is where they are *here*:
/// handing a terminal the path the copying machine used would be handing
/// it a path that does not exist. What the source offered besides is kept.
/// The preview is not touched, so `yank list` still shows where the entry
/// came from.
fn rehomed(copy: &Copy, tree: &Path) -> Selection {
    let paths: Vec<PathBuf> = copy
        .files
        .iter()
        .map(|file| tree.join(&file.path))
        .collect();

    let mut selection = Selection::of_files(&paths);
    selection.alternates.extend(
        copy.selection
            .reps()
            .filter(|rep| !mime::is_local(&rep.mime) && !mime::is_text(&rep.mime))
            .cloned(),
    );

    selection
}

/// What the compositor is handed for a selection: every representation
/// under its own type, text under all of the names for text (see
/// [`mime::aliases`]).
fn serves(selection: &Selection) -> Vec<Serve> {
    let mut serves: Vec<Serve> = Vec::new();
    for rep in selection.reps() {
        let mimes: Vec<String> = mime::aliases(&rep.mime)
            .into_iter()
            .filter(|mime| !serves.iter().any(|serve| serve.mimes.contains(mime)))
            .collect();
        if mimes.is_empty() {
            continue;
        }
        serves.push(Serve {
            mimes,
            bytes: payload(rep.bytes.clone()),
        });
    }

    serves
}

/// One line describing an entry, safe to print.
fn preview(mime: &str, bytes: &[u8], secret: bool) -> String {
    if secret {
        return "<secret>".to_owned();
    }
    if !mime::is_text(mime) {
        return format!("<{mime}, {}>", bytesize::ByteSize::b(bytes.len() as u64));
    }

    let text = String::from_utf8_lossy(bytes);
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut preview = crate::config::sanitize(line.trim());
    if let Some((cut, _)) = preview.char_indices().nth(PREVIEW_CHARS) {
        preview.truncate(cut);
        preview.push('…');
    }
    if text.lines().filter(|line| !line.trim().is_empty()).count() > 1 {
        preview.push_str(" ⏎");
    }

    preview
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Checkpoint, Watermark};

    fn clipboard() -> Clipboard {
        settings_clipboard(Settings::default())
    }

    fn settings_clipboard(settings: Settings) -> Clipboard {
        let settings = Arc::new(settings);
        let log = Log::new(
            iroh::SecretKey::generate().public(),
            Clipboard::limits(&settings),
            Checkpoint::default(),
        );

        Clipboard::new(log, settings, Pause::default())
    }

    fn no_files() -> Vec<FileRef> {
        Vec::new()
    }

    fn text(text: &str) -> Selection {
        Selection::of(Rep::new("text/plain", text.as_bytes().to_vec()))
    }

    fn selection(reps: Vec<Rep>) -> Selection {
        Selection::new(reps).unwrap()
    }

    fn copy(board: &mut Clipboard, contents: &str) -> Vec<Effect> {
        board
            .copy(text(contents), no_files(), false, None)
            .unwrap()
            .1
    }

    fn captured(contents: &str, secret: bool) -> Captured {
        Captured {
            selection: text(contents),
            secret,
        }
    }

    /// Everything `from` holds, as a peer would send it to a machine that
    /// holds nothing.
    fn wire(from: &Clipboard) -> Vec<WireEntry> {
        from.since(&Watermark::default())
    }

    /// What `from` would answer a fetch from `to` with. Unlike [`wire`]
    /// this respects the asking machine's watermark, so it never hands a
    /// machine its own entries back, which no real fetch does either.
    fn peer_fetch(from: &Clipboard, to: &Clipboard) -> Vec<WireEntry> {
        from.since(&to.have())
    }

    /// The first representation of everything put on the clipboard.
    fn applied(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Apply(serves) => {
                    Some(String::from_utf8_lossy(&serves[0].bytes).into_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// What one mutation put on the clipboard, by type.
    fn served(effects: &[Effect]) -> Vec<(Vec<String>, String)> {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Apply(serves) => Some(serves),
                _ => None,
            })
            .expect("nothing was put on the clipboard")
            .iter()
            .map(|serve| {
                (
                    serve.mimes.clone(),
                    String::from_utf8_lossy(&serve.bytes).into_owned(),
                )
            })
            .collect()
    }

    fn previews(board: &Clipboard) -> Vec<String> {
        board
            .history()
            .into_iter()
            .map(|item| item.preview.clone())
            .collect()
    }

    #[test]
    fn copying_shares_it_and_puts_it_on_the_clipboard() {
        let mut board = clipboard();
        let effects = copy(&mut board, "hello");

        assert_eq!(applied(&effects), vec!["hello"]);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Store(_)))
                .count(),
            1
        );
        assert_eq!(board.selection().unwrap().preview, "hello");
    }

    #[test]
    fn what_the_compositor_reports_is_not_put_back() {
        let mut board = clipboard();
        let effects = board
            .captured(captured("typed elsewhere", false), no_files())
            .unwrap();

        // It is already on the clipboard: taking the selection away from
        // the application that owns it would gain nothing.
        assert!(applied(&effects).is_empty());
        assert_eq!(previews(&board), vec!["typed elsewhere"]);
    }

    /// A restart is not one either. What it would put on the clipboard is
    /// whatever was copied last before it, and it would take the selection
    /// from the application that owns it now.
    #[test]
    fn a_restored_history_is_not_put_back_on_the_clipboard() {
        let mut board = clipboard();
        copy(&mut board, "before the restart");

        let mut restarted = clipboard();
        let effects = restarted.restore(wire(&board));
        assert!(applied(&effects).is_empty());
        // Including on every reconciliation that follows: the service loop
        // runs one whenever it wakes to expire entries.
        assert!(applied(&restarted.expire(SystemTime::now())).is_empty());
        assert_eq!(restarted.selection().unwrap().preview, "before the restart");
    }

    /// What lands *after* the restart is a reason, or a machine coming
    /// back would never take the clipboard the mesh moved on to.
    #[test]
    fn an_entry_arriving_after_a_restart_still_reaches_the_clipboard() {
        let mut board = clipboard();
        copy(&mut board, "before the restart");
        let mut restarted = clipboard();
        restarted.restore(wire(&board));

        copy(&mut board, "copied elsewhere since");
        let effects = restarted.accept(peer_fetch(&board, &restarted)).unwrap();

        assert_eq!(applied(&effects), vec!["copied elsewhere since"]);
    }

    #[test]
    fn the_same_content_coming_back_is_not_a_new_entry() {
        let mut board = clipboard();
        copy(&mut board, "hello");

        // What a compositor announces after we set the selection, and
        // what a restart finds on the clipboard.
        let effects = board
            .captured(captured("hello", false), no_files())
            .unwrap();
        assert!(effects.is_empty());
        assert_eq!(board.history().len(), 1);
    }

    #[test]
    fn catching_up_touches_the_clipboard_once() {
        let mut peer = clipboard();
        for text in ["one", "two", "three"] {
            copy(&mut peer, text);
        }

        let mut board = clipboard();
        let effects = board.accept(wire(&peer)).unwrap();

        // The whole backlog lands in the history, in order, and only the
        // entry that won reaches the clipboard.
        assert_eq!(previews(&board), vec!["three", "two", "one"]);
        assert_eq!(applied(&effects), vec!["three"]);
    }

    #[test]
    fn a_local_copy_outranks_an_older_one_arriving_late() {
        let mut peer = clipboard();
        copy(&mut peer, "from the peer");
        // The clock has millisecond resolution, and two entries sharing a
        // millisecond are ordered by machine, not by who copied first.
        std::thread::sleep(Duration::from_millis(2));

        let mut board = clipboard();
        copy(&mut board, "typed just now");
        let effects = board.accept(wire(&peer)).unwrap();

        // The peer's entry joins the history but does not win.
        assert!(applied(&effects).is_empty());
        assert_eq!(board.selection().unwrap().preview, "typed just now");
        assert_eq!(board.history().len(), 2);
    }

    #[test]
    fn two_machines_fold_the_same_entries_into_the_same_state() {
        let mut first = clipboard();
        let mut second = clipboard();
        copy(&mut first, "from the first");
        std::thread::sleep(Duration::from_millis(2));
        copy(&mut second, "from the second");

        let (from_first, from_second) = (wire(&first), wire(&second));
        first.accept(from_second).unwrap();
        second.accept(from_first).unwrap();

        assert_eq!(previews(&first), previews(&second));
        assert_eq!(
            first.selection().unwrap().id,
            second.selection().unwrap().id,
        );
    }

    #[test]
    fn clearing_empties_the_clipboard_and_keeps_the_history() {
        let mut board = clipboard();
        copy(&mut board, "hello");
        let effects = board.clear().unwrap();

        assert!(matches!(effects.as_slice(), [.., Effect::ClearSelection]));
        assert!(board.selection().is_none());
        assert_eq!(previews(&board), vec!["hello"]);
    }

    #[test]
    fn a_clear_reaches_a_machine_that_was_offline() {
        let mut peer = clipboard();
        copy(&mut peer, "hello");
        peer.clear().unwrap();

        let mut board = clipboard();
        let effects = board.accept(wire(&peer)).unwrap();

        // The entry is in the history, but nothing is put on the
        // clipboard: the clear that followed it applies here too.
        assert_eq!(previews(&board), vec!["hello"]);
        assert!(board.selection().is_none());
        assert!(applied(&effects).is_empty());
    }

    #[test]
    fn forgetting_an_entry_removes_it_everywhere() {
        let mut peer = clipboard();
        copy(&mut peer, "a mistake");
        let id = peer.selection().unwrap().id;
        peer.forget(id).unwrap();

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();

        assert!(board.history().is_empty());
        assert!(peer.history().is_empty());
    }

    #[test]
    fn a_forget_arriving_before_its_entry_still_applies() {
        let mut peer = clipboard();
        copy(&mut peer, "a mistake");
        let id = peer.selection().unwrap().id;
        peer.forget(id).unwrap();

        // The entry itself is dropped from the batch, as if it had been
        // lost and re-sent later by another machine.
        let mut entries = wire(&peer);
        let copied = entries.remove(0);

        let mut board = clipboard();
        board.accept(entries).unwrap();
        board.accept(vec![copied]).unwrap();

        assert!(board.history().is_empty());
    }

    #[test]
    fn purging_drops_the_history_everywhere() {
        let mut peer = clipboard();
        copy(&mut peer, "one");
        copy(&mut peer, "two");
        peer.purge().unwrap();

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();

        assert!(board.history().is_empty());
        assert!(peer.history().is_empty());
    }

    #[test]
    fn a_lifetime_removes_the_entry_and_empties_the_clipboard() {
        let mut board = clipboard();
        board
            .copy(
                text("a password"),
                no_files(),
                true,
                Some(Duration::from_secs(90)),
            )
            .unwrap();

        let deadline = board.next_expiry().unwrap();
        assert!(board.selection().is_some());

        let effects = board.expire(deadline);
        assert!(matches!(effects.as_slice(), [.., Effect::ClearSelection]));
        assert!(board.history().is_empty());
    }

    #[test]
    fn an_expired_entry_does_not_fall_back_to_the_previous_one() {
        let mut board = clipboard();
        copy(&mut board, "something ordinary");
        board
            .copy(
                text("a password"),
                no_files(),
                true,
                Some(Duration::from_secs(90)),
            )
            .unwrap();

        let effects = board.expire(board.next_expiry().unwrap());
        assert!(applied(&effects).is_empty());
        assert!(board.selection().is_none());
        // The ordinary entry is still there to pick by hand.
        assert_eq!(previews(&board), vec!["something ordinary"]);
    }

    #[test]
    fn an_entry_whose_lifetime_ran_out_in_transit_never_lands() {
        let mut peer = clipboard();
        peer.copy(
            text("a password"),
            no_files(),
            true,
            Some(Duration::from_secs(1)),
        )
        .unwrap();
        let entries = wire(&peer);

        let mut board = clipboard();
        std::thread::sleep(Duration::from_millis(1100));
        board.accept(entries).unwrap();

        assert!(board.history().is_empty());
        assert!(board.selection().is_none());
    }

    /// A batch with a bad entry in the middle must leave the history and
    /// the log exactly as they were. Taking the good half would put
    /// entries in the log that never reached the history, and announce
    /// them to everyone else.
    #[test]
    fn half_a_bad_batch_is_not_taken() {
        let mut peer = clipboard();
        copy(&mut peer, "perfectly fine");
        let mut entries = wire(&peer);

        let mut board = clipboard();
        entries.push(WireEntry {
            // An entry written under the receiver's own identity.
            id: EntryId {
                origin: board.log.origin(),
                seq: 1,
            },
            clock: Hlc::default(),
            payload: Vec::new(),
        });

        assert!(board.accept(entries).is_err());
        assert!(board.history().is_empty());
        assert_eq!(board.have(), Watermark::default());
    }

    /// A history the protocol cannot carry in one answer is a history
    /// that never reaches a peer: the receiver refuses the over-long
    /// response, drops the batch, and the two machines stop syncing with
    /// nothing to show for it but a debug line.
    #[test]
    fn the_history_cannot_be_configured_past_what_a_fetch_carries() {
        let limits = Clipboard::limits(&Settings {
            history_limit: proto::MAX_FETCH_ENTRIES * 2,
            ..Settings::default()
        });

        assert_eq!(limits.entries, proto::MAX_FETCH_ENTRIES);
    }

    /// Reading a history back under a smaller limit has to say which
    /// files to delete. Nothing else ever would: the entries are gone from
    /// the log, so no later purge can name them, and their contents would
    /// stay on disk in plain text for good.
    #[test]
    fn shrinking_the_history_asks_for_the_dropped_files_back() {
        let mut wide = settings_clipboard(Settings {
            history_limit: 4,
            ..Settings::default()
        });
        for text in ["one", "two", "three", "four"] {
            copy(&mut wide, text);
        }
        let stored = wire(&wide);

        // Both orders, because the directory hands files back in
        // whichever it likes, and an entry the caps drop the moment it
        // lands is the case that reports nothing on its own.
        for order in [stored.clone(), stored.into_iter().rev().collect()] {
            let mut narrow = settings_clipboard(Settings {
                history_limit: 2,
                ..Settings::default()
            });
            let effects = narrow.restore(order);

            assert_eq!(narrow.history().len(), 2);
            assert_eq!(
                effects.len(),
                2,
                "the two entries that did not fit must be deleted",
            );
            assert!(
                effects
                    .iter()
                    .all(|effect| matches!(effect, Effect::Forget(_))),
                "restoring must not ask for files it just read to be rewritten",
            );
        }
    }

    /// A purge covers everything written before it, including entries the
    /// machine issuing it has never seen. Deriving its reach from the
    /// local history would make `yank clear --history` on a machine that
    /// has not caught up a no-op that reports success.
    #[test]
    fn purging_from_an_empty_machine_still_clears_the_others() {
        let mut peer = clipboard();
        copy(&mut peer, "one");
        copy(&mut peer, "two");
        std::thread::sleep(Duration::from_millis(2));

        let mut board = clipboard();
        assert!(board.history().is_empty());
        board.purge().unwrap();

        // The purge reaches the machine that had the history...
        peer.accept(peer_fetch(&board, &peer)).unwrap();
        assert!(peer.history().is_empty());

        // ...and its entries, arriving afterwards, do not come alive
        // again on the machine that purged them.
        board.accept(peer_fetch(&peer, &board)).unwrap();
        assert!(board.history().is_empty());
    }

    /// The one property a later version of yank depends on: an event this
    /// build cannot read is carried anyway.
    ///
    /// It is what makes adding an event variant a change one machine can
    /// make on its own. It is deliberately *not* written to disk, since a
    /// payload we cannot read is one we cannot know is a secret; the
    /// watermark is rebuilt from the disk on start, so a machine that
    /// restarts simply asks for it again.
    #[test]
    fn an_event_from_a_newer_version_is_carried_but_not_stored() {
        let origin = iroh::SecretKey::generate().public();
        let mut board = clipboard();
        // A variant number no version of this enum has.
        let from_the_future = WireEntry {
            id: EntryId { origin, seq: 1 },
            clock: Hlc {
                millis: 1,
                counter: 0,
            },
            payload: vec![99],
        };

        let effects = board.accept(vec![from_the_future]).unwrap();

        assert!(board.history().is_empty(), "it means nothing to this build");
        assert_eq!(
            board.since(&Watermark::default()).len(),
            1,
            "but a peer that understands it must still be able to get it",
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Store(_))),
            "a payload we cannot read is one we cannot know is safe to keep",
        );
        assert_eq!(
            board.have().get(&origin),
            1,
            "and it is not asked for twice"
        );

        // Restarting rebuilds the watermark from what is on disk, so the
        // entry comes back around instead of being lost for good.
        let mut restarted = clipboard();
        restarted.restore(Vec::new());
        assert_eq!(restarted.have(), Watermark::default());
    }

    #[test]
    fn a_secret_never_goes_to_disk_and_is_never_shown() {
        let mut board = clipboard();
        board
            .captured(captured("hunter2", true), no_files())
            .unwrap();

        assert_eq!(previews(&board), vec!["<secret>"]);
        let entries = wire(&board);
        // It still reaches the other machines, which is the point.
        assert_eq!(entries.len(), 1);

        let mut peer = clipboard();
        let effects = peer.accept(entries).unwrap();
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Store(_))),
            "a secret must not be queued for the disk",
        );
        assert!(peer.selection().unwrap().secret);
    }

    #[test]
    fn a_password_manager_hint_makes_an_entry_secret() {
        let mut board = settings_clipboard(Settings {
            secret_ttl: Duration::from_secs(30),
            ..Settings::default()
        });
        board
            .captured(captured("hunter2", true), no_files())
            .unwrap();

        let item = board.selection().unwrap();
        assert!(item.secret);
        assert!(item.expires_at.is_some());
    }

    #[test]
    fn pausing_capture_stops_sharing_what_is_copied_here() {
        let mut board = clipboard();
        board.set_pause(Some(Switch::Paused { until: None }), None);

        assert!(
            board
                .captured(captured("private", false), no_files())
                .unwrap()
                .is_empty()
        );
        assert!(board.history().is_empty());
    }

    #[test]
    fn pausing_apply_keeps_the_history_and_leaves_the_clipboard_alone() {
        let mut peer = clipboard();
        copy(&mut peer, "from the peer");

        let mut board = clipboard();
        board.set_pause(None, Some(Switch::Paused { until: None }));
        let effects = board.accept(wire(&peer)).unwrap();

        assert!(applied(&effects).is_empty());
        assert_eq!(previews(&board), vec!["from the peer"]);

        // Resuming applies what was decided while it was paused.
        let effects = board.set_pause(None, Some(Switch::On));
        assert_eq!(applied(&effects), vec!["from the peer"]);
    }

    #[test]
    fn a_pause_with_a_deadline_lifts_itself() {
        let past = SystemTime::now() - Duration::from_secs(1);
        assert!(Switch::Paused { until: Some(past) }.is_on(SystemTime::now()));
        assert!(!Switch::Paused { until: None }.is_on(SystemTime::now()));
    }

    #[test]
    fn picking_an_entry_makes_it_the_selection_again() {
        let mut board = clipboard();
        copy(&mut board, "first");
        let first = board.selection().unwrap().id;
        copy(&mut board, "second");

        let (picked, effects) = board.pick(first).unwrap();
        assert_eq!(applied(&effects), vec!["first"]);
        assert_eq!(board.selection().unwrap().id, picked);
        assert_eq!(previews(&board), vec!["first", "second", "first"]);
    }

    /// What a browser copy is: the same selection twice over. Both reach
    /// the compositor, and the plain one under every name for text.
    #[test]
    fn every_representation_reaches_the_clipboard() {
        let mut board = clipboard();
        let reps = selection(vec![
            Rep::new("text/plain;charset=utf-8", b"hello".to_vec()),
            Rep::new("text/html", b"<b>hello</b>".to_vec()),
        ]);
        let effects = board.copy(reps, no_files(), false, None).unwrap().1;

        let served = served(&effects);
        assert_eq!(served.len(), 2);
        assert!(served[0].0.contains(&"UTF8_STRING".to_owned()));
        assert_eq!(served[0].1, "hello");
        assert_eq!(served[1].0, vec!["text/html".to_owned()]);

        let item = board.selection().unwrap();
        assert_eq!(item.mime, "text/plain;charset=utf-8");
        assert_eq!(item.size, 17);
    }

    #[test]
    fn picking_an_entry_keeps_every_representation() {
        let mut board = clipboard();
        let reps = selection(vec![
            Rep::new("text/plain", b"hello".to_vec()),
            Rep::new("text/html", b"<b>hello</b>".to_vec()),
        ]);
        board.copy(reps, no_files(), false, None).unwrap();
        let first = board.selection().unwrap().id;
        copy(&mut board, "something else");

        let (_, effects) = board.pick(first).unwrap();
        assert_eq!(served(&effects).len(), 2);
    }

    /// Copying a file copies its path, and a path belongs to the machine
    /// it is on. Handing it back as a file reference anywhere else would
    /// point a file manager at nothing, or at a different file.
    #[test]
    fn a_file_reference_is_only_offered_where_it_came_from() {
        let mut peer = clipboard();
        let reps = selection(vec![
            Rep::new("text/uri-list", b"file:///home/x/y.iso\r\n".to_vec()),
            Rep::new(
                "x-special/gnome-copied-files",
                b"cut\nfile:///home/x/y.iso".to_vec(),
            ),
        ]);
        let effects = peer.copy(reps, no_files(), false, None).unwrap().1;
        assert_eq!(served(&effects)[0].0, vec!["text/uri-list".to_owned()]);

        let mut board = clipboard();
        let effects = board.accept(wire(&peer)).unwrap();

        let served = served(&effects);
        assert_eq!(served.len(), 1, "neither type survives the trip");
        assert!(served[0].0.contains(&"text/plain".to_owned()));
        assert_eq!(served[0].1, "file:///home/x/y.iso\r\n");
    }

    /// Picking writes the entry anew, under this machine's identity. What
    /// it claims has to be what this machine can back up, so the paths of
    /// a file copied elsewhere must not come back as a file reference.
    #[test]
    fn picking_a_file_copied_elsewhere_does_not_make_it_a_file_here() {
        let mut peer = clipboard();
        let reps = Selection::of(Rep::new("text/uri-list", b"file:///home/x/y.iso".to_vec()));
        peer.copy(reps, no_files(), false, None).unwrap();

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();
        let picked = board.selection().unwrap().id;
        let (_, effects) = board.pick(picked).unwrap();

        let served = served(&effects);
        assert_eq!(served.len(), 1);
        assert!(served[0].0.contains(&"text/plain".to_owned()));
        assert!(!served[0].0.contains(&mime::URI_LIST.to_owned()));
    }

    /// A file manager offers what to do with the paths beside the paths
    /// themselves. What survives on another machine is the paths.
    #[test]
    fn what_is_left_of_a_file_reference_is_the_paths_alone() {
        let mut peer = clipboard();
        let reps = selection(vec![
            Rep::new(
                "x-special/gnome-copied-files",
                b"cut\nfile:///home/x/y.iso".to_vec(),
            ),
            Rep::new("text/uri-list", b"file:///home/x/y.iso".to_vec()),
        ]);
        peer.copy(reps, no_files(), false, None).unwrap();

        let mut board = clipboard();
        let effects = board.accept(wire(&peer)).unwrap();

        assert_eq!(served(&effects)[0].1, "file:///home/x/y.iso");
    }

    fn one_file() -> Vec<FileRef> {
        vec![FileRef {
            path: "y.iso".to_owned(),
            size: 4,
            hash: Hash::of(b"iso!"),
        }]
    }

    fn file_copy(board: &mut Clipboard) -> Vec<Effect> {
        let selection = Selection::of_files(&[PathBuf::from("/home/x/y.iso")]);

        board.copy(selection, one_file(), false, None).unwrap().1
    }

    /// A file entry reaching the clipboard is asked for, once, and holds
    /// the paths as text until it arrives: the clipboard is never held up
    /// for a transfer.
    #[test]
    fn a_file_entry_asks_for_its_contents_and_waits_as_text() {
        let mut peer = clipboard();
        file_copy(&mut peer);

        let mut board = clipboard();
        let effects = board.accept(wire(&peer)).unwrap();

        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Fetch(_)))
                .count(),
            1,
        );
        let served = served(&effects);
        assert_eq!(served.len(), 1, "no file type is offered yet");
        assert!(served[0].0.contains(&"text/plain".to_owned()));
    }

    /// And once they land it is a file again, pointing at the copies that
    /// are here rather than at a path on the machine that copied it.
    #[test]
    fn the_files_landing_makes_it_a_file_reference_again() {
        let mut peer = clipboard();
        file_copy(&mut peer);

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();
        let id = board.selection().unwrap().id;
        let effects = board.materialized(id, PathBuf::from("/spool/trees/abc"));

        let served = served(&effects);
        let types: Vec<&str> = served.iter().map(|(mimes, _)| mimes[0].as_str()).collect();
        assert_eq!(
            types,
            vec![mime::PLAIN_TEXT, mime::URI_LIST, mime::GNOME_COPIED_FILES],
        );
        assert_eq!(
            served[0].1, "/spool/trees/abc/y.iso\n",
            "the text is a path this machine has, not the one it was copied from",
        );
        assert_eq!(served[1].1, "file:///spool/trees/abc/y.iso\r\n");
        assert_eq!(
            served[2].1, "copy\nfile:///spool/trees/abc/y.iso",
            "a cut pasted here would move the only copy of the content",
        );
    }

    /// And a picked one is a file again under the tree it earned, not
    /// under the one it was picked from: the entry is written here, so
    /// nothing else would ever rehome it.
    #[test]
    fn a_picked_file_entry_is_a_file_again_once_its_own_tree_lands() {
        let mut peer = clipboard();
        file_copy(&mut peer);

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();
        let id = board.selection().unwrap().id;
        board.materialized(id, PathBuf::from("/spool/trees/first"));

        let (picked, _) = board.pick(id).unwrap();
        let effects = board.materialized(picked, PathBuf::from("/spool/trees/second"));

        let served = served(&effects);
        let types: Vec<&str> = served.iter().map(|(mimes, _)| mimes[0].as_str()).collect();
        assert_eq!(
            types,
            vec![mime::PLAIN_TEXT, mime::URI_LIST, mime::GNOME_COPIED_FILES],
        );
        assert_eq!(served[1].1, "file:///spool/trees/second/y.iso\r\n");
    }

    /// Picking a file entry copied elsewhere writes it under this
    /// machine's identity, so it has to earn a tree of its own: the one it
    /// was pointing at belongs to the entry it came from, and goes when
    /// that entry does.
    #[test]
    fn picking_a_file_entry_asks_for_a_tree_of_its_own() {
        let mut peer = clipboard();
        file_copy(&mut peer);

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();
        let id = board.selection().unwrap().id;
        board.materialized(id, PathBuf::from("/spool/trees/first"));

        let (picked, effects) = board.pick(id).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Fetch(asked) if *asked == picked)),
        );
        assert_eq!(board.files(picked).len(), 1, "it still names the content");

        let served = served(&effects);
        assert_eq!(served.len(), 1, "and offers no file until its tree exists");
        assert!(served[0].0.contains(&"text/plain".to_owned()));
    }

    /// And picking one whose files never arrived names no file at all: an
    /// entry written here must not claim content this machine cannot
    /// serve.
    #[test]
    fn picking_a_file_entry_that_never_arrived_carries_no_manifest() {
        let mut peer = clipboard();
        file_copy(&mut peer);

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();
        let id = board.selection().unwrap().id;

        let (picked, effects) = board.pick(id).unwrap();
        assert!(board.files(picked).is_empty());
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Fetch(_))),
        );
    }

    /// The spool may keep what the history still names, and nothing else.
    #[test]
    fn what_the_history_drops_the_spool_stops_keeping() {
        let mut peer = clipboard();
        file_copy(&mut peer);

        let mut board = clipboard();
        board.accept(wire(&peer)).unwrap();
        let id = board.selection().unwrap().id;

        let (entries, content) = board.referenced();
        assert!(entries.contains(&id));
        assert!(content.contains(&one_file()[0].hash));

        board.forget(id).unwrap();
        let (entries, content) = board.referenced();
        assert!(entries.is_empty() && content.is_empty());
    }

    /// A selection whose alternates do not fit is still worth sharing.
    /// What the cap takes is the alternates, from the back, rather than
    /// the entry.
    #[test]
    fn what_does_not_fit_costs_the_alternates_not_the_entry() {
        let mut board = settings_clipboard(Settings {
            max_entry_size: bytesize::ByteSize::b(16),
            ..Settings::default()
        });
        board
            .captured(
                Captured {
                    selection: selection(vec![
                        Rep::new("text/plain", b"hello".to_vec()),
                        Rep::new("text/html", vec![b'x'; 32]),
                    ]),
                    secret: false,
                },
                no_files(),
            )
            .unwrap();

        let item = board.selection().unwrap();
        assert_eq!(item.mime, "text/plain");
        assert_eq!(item.size, 5);
    }

    #[test]
    fn oversized_selections_are_refused_locally() {
        let mut board = settings_clipboard(Settings {
            max_entry_size: bytesize::ByteSize::b(8),
            ..Settings::default()
        });

        assert!(
            board
                .copy(
                    Selection::of(Rep::new("text/plain", vec![b'x'; 9])),
                    no_files(),
                    false,
                    None
                )
                .is_err(),
        );
        assert!(
            board
                .captured(captured("much too long", false), no_files())
                .unwrap()
                .is_empty(),
        );
    }

    #[test]
    fn entries_are_found_by_the_start_of_their_label() {
        let mut board = clipboard();
        copy(&mut board, "hello");
        let id = board.selection().unwrap().id;

        assert_eq!(board.resolve(&id.label()).unwrap().id, id);
        assert_eq!(board.resolve(&id.label()[..4]).unwrap().id, id);
        assert!(board.resolve("nope").is_err());
    }

    #[test]
    fn previews_are_one_safe_line() {
        assert_eq!(preview("text/plain", b"one\ntwo", false), "one ⏎");
        assert_eq!(preview("text/plain", b"a\x1b[2Kb", false), "a?[2Kb");
        assert_eq!(
            preview("image/png", &[0; 2048], false),
            "<image/png, 2.0 KiB>"
        );
        assert_eq!(preview("text/plain", b"secret", true), "<secret>");
    }
}
