//! [`RunnerHandle`]: the surface that the daemon uses, for an in-process
//! [`Runner`] and for a runner process ([`crate::RemoteRunner`]).

use std::path::Path;

use terrarium_core::{AgentId, AgentStatus, Answer, EventRecord};
use tokio::sync::{broadcast, watch};

use crate::attach::AttachStream;
use crate::error::Result;
use crate::protocol::BoxFuture;
use crate::runner::{Delivery, ExitStatus, Runner, ScreenSnapshot};

/// One harness, in this process or in a runner process. The trait is
/// object safe: hold an `Arc<dyn RunnerHandle>`.
pub trait RunnerHandle: Send + Sync + 'static {
    fn agent(&self) -> AgentId;
    /// The hook and agent RPC socket, `<run>/agents/<ulid>.sock`.
    fn socket_path(&self) -> &Path;
    /// The `events.jsonl` path.
    fn events_path(&self) -> &Path;
    /// The current status. For a runner process, the last status that it
    /// reported.
    fn status(&self) -> AgentStatus;
    /// A receiver that always holds the current status.
    fn watch_status(&self) -> watch::Receiver<AgentStatus>;
    /// Every event from now on. For a runner process, the first call also
    /// gets the replay that `ConnectOptions::events_since` asked for.
    fn subscribe_events(&self) -> broadcast::Receiver<EventRecord>;
    fn screen(&self) -> BoxFuture<'_, Result<ScreenSnapshot>>;
    /// The current screen and the output that follows it, with no gap and
    /// no overlap. When the stream falls behind, it attaches again by
    /// itself and gives `AttachEvent::Resynced` with a new snapshot. The
    /// stream ends after the harness exits.
    fn attach(&self) -> BoxFuture<'_, Result<(ScreenSnapshot, AttachStream)>>;
    /// Write raw bytes to the PTY, as keystrokes.
    fn write<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, Result<()>>;
    fn resize(&self, rows: u16, cols: u16) -> BoxFuture<'_, Result<()>>;
    /// Queue a user turn (see `docs/runner.md`, "Typing a turn").
    fn type_turn(&self, text: String) -> Delivery;
    fn answer<'a>(&'a self, request_id: &'a str, answer: &'a Answer) -> BoxFuture<'a, Result<()>>;
    fn answer_keys<'a>(&'a self, keys: &'a str) -> BoxFuture<'a, Result<()>>;
    /// Stop the harness on purpose.
    fn kill(&self) -> BoxFuture<'_, Result<()>>;
    /// Wait for the harness to exit.
    fn wait(&self) -> BoxFuture<'_, ExitStatus>;
}

impl RunnerHandle for Runner {
    fn agent(&self) -> AgentId {
        Runner::agent(self)
    }
    fn socket_path(&self) -> &Path {
        Runner::socket_path(self)
    }
    fn events_path(&self) -> &Path {
        Runner::events_path(self)
    }
    fn status(&self) -> AgentStatus {
        Runner::status(self)
    }
    fn watch_status(&self) -> watch::Receiver<AgentStatus> {
        Runner::watch_status(self)
    }
    fn subscribe_events(&self) -> broadcast::Receiver<EventRecord> {
        Runner::subscribe_events(self)
    }
    fn screen(&self) -> BoxFuture<'_, Result<ScreenSnapshot>> {
        let s = Runner::screen(self);
        Box::pin(async move { Ok(s) })
    }
    fn attach(&self) -> BoxFuture<'_, Result<(ScreenSnapshot, AttachStream)>> {
        let a = Runner::attach_stream(self);
        Box::pin(async move { Ok(a) })
    }
    fn write<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        Box::pin(Runner::write(self, bytes))
    }
    fn resize(&self, rows: u16, cols: u16) -> BoxFuture<'_, Result<()>> {
        let r = Runner::resize(self, rows, cols);
        Box::pin(async move { r })
    }
    fn type_turn(&self, text: String) -> Delivery {
        Runner::type_turn(self, text)
    }
    fn answer<'a>(&'a self, request_id: &'a str, answer: &'a Answer) -> BoxFuture<'a, Result<()>> {
        Box::pin(Runner::answer(self, request_id, answer))
    }
    fn answer_keys<'a>(&'a self, keys: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(Runner::answer_keys(self, keys))
    }
    fn kill(&self) -> BoxFuture<'_, Result<()>> {
        let r = Runner::kill(self);
        Box::pin(async move { r })
    }
    fn wait(&self) -> BoxFuture<'_, ExitStatus> {
        Box::pin(Runner::wait(self))
    }
}
