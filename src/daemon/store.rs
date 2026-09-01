//! The owner of the mesh state.
//!
//! Every change to who is in the mesh goes through [`MeshStore::update`],
//! whether it came from the user over the control socket or from a peer's
//! gossip: persist, commit in memory, tell the peer set to open or drop
//! connections, and re-broadcast. Owning both the file and the peer set is
//! what makes that ordering structural instead of a rule callers have to
//! remember.

use std::sync::{Arc, Mutex};

use color_eyre::eyre::Result;

use super::{hub::Hub, peers::PeerSet};
use crate::{
    config::{Dirs, MeshState},
    net::pair,
};

/// The mesh state and everything a change to it has to reach.
#[derive(Debug)]
pub struct MeshStore {
    dirs: Dirs,
    state: Mutex<MeshState>,
    peers: Arc<PeerSet>,
    hub: Arc<Hub>,
}

impl MeshStore {
    /// Adopts the loaded state: publishes the membership before opening
    /// any connection, so a peer cannot connect and be told the mesh is
    /// empty.
    pub fn new(dirs: Dirs, state: MeshState, peers: Arc<PeerSet>, hub: Arc<Hub>) -> Self {
        hub.publish_membership(&state.membership());
        peers.sync(&state);

        MeshStore {
            dirs,
            state: Mutex::new(state),
            peers,
            hub,
        }
    }

    /// A copy of the current state.
    pub fn snapshot(&self) -> MeshState {
        self.state.lock().unwrap().clone()
    }

    /// Re-sends the membership unchanged.
    ///
    /// Gossip is not acknowledged and is only sent when something changes,
    /// so a snapshot lost to a dropped stream would otherwise stay lost
    /// until the next unrelated change. This is what heals it.
    pub fn republish(&self) {
        let state = self.state.lock().unwrap();
        self.hub.publish_membership(&state.membership());
    }

    /// Registers a machine that just paired. Doing it twice is harmless,
    /// which is what makes a half-finished pairing repairable by pairing
    /// again.
    pub fn add_paired(&self, peer: &pair::PairedPeer) -> Result<()> {
        self.update(|state| {
            if state.peer_name(&peer.endpoint).is_some() {
                return Ok(());
            }
            state.add_peer(peer.endpoint, peer.name.clone())
        })
    }

    /// Changes the state: persist, commit, align the connections, and
    /// broadcast when the membership moved.
    ///
    /// Nothing is committed if the change or the save fails, and a change
    /// that changes nothing writes and sends nothing, which is what stops
    /// the gossip from echoing back and forth forever. The lock is held
    /// across the whole thing on purpose, so concurrent changes reach the
    /// peer set in the order they were committed.
    pub fn update<T>(&self, mutate: impl FnOnce(&mut MeshState) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().unwrap();

        let mut next = state.clone();
        let value = mutate(&mut next)?;
        if next == *state {
            return Ok(value);
        }

        next.save(&self.dirs)?;
        *state = next;

        self.peers.sync(&state);
        self.hub.publish_membership(&state.membership());

        Ok(value)
    }
}
