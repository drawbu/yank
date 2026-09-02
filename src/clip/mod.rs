//! The clipboard.
//!
//! [`state`] is the state machine that decides what the clipboard should
//! hold; [`backend`] is what a platform has to answer to hold it, and
//! [`wayland`] is the one that does; [`event`] is what machines send each
//! other about it; [`mime`] is the policy for which representation of a
//! selection to carry.
//!
//! The daemon side that ties them together lives in
//! [`crate::daemon::clip`].
//!
//! [`wayland`] is the only backend there is, but it is not the only one
//! there could be: [`backend`] is the whole of what the rest of the
//! clipboard knows about it. Another platform means another module
//! implementing [`backend::Backend`], a line in [`backend::Platform`], and
//! the mime policy in [`mime`] applying to what it produces.

pub mod backend;
pub mod event;
pub mod mime;
pub mod state;
#[cfg(target_os = "linux")]
pub mod wayland;

pub use backend::{Backend, Captured};
pub use event::{Copy, Event};
pub use state::{Clipboard, Effect, Item, Pause, Switch};
