//! The Terrarium agent runner. One runner holds one harness in a PTY. It
//! keeps the screen, records `pty.log` and `events.jsonl`, serves the runner
//! socket for hooks and the agent CLI, and runs the status machine of PLAN §5.
//!
//! A runner runs in the daemon process ([`Runner`]) or in its own process,
//! the `terrarium-runner` binary ([`process::run`]), so that the harness
//! lives through a daemon restart. The daemon talks to a runner process
//! with [`RemoteRunner`]. Both implement [`RunnerHandle`]. See
//! `docs/runner.md`.

mod config;
pub mod control;
mod error;
pub mod events;
mod handle;
pub mod process;
pub mod protocol;
mod ptylog;
pub mod ready;
mod remote;
mod runner;
mod server;
mod socket;
pub mod spec;

pub use config::{
    RunnerConfig, agent_dir, control_socket_path, pid_path, runners_dir, socket_path,
};
pub use error::{DeliveryError, Result, RunnerError};
pub use handle::RunnerHandle;
pub use process::{live_runner_pid, read_exit_status};
pub use protocol::{BoxFuture, Message, NoHandler, RequestContext, RequestHandler, RpcError};
pub use ready::{DefaultReadyDetector, ReadyDetector};
pub use remote::{ConnectOptions, LOST_SIGNAL, RemoteRunner, SpawnOptions};
pub use runner::{Delivery, ExitStatus, OutputChunk, Runner, ScreenSnapshot};
pub use spec::{AdapterKind, AdapterOptions, ReadyDetectorSpec, RunnerSpec};
