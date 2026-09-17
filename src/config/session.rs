//! The graphical session's environment, as the systemd user manager holds
//! it.
//!
//! The daemon starts from `default.target`, before any compositor, and
//! outlives sessions. Its own environment is a snapshot taken then, so the
//! compositor's socket has to come from the manager, which compositors
//! update with `import-environment` as they start.

use std::{
    env,
    path::{Path, PathBuf},
};

use eyre::WrapErr as _;

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    gen_blocking = false
)]
trait Manager {
    #[zbus(property)]
    fn environment(&self) -> zbus::Result<Vec<String>>;
}

/// The compositor socket of the current session, or `None` when the
/// manager knows no display.
///
/// Fails when there is no user manager on the session bus.
pub async fn wayland_socket() -> eyre::Result<Option<PathBuf>> {
    let bus = zbus::Connection::session()
        .await
        .wrap_err("cannot reach the session bus")?;
    let environment = ManagerProxy::new(&bus)
        .await
        .wrap_err("cannot reach the user manager")?
        .environment()
        .await
        .wrap_err("cannot read the user manager's environment")?;

    let Some(display) = environment
        .iter()
        .find_map(|variable| variable.strip_prefix("WAYLAND_DISPLAY="))
    else {
        return Ok(None);
    };
    let display = Path::new(display);
    if display.is_absolute() {
        return Ok(Some(display.to_owned()));
    }
    let runtime =
        env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| eyre::eyre!("XDG_RUNTIME_DIR is not set"))?;
    Ok(Some(Path::new(&runtime).join(display)))
}
