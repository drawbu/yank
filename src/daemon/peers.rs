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
//! Each connection runs four things at once:
//!
//! ```text
//!   incoming uni  ─► membership  ─► the mesh state
//!                 └─ head        ─► the fetcher below
//!   incoming bi   ─► clip ask    ─► our clip sent back
//!                 └─ content ask ─► a file streamed out of the spool
//!   fetcher       ─► clip ask    ─► the peer's clip, to the clipboard
//!   sender        ◄─ our mesh state and head, on connect and change
//! ```
//!
//! The fetcher and the sender are tasks of their own so that talking to a
//! peer never stops us serving it. Both belong to the connection and die
//! with it.
//!
//! The sender only ever sends the latest of each: everything it carries
//! is idempotent and latest-wins, so a slow peer or one that just
//! reconnected receives the current state rather than a backlog.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use eyre::WrapErr as _;
use iroh::{
    Endpoint, EndpointId, TransportAddr, Watcher as _,
    endpoint::{Connection, RecvStream, SendStream},
};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinSet,
};

use super::{backoff::Backoff, clip::ClipService, control};
use crate::{
    clip::{Clip, Stamp},
    config::MeshState,
    files::Store,
    net::{
        proto::{self, Ask, ContentReply, ContentRequest, UniMessage},
        wire::{read_message, write_encoded, write_message},
    },
};

/// Budget for reading one message, so a stalled stream does not hold its
/// slot forever.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for a whole pull, from opening the stream to the last byte.
const FETCH_TIMEOUT: Duration = Duration::from_mins(1);

/// Budget for sending one message. A connection that cannot take one is
/// dropped, and the redial sends the current state again.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the mesh state and the head are re-sent although nothing
/// changed. Neither is acknowledged, so one lost would otherwise stay
/// lost until the next unrelated change.
const ANTI_ENTROPY: Duration = Duration::from_mins(5);

/// Delay before redialing after a first failure; doubles up to the
/// ceiling.
const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Ceiling of that delay. It can be long, since each attempt at an absent
/// machine is a lookup on the discovery servers: a machine coming back dials
/// us itself, and our own network changing cuts the wait short.
const BACKOFF_MAX: Duration = Duration::from_mins(10);

/// How long a wait lasts before a change of our own addresses cuts it
/// short. Behind some NATs they change every few seconds, and this bounds
/// how often that redials an absent machine.
const ADDR_CHANGE_GRACE: Duration = Duration::from_mins(1);

/// A connection that lasted this long resets the delay. Connections dying
/// younger (closed as a duplicate, or a machine that keeps flapping)
/// advance it instead, so an establish-then-close cycle cannot redial hot.
const STABLE_UPTIME: Duration = Duration::from_secs(10);

/// The paired machines and their connections.
#[derive(Debug)]
pub struct PeerSet {
    endpoint: Endpoint,
    clip: Arc<ClipService>,
    /// The mesh state, to gossip it.
    mesh: watch::Receiver<MeshState>,
    /// Mesh states received from peers, merged by the store.
    gossip: mpsc::Sender<MeshState>,
    /// The spool, to serve a peer the content a clip named.
    store: Arc<Store>,
    pulls: Arc<Pulls>,
    peers: Mutex<BTreeMap<EndpointId, PeerHandle>>,
}

/// The clips being pulled, from any peer: a clip announced by several
/// peers at once is fetched from one of them.
#[derive(Debug, Default)]
struct Pulls {
    stamps: Mutex<BTreeSet<Stamp>>,
    done: Notify,
}

/// The pull of one clip, released when dropped.
struct Pull<'a> {
    pulls: &'a Pulls,
    stamp: Stamp,
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
        clip: Arc<ClipService>,
        mesh: watch::Receiver<MeshState>,
        gossip: mpsc::Sender<MeshState>,
        store: Arc<Store>,
    ) -> Self {
        PeerSet {
            endpoint,
            clip,
            mesh,
            gossip,
            store,
            pulls: Arc::default(),
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
            tracing::info!(peer = %handle.name, "removing machine");
            handle.shutdown();
        }

        for (id, handle) in peers.iter_mut() {
            let name = desired[id];
            if handle.name != name {
                // A rename must not cost the live connection; the task
                // keeps logging under the name it started with.
                tracing::info!(old = %handle.name, new = %name, "renaming machine");
                name.clone_into(&mut handle.name);
            }
        }

        for (id, name) in desired {
            peers.entry(id).or_insert_with(|| {
                tracing::info!(peer = %name, "watching machine");
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
            tracing::debug!("refusing a connection from unpaired machine {id}");
            conn.close(0u32.into(), b"unauthorized");
            return;
        };

        if let Err(err) = handle.inbound.try_send(conn) {
            tracing::debug!(peer = %handle.name, "dropping a surplus connection");
            err.into_inner().close(0u32.into(), b"busy");
        }
    }

    /// Every machine reachable right now, closest to `first` in the list.
    ///
    /// Content is asked for outside the replication loop, from whoever
    /// happens to be up: the machine that copied a file is the one most
    /// likely to still have it, and is also the one most likely to be
    /// asleep.
    pub fn connected(&self, first: EndpointId) -> Vec<(EndpointId, Connection)> {
        let peers = self.peers.lock().unwrap();

        let mut connected: Vec<(EndpointId, Connection)> = peers
            .iter()
            .filter_map(|(id, handle)| match &*handle.state.lock().unwrap() {
                PeerState::Connected { conn, .. } => Some((*id, conn.clone())),
                _ => None,
            })
            .collect();
        connected.sort_by_key(|(id, _)| *id != first);

        connected
    }

    /// A snapshot of every machine, for `yank status`.
    pub fn statuses(&self) -> Vec<control::PeerInfo> {
        let peers = self.peers.lock().unwrap();

        peers
            .values()
            .map(|handle| {
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

                control::PeerInfo {
                    name: handle.name.clone(),
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
            peer_id,
            name: name.clone(),
            state: state.clone(),
            clip: self.clip.clone(),
            mesh: self.mesh.clone(),
            gossip: self.gossip.clone(),
            store: self.store.clone(),
            pulls: self.pulls.clone(),
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
    peer_id: EndpointId,
    name: String,
    state: Arc<Mutex<PeerState>>,
    clip: Arc<ClipService>,
    mesh: watch::Receiver<MeshState>,
    gossip: mpsc::Sender<MeshState>,
    store: Arc<Store>,
    pulls: Arc<Pulls>,
    inbound: mpsc::Receiver<Connection>,
}

/// Keeps one machine connected, forever.
async fn run_peer(mut task: PeerTask) {
    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    let mut delay = None;
    // Kept across attempts, so a change during a dial is not missed.
    let mut addr = task.endpoint.watch_addr();

    loop {
        // Wait out the delay, but take an incoming connection if the peer
        // dials us first, and try again early when our own addresses
        // change: what failed on the old network may work on the new one.
        let mut adopted = None;
        if let Some(delay) = delay.take() {
            task.set_state(PeerState::Backoff {
                until: Instant::now() + delay,
            });
            let moved = async {
                tokio::time::sleep(ADDR_CHANGE_GRACE).await;
                addr.updated().await
            };
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                Some(conn) = task.inbound.recv() => adopted = Some((conn, false)),
                Ok(_) = moved => {}
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
        tracing::info!(peer = %task.name, outbound, "connected");
        task.serve(conn, outbound).await;
        tracing::info!(peer = %task.name, "disconnected");

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

        if self.endpoint.id() < self.peer_id {
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

        let (mut heads, mut tasks) = self.spawn_tasks(&conn);
        loop {
            self.set_state(PeerState::Connected {
                conn: conn.clone(),
                since,
            });

            tokio::select! {
                reason = conn.closed() => {
                    tracing::debug!(peer = %self.name, "connection closed: {reason}");
                    break;
                }
                Some(new) = self.inbound.recv() => {
                    // Keep the connection dialed by the lower id: both
                    // sides pick the same one, so the duplicate dies
                    // without taking the survivor with it.
                    if outbound && self.endpoint.id() < self.peer_id {
                        new.close(0u32.into(), b"duplicate");
                    } else {
                        conn.close(0u32.into(), b"duplicate");
                        conn = new;
                        outbound = false;
                        since = SystemTime::now();
                        (heads, tasks) = self.spawn_tasks(&conn);
                    }
                }
                Some(_) = tasks.join_next() => {
                    tracing::debug!(peer = %self.name, "connection stalled");
                    conn.close(0u32.into(), b"stalled");
                    break;
                }
                stream = conn.accept_uni() => {
                    let Ok(stream) = stream else {
                        tracing::debug!(peer = %self.name, "connection lost");
                        break;
                    };
                    self.serve_uni(stream, &heads);
                }
                stream = conn.accept_bi() => {
                    let Ok((send, recv)) = stream else {
                        tracing::debug!(peer = %self.name, "connection lost");
                        break;
                    };
                    self.serve_ask(send, recv);
                }
            }
        }
    }

    /// Starts the fetcher and the sender of `conn`. Dropping the set stops
    /// both.
    fn spawn_tasks(&self, conn: &Connection) -> (watch::Sender<Option<Stamp>>, JoinSet<()>) {
        let (heads, inbox) = watch::channel(None);
        let mut tasks = JoinSet::new();
        tasks.spawn(run_fetcher(
            conn.clone(),
            self.clip.clone(),
            self.pulls.clone(),
            self.name.clone(),
            inbox,
        ));
        tasks.spawn(run_sender(
            conn.clone(),
            self.mesh.clone(),
            self.clip.subscribe(),
            self.name.clone(),
        ));

        (heads, tasks)
    }

    /// Reads one head or membership and routes it.
    fn serve_uni(&self, mut stream: RecvStream, heads: &watch::Sender<Option<Stamp>>) {
        let gossip = self.gossip.clone();
        let heads = heads.clone();
        let name = self.name.clone();
        tokio::spawn(async move {
            let read = tokio::time::timeout(
                STREAM_READ_TIMEOUT,
                read_message::<UniMessage>(&mut stream, proto::MAX_UNI_SIZE),
            );
            let message = match read.await {
                Ok(Ok(message)) => message,
                Ok(Err(err)) => return tracing::debug!(peer = %name, "bad message: {err:#}"),
                Err(_) => return tracing::debug!(peer = %name, "message timed out"),
            };

            match message {
                // A full queue means a flood; dropping is safe, since the
                // next change or the periodic resend says it again.
                UniMessage::Membership(state) => {
                    if gossip.try_send(state).is_err() {
                        tracing::debug!(peer = %name, "dropping a mesh state: the queue is full");
                    }
                }
                // Streams carry no order between them: keep the newest.
                // An equal one still wakes the fetcher, which is how a
                // failed pull is retried. One too far ahead would be
                // refused, and kept, it would hide every head after it.
                UniMessage::Head(head) if head.is_plausible() => {
                    heads.send_modify(|held| *held = (*held).max(Some(head)));
                }
                UniMessage::Head(_) => {
                    tracing::debug!(peer = %name, "ignoring a head from too far ahead");
                }
            }
        });
    }

    /// Answers whatever a peer opened a stream to ask for.
    fn serve_ask(&self, send: SendStream, mut recv: RecvStream) {
        let clip = self.clip.clone();
        let store = self.store.clone();
        let name = self.name.clone();
        tokio::spawn(async move {
            let read = tokio::time::timeout(
                STREAM_READ_TIMEOUT,
                read_message::<Ask>(&mut recv, proto::MAX_REQUEST_SIZE),
            );
            let ask = match read.await {
                Ok(Ok(ask)) => ask,
                Ok(Err(err)) => return tracing::debug!(peer = %name, "bad request: {err:#}"),
                Err(_) => return tracing::debug!(peer = %name, "request timed out"),
            };
            if let Err(err) = ask.validate() {
                return tracing::debug!(peer = %name, "bad request: {err:#}");
            }

            let served = match ask {
                Ask::Clip => send_clip(send, &clip.encoded()).await,
                Ask::Contents(requests) => send_contents(send, &store, &requests).await,
            };
            if let Err(err) = served {
                tracing::debug!(peer = %name, "cannot serve a request: {err:#}");
            }
        });
    }

    fn set_state(&self, state: PeerState) {
        *self.state.lock().unwrap() = state;
    }
}

/// Streams files out of the spool, in the order asked, each one either
/// said to be missing or sent raw after its header.
///
/// Raw rather than framed: a file does not fit in a frame, and the
/// receiver knows how much follows each header. What it may not know is
/// whether the bytes are the ones it asked for, which is why they are
/// named by their hash. Exactly the size announced is sent, or the stream
/// fails: a short file would have the rest read as the next one.
async fn send_contents(
    mut send: SendStream,
    store: &Store,
    requests: &[ContentRequest],
) -> eyre::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    let mut chunk = vec![0u8; proto::CONTENT_CHUNK];
    for request in requests {
        let Ok(mut file) = tokio::fs::File::open(store.blob(request.hash)).await else {
            write_message(&mut send, &ContentReply::Missing, proto::MAX_REPLY_SIZE).await?;
            continue;
        };

        let size = file.metadata().await?.len();
        let at = request.at.min(size);
        file.seek(std::io::SeekFrom::Start(at)).await?;
        let mut left = size - at;
        write_message(
            &mut send,
            &ContentReply::Sending { size: left },
            proto::MAX_REPLY_SIZE,
        )
        .await?;

        while left > 0 {
            let want = usize::try_from(left).unwrap_or(usize::MAX).min(chunk.len());
            let read = file.read(&mut chunk[..want]).await?;
            eyre::ensure!(read > 0, "{} shrank while being sent", request.hash);
            send.write_all(&chunk[..read]).await?;
            left -= read as u64;
        }
    }
    send.finish()?;

    Ok(())
}

/// Sends our clip, encoded as an `Option<Clip>`, to a peer that asked for
/// it.
async fn send_clip(mut send: SendStream, clip: &[u8]) -> eyre::Result<()> {
    write_encoded(&mut send, clip, proto::MAX_CLIP_SIZE).await?;
    send.finish()?;

    Ok(())
}

/// Sends a peer our mesh state and head: both on connect, each again when
/// it changes, and both periodically. Returns when the connection cannot
/// take a message.
///
/// The mesh state goes first: a machine should learn who is in the mesh
/// before it is told what to fetch.
async fn run_sender(
    conn: Connection,
    mut mesh: watch::Receiver<MeshState>,
    mut head: watch::Receiver<Option<Stamp>>,
    name: String,
) {
    let mut resend = tokio::time::interval(ANTI_ENTROPY);
    loop {
        let message = tokio::select! {
            biased;
            _ = resend.tick() => {
                mesh.mark_changed();
                head.mark_changed();
                continue;
            }
            Ok(()) = mesh.changed() => UniMessage::Membership(mesh.borrow_and_update().clone()),
            Ok(()) = head.changed() => match *head.borrow_and_update() {
                Some(head) => UniMessage::Head(head),
                None => continue,
            },
        };

        let sent = tokio::time::timeout(SEND_TIMEOUT, async {
            let mut stream = conn.open_uni().await?;
            write_message(&mut stream, &message, proto::MAX_UNI_SIZE).await?;
            stream.finish()?;
            eyre::Ok(())
        });
        match sent.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return tracing::debug!(peer = %name, "cannot send: {err:#}"),
            Err(_) => return tracing::debug!(peer = %name, "sending timed out"),
        }
    }
}

/// Pulls from one peer whenever it announces a newer clip than ours.
///
/// One at a time: heads announced meanwhile collapse into the newest, which
/// is checked again after the pull. A clip another peer is sending already
/// is waited for rather than asked for twice. A failed pull is not retried
/// here: the peer's next announcement, or the periodic one, comes back to
/// it.
async fn run_fetcher(
    conn: Connection,
    clip: Arc<ClipService>,
    pulls: Arc<Pulls>,
    name: String,
    mut heads: watch::Receiver<Option<Stamp>>,
) {
    while heads.changed().await.is_ok() {
        let Some(head) = *heads.borrow_and_update() else {
            continue;
        };
        let _pull = pulls.claim(head).await;
        if Some(head) <= clip.head() {
            continue;
        }

        match tokio::time::timeout(FETCH_TIMEOUT, fetch(&conn)).await {
            Ok(Ok(Some(pulled))) => {
                if let Err(err) = clip.accept(pulled) {
                    tracing::warn!(peer = %name, "refusing its clip: {err:#}");
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(err)) => tracing::debug!(peer = %name, "cannot fetch: {err:#}"),
            Err(_) => tracing::debug!(peer = %name, "fetch timed out"),
        }
    }
}

impl Pulls {
    /// Claims the pull of the clip stamped `stamp`, waiting while another
    /// peer's pull of it is under way.
    async fn claim(&self, stamp: Stamp) -> Pull<'_> {
        loop {
            let done = self.done.notified();
            if self.stamps.lock().unwrap().insert(stamp) {
                return Pull { pulls: self, stamp };
            }
            done.await;
        }
    }
}

impl Drop for Pull<'_> {
    fn drop(&mut self) {
        self.pulls.stamps.lock().unwrap().remove(&self.stamp);
        self.pulls.done.notify_waiters();
    }
}

/// Asks a peer for its clip.
async fn fetch(conn: &Connection) -> eyre::Result<Option<Clip>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_message(&mut send, &Ask::Clip, proto::MAX_REQUEST_SIZE).await?;
    send.finish()?;

    read_message(&mut recv, proto::MAX_CLIP_SIZE)
        .await
        .wrap_err("cannot read the answer")
}

/// Dials a peer on the replication ALPN.
async fn dial(endpoint: &Endpoint, peer: EndpointId, name: &str) -> Option<Connection> {
    match endpoint.connect(peer, proto::ALPN).await {
        Ok(conn) => Some(conn),
        Err(err) => {
            tracing::debug!(peer = %name, "cannot connect: {err:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clip is pulled from one peer at a time, and only that clip waits.
    #[tokio::test]
    async fn a_clip_is_pulled_once_at_a_time() {
        let origin = iroh::SecretKey::generate().public();
        let (one, two) = (Stamp { millis: 1, origin }, Stamp { millis: 2, origin });
        let pulls = Pulls::default();

        let first = pulls.claim(one).await;
        let _other = tokio::time::timeout(Duration::from_secs(1), pulls.claim(two))
            .await
            .expect("another clip does not wait");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pulls.claim(one))
                .await
                .is_err(),
            "the same clip waits",
        );

        drop(first);
        tokio::time::timeout(Duration::from_secs(1), pulls.claim(one))
            .await
            .expect("and gets its turn once the first is done");
    }
}
