//! The mesh state (`mesh.json`): which machines are paired, and their names.
//!
//! Every machine keeps its own copy and the copies converge on their own.
//! A [`Peer`] record is a *versioned register*: a local change bumps its
//! version, the higher version wins, and ties resolve the same way
//! everywhere, so no clock is needed. Removals are kept as tombstones,
//! because a machine that missed a removal would otherwise re-introduce
//! the peer on the next exchange.
//!
//! The daemon is the only writer. The CLI mutates the file through the
//! control socket and may read it directly for pre-checks, treating what
//! it sees as advisory.

use std::{cmp, collections::BTreeMap, fs};

use eyre::WrapErr as _;
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use super::{Dirs, read_existing, validate_name, write_private};

/// Cap on the machines tracked, tombstones included. A personal mesh is a
/// handful of machines; the cap keeps a peer from growing the state file,
/// and the membership we must be able to send in one message, without
/// bound.
pub const MAX_MESH_PEERS: usize = 64;

/// Cap on a record's version. Versions advance by one per local change, so
/// a record past this is corrupt or hostile: without the cap, a record
/// parked at `u64::MAX` could never be superseded, freezing a machine out
/// of the mesh for good. A tombstone may reach it, and is final there: a
/// record pinned just under the cap can still be removed everywhere.
const MAX_RECORD_VERSION: u64 = 1 << 32;

/// This machine's copy of the mesh.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeshState {
    /// Every machine ever paired, keyed by endpoint id, tombstones
    /// included.
    pub peers: BTreeMap<EndpointId, Peer>,
}

/// One machine's record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// Bumped on every local change, so the change outranks the record
    /// every other machine holds.
    pub version: u64,
    pub status: PeerStatus,
}

/// Whether a machine is part of the mesh. `Removed` is a tombstone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerStatus {
    Alive { name: String },
    Removed,
}

impl Peer {
    /// The machine's name while it is in the mesh; `None` for a tombstone.
    pub fn name(&self) -> Option<&str> {
        match &self.status {
            PeerStatus::Alive { name } => Some(name),
            PeerStatus::Removed => None,
        }
    }

    /// Whether this record replaces `other` on merge: higher version
    /// first, then removal over presence (so a tombstone cannot be undone
    /// by a concurrent rename), then the smaller name (so two machines
    /// renaming at the same version still settle on one).
    fn outranks(&self, other: &Self) -> bool {
        fn rank(peer: &Peer) -> (u64, bool, cmp::Reverse<&str>) {
            (
                peer.version,
                peer.name().is_none(),
                cmp::Reverse(peer.name().unwrap_or("")),
            )
        }
        rank(self) > rank(other)
    }
}

impl MeshState {
    /// Loads `mesh.json`, treating a missing file as an empty mesh.
    pub fn load(dirs: &Dirs) -> eyre::Result<Self> {
        let path = dirs.mesh_file();
        let Some(text) = read_existing(&path, fs::read_to_string)? else {
            return Ok(Self::default());
        };

        serde_json::from_str(&text).wrap_err_with(|| format!("cannot parse {}", path.display()))
    }

    /// Writes `mesh.json` atomically: a crash mid-write must not cost the
    /// user every pairing.
    pub fn save(&self, dirs: &Dirs) -> eyre::Result<()> {
        write_private(&dirs.mesh_file(), |file| {
            serde_json::to_writer_pretty(file, self).map_err(Into::into)
        })
    }

    /// The machines currently in the mesh, with their names.
    pub fn alive_peers(&self) -> impl Iterator<Item = (&EndpointId, &str)> {
        self.peers
            .iter()
            .filter_map(|(id, peer)| Some((id, peer.name()?)))
    }

    /// The name of a machine still in the mesh.
    pub fn peer_name(&self, id: &EndpointId) -> Option<&str> {
        self.peers.get(id)?.name()
    }

    /// Registers a machine under `name`.
    pub fn add_peer(&mut self, id: EndpointId, name: String) -> eyre::Result<()> {
        self.validate_new_peer(&name, &id)?;

        let version = self.next_version(&id)?;
        self.peers.insert(
            id,
            Peer {
                version,
                status: PeerStatus::Alive { name },
            },
        );

        Ok(())
    }

    /// Retires a machine, leaving a tombstone so the removal propagates
    /// instead of being undone by a machine that missed it.
    pub fn remove_peer(&mut self, id: &EndpointId) -> eyre::Result<()> {
        eyre::ensure!(self.peer_name(id).is_some(), "unknown machine");

        let version = self.next_version(id)?;
        self.peers.insert(
            *id,
            Peer {
                version,
                status: PeerStatus::Removed,
            },
        );

        Ok(())
    }

    /// Whether a machine not yet in the mesh could be added under `name`.
    /// Checked before pairing, so a refusal reaches the other side while
    /// it is still listening.
    pub fn validate_new_peer(&self, name: &str, id: &EndpointId) -> eyre::Result<()> {
        validate_name(name)?;
        eyre::ensure!(
            self.peers.len() < MAX_MESH_PEERS || self.peers.contains_key(id),
            "the mesh already has {MAX_MESH_PEERS} machines",
        );

        Ok(())
    }

    /// Resolves what the user typed to a machine: an exact name, or a
    /// prefix of an endpoint id when names are ambiguous or unknown.
    pub fn resolve_peer(&self, needle: &str) -> eyre::Result<EndpointId> {
        let by_name: Vec<EndpointId> = self
            .alive_peers()
            .filter(|(_, name)| *name == needle)
            .map(|(id, _)| *id)
            .collect();
        let matched = match by_name.as_slice() {
            [id] => return Ok(*id),
            [] => self
                .alive_peers()
                .filter(|(id, _)| id.to_string().starts_with(needle))
                .map(|(id, _)| *id)
                .collect(),
            _ => by_name,
        };

        match matched.as_slice() {
            [id] => Ok(*id),
            [] => eyre::bail!("no machine named `{}`", super::sanitize(needle)),
            ids => eyre::bail!(
                "`{}` matches {} machines; use an endpoint id instead",
                super::sanitize(needle),
                ids.len(),
            ),
        }
    }

    /// Adopts everything in `remote` that outranks what we hold.
    ///
    /// Our own record is skipped: it is ours to write, and adopting a
    /// stale copy of it would let a lagging peer undo a local rename. A
    /// machine removed by someone else therefore keeps its own record
    /// alive, but every peer refuses it, which is the same outcome without
    /// the risk of a peer editing our identity.
    pub fn merge(&mut self, remote: &MeshState, local: &EndpointId) {
        for (id, record) in &remote.peers {
            // Nothing alive at the ceiling, where a local change could never
            // supersede it, and nothing past it.
            let valid = match &record.status {
                PeerStatus::Alive { name } => {
                    record.version < MAX_RECORD_VERSION && validate_name(name).is_ok()
                }
                PeerStatus::Removed => record.version <= MAX_RECORD_VERSION,
            };
            if id == local || !valid {
                continue;
            }

            let adopt = match self.peers.get(id) {
                Some(ours) => record.outranks(ours),
                // An unknown machine is only adopted while there is room,
                // so a peer cannot grow our state without bound.
                None => self.peers.len() < MAX_MESH_PEERS,
            };
            if adopt {
                self.peers.insert(*id, record.clone());
            }
        }
    }

    /// The version a local change to `id` must carry to outrank the stored
    /// record. Refuses to pass the ceiling: saturating there would make the
    /// record unchangeable, so a corrupt one is reported instead.
    fn next_version(&self, id: &EndpointId) -> eyre::Result<u64> {
        let version = self.peers.get(id).map_or(0, |peer| peer.version);
        eyre::ensure!(
            version < MAX_RECORD_VERSION,
            "the record of `{id}` is corrupt (version {version}); \
             remove it from mesh.json on every machine",
        );

        Ok(version + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> EndpointId {
        iroh::SecretKey::generate().public()
    }

    fn membership(id: EndpointId, version: u64, name: Option<&str>) -> MeshState {
        let status = match name {
            Some(name) => PeerStatus::Alive {
                name: name.to_owned(),
            },
            None => PeerStatus::Removed,
        };
        MeshState {
            peers: BTreeMap::from([(id, Peer { version, status })]),
        }
    }

    /// A machine pinned just under the ceiling can still be removed, and
    /// the removal spreads.
    #[test]
    fn a_removal_at_the_ceiling_spreads() {
        let (local, remover, peer) = (endpoint(), endpoint(), endpoint());
        let pinned = membership(peer, MAX_RECORD_VERSION - 1, Some("pinned"));
        let mut state = MeshState::default();
        state.merge(&pinned, &local);

        let mut theirs = MeshState::default();
        theirs.merge(&pinned, &remover);
        theirs.remove_peer(&peer).unwrap();
        state.merge(&theirs, &local);

        assert_eq!(state.peer_name(&peer), None);
    }

    #[test]
    fn merge_converges_on_one_record() {
        let (local, peer) = (endpoint(), endpoint());
        let mut state = MeshState::default();

        state.merge(&membership(peer, 2, Some("laptop")), &local);
        assert_eq!(state.peer_name(&peer), Some("laptop"));

        // A lower version never downgrades the record.
        state.merge(&membership(peer, 1, Some("aaa")), &local);
        assert_eq!(state.peer_name(&peer), Some("laptop"));

        // An equal version only wins with a smaller name, so two machines
        // renaming concurrently settle on the same one.
        state.merge(&membership(peer, 2, Some("zzz")), &local);
        assert_eq!(state.peer_name(&peer), Some("laptop"));
        state.merge(&membership(peer, 2, Some("aaa")), &local);
        assert_eq!(state.peer_name(&peer), Some("aaa"));

        // Re-merging changes nothing, which is what stops the gossip from
        // echoing forever.
        let before = state.clone();
        state.merge(&membership(peer, 2, Some("aaa")), &local);
        assert_eq!(state, before);
    }

    #[test]
    fn a_tombstone_beats_a_rename_at_the_same_version() {
        let (local, peer) = (endpoint(), endpoint());
        let mut state = MeshState::default();

        state.merge(&membership(peer, 3, Some("laptop")), &local);
        state.merge(&membership(peer, 3, None), &local);
        assert_eq!(state.peer_name(&peer), None);

        // ...and it cannot be undone at that version either.
        state.merge(&membership(peer, 3, Some("laptop")), &local);
        assert_eq!(state.peer_name(&peer), None);

        // Pairing again outranks the tombstone.
        state.add_peer(peer, "laptop".to_owned()).unwrap();
        assert_eq!(state.peer_name(&peer), Some("laptop"));
    }

    #[test]
    fn our_own_record_is_never_adopted() {
        let local = endpoint();
        let mut state = MeshState::default();

        state.merge(&membership(local, 9, Some("impostor")), &local);
        assert!(state.peers.is_empty());
    }

    #[test]
    fn peers_resolve_by_name_or_id_prefix() {
        let peer = endpoint();
        let mut state = MeshState::default();
        state.add_peer(peer, "laptop".to_owned()).unwrap();

        assert_eq!(state.resolve_peer("laptop").unwrap(), peer);
        assert_eq!(state.resolve_peer(&peer.to_string()[..8]).unwrap(), peer,);
        assert!(state.resolve_peer("desktop").is_err());
    }
}
