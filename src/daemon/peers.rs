//! Peer connections.
//!
//! One task per paired machine holds a connection open: dial, hold,
//! redial with a growing delay. Both machines dial each other, and the
//! duplicate is resolved the same way on both sides (the connection whose
//! *dialer* has the lower endpoint id survives), so a pair settles on one
//! connection instead of tearing each other's down in turn.
//!
//! The set also *is* the allowlist: an incoming connection whose machine
//! is not paired has nowhere to be routed and is refused.
//!
//! Each connection runs three things at once:
//!
//! ```text
//!   incoming uni  ─► membership  ─► the mesh state
//!                 └─ summary     ─► the fetcher below
//!   incoming bi   ─► fetch request ─► entries streamed back
//!   fetcher       ─► fetch request ─► entries handed to the topic
//! ```
//!
//! The fetcher is a task of its own so that pulling from a peer never
//! stops us serving it: two machines that both went offline come back and
//! pull from each other at the same time.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use color_eyre::eyre::{Result, WrapErr as _, ensure};
use iroh::{
    Endpoint, EndpointId, TransportAddr,
    endpoint::{Connection, RecvStream, SendStream},
};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info};

use super::{backoff::Backoff, control, hub::Hub, topics::Topics};
use crate::{
    config::{Membership, MeshState},
    log::WireEntry,
    net::{
        proto::{self, FetchFrame, FetchRequest, Summary, UniMessage},
        wire::{read_message, write_message},
    },
};

/// Incoming one-shot streams handled at once per connection.
const MAX_UNI_STREAMS: usize = 16;

/// Incoming fetch requests served at once per connection.
const MAX_FETCH_STREAMS: usize = 4;

/// Budget for reading one message, so a stalled stream does not hold its
/// slot forever.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for a whole fetch, from opening the stream to the last entry.
const FETCH_TIMEOUT: Duration = Duration::from_mins(1);

/// Pending announcements per connection. Full means a flood, and dropping
/// is safe: the next announcement or reconnect says the same thing.
const SUMMARY_QUEUE: usize = 16;

/// Delay before redialing after a first failure; doubles up to the
/// ceiling.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_mins(1);

/// A connection that lasted this long resets the delay. Connections dying
/// younger (closed as a duplicate, or a machine that keeps flapping)
/// advance it instead, so an establish-then-close cycle cannot redial hot.
const STABLE_UPTIME: Duration = Duration::from_secs(10);

/// The paired machines and their connections.
#[derive(Debug)]
pub struct PeerSet {
    endpoint: Endpoint,
    local_id: EndpointId,
    hub: Arc<Hub>,
    topics: Arc<Topics>,
    /// Memberships received from peers, drained by the daemon.
    gossip: mpsc::Sender<(EndpointId, Membership)>,
    peers: Mutex<BTreeMap<EndpointId, PeerHandle>>,
}

/// Bookkeeping for one peer task.
#[derive(Debug)]
struct PeerHandle {
    name: String,
    state: Arc<Mutex<PeerState>>,
    inbound: mpsc::Sender<Connection>,
    task: tokio::task::JoinHandle<()>,
}

/// Where one peer's connection is at.
#[derive(Debug)]
enum PeerState {
    Connecting,
    Connected { conn: Connection, since: SystemTime },
    Backoff { until: Instant },
}

impl PeerSet {
    pub fn new(
        endpoint: Endpoint,
        local_id: EndpointId,
        hub: Arc<Hub>,
        topics: Arc<Topics>,
        gossip: mpsc::Sender<(EndpointId, Membership)>,
    ) -> Self {
        PeerSet {
            endpoint,
            local_id,
            hub,
            topics,
            gossip,
            peers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Brings the running tasks in line with the mesh state: a task for
    /// every machine in it, and none for the ones that left.
    pub fn sync(&self, state: &MeshState) {
        let desired: BTreeMap<EndpointId, &str> =
            state.alive_peers().map(|(id, name)| (*id, name)).collect();
        let mut peers = self.peers.lock().unwrap();

        let removed: Vec<EndpointId> = peers
            .keys()
            .filter(|id| !desired.contains_key(*id))
            .copied()
            .collect();
        for id in removed {
            let handle = peers.remove(&id).expect("the id came from this map");
            info!(peer = %handle.name, "removing machine");
            handle.shutdown();

            // An aborted task stops at its next await point, which may be
            // *after* it registered a connection with the hub. Cleaning up
            // once it is truly gone is what stops a removed machine from
            // still being sent announcements.
            let hub = self.hub.clone();
            tokio::spawn(async move {
                let _ = handle.task.await;
                hub.disconnected(&id);
            });
        }

        for (id, handle) in peers.iter_mut() {
            let name = desired[id];
            if handle.name != name {
                // A rename must not cost the live connection; the task
                // keeps logging under the name it started with.
                info!(old = %handle.name, new = %name, "renaming machine");
                name.clone_into(&mut handle.name);
            }
        }

        for (id, name) in desired {
            peers.entry(id).or_insert_with(|| {
                info!(peer = %name, "watching machine");
                self.spawn(id, name.to_owned())
            });
        }
    }

    /// Hands an accepted connection to the machine it belongs to, refusing
    /// machines that are not paired.
    pub fn route_inbound(&self, conn: Connection) {
        let id = conn.remote_id();
        let peers = self.peers.lock().unwrap();

        let Some(handle) = peers.get(&id) else {
            // Kept quiet on purpose: anyone who learns our endpoint id can
            // reach this, and logging louder would let them fill the logs.
            debug!("refusing a connection from unpaired machine {id}");
            conn.close(0u32.into(), b"unauthorized");
            return;
        };

        if let Err(err) = handle.inbound.try_send(conn) {
            debug!(peer = %handle.name, "dropping a surplus connection");
            err.into_inner().close(0u32.into(), b"busy");
        }
    }

    /// A snapshot of every machine, for `yank status`.
    pub fn statuses(&self) -> Vec<control::PeerStatus> {
        let peers = self.peers.lock().unwrap();

        peers
            .iter()
            .map(|(id, handle)| {
                let connection = match &*handle.state.lock().unwrap() {
                    PeerState::Connecting => control::ConnectionStatus::Connecting,
                    PeerState::Backoff { until } => control::ConnectionStatus::Backoff {
                        retry_in_secs: until.saturating_duration_since(Instant::now()).as_secs(),
                    },
                    PeerState::Connected { conn, since } => control::ConnectionStatus::Connected {
                        route: route_of(conn),
                        since_secs: since.elapsed().unwrap_or_default().as_secs(),
                    },
                };

                control::PeerStatus {
                    name: handle.name.clone(),
                    endpoint: *id,
                    connection,
                }
            })
            .collect()
    }

    fn spawn(&self, peer_id: EndpointId, name: String) -> PeerHandle {
        let state = Arc::new(Mutex::new(PeerState::Connecting));
        let (tx, rx) = mpsc::channel(4);

        let task = tokio::spawn(run_peer(PeerTask {
            endpoint: self.endpoint.clone(),
            local_id: self.local_id,
            peer_id,
            name: name.clone(),
            state: state.clone(),
            hub: self.hub.clone(),
            topics: self.topics.clone(),
            gossip: self.gossip.clone(),
            inbound: rx,
        }));

        PeerHandle {
            name,
            state,
            inbound: tx,
            task,
        }
    }
}

impl PeerHandle {
    fn shutdown(&self) {
        self.task.abort();
        if let PeerState::Connected { conn, .. } = &*self.state.lock().unwrap() {
            conn.close(0u32.into(), b"machine removed");
        }
    }
}

/// How traffic reaches a peer right now.
fn route_of(conn: &Connection) -> Option<control::Route> {
    let paths = conn.paths();
    let path = paths.iter().find(iroh::endpoint::Path::is_selected)?;
    let rtt_ms = u64::try_from(path.rtt().as_millis()).unwrap_or(u64::MAX);

    Some(match path.remote_addr() {
        TransportAddr::Ip(addr) => control::Route::Direct {
            addr: addr.to_string(),
            rtt_ms,
        },
        TransportAddr::Relay(url) => control::Route::Relay {
            url: url.to_string(),
            rtt_ms,
        },
        other => control::Route::Direct {
            addr: format!("{other:?}"),
            rtt_ms,
        },
    })
}

/// Everything one peer task owns.
struct PeerTask {
    endpoint: Endpoint,
    local_id: EndpointId,
    peer_id: EndpointId,
    name: String,
    state: Arc<Mutex<PeerState>>,
    hub: Arc<Hub>,
    topics: Arc<Topics>,
    gossip: mpsc::Sender<(EndpointId, Membership)>,
    inbound: mpsc::Receiver<Connection>,
}

/// Keeps one machine connected, forever.
async fn run_peer(mut task: PeerTask) {
    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    let mut delay = None;

    loop {
        // Wait out the delay, but take an incoming connection if the peer
        // dials us first.
        let mut adopted = None;
        if let Some(delay) = delay.take() {
            task.set_state(PeerState::Backoff {
                until: Instant::now() + delay,
            });
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                Some(conn) = task.inbound.recv() => adopted = Some((conn, false)),
            }
        }

        let established = match adopted {
            Some(adopted) => Some(adopted),
            None => task.establish().await,
        };
        let Some((conn, outbound)) = established else {
            delay = Some(backoff.next_delay());
            continue;
        };

        let held = Instant::now();
        info!(peer = %task.name, outbound, "connected");
        task.serve(conn, outbound).await;
        info!(peer = %task.name, "disconnected");

        if held.elapsed() >= STABLE_UPTIME {
            backoff.reset();
        } else {
            delay = Some(backoff.next_delay());
        }
    }
}

impl PeerTask {
    /// Dials the peer, `None` when it fails.
    ///
    /// The machine with the lower id prefers its own dial and leaves
    /// incoming connections queued for the duplicate tie-break; the other
    /// takes whichever lands first. Both taking the first would have them
    /// adopt mirrored connections the other side just abandoned, and
    /// redial in a loop.
    async fn establish(&mut self) -> Option<(Connection, bool)> {
        self.set_state(PeerState::Connecting);

        if self.local_id < self.peer_id {
            dial(&self.endpoint, self.peer_id, &self.name)
                .await
                .map(|conn| (conn, true))
        } else {
            tokio::select! {
                dialed = dial(&self.endpoint, self.peer_id, &self.name) => {
                    dialed.map(|conn| (conn, true))
                }
                Some(conn) = self.inbound.recv() => Some((conn, false)),
            }
        }
    }

    /// Serves one connection until it closes.
    async fn serve(&mut self, mut conn: Connection, mut outbound: bool) {
        let mut since = SystemTime::now();
        // The peer is authenticated, which is not a licence to make us
        // spawn as much work as it likes.
        let uni_permits = Arc::new(Semaphore::new(MAX_UNI_STREAMS));
        let fetch_permits = Arc::new(Semaphore::new(MAX_FETCH_STREAMS));

        let (mut summaries, inbox) = mpsc::channel(SUMMARY_QUEUE);
        let mut fetcher = tokio::spawn(run_fetcher(
            conn.clone(),
            self.topics.clone(),
            self.name.clone(),
            inbox,
        ));

        self.hub.connected(self.peer_id, &conn);
        loop {
            self.set_state(PeerState::Connected {
                conn: conn.clone(),
                since,
            });

            tokio::select! {
                reason = conn.closed() => {
                    debug!(peer = %self.name, "connection closed: {reason}");
                    break;
                }
                Some(new) = self.inbound.recv() => {
                    // Keep the connection dialed by the lower id: both
                    // sides pick the same one, so the duplicate dies
                    // without taking the survivor with it.
                    if outbound && self.local_id < self.peer_id {
                        new.close(0u32.into(), b"duplicate");
                    } else {
                        conn.close(0u32.into(), b"duplicate");
                        conn = new;
                        outbound = false;
                        since = SystemTime::now();
                        // The fetcher belongs to the connection it pulls
                        // over, and its record of which announcements it
                        // has seen only means anything on that one.
                        fetcher.abort();
                        let (next, inbox) = mpsc::channel(SUMMARY_QUEUE);
                        summaries = next;
                        fetcher = tokio::spawn(run_fetcher(
                            conn.clone(),
                            self.topics.clone(),
                            self.name.clone(),
                            inbox,
                        ));
                        self.hub.connected(self.peer_id, &conn);
                    }
                }
                stream = conn.accept_uni() => {
                    let Ok(stream) = stream else {
                        debug!(peer = %self.name, "connection lost");
                        break;
                    };
                    self.serve_uni(stream, &uni_permits, &summaries);
                }
                stream = conn.accept_bi() => {
                    let Ok((send, recv)) = stream else {
                        debug!(peer = %self.name, "connection lost");
                        break;
                    };
                    self.serve_fetch(send, recv, &fetch_permits);
                }
            }
        }

        fetcher.abort();
        self.hub.disconnected(&self.peer_id);
    }

    /// Reads one announcement or membership and routes it.
    fn serve_uni(
        &self,
        mut stream: RecvStream,
        permits: &Arc<Semaphore>,
        summaries: &mpsc::Sender<Summary>,
    ) {
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            debug!(peer = %self.name, "dropping a message: too many open streams");
            return;
        };

        let gossip = self.gossip.clone();
        let summaries = summaries.clone();
        let peer = self.peer_id;
        let name = self.name.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let read = tokio::time::timeout(
                STREAM_READ_TIMEOUT,
                read_message::<UniMessage>(&mut stream, proto::MAX_UNI_SIZE),
            );
            let message = match read.await {
                Ok(Ok(message)) => message,
                Ok(Err(err)) => return debug!(peer = %name, "bad message: {err:#}"),
                Err(_) => return debug!(peer = %name, "message timed out"),
            };
            if let Err(err) = message.validate() {
                return debug!(peer = %name, "bad message: {err:#}");
            }

            // A full queue means a flood in either direction; dropping is
            // safe, since the next change or reconnect says it again.
            match message {
                UniMessage::Membership(membership) => {
                    if gossip.try_send((peer, membership)).is_err() {
                        debug!(peer = %name, "dropping a membership: the queue is full");
                    }
                }
                UniMessage::Summary(summary) => {
                    if summaries.try_send(summary).is_err() {
                        debug!(peer = %name, "dropping an announcement: the queue is full");
                    }
                }
            }
        });
    }

    /// Answers a peer's pull with the entries it lacks.
    fn serve_fetch(&self, send: SendStream, mut recv: RecvStream, permits: &Arc<Semaphore>) {
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            debug!(peer = %self.name, "dropping a fetch: too many open streams");
            return;
        };

        let topics = self.topics.clone();
        let name = self.name.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let read = tokio::time::timeout(
                STREAM_READ_TIMEOUT,
                read_message::<FetchRequest>(&mut recv, proto::MAX_REQUEST_SIZE),
            );
            let request = match read.await {
                Ok(Ok(request)) => request,
                Ok(Err(err)) => return debug!(peer = %name, "bad fetch request: {err:#}"),
                Err(_) => return debug!(peer = %name, "fetch request timed out"),
            };

            if let Err(err) = send_entries(send, &topics, &request).await {
                debug!(peer = %name, "cannot serve a fetch: {err:#}");
            }
        });
    }

    fn set_state(&self, state: PeerState) {
        *self.state.lock().unwrap() = state;
    }
}

/// Streams the entries a peer asked for, then says so.
async fn send_entries(mut send: SendStream, topics: &Topics, request: &FetchRequest) -> Result<()> {
    for entry in topics.since(request.topic, &request.since) {
        let frame = FetchFrame::Entry(entry);
        write_message(&mut send, &frame, proto::MAX_FRAME_SIZE).await?;
    }
    write_message(&mut send, &FetchFrame::End, proto::MAX_FRAME_SIZE).await?;
    send.finish()?;

    Ok(())
}

/// Pulls from one peer whenever it announces something we lack.
///
/// One at a time, and re-checked after every pull, so a peer that
/// announces while we are already pulling from it is not missed and does
/// not start a second pull. A failed pull is not retried here: the peer's
/// next announcement, or the periodic one, comes back to it.
async fn run_fetcher(
    conn: Connection,
    topics: Arc<Topics>,
    name: String,
    mut summaries: mpsc::Receiver<Summary>,
) {
    let mut last_seq = 0;

    while let Some(summary) = summaries.recv().await {
        // Streams have no order between them, so an announcement that
        // overtook a newer one is discarded rather than acted on.
        if summary.seq <= last_seq {
            continue;
        }
        last_seq = summary.seq;

        while summary.have.outruns(&topics.have(summary.topic)) {
            let have = topics.have(summary.topic);
            let pulled = tokio::time::timeout(FETCH_TIMEOUT, fetch(&conn, summary.topic, have));
            let entries = match pulled.await {
                Ok(Ok(entries)) => entries,
                Ok(Err(err)) => {
                    debug!(peer = %name, "cannot fetch: {err:#}");
                    break;
                }
                Err(_) => {
                    debug!(peer = %name, "fetch timed out");
                    break;
                }
            };
            // Nothing came back although the peer said it had more: it
            // dropped those entries. Asking again would loop.
            if entries.is_empty() {
                break;
            }

            debug!(peer = %name, "fetched {} entries", entries.len());
            if let Err(err) = topics.accept(summary.topic, entries) {
                debug!(peer = %name, "cannot apply what was fetched: {err:#}");
                break;
            }
        }
    }
}

/// Asks a peer for everything past `since`.
async fn fetch(
    conn: &Connection,
    topic: proto::Topic,
    since: crate::log::Watermark,
) -> Result<Vec<WireEntry>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    let request = FetchRequest { topic, since };
    write_message(&mut send, &request, proto::MAX_REQUEST_SIZE).await?;
    send.finish()?;

    let mut entries = Vec::new();
    loop {
        let frame = read_message::<FetchFrame>(&mut recv, proto::MAX_FRAME_SIZE)
            .await
            .wrap_err("cannot read the answer")?;
        match frame {
            FetchFrame::Entry(entry) => {
                ensure!(
                    entries.len() < proto::MAX_FETCH_ENTRIES,
                    "the peer sent more entries than a log can hold",
                );
                entries.push(entry);
            }
            FetchFrame::End => return Ok(entries),
        }
    }
}

/// Dials a peer on the replication ALPN.
async fn dial(endpoint: &Endpoint, peer: EndpointId, name: &str) -> Option<Connection> {
    match endpoint.connect(peer, proto::ALPN).await {
        Ok(conn) => Some(conn),
        Err(err) => {
            debug!(peer = %name, "cannot connect: {err:#}");
            None
        }
    }
}
