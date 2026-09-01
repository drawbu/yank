//! The daemon: the process that does the work.
//!
//! It holds the identity key, so it is the only one that can reach peers;
//! it owns the clipboard and the mesh state, so it is the only writer of
//! either. The CLI is a client of it.
//!
//! ```text
//!                        ┌───────────────────────── daemon ──┐
//!   CLI ── unix socket ──┤ control ─┐                        │
//!                        │          ├─► clipboard ◄─► topics │
//!   compositor ──────────┤ wayland ─┘        │           │   │
//!                        │                   ▼           ▼   │
//!   peers ── QUIC ───────┤ peer tasks ◄──── hub      mesh    │
//!                        └───────────────────────────────────┘
//! ```
//!
//! Subsystems talk through the types they share, never directly: the
//! the `hub` carries what the clipboard has to say to peers, `topics` is
//! how a peer's entries find their way back to a feature, and `store`
//! owns the mesh state everything else reads.

mod backoff;
pub mod clip;
pub mod control;
mod hub;
mod pairing;
mod peers;
mod store;
mod topics;

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use color_eyre::eyre::{Result, eyre};
use iroh::{Endpoint, EndpointId};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};

use self::{
    clip::ClipService,
    control::{Context, Server},
    hub::Hub,
    pairing::Pairing,
    peers::PeerSet,
    store::MeshStore,
    topics::Topics,
};
use crate::{
    config::{Dirs, MachineKey, Membership, MeshState, Settings},
    net::{EndpointOptions, bind_endpoint, pair, proto},
};

/// Handshakes accepted at once from machines we have not identified yet.
const MAX_PENDING_HANDSHAKES: usize = 32;

/// Budget for one handshake, so a stalled attempt frees its slot rather
/// than waiting out the transport timeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Memberships waiting to be merged. Merging is fast, so a full queue
/// means a flood, and dropping is safe: the periodic pass heals it.
const GOSSIP_QUEUE: usize = 16;

/// How often the mesh state and the announcements are re-sent although
/// nothing changed, so anything dropped under load is not lost until the
/// next unrelated change.
const ANTI_ENTROPY: Duration = Duration::from_mins(5);

/// The protocols the daemon serves.
///
/// Pairing is always among them: what gates it is the one-time ticket, not
/// whether the protocol is offered, and an unknown machine can complete a
/// handshake on the replication ALPN anyway (it is refused right after,
/// for not being paired).
fn alpns() -> Vec<Vec<u8>> {
    vec![proto::ALPN.to_vec(), pair::ALPN.to_vec()]
}

/// A running daemon.
///
/// The binary drives it through [`run`]; tests start and stop it directly.
pub struct Daemon {
    tasks: tokio::task::JoinSet<()>,
    endpoint: Endpoint,
}

impl Daemon {
    /// Starts every subsystem.
    pub async fn start(dirs: &Dirs, options: &EndpointOptions) -> Result<Self> {
        let key = MachineKey::load(dirs)?;
        let state = MeshState::load(dirs)?;

        // Binding the socket first doubles as the single-daemon guard, so
        // nothing else is set up beside another daemon.
        let server = Server::bind(dirs)?;

        // Settings are read once. A broken file must not keep the daemon
        // down, so it falls back to the defaults and says so.
        if let Err(err) = Settings::write_template(dirs) {
            warn!("cannot write the config.toml template: {err:#}");
        }
        let settings = Arc::new(Settings::load(dirs).unwrap_or_else(|err| {
            warn!("cannot read config.toml, using the defaults: {err:#}");
            Settings::default()
        }));

        let endpoint = bind_endpoint(&key, alpns(), options).await?;
        info!("daemon started as {}", key.endpoint_id());

        let hub = Arc::new(Hub::new());
        let topics = Arc::new(Topics {
            clipboard: ClipService::open(dirs, key.endpoint_id(), settings, hub.clone())?,
        });
        // Announced before anything can connect, so the hub has something
        // to replay to the first peer that does. Without it a machine that
        // starts with a history and copies nothing would look empty to
        // everyone until the first anti-entropy pass.
        topics.announce_all(&hub);

        let (gossip_tx, gossip_rx) = mpsc::channel(GOSSIP_QUEUE);
        let peers = Arc::new(PeerSet::new(
            endpoint.clone(),
            key.endpoint_id(),
            hub.clone(),
            topics.clone(),
            gossip_tx,
        ));
        let store = Arc::new(MeshStore::new(
            dirs.clone(),
            state,
            peers.clone(),
            hub.clone(),
        ));
        let pairing = Arc::new(Pairing::new(
            endpoint.clone(),
            options.uses_relays(),
            store.clone(),
        ));

        let ctx = Arc::new(Context {
            endpoint: endpoint.clone(),
            started: SystemTime::now(),
            peers: peers.clone(),
            topics: topics.clone(),
            store: store.clone(),
            pairing: pairing.clone(),
        });

        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move { server.serve(ctx).await });
        tasks.spawn(accept_loop(endpoint.clone(), peers, pairing));
        tasks.spawn(merge_loop(gossip_rx, key.endpoint_id(), store.clone()));
        tasks.spawn(anti_entropy_loop(store, topics.clone(), hub));
        tasks.spawn(clip::run(topics.clipboard.clone()));

        Ok(Daemon { tasks, endpoint })
    }

    /// Resolves when a subsystem stops on its own, which is always fatal:
    /// it would leave a daemon that looks alive but no longer answers the
    /// socket, or no longer accepts connections.
    pub async fn failed(&mut self) -> color_eyre::Report {
        let outcome = self.tasks.join_next().await;

        eyre!("a daemon subsystem stopped unexpectedly: {outcome:?}")
    }

    /// Stops everything, waiting for the tasks so the socket file is gone
    /// and its lock released before this returns.
    pub async fn shutdown(mut self) {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        self.endpoint.close().await;
    }
}

/// Runs the daemon until it is asked to stop.
pub async fn run(dirs: &Dirs) -> Result<()> {
    let mut daemon = Daemon::start(dirs, &EndpointOptions::default()).await?;

    let outcome = tokio::select! {
        () = shutdown_signal() => {
            info!("shutting down");
            Ok(())
        }
        err = daemon.failed() => Err(err),
    };
    daemon.shutdown().await;

    outcome
}

/// Accepts connections and sorts them by protocol: replication to the
/// machine's own task, which refuses it when unpaired, pairing to the
/// pairing state, which refuses it unless a ticket is out.
async fn accept_loop(endpoint: Endpoint, peers: Arc<PeerSet>, pairing: Arc<Pairing>) {
    // Who is connecting is only known once the handshake finishes, so
    // anyone can start one: bound how many run at a time.
    let handshakes = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));

    while let Some(incoming) = endpoint.accept().await {
        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
            debug!("dropping a connection: too many handshakes in flight");
            continue;
        };
        let Ok(connecting) = incoming.accept() else {
            continue;
        };

        let peers = peers.clone();
        let pairing = pairing.clone();
        tokio::spawn(async move {
            // The permit lives as long as this task, so it covers the
            // whole handshake.
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting).await {
                Ok(Ok(conn)) if conn.alpn() == proto::ALPN => peers.route_inbound(conn),
                Ok(Ok(conn)) if conn.alpn() == pair::ALPN => {
                    // The exchange outlives the handshake; release the
                    // permit and let pairing bound its own work.
                    drop(permit);
                    pairing.serve_inbound(conn).await;
                }
                Ok(Ok(conn)) => {
                    debug!("closing a connection with an unexpected protocol");
                    conn.close(0u32.into(), b"unexpected alpn");
                }
                Ok(Err(err)) => debug!("an incoming connection failed: {err}"),
                Err(_) => debug!("an incoming handshake timed out"),
            }
        });
    }
}

/// Merges the mesh states peers send us.
///
/// A merge that changes something is saved and sent on, which is what
/// carries membership to machines we are not talking to directly; a merge
/// that changes nothing is silent, which is what stops the echo.
async fn merge_loop(
    mut gossip: mpsc::Receiver<(EndpointId, Membership)>,
    local: EndpointId,
    store: Arc<MeshStore>,
) {
    while let Some((peer, membership)) = gossip.recv().await {
        let merged = store.update(|state| {
            state.merge(&membership, &local);
            Ok(())
        });
        if let Err(err) = merged {
            warn!("cannot apply the mesh state from {peer}: {err:#}");
        }
    }
}

/// Re-sends the mesh state and the announcements on a timer.
///
/// Neither is acknowledged, and both are only sent when something changes,
/// so one dropped under load would otherwise stay lost until the next
/// unrelated change. This is what heals that.
async fn anti_entropy_loop(store: Arc<MeshStore>, topics: Arc<Topics>, hub: Arc<Hub>) {
    loop {
        tokio::time::sleep(ANTI_ENTROPY).await;
        store.republish();
        topics.announce_all(&hub);
    }
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate()).expect("the SIGTERM handler must install");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
