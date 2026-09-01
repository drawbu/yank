//! Sending to peers, without knowing which peers there are.
//!
//! The clipboard has to announce itself to every connected machine, and
//! the mesh state has to be gossiped to them, but neither knows anything
//! about connections. The hub sits in between: it holds one outbox per
//! connected peer, drained by one task per peer.
//!
//! ```text
//!  clipboard ─┐                    ┌─► outbox(A) ─► sender task ─► peer A
//!             ├─ publish_* ─► Hub ─┤
//!  mesh state ┘                    └─► outbox(B) ─► sender task ─► peer B
//! ```
//!
//! Outboxes coalesce: one slot for the membership and one per topic, each
//! overwritten by the next publication until the sender takes it. A peer
//! that is slow, or that just reconnected, therefore receives the current
//! state rather than a backlog of everything it missed. That is safe
//! because everything sent here is idempotent and latest-wins; nothing
//! needs to arrive, only the newest does.
//!
//! The hub also keeps what it last published, and replays it to a peer as
//! it connects. That replay is what makes a machine coming back from
//! offline catch up without anyone having to notice it was gone.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use iroh::{EndpointId, endpoint::Connection};
use tokio::sync::Notify;
use tracing::debug;

use crate::{
    config::Membership,
    log::Watermark,
    net::proto::{self, Summary, Topic, UniMessage},
};

/// Budget for sending one message. A peer whose connection has wedged
/// loses its sender task, and the reconnect replay recovers it.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The router between what the daemon publishes and the connected peers.
#[derive(Debug, Default)]
pub struct Hub {
    state: Mutex<HubState>,
}

#[derive(Debug, Default)]
struct HubState {
    peers: BTreeMap<EndpointId, PeerSender>,
    /// Publication counter, monotonic across topics for this run of the
    /// daemon. Streams have no order between them, so this is how a
    /// receiver spots an announcement that arrived out of order.
    seq: u64,
    /// The latest membership, replayed to a connecting peer.
    membership: Membership,
    /// The latest announcement per topic, replayed likewise.
    summaries: BTreeMap<u8, Summary>,
}

/// One connected peer's outbox and the task draining it.
#[derive(Debug)]
struct PeerSender {
    outbox: Arc<Outbox>,
    task: tokio::task::JoinHandle<()>,
}

/// What is waiting to go out to one peer.
#[derive(Debug, Default)]
struct Outbox {
    pending: Mutex<Pending>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct Pending {
    membership: Option<Membership>,
    summaries: BTreeMap<u8, Summary>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts sending to a peer, replaying the current state to it.
    ///
    /// Replacing an existing registration closes the previous connection:
    /// two connections to one machine would each get half the
    /// announcements.
    pub fn connected(&self, peer: EndpointId, conn: &Connection) {
        let mut state = self.state.lock().unwrap();

        let outbox = Arc::new(Outbox::default());
        outbox.push_membership(state.membership.clone());
        for summary in state.summaries.values() {
            outbox.push_summary(summary.clone());
        }

        let task = tokio::spawn(run_sender(conn.clone(), outbox.clone()));
        if let Some(previous) = state.peers.insert(peer, PeerSender { outbox, task }) {
            previous.task.abort();
        }
    }

    /// Stops sending to a peer.
    pub fn disconnected(&self, peer: &EndpointId) {
        if let Some(sender) = self.state.lock().unwrap().peers.remove(peer) {
            sender.task.abort();
        }
    }

    /// How many peers are connected right now.
    pub fn peer_count(&self) -> usize {
        self.state.lock().unwrap().peers.len()
    }

    /// Sends the mesh state to every connected peer, and to every peer
    /// that connects later.
    pub fn publish_membership(&self, membership: &Membership) {
        let mut state = self.state.lock().unwrap();
        state.membership = membership.clone();
        for sender in state.peers.values() {
            sender.outbox.push_membership(membership.clone());
        }
    }

    /// Announces what this machine holds of a topic.
    pub fn announce(&self, topic: Topic, have: Watermark) {
        let mut state = self.state.lock().unwrap();
        state.seq += 1;

        let summary = Summary {
            topic,
            seq: state.seq,
            have,
        };
        state.summaries.insert(topic_key(topic), summary.clone());
        for sender in state.peers.values() {
            sender.outbox.push_summary(summary.clone());
        }
    }
}

impl Outbox {
    fn push_membership(&self, membership: Membership) {
        self.pending.lock().unwrap().membership = Some(membership);
        self.notify.notify_one();
    }

    fn push_summary(&self, summary: Summary) {
        self.pending
            .lock()
            .unwrap()
            .summaries
            .insert(topic_key(summary.topic), summary);
        self.notify.notify_one();
    }

    /// Takes the next message, the membership first: a machine should
    /// learn who is in the mesh before it is told what to fetch.
    fn pop(&self) -> Option<UniMessage> {
        let mut pending = self.pending.lock().unwrap();
        if let Some(membership) = pending.membership.take() {
            return Some(UniMessage::Membership(membership));
        }

        pending
            .summaries
            .pop_first()
            .map(|(_, summary)| UniMessage::Summary(summary))
    }
}

/// Drains one peer's outbox until its connection fails. Messages lost with
/// the connection come back on the reconnect replay.
async fn run_sender(conn: Connection, outbox: Arc<Outbox>) {
    loop {
        let Some(message) = outbox.pop() else {
            outbox.notify.notified().await;
            continue;
        };

        let sent = tokio::time::timeout(SEND_TIMEOUT, async {
            let mut stream = conn.open_uni().await?;
            crate::net::wire::write_message(&mut stream, &message, proto::MAX_UNI_SIZE).await?;
            stream.finish()?;
            color_eyre::eyre::Ok(())
        });
        match sent.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return debug!("cannot send to a peer: {err:#}"),
            Err(_) => return debug!("sending to a peer timed out"),
        }
    }
}

/// The map key of a topic. A `Topic` is not `Ord` and does not need to be;
/// this only has to be stable within one run.
fn topic_key(topic: Topic) -> u8 {
    match topic {
        Topic::Clipboard => 0,
    }
}
