//! Peer-to-peer networking.
//!
//! Holds the iroh endpoint, the framing every protocol shares, the pairing
//! exchange that lets a machine join the mesh, and the vocabulary machines
//! use to replicate the log.

mod endpoint;
pub mod pair;
pub mod proto;
pub mod wire;

pub use endpoint::{EndpointOptions, bind_endpoint};
