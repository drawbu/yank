//! The daemon: the process that does the work.
//!
//! It holds the identity key, so it is the only one that can reach peers;
//! it owns the clipboard and the mesh state, so it is the only writer of
//! either. The CLI is a client of it.
//!
//! ```text
//!                        ┌────────────────────────────────── daemon ─┐
//!   CLI ── unix socket ──┤ control ──┐                               │
//!   compositor ──────────┤ backend ──┴─► clipboard ─► files ─► spool │
//!                        │                 ▲     │                   │
//!                        │            pull │     ▼ head              │
//!   peers ── QUIC ───────┤ peer tasks ─────┴─────┘                   │
//!                        │      │      ▲                             │
//!                        │ merge▼      │ gossip                      │
//!                        │     mesh state                            │
//!                        └───────────────────────────────────────────┘
//! ```
//!
//! The clipboard and `store` publish the head and the mesh state, every
//! peer connection watches both and gossips them on, and merges what its
//! peer gossips back. The peer tasks pull a newer clip into the clipboard,
//! and `files` is where everything slow about a copied file happens, off
//! the clipboard's path.

mod backoff;
pub mod clip;
pub mod control;
pub mod files;
mod pairing;
mod peers;
mod store;

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use iroh::{Endpoint, address_lookup::MemoryLookup};
use tokio::sync::{Semaphore, mpsc, watch};

use self::{
    clip::ClipService,
    control::{Context, Server},
    pairing::Pairing,
    peers::PeerSet,
    store::MeshStore,
};
use crate::{
    config::{Dirs, MachineKey, MeshState, Settings},
    net::{bind_endpoint, pair, proto},
};

/// Handshakes accepted at once from machines we have not identified yet.
const MAX_PENDING_HANDSHAKES: usize = 32;

/// Budget for one handshake, so a stalled attempt frees its slot rather
/// than waiting out the transport timeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Mesh states from peers waiting to be merged.
const GOSSIP_QUEUE: usize = 16;

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
    /// Starts every subsystem, off the network when given a `lookup` (see
    /// [`bind_endpoint`]).
    pub async fn start(dirs: &Dirs, lookup: Option<&MemoryLookup>) -> eyre::Result<Self> {
        let key = MachineKey::load(dirs)?;
        let state = MeshState::load(dirs)?;

        // Binding the socket first doubles as the single-daemon guard, so
        // nothing else is set up beside another daemon.
        let server = Server::bind(dirs)?;

        // Settings are read once. A broken file must not keep the daemon
        // down, so it falls back to the defaults and says so.
        if let Err(err) = Settings::write_template(dirs) {
            tracing::warn!("cannot write the config.toml template: {err:#}");
        }
        let settings = Arc::new(Settings::load(dirs).unwrap_or_else(|err| {
            tracing::warn!("cannot read config.toml, using the defaults: {err:#}");
            Settings::default()
        }));

        let endpoint = bind_endpoint(&key, alpns(), lookup).await?;
        tracing::info!("daemon started as {}", key.endpoint_id());

        let store = Arc::new(crate::files::Store::open(&dirs.files_dir())?);
        let (jobs, jobs_rx) = files::Jobs::new();
        let clip = ClipService::open(dirs, key.endpoint_id(), settings.clone(), jobs)?;

        let (gossip_tx, gossip_rx) = mpsc::channel(GOSSIP_QUEUE);
        let mesh = watch::Sender::new(state);
        let peers = Arc::new(PeerSet::new(
            endpoint.clone(),
            clip.clone(),
            mesh.subscribe(),
            gossip_tx,
            store.clone(),
        ));
        let mesh = Arc::new(MeshStore::new(
            dirs.clone(),
            key.endpoint_id(),
            mesh,
            peers.clone(),
        ));
        let pairing = Arc::new(Pairing::new(
            endpoint.clone(),
            lookup.is_none(),
            mesh.clone(),
        ));

        let ctx = Arc::new(Context {
            endpoint: endpoint.clone(),
            started: SystemTime::now(),
            peers: peers.clone(),
            clip: clip.clone(),
            store: mesh.clone(),
            pairing: pairing.clone(),
        });

        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move { server.serve(ctx).await });
        tasks.spawn(files::run(
            clip.clone(),
            peers.clone(),
            store,
            settings,
            jobs_rx,
        ));
        tasks.spawn(mesh.clone().merge_gossip(gossip_rx));
        tasks.spawn(accept_loop(endpoint.clone(), peers, pairing));
        tasks.spawn(clip::run(clip));

        Ok(Daemon { tasks, endpoint })
    }

    /// Resolves when a subsystem stops on its own, which is always fatal:
    /// it would leave a daemon that looks alive but no longer answers the
    /// socket, or no longer accepts connections.
    pub async fn failed(&mut self) -> eyre::Report {
        let outcome = self.tasks.join_next().await;

        eyre::eyre!("a daemon subsystem stopped unexpectedly: {outcome:?}")
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
pub async fn run(dirs: &Dirs) -> eyre::Result<()> {
    let mut daemon = Daemon::start(dirs, None).await?;

    let outcome = tokio::select! {
        () = shutdown_signal() => {
            tracing::info!("shutting down");
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
            tracing::debug!("dropping a connection: too many handshakes in flight");
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
                    tracing::debug!("closing a connection with an unexpected protocol");
                    conn.close(0u32.into(), b"unexpected alpn");
                }
                Ok(Err(err)) => tracing::debug!("an incoming connection failed: {err}"),
                Err(_) => tracing::debug!("an incoming handshake timed out"),
            }
        });
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
