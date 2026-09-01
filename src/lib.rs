//! `yank` replicates the clipboard, and a bounded history of it, across
//! the machines you own.
//!
//! They form a *mesh*: they pair once, then hold direct connections to
//! each other over [iroh](https://www.iroh.computer/), with no server ever
//! holding the data. What they replicate is a bounded, append-only **log**
//! of events, keyed by the machine that wrote them; the clipboard is the
//! first topic carried over it.
//!
//! ```text
//! ┌─────┐  control  ┌────────┐    iroh (QUIC)    ┌──────────────┐
//! │ CLI │──socket──►│ daemon │◄─────────────────►│ peer daemons │
//! └─────┘           └───┬────┘                   └──────────────┘
//!                       │ ext/wlr-data-control
//!                       ▼
//!                 Wayland compositor
//! ```
//!
//! The layers, from the wire up:
//!
//! - [`config`] resolves the XDG directories and owns everything written
//!   to them: the machine identity key, the mesh state, the user settings.
//! - [`net`] is the transport: the iroh endpoint, the message framing, the
//!   pairing protocol and the replication vocabulary.
//! - [`log`] is the replication engine: a per-origin append-only log,
//!   ordered by a hybrid logical clock, exchanged by announcing watermarks
//!   and pulling what is missing. It knows nothing about clipboards.
//! - [`clip`] is the clipboard topic: the entry model (mime types,
//!   lifetimes, secrecy), the Wayland data-control backend, and the state
//!   machine tying them to the log.
//! - [`daemon`] runs it all: peer connections, routing, the control socket
//!   the CLI talks to.
//! - [`cli`] is the `yank` command itself.
//!
//! Adding a feature that is not the clipboard means adding a variant to
//! [`net::proto::Topic`] and a service alongside [`clip`]; the log, the
//! connections and the control socket are already topic-agnostic.

#![warn(clippy::pedantic)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

pub mod cli;
pub mod clip;
pub mod config;
pub mod daemon;
pub mod log;
pub mod net;
