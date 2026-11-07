//! The Terrarium agent runner. One runner holds one harness in a PTY. It
//! keeps the screen, records `pty.log` and `events.jsonl`, serves the runner
//! socket for hooks and the agent CLI, and runs the status machine of PLAN §5.
//!
//! A runner runs in the daemon process ([`Runner`]) or in its own process,
//! the `terrarium-runner` binary ([`process::run`]), so that the harness
//! lives through a daemon restart. The daemon talks to a runner process
//! with [`RemoteRunner`]. Both implement [`RunnerHandle`]. See
//! `docs/runner.md`.

mod attach;
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
pub mod safefs;
mod server;
mod socket;
pub mod spec;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use attach::{AttachEvent, AttachStream};
pub use config::{
    DEFAULT_KILL_GRACE, DEFAULT_OUTPUT_CAPACITY, DEFAULT_STARTUP_HOOK_TIMEOUT, DEFAULT_STOP_GRACE,
    HARNESS_DIR, PRIVATE_DIR, RunnerConfig, WORKSPACE_DIR, agent_dir, control_socket_path,
    create_private_dir, harness_dir, migrate_legacy_layout, pid_path, private_dir, runners_dir,
    socket_path, workspace_dir,
};
pub use error::{DeliveryError, Result, RunnerError};
pub use handle::RunnerHandle;
pub use process::{ExitRecord, StoppedBy, live_runner_pid, read_exit_record, read_exit_status};
pub use protocol::{BoxFuture, Message, NoHandler, RequestContext, RequestHandler, RpcError};
pub use ready::{DefaultReadyDetector, ReadyDetector};
pub use remote::{ConnectOptions, LOST_SIGNAL, RemoteRunner, SpawnOptions};
pub use runner::{
    Delivery, ExitStatus, MAX_RESUBMITS, OutputChunk, RESUBMIT_AFTER, Runner, ScreenSnapshot,
};
pub use spec::{AdapterKind, AdapterOptions, ReadyDetectorSpec, RunnerSpec};
