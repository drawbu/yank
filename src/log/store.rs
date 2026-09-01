//! The entry store: what this machine holds of the log.
//!
//! Pure in-memory state. Mutations report what changed and the caller
//! decides what that means: writing a file, telling peers, putting
//! something on the clipboard. Keeping the disk out of here is what makes
//! the store testable without one, and what keeps a one-megabyte write off
//! the async runtime.

use std::collections::{BTreeMap, BTreeSet};

use color_eyre::eyre::{Result, bail, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use super::{Clock, Entry, EntryId, Hlc, MAX_ENTRY_BYTES, Payload, Watermark, WireEntry, payload};

/// What the store is allowed to hold.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// How many entries to keep before dropping the oldest.
    pub entries: usize,
    /// How many bytes of payload to keep, same rule.
    pub bytes: usize,
    /// Largest single payload accepted, from here or from a peer.
    pub payload: usize,
}

/// What a mutation did, for the caller to act on.
#[derive(Clone, Debug)]
pub enum Change {
    /// The entry is now part of the log.
    Added(Entry),
    /// The entry is gone: expired, evicted, or dropped on request.
    Dropped(EntryId),
}

/// The sequence and clock state to persist, so a restart cannot re-issue
/// a name or a timestamp it already used.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Checkpoint {
    pub next_seq: u64,
    pub clock: Hlc,
}

/// This machine's copy of the log.
#[derive(Debug)]
pub struct Log {
    /// This machine's identity: the only origin it may write under.
    origin: EndpointId,
    limits: Limits,
    clock: Clock,
    next_seq: u64,
    entries: BTreeMap<EntryId, Entry>,
    /// Every entry again, in clock order, so eviction and history both
    /// have the oldest at hand without sorting.
    order: BTreeSet<(Hlc, EntryId)>,
    have: Watermark,
    bytes: usize,
}

impl Log {
    /// Opens an empty log for `origin`, resuming from `checkpoint`.
    pub fn new(origin: EndpointId, limits: Limits, checkpoint: Checkpoint) -> Self {
        Log {
            origin,
            limits: Limits {
                payload: limits.payload.min(MAX_ENTRY_BYTES as usize),
                ..limits
            },
            clock: Clock::resume(checkpoint.clock),
            next_seq: checkpoint.next_seq.max(1),
            entries: BTreeMap::new(),
            order: BTreeSet::new(),
            have: Watermark::default(),
            bytes: 0,
        }
    }

    /// This machine's identity.
    pub fn origin(&self) -> EndpointId {
        self.origin
    }

    /// The state to persist alongside the entries.
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            next_seq: self.next_seq,
            clock: self.clock.last(),
        }
    }

    /// What we hold, to announce to peers.
    pub fn have(&self) -> Watermark {
        self.have.clone()
    }

    /// Writes a local entry. `durable` decides whether it may be persisted.
    pub fn append(&mut self, bytes: Payload, durable: bool) -> Result<(EntryId, Vec<Change>)> {
        ensure!(
            bytes.len() <= self.limits.payload,
            "entry is larger than the {} byte limit",
            self.limits.payload,
        );

        let id = EntryId {
            origin: self.origin,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        self.have.advance(id.origin, id.seq);

        let entry = Entry {
            id,
            clock: self.clock.tick(),
            payload: bytes,
            durable,
        };

        Ok((id, self.insert(entry)))
    }

    /// Checks an entry from a peer without taking it.
    ///
    /// Separate from [`Self::accept`] so a caller can check a whole batch
    /// before changing anything: half a batch applied, with the other half
    /// refused, would leave entries in the log that never reached the
    /// history and never reached the disk, while still being announced to
    /// everyone else.
    pub fn validate(&self, wire: &WireEntry) -> Result<()> {
        // A peer writing under our identity could rewrite our history and
        // make every machine disagree about what our entries are.
        if wire.id.origin == self.origin {
            bail!("a peer sent an entry under our own identity");
        }
        ensure!(wire.id.seq > 0, "an entry has no sequence number");
        ensure!(
            wire.payload.len() <= self.limits.payload,
            "an entry is larger than the {} byte limit",
            self.limits.payload,
        );

        Ok(())
    }

    /// Takes an entry from a peer. An entry we have already seen, or
    /// knowingly skipped, is ignored.
    pub fn accept(&mut self, wire: WireEntry, durable: bool) -> Result<Vec<Change>> {
        self.validate(&wire)?;

        if !self.have.advance(wire.id.origin, wire.id.seq) {
            return Ok(Vec::new());
        }
        self.clock.observe(wire.clock);

        Ok(self.insert(Entry {
            id: wire.id,
            clock: wire.clock,
            payload: payload(wire.payload),
            durable,
        }))
    }

    /// Re-inserts an entry read back from disk. Unlike [`Self::accept`]
    /// this ignores the watermark, since files come back in whatever order
    /// the directory lists them.
    pub fn restore(&mut self, wire: WireEntry) -> Vec<Change> {
        self.have.advance(wire.id.origin, wire.id.seq);
        self.clock.observe(wire.clock);
        if wire.id.origin == self.origin {
            self.next_seq = self.next_seq.max(wire.id.seq + 1);
        }

        self.insert(Entry {
            id: wire.id,
            clock: wire.clock,
            payload: payload(wire.payload),
            durable: true,
        })
    }

    /// Drops an entry. The watermark stays where it is, so the entry is
    /// not fetched again from a peer that still holds it.
    pub fn remove(&mut self, id: EntryId) -> Option<Change> {
        let entry = self.entries.remove(&id)?;
        self.order.remove(&entry.order_key());
        self.bytes -= entry.payload.len();

        Some(Change::Dropped(id))
    }

    /// One entry by name.
    pub fn get(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(&id)
    }

    /// Every entry, newest first.
    pub fn newest_first(&self) -> impl Iterator<Item = &Entry> {
        self.order.iter().rev().map(|(_, id)| &self.entries[id])
    }

    /// The entries a peer at `want` is missing, in log order so the
    /// receiver applies each origin's entries oldest first.
    pub fn since(&self, want: &Watermark) -> impl Iterator<Item = &Entry> {
        self.entries
            .values()
            .filter(move |entry| entry.id.seq > want.get(&entry.id.origin))
    }

    /// Adds an entry and enforces the caps.
    ///
    /// An entry that the caps evict in the same breath (a very old one
    /// arriving into a full log) is reported as neither added nor dropped:
    /// as far as the caller is concerned it never landed.
    fn insert(&mut self, entry: Entry) -> Vec<Change> {
        let id = entry.id;
        self.bytes += entry.payload.len();
        self.order.insert(entry.order_key());
        self.entries.insert(id, entry.clone());

        let evicted = self.evict();
        let mut changes = Vec::with_capacity(evicted.len() + 1);
        if !evicted.contains(&id) {
            changes.push(Change::Added(entry));
        }
        changes.extend(
            evicted
                .into_iter()
                .filter(|dropped| *dropped != id)
                .map(Change::Dropped),
        );

        changes
    }

    /// Drops the oldest entries until both caps hold.
    fn evict(&mut self) -> Vec<EntryId> {
        let mut evicted = Vec::new();
        while self.entries.len() > self.limits.entries || self.bytes > self.limits.bytes {
            let Some((_, oldest)) = self.order.first().copied() else {
                break;
            };
            if self.remove(oldest).is_some() {
                evicted.push(oldest);
            }
        }

        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> EndpointId {
        iroh::SecretKey::generate().public()
    }

    fn limits() -> Limits {
        Limits {
            entries: 3,
            bytes: 1024,
            payload: 64,
        }
    }

    fn log() -> Log {
        Log::new(endpoint(), limits(), Checkpoint::default())
    }

    fn added(changes: &[Change]) -> Vec<EntryId> {
        changes
            .iter()
            .filter_map(|change| match change {
                Change::Added(entry) => Some(entry.id),
                Change::Dropped(_) => None,
            })
            .collect()
    }

    fn dropped(changes: &[Change]) -> Vec<EntryId> {
        changes
            .iter()
            .filter_map(|change| match change {
                Change::Dropped(id) => Some(*id),
                Change::Added(_) => None,
            })
            .collect()
    }

    #[test]
    fn local_entries_are_numbered_from_one() {
        let mut log = log();
        let (first, _) = log.append(payload(b"a".to_vec()), true).unwrap();
        let (second, _) = log.append(payload(b"b".to_vec()), true).unwrap();

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(log.have().get(&log.origin()), 2);
    }

    #[test]
    fn a_peer_cannot_write_under_our_identity() {
        let mut log = log();
        let forged = WireEntry {
            id: EntryId {
                origin: log.origin(),
                seq: 1,
            },
            clock: Hlc::default(),
            payload: b"forged".to_vec(),
        };

        assert!(log.accept(forged, true).is_err());
    }

    #[test]
    fn entries_are_taken_once() {
        let mut log = log();
        let peer = endpoint();
        let wire = |seq| WireEntry {
            id: EntryId { origin: peer, seq },
            clock: Hlc {
                millis: seq,
                counter: 0,
            },
            payload: vec![0; 8],
        };

        assert_eq!(added(&log.accept(wire(1), true).unwrap()).len(), 1);
        assert!(log.accept(wire(1), true).unwrap().is_empty());

        // A gap is skipped for good: the watermark tracks what we saw, not
        // a contiguous prefix.
        assert_eq!(added(&log.accept(wire(5), true).unwrap()).len(), 1);
        assert!(log.accept(wire(3), true).unwrap().is_empty());
        assert_eq!(log.have().get(&peer), 5);
    }

    #[test]
    fn oversized_payloads_are_refused() {
        let mut log = log();
        assert!(log.append(payload(vec![0; 65]), true).is_err());

        assert!(
            log.accept(
                WireEntry {
                    id: EntryId {
                        origin: endpoint(),
                        seq: 1
                    },
                    clock: Hlc::default(),
                    payload: vec![0; 65],
                },
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn the_oldest_entries_go_first() {
        let mut log = log();
        let mut ids = Vec::new();
        for _ in 0..4 {
            let (id, changes) = log.append(payload(b"x".to_vec()), true).unwrap();
            ids.push((id, changes));
        }

        let (_, last) = ids.pop().unwrap();
        assert_eq!(dropped(&last), vec![ids[0].0]);
        assert_eq!(log.newest_first().count(), 3);
    }

    #[test]
    fn an_entry_evicted_on_arrival_never_landed() {
        let mut log = log();
        for _ in 0..3 {
            log.append(payload(b"x".to_vec()), true).unwrap();
        }

        // Older than everything held, into a full log.
        let stale = WireEntry {
            id: EntryId {
                origin: endpoint(),
                seq: 1,
            },
            clock: Hlc::default(),
            payload: b"stale".to_vec(),
        };
        let changes = log.accept(stale, true).unwrap();

        assert!(changes.is_empty());
        assert_eq!(log.newest_first().count(), 3);
    }

    #[test]
    fn a_fetch_serves_only_what_the_peer_lacks() {
        let mut log = log();
        let (first, _) = log.append(payload(b"a".to_vec()), true).unwrap();
        let (second, _) = log.append(payload(b"b".to_vec()), true).unwrap();

        let mut want = Watermark::default();
        assert_eq!(log.since(&want).count(), 2);

        want.advance(first.origin, first.seq);
        let missing: Vec<EntryId> = log.since(&want).map(|entry| entry.id).collect();
        assert_eq!(missing, vec![second]);
    }

    #[test]
    fn restoring_from_disk_ignores_the_order_files_come_back_in() {
        let origin = endpoint();
        let mut log = Log::new(origin, limits(), Checkpoint::default());
        for seq in [2, 1] {
            log.restore(WireEntry {
                id: EntryId { origin, seq },
                clock: Hlc {
                    millis: seq,
                    counter: 0,
                },
                payload: b"x".to_vec(),
            });
        }

        assert_eq!(log.newest_first().count(), 2);
        // The next local entry must not reuse a name already on disk.
        let (next, _) = log.append(payload(b"y".to_vec()), true).unwrap();
        assert_eq!(next.seq, 3);
    }
}
