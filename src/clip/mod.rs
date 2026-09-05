//! Clipboard state, event payloads, mime policy, and Wayland integration.

pub mod backend;
pub mod event;
pub mod mime;
pub mod state;
#[cfg(target_os = "linux")]
pub mod wayland;

pub use backend::{Backend, Captured, Policy, Serve};
pub use event::{Copy, Event, Rep, Selection};
pub use state::{Clipboard, Effect, Item, Pause, Switch};
