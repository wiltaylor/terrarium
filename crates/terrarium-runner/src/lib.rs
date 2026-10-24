//! The Terrarium agent runner. One runner holds one harness in a PTY. It
//! keeps the screen, records `pty.log` and `events.jsonl`, serves the runner
//! socket for hooks and the agent CLI, and runs the status machine of PLAN §5.
//! See `docs/runner.md`.

mod config;
mod error;
pub mod protocol;
mod ptylog;
pub mod ready;
mod runner;
mod socket;

pub use config::{RunnerConfig, agent_dir, socket_path};
pub use error::{DeliveryError, Result, RunnerError};
pub use protocol::{BoxFuture, Message, NoHandler, RequestContext, RequestHandler, RpcError};
pub use ready::{DefaultReadyDetector, ReadyDetector};
pub use runner::{Delivery, ExitStatus, OutputChunk, Runner, ScreenSnapshot};
