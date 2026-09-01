//! The clipboard.
//!
//! [`state`] is the state machine that decides what the clipboard should
//! hold; [`wayland`] is the compositor it holds it in; [`event`] is what
//! machines send each other about it; [`mime`] is the policy for which
//! representation of a selection to carry.
//!
//! The daemon side that ties them together lives in
//! [`crate::daemon::clip`].

pub mod event;
pub mod mime;
pub mod state;
pub mod wayland;

pub use event::{Copy, Event};
pub use state::{Clipboard, Effect, Item, Pause, Switch};
