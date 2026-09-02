//! What machines say to each other once connected.
//!
//! Everything rides one persistent QUIC connection per peer. Small,
//! idempotent state goes out on one-shot unidirectional streams
//! ([`UniMessage`]); pulling entries takes a bidirectional stream, since
//! the answer is a stream of frames the receiver has to bound.
//!
//! ```text
//! ── uni: membership ──►  who is in the mesh
//! ── uni: summary ─────►  what I hold, per topic
//! ◄─ bi:  entries ──────  what you hold that I lack
//! ── bi:  entry, …, end ►
//! ◄─ bi:  content ──────  the bytes named by this hash
//! ── bi:  sending, … ───►
//! ```
//!
//! Entries are bounded and replicated to everyone; content is neither. It
//! is named by hash in an entry, asked for only by a machine that wants
//! it, and streamed raw after a header rather than framed, because a file
//! does not fit in a frame.
//!
//! Messages are tagged with the [`Topic`] they belong to, which is the
//! seam a second feature would come in through: the connection, the
//! routing and the framing stay as they are.
//!
//! Every size below is a cap the *receiver* enforces. A peer is
//! authenticated, which is not the same as trusted with our memory.

use serde::{Deserialize, Serialize};

use crate::{
    config::{MAX_MESH_PEERS, Membership},
    files::Hash,
    log::{MAX_ENTRY_BYTES, Watermark, WireEntry},
};

/// ALPN of the replication protocol. Bumped whenever the wire format
/// changes, so mismatched daemons refuse each other instead of
/// misreading each other.
pub const ALPN: &[u8] = b"yank/sync/2";

/// Cap on a unidirectional message. Sized for the largest legitimate one,
/// a membership listing a full mesh, with room to spare.
pub const MAX_UNI_SIZE: u32 = 64 * 1024;

/// Cap on what opens a bidirectional stream.
pub const MAX_REQUEST_SIZE: u32 = 16 * 1024;

/// How much of a content transfer is read at a time.
pub const CONTENT_CHUNK: usize = 256 * 1024;

/// Cap on one frame of a fetch response: an entry at the protocol ceiling
/// plus its envelope.
pub const MAX_FRAME_SIZE: u32 = MAX_ENTRY_BYTES + 64 * 1024;

/// Cap on the entries one fetch response may carry, so a peer cannot keep
/// a stream open forever by never sending [`FetchFrame::End`].
pub const MAX_FETCH_ENTRIES: usize = 4096;

/// The feature a message belongs to.
///
/// Postcard encodes variants by position: existing ones keep their
/// position and meaning, new ones are appended, so daemons of different
/// versions still understand the topics they share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Topic {
    Clipboard,
}

/// Every topic this build replicates, for the loops that have to touch
/// all of them.
pub const TOPICS: &[Topic] = &[Topic::Clipboard];

/// A message on a one-shot unidirectional stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UniMessage {
    /// The sender's whole view of the mesh.
    Membership(Membership),
    /// What the sender holds of a topic's log.
    Summary(Summary),
}

/// An announcement of what the sender holds.
///
/// Sent when the log changes, when a peer connects, and periodically.
/// Announcements are idempotent and latest-wins: a lost one is healed by
/// the next, so nothing here needs delivery guarantees.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub topic: Topic,
    /// Sender-wide publication counter, monotonic for the daemon's run.
    /// Streams carry no order between them, so this is what lets a
    /// receiver drop an announcement that overtook a newer one.
    pub seq: u64,
    pub have: Watermark,
}

/// What opens a bidirectional stream.
///
/// Same rule as [`Topic`] about variant positions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Ask {
    /// Send me the entries of a topic I lack.
    Entries(FetchRequest),
    /// Send me the bytes named by a hash.
    Content(ContentRequest),
}

/// Opens a pull: "send me everything past this".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FetchRequest {
    pub topic: Topic,
    pub since: Watermark,
}

/// Asks for content named by an entry, and says how much of it is already
/// here: a transfer that died halfway resumes rather than starting over.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentRequest {
    pub hash: Hash,
    pub at: u64,
}

/// What comes back before the bytes of a content transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContentReply {
    /// This many bytes follow, being what is left after the offset asked
    /// for. The receiver knows what the whole should weigh, and what it
    /// should hash to, from the entry that named it.
    Sending { size: u64 },
    /// Not on this machine. Another peer may have it, and the machine that
    /// copied it may not be up.
    Missing,
}

/// One frame of a fetch response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FetchFrame {
    Entry(WireEntry),
    /// The server has nothing more. Distinguishes a finished pull from a
    /// connection that died halfway.
    End,
}

impl UniMessage {
    /// Refuses a message that is structurally too big to be legitimate.
    /// What is stored is capped later regardless; this keeps the flood off
    /// the queue in the first place.
    pub fn validate(&self) -> color_eyre::eyre::Result<()> {
        use color_eyre::eyre::ensure;

        match self {
            UniMessage::Membership(membership) => {
                ensure!(
                    membership.peers.len() <= MAX_MESH_PEERS,
                    "membership too large"
                );
            }
            UniMessage::Summary(summary) => {
                ensure!(
                    summary.have.origins() <= MAX_MESH_PEERS,
                    "summary too large"
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{MAX_NAME_LEN, Peer, PeerStatus};

    /// A machine holding the largest state we accept must still be able to
    /// gossip it. If it could not, a mesh at the cap would go silent, which
    /// is a far worse failure than refusing the 65th machine.
    #[test]
    fn a_full_membership_fits_on_the_wire() {
        let peers = (0..MAX_MESH_PEERS)
            .map(|_| {
                (
                    iroh::SecretKey::generate().public(),
                    Peer {
                        version: u64::MAX,
                        status: PeerStatus::Alive {
                            name: "n".repeat(MAX_NAME_LEN),
                        },
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        let encoded = postcard::to_stdvec(&UniMessage::Membership(Membership { peers })).unwrap();
        assert!(
            encoded.len() <= MAX_UNI_SIZE as usize,
            "a full membership is {} bytes, over the {MAX_UNI_SIZE} byte cap",
            encoded.len(),
        );
    }

    /// Same for the announcement: one origin per machine in the mesh.
    #[test]
    fn a_full_summary_fits_on_the_wire() {
        let mut have = Watermark::default();
        for _ in 0..MAX_MESH_PEERS {
            have.advance(iroh::SecretKey::generate().public(), u64::MAX);
        }

        let encoded = postcard::to_stdvec(&UniMessage::Summary(Summary {
            topic: Topic::Clipboard,
            seq: u64::MAX,
            have,
        }))
        .unwrap();
        assert!(
            encoded.len() <= MAX_UNI_SIZE as usize,
            "a full summary is {} bytes, over the {MAX_UNI_SIZE} byte cap",
            encoded.len(),
        );
    }
}
