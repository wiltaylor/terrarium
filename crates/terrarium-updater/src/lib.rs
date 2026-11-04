//! The Terrarium updater (docs/api.md §34, PLAN §23.8).
//!
//! A small service next to the daemon. It holds `docker.sock`, and the
//! daemon never does: a sandbox escape into the daemon must not give root
//! on the host. It runs no agents and listens only on a Unix socket that
//! it shares with the daemon (`protocol::DEFAULT_SOCKET`). Its commands:
//! `status`, `update <version|latest|pre>`, and `rollback`.
//!
//! - `protocol`, `client`: the wire format and the client of the daemon.
//! - `machine`: the update and the rollback, over the seams in `engine`.
//! - `store`: the history and the rollback target in the state dir.
//! - `verify`: the signature check (a program: gh attestation verify by default, cosign, or a command).
//! - `docker`, `registry`, `server` (feature `bin`): Docker on
//!   docker.sock, the registry API, and the socket listener.

pub mod client;
pub mod engine;
pub mod machine;
pub mod protocol;
pub mod store;
pub mod verify;

#[cfg(feature = "bin")]
pub mod docker;
#[cfg(feature = "bin")]
pub mod registry;
#[cfg(feature = "bin")]
pub mod server;
