//! The clipboard.
//!
//! [`wayland`] is the compositor side; [`mime`] is the policy for which
//! representation of a selection to carry.

pub mod mime;
pub mod wayland;
