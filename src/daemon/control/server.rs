//! The daemon's side of the control socket.
//!
//! Binding the socket is also the single-daemon guard: two daemons sharing
//! one identity would race each other's connections, and two sharing one
//! history would each renumber it.

use std::{
    fs::{self, File, TryLockError},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use color_eyre::eyre::{Result, WrapErr as _, bail};
use iroh::Endpoint;
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, warn};

use super::protocol::{ClipboardStatus, HistoryEntry, MAX_MESSAGE_SIZE, Request, Response, Status};
use crate::{
    clip::Item,
    config::{Dirs, sanitize_bounded},
    daemon::{
        backoff::Backoff, clip::BackendState, pairing::Pairing, peers::PeerSet, store::MeshStore,
        topics::Topics,
    },
    net::{pair, wire::read_message, wire::write_message},
};

/// Budget for reading a request, so a client that connects and says
/// nothing does not hold a task.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for a whole pairing, from dialing to the confirmation.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(90);

/// First and last delay between retries when accepting fails.
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(100);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Everything the request handlers need.
#[derive(Debug)]
pub struct Context {
    pub endpoint: Endpoint,
    pub started: SystemTime,
    pub peers: Arc<PeerSet>,
    pub topics: Arc<Topics>,
    pub store: Arc<MeshStore>,
    pub pairing: Arc<Pairing>,
}

/// The listening control socket.
///
/// Dropping it removes the socket file, which covers both a clean shutdown
/// and a failure during startup.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    /// Held for the daemon's lifetime. This, not the socket file, is what
    /// arbitrates two daemons starting at once.
    _lock: File,
}

impl Server {
    /// Binds the socket, refusing to start beside another daemon.
    pub fn bind(dirs: &Dirs) -> Result<Self> {
        let path = dirs.socket_file();

        let lock_path = path.with_extension("lock");
        let lock = File::create(&lock_path)
            .wrap_err_with(|| format!("cannot create {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!("another yank daemon is already running"),
            Err(TryLockError::Error(err)) => {
                return Err(err).wrap_err_with(|| format!("cannot lock {}", lock_path.display()));
            }
        }

        // Bound on a temporary path and renamed into place: the socket is
        // never reachable while its permissions are still open, and a
        // stale one left by a crash is replaced in one step.
        let tmp = path.with_extension("sock.tmp");
        let _ = fs::remove_file(&tmp);
        let listener =
            UnixListener::bind(&tmp).wrap_err_with(|| format!("cannot bind {}", tmp.display()))?;
        fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        fs::rename(&tmp, &path)
            .wrap_err_with(|| format!("cannot move the socket to {}", path.display()))?;

        Ok(Server {
            listener,
            path,
            _lock: lock,
        })
    }

    /// Answers requests until the daemon stops.
    pub async fn serve(self, ctx: Arc<Context>) -> ! {
        let mut backoff = Backoff::new(ACCEPT_BACKOFF_MIN, ACCEPT_BACKOFF_MAX);

        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    backoff.reset();
                    tokio::spawn(handle(stream, ctx.clone()));
                }
                Err(err) => {
                    // A lasting failure, file descriptors run out for
                    // instance, backs off instead of logging in a spin.
                    warn!("cannot accept on the control socket: {err}");
                    tokio::time::sleep(backoff.next_delay()).await;
                }
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Answers one client.
async fn handle(mut stream: UnixStream, ctx: Arc<Context>) {
    let read = tokio::time::timeout(
        REQUEST_TIMEOUT,
        read_message::<Request>(&mut stream, MAX_MESSAGE_SIZE),
    );
    let request = match read.await {
        Ok(Ok(request)) => request,
        Ok(Err(err)) => return debug!("bad control request: {err:#}"),
        Err(_) => return debug!("a control client said nothing"),
    };

    let response = match answer(&ctx, request).await {
        Ok(response) => response,
        // Errors carry file paths and peer names, so they are cleaned up
        // before they are handed back to be printed.
        Err(err) => Response::Error(sanitize_bounded(&format!("{err:#}"))),
    };
    if let Err(err) = write_message(&mut stream, &response, MAX_MESSAGE_SIZE).await {
        debug!("cannot answer a control client: {err:#}");
    }
}

/// Runs one request.
async fn answer(ctx: &Context, request: Request) -> Result<Response> {
    let clip = &ctx.topics.clipboard;

    match request {
        Request::Status => Ok(Response::Status(Box::new(status(ctx)))),
        Request::PairHost { name } => {
            crate::config::validate_name("machine", &name)?;
            Ok(Response::PairTicket(
                ctx.pairing.host(name).await?.to_string(),
            ))
        }
        Request::PairJoin { ticket, name } => {
            crate::config::validate_name("machine", &name)?;
            let ticket: pair::PairTicket = ticket.parse()?;
            let state = ctx.store.snapshot();

            let joining = pair::join(&ctx.endpoint, &ticket, &name, &state);
            let peer = tokio::time::timeout(PAIRING_TIMEOUT, joining)
                .await
                .map_err(|_| color_eyre::eyre::eyre!("pairing timed out"))??;
            ctx.store.add_paired(&peer)?;

            Ok(Response::Paired {
                name: peer.name,
                endpoint: peer.endpoint,
            })
        }
        Request::RemovePeer { peer } => {
            let id = ctx.store.snapshot().resolve_peer(&peer)?;
            ctx.store.update(|state| state.remove_peer(&id))?;

            Ok(Response::PeerRemoved(id))
        }
        Request::Copy {
            mime,
            bytes,
            secret,
            ttl_secs,
        } => {
            let ttl = ttl_secs.map(Duration::from_secs);
            let id = clip.copy(mime, bytes, secret, ttl)?;

            Ok(Response::Wrote { label: id.label() })
        }
        Request::Paste { entry } => {
            let (mime, bytes) = clip.paste(entry.as_deref())?;

            Ok(Response::Contents { mime, bytes })
        }
        Request::History { limit } => {
            let entries = history(ctx, limit);

            Ok(Response::History(entries))
        }
        Request::Pick { entry } => {
            let id = clip.pick(&entry)?;

            Ok(Response::Wrote { label: id.label() })
        }
        Request::Forget { entry } => {
            let id = clip.forget(&entry)?;

            Ok(Response::Wrote { label: id.label() })
        }
        Request::Clear { history } => {
            clip.clear(history)?;

            Ok(Response::Cleared)
        }
        Request::SetPause { capture, apply } => {
            Ok(Response::Paused(clip.set_pause(capture, apply)?))
        }
    }
}

/// Snapshots the daemon for `yank status`.
fn status(ctx: &Context) -> Status {
    let clip = &ctx.topics.clipboard;
    let (items, selected) = clip.history();
    let state = ctx.store.snapshot();

    let local = ctx.endpoint.secret_key().public();
    let selection = items
        .iter()
        .find(|item| Some(item.id) == selected)
        .map(|item| entry(item, &state, local, selected));

    Status {
        endpoint: ctx.endpoint.secret_key().public(),
        uptime_secs: ctx.started.elapsed().unwrap_or_default().as_secs(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        clipboard: ClipboardStatus {
            backend: match clip.backend_state() {
                BackendState::Running => None,
                BackendState::Down(reason) => Some(sanitize_bounded(&reason)),
            },
            pause: clip.pause(),
            entries: items.len(),
            selection,
        },
        peers: ctx.peers.statuses(),
    }
}

/// The history, resolved for display.
fn history(ctx: &Context, limit: Option<usize>) -> Vec<HistoryEntry> {
    let (items, selected) = ctx.topics.clipboard.history();
    let state = ctx.store.snapshot();

    let local = ctx.endpoint.secret_key().public();

    items
        .iter()
        .take(limit.unwrap_or(usize::MAX))
        .map(|item| entry(item, &state, local, selected))
        .collect()
}

/// Describes one entry, naming the machine it came from.
fn entry(
    item: &Item,
    state: &crate::config::MeshState,
    local: iroh::EndpointId,
    selected: Option<crate::log::EntryId>,
) -> HistoryEntry {
    // A machine that left the mesh keeps its entries in the history until
    // they age out, and there is no name left for it: show the id.
    let origin = if item.id.origin == local {
        "this machine".to_owned()
    } else {
        state
            .peer_name(&item.id.origin)
            .map_or_else(|| item.id.origin.to_string(), str::to_owned)
    };
    let now = SystemTime::now();

    HistoryEntry {
        label: item.id.label(),
        preview: item.preview.clone(),
        mime: item.mime.clone(),
        size: item.size,
        secret: item.secret,
        origin,
        age_secs: now
            .duration_since(item.clock.as_system_time())
            .unwrap_or_default()
            .as_secs(),
        expires_in_secs: item
            .expires_at
            .map(|at| at.duration_since(now).unwrap_or_default().as_secs()),
        selected: Some(item.id) == selected,
    }
}
