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
//! - [`display`]: target displays: the `DisplayDriver` trait, the engine,
//!   and the vmlab, X11/Wayland, VNC, and RDP drivers.
//! - [`verb`]: shared parts of the builders.
//! - [`workspace`]: agent workspaces under `work_dir`.
//! - [`labcheck`]: the allowlist check of a lab definition, and the
//!   generated lab file of an instance.
//! - [`prepare`]: `vmlab.prepare` and `vmlab.release` (worktrees and
//!   lab instances).
//! - [`config`]: the config file and the path policy.
//! - [`capabilities`]: tool detection for `hello` and `check`.
//! - [`limits`]: resource limits on a `host` target, with
//!   `systemd-run --user` and a slice for each holder.
//! - [`native`]: native GPU enforcement: the root helper, its protocol,
//!   and the session shim (TER-345).
//! - [`metrics`]: CPU, memory, network, disk IO, and disk space samples for
//!   the `metrics` frame.
//! - [`selfupdate`]: the host agent replaces itself with the daemon's
//!   build, and goes back to the previous binary when the new one does
//!   not start.
//! - [`tls`]: the transport check, fingerprint pinning, the probe, and
//!   self-signed certificates.

pub mod capabilities;
pub mod client;
pub mod config;
pub mod display;
pub mod docker;
pub mod labcheck;
pub mod limits;
pub mod logging;
pub mod metrics;
#[cfg(unix)]
pub mod native;
#[cfg(windows)]
#[path = "native_windows.rs"]
pub mod native;
pub mod outbox;
// Production reservation creation is Windows-only; portable tests exercise it.
#[cfg_attr(not(windows), allow(dead_code))]
mod pending_logon;
pub mod prepare;
pub mod protocol;
pub mod selfupdate;
pub mod session;
#[cfg(windows)]
mod session_windows;
pub mod tls;
pub mod transfer;
pub mod update_journal;
pub mod update_signature;
pub mod verb;
pub mod vmlab;
pub mod windows_run_as;
#[cfg(windows)]
pub mod windows_update;
pub mod workspace;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub mod workspace_helper;

pub use client::{Client, ClientOptions, SelfUpdateOptions};
pub use config::{Config, HostPaths};
pub mod gpu;
