//! [`AttachStream`]: the output of an attach, with a resync when the client
//! falls behind.

use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, mpsc};

use crate::runner::{OUTPUT_DRAIN, OutputChunk, Runner, ScreenSnapshot};

/// One item of an [`AttachStream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// Raw PTY output that follows the last snapshot.
    Output(OutputChunk),
    /// The stream fell behind and attached again. Output is missing before
    /// this item. Clear the terminal, write `snapshot.formatted`, then go on
    /// with the next output.
    Resynced(ScreenSnapshot),
}

/// The output that follows the snapshot of an attach. When the client falls
/// behind, the stream attaches once again by itself: it gives
/// [`AttachEvent::Resynced`] with a new snapshot, and the output goes on
/// from that snapshot. Each lag gets one new attach; when that attach fails,
/// the stream ends. The stream ends (`None`) after the harness exits and the
/// last output is out.
pub struct AttachStream {
    source: Source,
}

enum Source {
    Local {
        runner: Runner,
        rx: broadcast::Receiver<OutputChunk>,
        exited: bool,
    },
    Remote(mpsc::Receiver<AttachEvent>),
}

impl AttachStream {
    pub(crate) fn local(runner: Runner, rx: broadcast::Receiver<OutputChunk>) -> Self {
        Self {
            source: Source::Local {
                runner,
                rx,
                exited: false,
            },
        }
    }

    pub(crate) fn remote(rx: mpsc::Receiver<AttachEvent>) -> Self {
        Self {
            source: Source::Remote(rx),
        }
    }

    /// The next item. `None`: the stream has ended.
    pub async fn recv(&mut self) -> Option<AttachEvent> {
        match &mut self.source {
            Source::Remote(rx) => rx.recv().await,
            Source::Local { runner, rx, exited } => loop {
                if *exited {
                    return match rx.try_recv() {
                        Ok(chunk) => Some(AttachEvent::Output(chunk)),
                        Err(TryRecvError::Lagged(_)) => Some(resync(runner, rx)),
                        Err(_) => None,
                    };
                }
                tokio::select! {
                    r = rx.recv() => return match r {
                        Ok(chunk) => Some(AttachEvent::Output(chunk)),
                        Err(RecvError::Lagged(_)) => Some(resync(runner, rx)),
                        Err(RecvError::Closed) => None,
                    },
                    _ = runner.wait() => {
                        runner.output_drained(OUTPUT_DRAIN).await;
                        *exited = true;
                    }
                }
            },
        }
    }
}

fn resync(runner: &Runner, rx: &mut broadcast::Receiver<OutputChunk>) -> AttachEvent {
    tracing::debug!("an attach stream fell behind; attaching again");
    let (snap, fresh) = runner.attach();
    *rx = fresh;
    AttachEvent::Resynced(snap)
}
