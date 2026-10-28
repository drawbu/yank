//! The CLI's side of the control socket.

use std::io;

use eyre::WrapErr as _;
use tokio::net::UnixStream;

use super::Situation;
use crate::{
    config::Dirs,
    daemon::control::{MAX_MESSAGE_SIZE, Request, Response},
    net::wire::{read_message, write_message},
};

/// Sends one request to the daemon and returns the answer, turning a
/// [`Response::Error`] into an error so callers only match the shape they
/// asked for.
pub async fn ask(dirs: &Dirs, request: &Request) -> eyre::Result<Response> {
    let path = dirs.socket_file();
    let mut stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(Situation::DaemonNotRunning.into());
        }
        Err(err) => {
            return Err(err).wrap_err_with(|| format!("cannot connect to {}", path.display()));
        }
    };
    write_message(&mut stream, request, MAX_MESSAGE_SIZE).await?;

    let read = read_message(&mut stream, MAX_MESSAGE_SIZE);
    match tokio::time::timeout(request.timeout(), read).await {
        Ok(Ok(Response::Error(message))) => eyre::bail!("{message}"),
        Ok(Ok(response)) => Ok(response),
        Ok(Err(err)) => Err(err),
        Err(_) => eyre::bail!("the daemon did not answer"),
    }
}

/// The same, from synchronous code.
pub(super) fn request(dirs: &Dirs, request: &Request) -> eyre::Result<Response> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(ask(dirs, request))
}

/// What to say about an answer of the wrong shape.
pub(super) fn unexpected(response: &Response) -> eyre::Report {
    eyre::eyre!("unexpected answer from the daemon: {response:?}")
}
