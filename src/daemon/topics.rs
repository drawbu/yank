//! The features replicated over the mesh.
//!
//! Peer connections carry entries for a [`Topic`] without knowing what a
//! topic means; this is where a topic turns back into the thing that owns
//! it. Today there is one, the clipboard.
//!
//! Adding another means: a variant on [`Topic`], a field here, an arm in
//! each of the three methods below, and a service with the same three
//! operations. Nothing in [`crate::net`], [`crate::log`] or
//! [`super::peers`] has to know about it.

use std::sync::Arc;

use color_eyre::eyre::Result;

use super::{clip::ClipService, hub::Hub};
use crate::{
    log::{Watermark, WireEntry},
    net::proto::{self, Topic},
};

/// Every replicated feature this daemon runs.
#[derive(Debug)]
pub struct Topics {
    pub clipboard: Arc<ClipService>,
}

impl Topics {
    /// What this machine holds of a topic.
    pub fn have(&self, topic: Topic) -> Watermark {
        match topic {
            Topic::Clipboard => self.clipboard.have(),
        }
    }

    /// The entries a peer at `want` is missing.
    pub fn since(&self, topic: Topic, want: &Watermark) -> Vec<WireEntry> {
        match topic {
            Topic::Clipboard => self.clipboard.since(want),
        }
    }

    /// Takes a batch of entries from a peer.
    pub fn accept(&self, topic: Topic, entries: Vec<WireEntry>) -> Result<()> {
        match topic {
            Topic::Clipboard => self.clipboard.accept(entries),
        }
    }

    /// Announces every topic, for the periodic anti-entropy pass.
    pub fn announce_all(&self, hub: &Hub) {
        for topic in proto::TOPICS.iter().copied() {
            hub.announce(topic, self.have(topic));
        }
    }
}
