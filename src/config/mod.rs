//! The files `yank` keeps on disk, and the types that own them.
//!
//! Everything follows the XDG base directories, split by who writes it:
//!
//! - `$XDG_CONFIG_HOME/yank/config.toml`: the only hand-edited file, read
//!   once when the daemon starts.
//! - `$XDG_CONFIG_HOME/yank/service.toml`: which program installed the
//!   background service, written by `yank service install` or by an
//!   external manager such as the Nix module.
//! - `$XDG_STATE_HOME/yank/identity.key`: this machine's private identity
//!   on the mesh. Only the daemon ever reads it.
//! - `$XDG_STATE_HOME/yank/mesh.json`: the paired machines. The daemon is
//!   the sole writer; the CLI mutates it through the control socket.
//! - `$XDG_STATE_HOME/yank/clip.json` and `history/`: see [`crate::clip`].
//! - `$XDG_RUNTIME_DIR/yank.sock`: the control socket.

mod dirs;
mod key;
mod mesh;
mod name;
mod service;
mod settings;

pub use dirs::{Dirs, create_private, write_private};
pub use key::MachineKey;
pub use mesh::{MAX_MESH_PEERS, Membership, MeshState, Peer, PeerStatus};
pub use name::{MAX_NAME_LEN, sanitize, sanitize_bounded, validate_name};
pub use service::ServiceState;
pub use settings::Settings;
