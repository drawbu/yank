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
//! No I/O happens here. Mutations return [`Effect`]s and the caller
//! performs them, which is what lets the whole thing be tested without a
//! compositor, a disk or a peer.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime},
};

use color_eyre::eyre::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::{
    event::{Copy, Event},
    mime,
    wayland::Captured,
};
use crate::{
    config::Settings,
    log::{Change, Entry, EntryId, Hlc, Limits, Log, Payload, Watermark, WireEntry},
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
    /// Put these bytes on the local clipboard.
    Apply { mimes: Vec<String>, bytes: Payload },
    /// Empty the local clipboard.
    ClearSelection,
}

/// One entry as the history shows it. The bytes stay in the log; this is
/// what the daemon and the CLI pass around.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: EntryId,
    pub clock: Hlc,
    pub mime: String,
    /// Size of the copied bytes.
    pub size: usize,
    pub secret: bool,
    /// When this entry disappears everywhere, if it was given a lifetime.
    pub expires_at: Option<SystemTime>,
    /// One line describing the contents; `<secret>` when it must not be
    /// shown.
    pub preview: String,
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
    /// What we last put on the local clipboard, so we neither re-apply it
    /// nor mistake it for something the user copied.
    applied: Option<EntryId>,
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
    /// so this both shares them and puts them there.
    pub fn copy(
        &mut self,
        mime: String,
        bytes: Vec<u8>,
        secret: bool,
        ttl: Option<Duration>,
    ) -> Result<(EntryId, Vec<Effect>)> {
        ensure!(!bytes.is_empty(), "nothing to copy");
        ensure!(
            bytes.len() <= self.settings.max_entry_bytes(),
            "the selection is larger than the {} limit in config.toml",
            self.settings.max_entry_size,
        );

        let event = Event::Copy(Copy {
            mime,
            bytes,
            secret,
            ttl: self.lifetime(secret, ttl),
        });
        let (id, changes) = self.log.append(event.encode(), !secret)?;

        Ok((id, self.ingest(changes)))
    }

    /// Records something the compositor says was copied.
    ///
    /// The bytes are already on the clipboard, so the new entry is marked
    /// as applied: putting them back would take the selection away from
    /// the application that owns it, for nothing.
    pub fn captured(&mut self, captured: Captured) -> Result<Vec<Effect>> {
        let now = SystemTime::now();
        if !self.pause.capture.is_on(now) || self.settings.is_ignored_mime(&captured.mime) {
            return Ok(Vec::new());
        }
        if captured.bytes.len() > self.settings.max_entry_bytes() {
            tracing::debug!(
                "ignoring a {} byte selection, over the configured limit",
                captured.bytes.len(),
            );
            return Ok(Vec::new());
        }

        // The clipboard already holding what the mesh selected is the
        // normal case after a restart, or after we applied an entry an
        // application then re-announced. Recording it again would put a
        // duplicate in everyone's history on every restart.
        let hash = hash(&captured.bytes);
        if let Some(current) = self.selection()
            && current.hash == hash
        {
            self.applied = Some(current.id);
            return Ok(Vec::new());
        }

        let event = Event::Copy(Copy {
            mime: captured.mime,
            bytes: captured.bytes,
            secret: captured.secret,
            ttl: self.lifetime(captured.secret, None),
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
    /// Nothing reaches the clipboard here: [`Self::settle`] does that once
    /// the compositor has said what it already holds.
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

        restored
            .into_iter()
            .filter(|id| self.log.get(*id).is_none())
            .map(Effect::Forget)
            .collect()
    }

    /// Brings the clipboard in line with the history: after a restore,
    /// after a pause is lifted, and once the compositor has said what it
    /// holds.
    pub fn settle(&mut self) -> Vec<Effect> {
        self.reconcile()
    }

    /// Records that the local clipboard was emptied by somebody else.
    ///
    /// It is not shared with the other machines: applications empty the
    /// clipboard when they exit, and wiping every machine for that would
    /// be worse than doing nothing. All it does is forget that our
    /// selection is on the clipboard, so it can be put back the next time
    /// there is a reason to.
    pub fn emptied(&mut self) {
        self.applied = None;
    }

    /// Makes an entry already in the history the selection again, by
    /// copying it anew: history is append-only, so promoting an entry
    /// means writing it, not moving it.
    pub fn pick(&mut self, id: EntryId) -> Result<(EntryId, Vec<Effect>)> {
        let copy = self.body(id)?;
        let ttl = copy.ttl.map(u64::from).map(Duration::from_secs);

        self.copy(copy.mime, copy.bytes, copy.secret, ttl)
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
                if let Some(item) = Self::item(entry, &copy) {
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
        self.log.remove(id);
        effects.push(Effect::Forget(id));
    }

    /// Brings the local clipboard in line with the selection.
    fn reconcile(&mut self) -> Vec<Effect> {
        if !self.pause.apply.is_on(SystemTime::now()) {
            return Vec::new();
        }

        match self.selection().map(|item| (item.id, item.mime.clone())) {
            Some((id, mime)) if self.applied != Some(id) => {
                let Ok(copy) = self.body(id) else {
                    return Vec::new();
                };
                self.applied = Some(id);

                vec![Effect::Apply {
                    mimes: mime::aliases(&mime),
                    bytes: crate::log::payload(copy.bytes),
                }]
            }
            None if self.applied.is_some() => {
                self.applied = None;

                vec![Effect::ClearSelection]
            }
            _ => Vec::new(),
        }
    }

    /// Builds the history entry for a copy, or `None` when its lifetime
    /// has already run out.
    fn item(entry: &Entry, copy: &Copy) -> Option<Item> {
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

        Some(Item {
            id: entry.id,
            clock: entry.clock,
            mime: copy.mime.clone(),
            size: copy.bytes.len(),
            secret: copy.secret,
            expires_at,
            preview: preview(&copy.mime, &copy.bytes, copy.secret),
            hash: hash(&copy.bytes),
        })
    }

    /// The lifetime an entry gets: what was asked for, or the configured
    /// default when it is a secret and nothing was asked.
    fn lifetime(&self, secret: bool, ttl: Option<Duration>) -> Option<u32> {
        let ttl = ttl.or_else(|| secret.then_some(self.settings.secret_ttl))?;

        Some(u32::try_from(ttl.as_secs()).unwrap_or(u32::MAX))
    }
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

    fn copy(board: &mut Clipboard, text: &str) -> Vec<Effect> {
        board
            .copy(
                "text/plain".to_owned(),
                text.as_bytes().to_vec(),
                false,
                None,
            )
            .unwrap()
            .1
    }

    fn captured(text: &str, secret: bool) -> Captured {
        Captured {
            mime: "text/plain".to_owned(),
            bytes: text.as_bytes().to_vec(),
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

    fn applied(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Apply { bytes, .. } => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            })
            .collect()
    }

    fn stored(effects: &[Effect]) -> usize {
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(_)))
            .count()
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
        assert_eq!(stored(&effects), 1);
        assert_eq!(board.selection().unwrap().preview, "hello");
    }

    #[test]
    fn what_the_compositor_reports_is_not_put_back() {
        let mut board = clipboard();
        let effects = board.captured(captured("typed elsewhere", false)).unwrap();

        // It is already on the clipboard: taking the selection away from
        // the application that owns it would gain nothing.
        assert!(applied(&effects).is_empty());
        assert_eq!(previews(&board), vec!["typed elsewhere"]);
    }

    #[test]
    fn the_same_content_coming_back_is_not_a_new_entry() {
        let mut board = clipboard();
        copy(&mut board, "hello");

        // What a compositor announces after we set the selection, and
        // what a restart finds on the clipboard.
        let effects = board.captured(captured("hello", false)).unwrap();
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
                "text/plain".to_owned(),
                b"a password".to_vec(),
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
                "text/plain".to_owned(),
                b"a password".to_vec(),
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
            "text/plain".to_owned(),
            b"a password".to_vec(),
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

    #[test]
    fn a_secret_never_goes_to_disk_and_is_never_shown() {
        let mut board = clipboard();
        board.captured(captured("hunter2", true)).unwrap();

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
        board.captured(captured("hunter2", true)).unwrap();

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
                .captured(captured("private", false))
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

    #[test]
    fn oversized_selections_are_refused_locally() {
        let mut board = settings_clipboard(Settings {
            max_entry_size: bytesize::ByteSize::b(8),
            ..Settings::default()
        });

        assert!(
            board
                .copy("text/plain".to_owned(), vec![b'x'; 9], false, None)
                .is_err(),
        );
        assert!(
            board
                .captured(captured("much too long", false))
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
