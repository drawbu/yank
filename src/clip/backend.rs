//! What a platform has to answer to hold the clipboard.
//!
//! [`state`](super::state) decides what the clipboard should hold and
//! knows nothing about how; a backend is the other half, and this is
//! everything the two say to each other:
//!
//! ```text
//!   state ──► Command ──►  backend  ──► Event ──► state
//!             Offer                    Copied(Captured)
//!             Clear                    Emptied
//!                                      Lost
//! ```
//!
//! Two properties are what make a platform implementable, and both are
//! properties yank needs rather than Wayland accidents:
//!
//! - **Mime is the vocabulary.** A platform that names its own types
//!   differently, macOS and its uniform type identifiers for instance,
//!   translates at its own edge. Two machines naming the same bytes
//!   differently would each have to understand both, forever.
//! - **Serving is lazy.** A backend is asked for bytes when somebody
//!   pastes, which is what `wl_data_source` does with a file descriptor
//!   and what `NSPasteboardItemDataProvider` does with a callback.
//!
//! Nothing here says how a backend notices a change. Wayland pushes one;
//! macOS has no such event and a backend there polls `changeCount`. Either
//! way what comes out is [`Event::Copied`].

use color_eyre::eyre::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::log::Payload;

/// What the daemon asks a backend to do.
#[derive(Debug)]
pub enum Command {
    /// Take the selection and serve `bytes` for every type in `mimes`.
    Offer { mimes: Vec<String>, bytes: Payload },
    /// Empty the selection.
    Clear,
}

/// What a backend tells the daemon.
#[derive(Debug)]
pub enum Event {
    /// Something was copied by an application other than us.
    Copied(Captured),
    /// The selection was emptied by somebody else.
    Emptied,
    /// The backend is gone: the session ended, or withdrew what the
    /// backend was using. The daemon reconnects.
    Lost(String),
}

/// A selection read off the local clipboard.
#[derive(Debug)]
pub struct Captured {
    /// The type the bytes are in.
    pub mime: String,
    pub bytes: Vec<u8>,
    /// Whether the source flagged this as a password (see
    /// [`mime::SECRET_HINT`](super::mime::SECRET_HINT)).
    pub secret: bool,
}

/// A running backend. Dropping it stops it.
///
/// Commands are queued rather than performed: a backend is somewhere else,
/// on a thread or behind a run loop, and the daemon must not wait on it.
pub trait Backend: std::fmt::Debug + Send + Sync {
    fn send(&self, command: Command);
}

/// The backend of this platform, named at compile time: a platform is not
/// something a machine changes its mind about, so the daemon holds the one
/// there is rather than paying for a choice it never makes.
#[cfg(target_os = "linux")]
pub type Platform = super::wayland::Wayland;

/// No platform, so no backend a value could be of.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub enum Platform {}

#[cfg(not(target_os = "linux"))]
impl Backend for Platform {
    fn send(&self, _command: Command) {
        match *self {}
    }
}

/// Connects to whatever holds the clipboard on this platform.
///
/// `max_bytes` caps what a single capture may read, so an application
/// offering a gigabyte cannot be used to exhaust our memory.
///
/// Failing here is ordinary: a machine with no graphical session has no
/// clipboard to hold, and the daemon keeps replicating without one.
pub fn connect(events: &UnboundedSender<Event>, max_bytes: usize) -> Result<Platform> {
    #[cfg(target_os = "linux")]
    {
        Platform::connect(events, max_bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (events, max_bytes);
        color_eyre::eyre::bail!("yank has no clipboard backend for this platform yet")
    }
}
