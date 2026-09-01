//! The replicated log: what machines actually exchange.
//!
//! Every machine writes entries under its own identity, numbered from one
//! and never reused. An entry is therefore named by `(origin, seq)` for
//! all time, on every machine, which is what makes replication idempotent:
//! receiving one twice is a no-op, and there is nothing to reconcile.
//!
//! ```text
//! A ── summary: have {A:12, B:7} ──────────────────► B   (announce)
//! A ◄─ fetch: since {A:9, B:7} ─────────────────────  B   (pull)
//! A ── entry A:10, A:11, A:12, end {A:12, B:7} ────► B
//! ```
//!
//! Announcements are tiny, idempotent and latest-wins, so a dropped one
//! costs nothing: the next change or the next reconnect carries the same
//! information. The pull is what moves data, which puts the receiver in
//! charge of what enters its store and makes a machine coming back from
//! days offline take the same code path as one syncing live.
//!
//! The log is deliberately **bounded and lossy**. Entries expire, and the
//! oldest are evicted once the caps are reached, so a peer may hold no
//! copy of an entry we never saw. A watermark therefore advances past
//! gaps: it records the highest sequence *seen*, not a contiguous prefix.
//! Re-asking forever for an entry every machine has already dropped would
//! be the alternative.
//!
//! Nothing here knows what an entry means. The payload is an opaque blob
//! a topic (today [`crate::clip`], tomorrow whatever else) encodes and
//! decodes; the caller decides whether an entry may touch the disk.

mod clock;
mod disk;
mod store;

use std::{collections::BTreeMap, fmt, sync::Arc};

use data_encoding::BASE32_NOPAD;
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub use self::{
    clock::{Clock, Hlc},
    disk::Writer,
    store::{Change, Checkpoint, Limits, Log},
};

/// Hard ceiling on one entry's payload, enforced before anything is
/// decoded or stored. The user-facing cap in `config.toml` sits under it.
///
/// A `u32` because it also bounds a wire frame, where the length prefix is
/// one.
pub const MAX_ENTRY_BYTES: u32 = 4 * 1024 * 1024;

/// The name of an entry, unique across the mesh and stable forever.
///
/// Ordered by origin then sequence, which is the order the store keeps
/// entries in and the order a fetch serves them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryId {
    /// The machine that wrote the entry.
    pub origin: EndpointId,
    /// Its position in that machine's log, counted from one.
    pub seq: u64,
}

impl EntryId {
    /// The short form shown to users and accepted (by prefix) by commands
    /// taking an entry: six characters of the origin, then the sequence.
    pub fn label(&self) -> String {
        let origin = BASE32_NOPAD.encode(self.origin.as_bytes()).to_lowercase();
        format!("{}-{}", &origin[..6], self.seq)
    }

    /// The file name the entry is persisted under, unique per entry and
    /// stable across restarts.
    fn file_name(&self) -> String {
        let origin = BASE32_NOPAD.encode(self.origin.as_bytes()).to_lowercase();
        format!("{origin}-{:020}.entry", self.seq)
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// The bytes of an entry.
///
/// Shared rather than copied, since the same megabyte goes to peers, to
/// the disk writer and to the clipboard backend at once, and wiped when
/// the last holder lets go: the store is where a copied password lives
/// longest. Buffers made while encoding a message or writing a file are
/// not scrubbed; the guarantee is about what yank *keeps*, not about every
/// byte the process ever touched.
pub type Payload = Arc<Zeroizing<Vec<u8>>>;

/// Wraps bytes as a payload, taking ownership of the buffer.
pub fn payload(bytes: Vec<u8>) -> Payload {
    Arc::new(Zeroizing::new(bytes))
}

/// An entry as the store holds it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: EntryId,
    /// When its author wrote it, on the mesh-wide clock.
    pub clock: Hlc,
    /// The topic's encoding of the event.
    pub payload: Payload,
    /// Whether this entry may be written to disk. Decided by the topic:
    /// a secret clipboard entry never is.
    pub durable: bool,
}

impl Entry {
    /// The total order over entries: the clock first, the id to break
    /// ties, so every machine sorts the history identically.
    pub fn order_key(&self) -> (Hlc, EntryId) {
        (self.clock, self.id)
    }
}

/// An entry as it crosses the wire and lands on the disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireEntry {
    pub id: EntryId,
    pub clock: Hlc,
    pub payload: Vec<u8>,
}

impl From<&Entry> for WireEntry {
    fn from(entry: &Entry) -> Self {
        WireEntry {
            id: entry.id,
            clock: entry.clock,
            payload: entry.payload.as_slice().to_vec(),
        }
    }
}

/// How far a machine has got in every origin's log: the highest sequence
/// it holds or has knowingly skipped.
///
/// Announced to say what we have, and sent back in a fetch to say what we
/// want. Origins absent from the map are at zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Watermark(BTreeMap<EndpointId, u64>);

impl Watermark {
    /// How far we are in `origin`'s log; zero when we hold nothing of it.
    pub fn get(&self, origin: &EndpointId) -> u64 {
        self.0.get(origin).copied().unwrap_or(0)
    }

    /// Moves an origin forward, never backwards. Returns whether it moved.
    pub fn advance(&mut self, origin: EndpointId, seq: u64) -> bool {
        let slot = self.0.entry(origin).or_default();
        if *slot >= seq {
            return false;
        }
        *slot = seq;
        true
    }

    /// Whether this watermark holds anything `other` does not, which is
    /// the whole question a received announcement has to answer.
    pub fn outruns(&self, other: &Watermark) -> bool {
        self.0.iter().any(|(origin, seq)| *seq > other.get(origin))
    }

    /// How many origins it mentions, for the caps applied on receipt.
    pub fn origins(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> EndpointId {
        iroh::SecretKey::generate().public()
    }

    #[test]
    fn watermarks_only_move_forward() {
        let origin = endpoint();
        let mut mark = Watermark::default();

        assert!(mark.advance(origin, 5));
        assert!(!mark.advance(origin, 3));
        assert_eq!(mark.get(&origin), 5);
    }

    #[test]
    fn outruns_compares_every_origin() {
        let (a, b) = (endpoint(), endpoint());
        let mut ours = Watermark::default();
        ours.advance(a, 4);
        let mut theirs = Watermark::default();
        theirs.advance(a, 4);

        assert!(!ours.outruns(&theirs));

        theirs.advance(b, 1);
        assert!(theirs.outruns(&ours));
        assert!(!ours.outruns(&theirs));
    }

    #[test]
    fn labels_are_short_and_stable() {
        let id = EntryId {
            origin: endpoint(),
            seq: 12,
        };
        assert_eq!(id.label(), format!("{id}"));
        assert!(id.label().ends_with("-12"));
        assert_eq!(id.label().len(), 9);
    }
}
