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
//! - [`workspace`]: agent workspaces under `work_dir`.
//! - [`labcheck`]: the allowlist check of a lab definition, and the
//!   generated lab file of an instance.
//! - [`prepare`]: `vmlab.prepare` and `vmlab.release` (worktrees and
//!   lab instances).
//! - [`harness_update`]: the `harness.update` verb (`scripts/harness.py`
//!   in `terrarium_repo_dir`).
//! - [`config`]: the config file and the path policy.
//! - [`capabilities`]: tool detection for `hello` and `check`.
//! - [`tls`]: the transport check, fingerprint pinning, the probe, and
//!   self-signed certificates.

pub mod capabilities;
pub mod client;
pub mod config;
pub mod docker;
pub mod harness_update;
pub mod labcheck;
pub mod outbox;
pub mod prepare;
pub mod protocol;
pub mod session;
pub mod tls;
pub mod transfer;
pub mod verb;
pub mod vmlab;
pub mod workspace;

pub use client::{Client, ClientOptions};
pub use config::{Config, HostPaths};
