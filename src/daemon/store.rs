//! The owner of the mesh state.
//!
//! Every change to who is in the mesh goes through [`MeshStore::update`],
//! whether it came from the user over the control socket or from a peer's
//! gossip: persist, commit, and tell the peer set to open or drop
//! connections before returning. Owning both the file and the peer set is
//! what makes that ordering structural: a removed machine is cut off
//! before the request removing it is answered. Every connection watches
//! the state to gossip it on.

use std::sync::{Arc, Mutex};

use iroh::EndpointId;
use tokio::sync::{mpsc, watch};

use super::peers::PeerSet;
use crate::{
    config::{Dirs, MeshState},
    net::pair,
};

/// The mesh state, persisted and watched.
#[derive(Debug)]
pub struct MeshStore {
    dirs: Dirs,
    /// This machine, whose own record gossip never overwrites.
    local: EndpointId,
    state: watch::Sender<MeshState>,
    /// Held by a change from start to finish, so changes reach the disk
    /// and the peer set in the order they were made. Apart from the state,
    /// so readers do not wait on the disk.
    writing: Mutex<()>,
    peers: Arc<PeerSet>,
}

impl MeshStore {
    /// Adopts the loaded state, opening a connection to every machine in
    /// it.
    pub fn new(
        dirs: Dirs,
        local: EndpointId,
        state: watch::Sender<MeshState>,
        peers: Arc<PeerSet>,
    ) -> Self {
        peers.sync(&state.borrow());

        MeshStore {
            dirs,
            local,
            state,
            writing: Mutex::new(()),
            peers,
        }
    }

    /// A copy of the current state.
    pub fn snapshot(&self) -> MeshState {
        self.state.borrow().clone()
    }

    /// Registers a machine that just paired. Doing it twice is harmless,
    /// which is what makes a half-finished pairing repairable by pairing
    /// again.
    pub fn add_paired(&self, peer: &pair::PairedPeer) -> eyre::Result<()> {
        self.update(|state| {
            if state.peer_name(&peer.endpoint).is_some() {
                return Ok(());
            }
            state.add_peer(peer.endpoint, peer.name.clone())
        })
    }

    /// Changes the state: persist, commit, and align the connections.
    ///
    /// Nothing is committed if the change or the save fails, and a change
    /// that changes nothing writes and wakes nothing, which is what stops
    /// the gossip from echoing back and forth forever.
    pub fn update<T>(
        &self,
        mutate: impl FnOnce(&mut MeshState) -> eyre::Result<T>,
    ) -> eyre::Result<T> {
        let _writing = self.writing.lock().unwrap();

        let mut next = self.snapshot();
        let value = mutate(&mut next)?;
        if next == *self.state.borrow() {
            return Ok(value);
        }
        next.save(&self.dirs)?;
        self.peers.sync(&next);
        self.state.send_replace(next);

        Ok(value)
    }

    /// Merges the mesh states peers gossip, forever.
    ///
    /// One at a time, off a bounded queue: merging is fast, so a full
    /// queue means a flood, and dropping is safe since the next change or
    /// the periodic resend says it again.
    pub async fn merge_gossip(self: Arc<Self>, mut gossip: mpsc::Receiver<MeshState>) {
        while let Some(remote) = gossip.recv().await {
            let store = self.clone();
            let merged = tokio::task::spawn_blocking(move || {
                store.update(|state| {
                    state.merge(&remote, &store.local);
                    Ok(())
                })
            })
            .await;
            if let Ok(Err(err)) = merged {
                tracing::warn!("cannot apply a peer's mesh state: {err:#}");
            }
        }
    }
}
