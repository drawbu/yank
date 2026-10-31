//! The clipboard: what the mesh shares, the state machine deciding what
//! this machine's clipboard holds, and the platform it talks to.

pub mod backend;
mod copy;
pub mod mime;
mod payload;
mod stamp;
mod state;
#[cfg(target_os = "linux")]
pub mod wayland;

pub use backend::{Captured, Policy, Serve};
pub use copy::{Clip, Copy, Rep, Selection};
pub use payload::{Payload, WeakPayload};
pub use stamp::Stamp;
pub use state::{Clipboard, Effect};

/// Hard ceiling on one selection, every representation together, enforced
/// before anything is stored. The user-facing cap in `config.toml` sits
/// under it.
pub const MAX_SIZE: u32 = 4 * 1024 * 1024;
