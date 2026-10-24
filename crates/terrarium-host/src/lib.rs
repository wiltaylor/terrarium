//! `terrarium-host`: the host agent (PLAN §19).
//!
//! The host agent dials out to the daemon over WebSocket and runs exec,
//! cp, Docker, and vmlab sessions on this machine for it. The wire contract
//! is `docs/host-protocol.md`. The daemon uses [`protocol`] for its side.
//!
//! Modules:
//!
//! - [`protocol`]: frame types and the codec.
//! - [`client`]: the dial-out loop, the multiplexer, and reconnects.
//! - [`outbox`]: the per-session ack and replay buffer.
//! - [`session`]: the exec runner (pipes or a PTY).
//! - [`transfer`]: `cp` in both directions.
//! - [`docker`], [`vmlab`]: verb builders and output parsers.
//! - [`verb`]: shared parts of the builders.
//! - [`config`]: the config file and the path policy.
//! - [`capabilities`]: tool detection for `hello` and `check`.

pub mod capabilities;
pub mod client;
pub mod config;
pub mod docker;
pub mod outbox;
pub mod protocol;
pub mod session;
pub mod transfer;
pub mod verb;
pub mod vmlab;

pub use client::{Client, ClientOptions};
pub use config::Config;
