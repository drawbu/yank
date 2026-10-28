//! The daemon's side of the control socket.
//!
//! Binding the socket is also the single-daemon guard: two daemons sharing
//! one identity would race each other's connections.

use std::{
    fs::{self, File, TryLockError},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use eyre::WrapErr as _;
use iroh::Endpoint;
use tokio::net::{UnixListener, UnixStream};

use super::protocol::{
    ClipboardStatus, FILES_WAIT, MAX_MESSAGE_SIZE, PAIRING_TIMEOUT, Request, Response, Status,
};
use crate::{
    clip::Rep,
    config::{Dirs, sanitize_bounded},
    daemon::{
        backoff::Backoff, clip::ClipService, pairing::Pairing, peers::PeerSet, store::MeshStore,
    },
    net::{pair, wire::read_message, wire::write_message},
};

/// Budget for reading a request, so a client that connects and says
/// nothing does not hold a task.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// First and last delay between retries when accepting fails.
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(100);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Everything the request handlers need.
#[derive(Debug)]
pub struct Context {
    pub endpoint: Endpoint,
    pub started: SystemTime,
    pub peers: Arc<PeerSet>,
    pub clip: Arc<ClipService>,
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
    pub fn bind(dirs: &Dirs) -> eyre::Result<Self> {
        let path = dirs.socket_file();

        let lock_path = path.with_extension("lock");
        let lock = File::create(&lock_path)
            .wrap_err_with(|| format!("cannot create {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => eyre::bail!("another yank daemon is already running"),
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
                    tracing::warn!("cannot accept on the control socket: {err}");
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
        Ok(Err(err)) => return tracing::debug!("bad control request: {err:#}"),
        Err(_) => return tracing::debug!("a control client said nothing"),
    };

    let response = match answer(&ctx, request).await {
        Ok(response) => response,
        // Errors carry file paths and peer names, so they are cleaned up
        // before they are handed back to be printed.
        Err(err) => Response::Error(sanitize_bounded(&format!("{err:#}"))),
    };
    if let Err(err) = write_message(&mut stream, &response, MAX_MESSAGE_SIZE).await {
        tracing::debug!("cannot answer a control client: {err:#}");
    }
}

/// Runs one request.
async fn answer(ctx: &Context, request: Request) -> eyre::Result<Response> {
    let clip = &ctx.clip;

    match request {
        Request::Status => Ok(Response::Status(Box::new(status(ctx)))),
        Request::PairHost { name } => {
            crate::config::validate_name(&name)?;
            Ok(Response::PairTicket(
                ctx.pairing.host(name).await?.to_string(),
            ))
        }
        Request::PairJoin { ticket, name } => {
            crate::config::validate_name(&name)?;
            let ticket: pair::PairTicket = ticket.parse()?;
            let state = ctx.store.snapshot();

            let joining = pair::join(&ctx.endpoint, &ticket, &name, &state);
            let peer = tokio::time::timeout(PAIRING_TIMEOUT, joining)
                .await
                .map_err(|_| eyre::eyre!("pairing timed out"))??;
            ctx.store.add_paired(&peer)?;

            Ok(Response::Paired(peer.name))
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
            ttl,
        } => {
            clip.copy(Rep::new(mime, bytes), secret, ttl)?;

            Ok(Response::Done)
        }
        Request::CopyFiles { paths, ttl } => {
            let paths = paths.into_iter().map(PathBuf::from).collect();
            clip.copy_files(paths, ttl).await?;

            Ok(Response::Done)
        }
        Request::Files => {
            let located = clip.located(FILES_WAIT).await?;

            Ok(Response::Files {
                tree: located.tree.map(|tree| tree.to_string_lossy().into_owned()),
                files: located.files,
            })
        }
        Request::Paste { mime } => {
            let (mime, alternates, bytes) = clip.paste(mime.as_deref())?;

            Ok(Response::Contents {
                mime,
                alternates,
                bytes,
            })
        }
        Request::Clear => {
            clip.clear();

            Ok(Response::Done)
        }
        Request::SetPause { capture, apply } => {
            Ok(Response::Paused(clip.set_pause(capture, apply)))
        }
    }
}

/// Snapshots the daemon for `yank status`.
fn status(ctx: &Context) -> Status {
    let local = ctx.endpoint.secret_key().public();
    let state = ctx.store.snapshot();
    // A machine that left the mesh keeps its clip until the next copy, and
    // there is no name left for it: show the id.
    let name = |origin| {
        if origin == local {
            "this machine".to_owned()
        } else {
            state
                .peer_name(&origin)
                .map_or_else(|| origin.to_string(), str::to_owned)
        }
    };

    Status {
        endpoint: local,
        uptime_secs: ctx.started.elapsed().unwrap_or_default().as_secs(),
        version: crate::VERSION.to_string(),
        clipboard: ClipboardStatus {
            backend: ctx
                .clip
                .backend_down()
                .map(|reason| sanitize_bounded(&reason)),
            pause: ctx.clip.pause(),
            current: ctx.clip.describe(name),
        },
        peers: ctx.peers.statuses(),
    }
}
