//! What machines say to each other once connected.
//!
//! Everything rides one persistent QUIC connection per peer. Small,
//! idempotent state goes out on one-shot unidirectional streams
//! ([`UniMessage`]); anything bigger is pulled over a bidirectional
//! stream ([`Ask`]), so the receiver decides what it takes in.
//!
//! ```text
//! ── uni: membership ──►  who is in the mesh
//! ── uni: head ────────►  the stamp of the clip I hold
//! ◄─ bi:  clip ─────────  send it, yours is newer
//! ── bi:  clip ────────►
//! ◄─ bi:  contents ─────  the bytes named by these hashes
//! ── bi:  each, in turn ►  sending and the bytes, or missing
//! ```
//!
//! A machine that adopts a newer clip announces it in turn, which is what
//! carries it to the machines that are not connected to its origin.
//!
//! Every size below is a cap the *receiver* enforces. A peer is
//! authenticated, which is not the same as trusted with our memory.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    clip::{MAX_SIZE, Stamp},
    config::MeshState,
    files::{Hash, MAX_FILES},
};

/// ALPN of the replication protocol. Bumped whenever the wire format
/// changes, so mismatched daemons refuse each other instead of
/// misreading each other.
pub const ALPN: &[u8] = b"yank/sync/3";

/// Cap on a unidirectional message. Sized for the largest legitimate one,
/// a membership listing a full mesh, with room to spare.
pub const MAX_UNI_SIZE: u32 = 64 * 1024;

/// Cap on what opens a bidirectional stream: a content request naming
/// every file a clip may carry.
pub const MAX_REQUEST_SIZE: u32 = 256 * 1024;

/// Cap on a content reply header.
pub const MAX_REPLY_SIZE: u32 = 64;

/// Cap on a clip: a copy at the ceiling, plus its envelope.
pub const MAX_CLIP_SIZE: u32 = MAX_SIZE + 64 * 1024;

/// How much of a content transfer is read at a time.
pub const CONTENT_CHUNK: usize = 256 * 1024;

/// A message on a one-shot unidirectional stream.
///
/// Sent on connect, on every change, and periodically. Idempotent and
/// latest-wins: a lost one is healed by the next, so nothing here needs
/// delivery guarantees.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UniMessage {
    /// The sender's whole view of the mesh.
    Membership(MeshState),
    /// The stamp of the clip the sender holds.
    Head(Stamp),
}

/// What opens a bidirectional stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Ask {
    /// Send me your clip; answered with an `Option<Clip>`.
    Clip,
    /// Send me the bytes named by these hashes, in this order, all on this
    /// stream, so a folder costs one round trip.
    Contents(Vec<ContentRequest>),
}

/// Asks for content a clip named, and says how much of it is already
/// here: a transfer that died halfway resumes rather than starting over.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentRequest {
    pub hash: Hash,
    pub at: u64,
}

/// What comes back before the bytes of each content asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContentReply {
    /// Exactly this many bytes follow, being what is left after the
    /// offset asked for. The receiver knows what the whole should weigh,
    /// and what it should hash to, from the clip that named it.
    Sending { size: u64 },
    /// Not on this machine. Another peer may have it, and the machine that
    /// copied it may not be up.
    Missing,
}

impl Ask {
    /// Refuses a request no honest machine sends: more files than a clip
    /// names, or one hash twice, which would have us send it twice.
    pub fn validate(&self) -> eyre::Result<()> {
        if let Ask::Contents(requests) = self {
            eyre::ensure!(requests.len() <= MAX_FILES, "too many files asked for");
            let mut seen = BTreeSet::new();
            eyre::ensure!(
                requests.iter().all(|request| seen.insert(request.hash)),
                "a file asked for twice",
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{MAX_MESH_PEERS, MAX_NAME_LEN, Peer, PeerStatus};

    /// A request for every file a clip may name must fit the request cap,
    /// or the largest folders could never be fetched.
    #[test]
    fn a_request_for_a_full_clip_fits_on_the_wire() {
        let requests = (0..MAX_FILES)
            .map(|i| ContentRequest {
                hash: Hash::of(&i.to_le_bytes()),
                at: u64::MAX,
            })
            .collect();

        let encoded = postcard::to_stdvec(&Ask::Contents(requests)).unwrap();
        assert!(encoded.len() <= MAX_REQUEST_SIZE as usize);
    }

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

        let encoded = postcard::to_stdvec(&UniMessage::Membership(MeshState { peers })).unwrap();
        assert!(
            encoded.len() <= MAX_UNI_SIZE as usize,
            "a full membership is {} bytes, over the {MAX_UNI_SIZE} byte cap",
            encoded.len(),
        );
    }
}
