//! The CLI's side of the control socket.

use std::{future::Future, io, time::Duration};

use color_eyre::eyre::{Report, Result, WrapErr as _, bail};
use tokio::net::UnixStream;

use super::protocol::{MAX_MESSAGE_SIZE, Request, Response};
use crate::{
    config::Dirs,
    net::wire::{read_message, write_message},
};

/// What every command that needs the daemon fails with when none is
/// running.
///
/// The CLI recognizes this and prints it plainly, without the error
/// dressing: not having started the daemon yet is a situation, not a
/// failure to debug.
#[derive(Debug)]
pub struct DaemonNotRunning;

impl std::fmt::Display for DaemonNotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The yank daemon is not running. Start it with `yank service start`.")
    }
}

impl std::error::Error for DaemonNotRunning {}

/// A connection to the daemon.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connects to the daemon, or `None` when none is listening.
    pub async fn connect(dirs: &Dirs) -> Result<Option<Self>> {
        let path = dirs.socket_file();

        match UnixStream::connect(&path).await {
            Ok(stream) => Ok(Some(Client { stream })),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(None)
            }
            Err(err) => Err(err).wrap_err_with(|| format!("cannot connect to {}", path.display())),
        }
    }

    /// Connects, failing with [`DaemonNotRunning`] when none is listening.
    pub async fn connect_required(dirs: &Dirs) -> Result<Self> {
        Self::connect(dirs)
            .await?
            .ok_or_else(|| Report::new(DaemonNotRunning))
    }

    /// Sends one request and returns the answer, turning a
    /// [`Response::Error`] into an error so callers only match the shape
    /// they asked for.
    pub async fn request(&mut self, request: &Request, limit: Duration) -> Result<Response> {
        write_message(&mut self.stream, request, MAX_MESSAGE_SIZE).await?;

        let read = read_message(&mut self.stream, MAX_MESSAGE_SIZE);
        match tokio::time::timeout(limit, read).await {
            Ok(Ok(Response::Error(message))) => bail!("{message}"),
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(err),
            Err(_) => bail!("the daemon did not answer"),
        }
    }
}

/// Runs a control operation on a current-thread Tokio runtime.
pub fn talk<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(future)
}

/// Sends one request to the daemon and returns its answer.
pub fn request(dirs: &Dirs, request: &Request, limit: Duration) -> Result<Response> {
    talk(async {
        Client::connect_required(dirs)
            .await?
            .request(request, limit)
            .await
    })
}
