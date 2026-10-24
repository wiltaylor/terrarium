//! The per-session replay buffer.
//!
//! Every output byte a session makes goes into its [`Outbox`] first. The
//! outbox sends a copy on the live link, if there is one, and keeps the bytes
//! until the daemon acknowledges them. On a reconnect it replays every byte
//! after the daemon's `received` offset, then the terminal frames.
//!
//! The buffer is bounded. While a link is up, a full buffer makes the writer
//! wait (backpressure: the process blocks on its pipe). While no link is up,
//! a full buffer is an overflow: the session is killed with
//! [`ExitReason::OutputOverflow`](crate::protocol::ExitReason::OutputOverflow).

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use tokio::sync::{Notify, mpsc};

use crate::protocol::{DataFrame, HostFrame, SessionId, StreamTag};

/// One outgoing item on a connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Data(DataFrame),
    Control(HostFrame),
}

/// The sending half of the live connection.
pub type Link = mpsc::UnboundedSender<Outgoing>;

/// The buffer was full and no link was up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("output buffer overflowed while the daemon was not connected")]
pub struct Overflow;

#[derive(Debug)]
pub struct Outbox {
    session: SessionId,
    limit: usize,
    state: Mutex<State>,
    room: Notify,
}

#[derive(Debug, Default)]
struct State {
    /// Unacknowledged chunks, oldest first.
    queue: VecDeque<(StreamTag, Vec<u8>)>,
    /// Bytes in `queue`.
    queued: usize,
    /// The offset of the first byte in `queue`.
    acked: u64,
    link: Option<Link>,
    /// The terminal frames (and a `result` before them), kept until release.
    finals: Vec<HostFrame>,
    finished: bool,
}

impl Outbox {
    pub fn new(session: SessionId, limit: usize) -> Self {
        Self {
            session,
            limit: limit.max(1),
            state: Mutex::new(State::default()),
            room: Notify::new(),
        }
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue output bytes and send them on the link. Waits while the buffer
    /// is full and a link is up. Fails when the buffer is full and no link is up.
    pub async fn push(&self, stream: StreamTag, data: Vec<u8>) -> Result<(), Overflow> {
        if data.is_empty() {
            return Ok(());
        }
        loop {
            let notified = self.room.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.lock();
                // A chunk always fits into an empty buffer, so one chunk
                // larger than the limit cannot stall the session.
                if st.queued == 0 || st.queued + data.len() <= self.limit {
                    if let Some(link) = &st.link {
                        let frame = DataFrame::new(self.session, stream, data.clone());
                        if link.send(Outgoing::Data(frame)).is_err() {
                            st.link = None;
                        }
                    }
                    st.queued += data.len();
                    st.queue.push_back((stream, data));
                    return Ok(());
                }
                if st.link.is_none() {
                    return Err(Overflow);
                }
            }
            notified.await;
        }
    }

    /// Send a control frame on the live link only. Used for `started`.
    pub fn send_now(&self, frame: HostFrame) {
        let mut st = self.lock();
        let failed = match &st.link {
            Some(link) => link.send(Outgoing::Control(frame)).is_err(),
            None => false,
        };
        if failed {
            st.link = None;
        }
    }

    /// The session ended. Keep `frames` until release and send them now if
    /// a link is up. They follow every data byte.
    pub fn finish(&self, frames: Vec<HostFrame>) {
        let mut st = self.lock();
        if st.finished {
            return;
        }
        st.finished = true;
        if let Some(link) = &st.link {
            for f in &frames {
                if link.send(Outgoing::Control(f.clone())).is_err() {
                    st.link = None;
                    break;
                }
            }
        }
        st.finals = frames;
    }

    pub fn is_finished(&self) -> bool {
        self.lock().finished
    }

    /// The daemon holds the first `bytes` bytes. Drop them.
    pub fn ack(&self, bytes: u64) {
        let mut st = self.lock();
        trim_to(&mut st, bytes);
        drop(st);
        self.room.notify_waiters();
    }

    /// Total output bytes produced so far.
    pub fn sent(&self) -> u64 {
        let st = self.lock();
        st.acked + st.queued as u64
    }

    /// Unacknowledged bytes held.
    pub fn buffered(&self) -> usize {
        self.lock().queued
    }

    pub fn is_attached(&self) -> bool {
        self.lock().link.is_some()
    }

    /// Attach to a new link after `welcome`. The daemon has `received`
    /// bytes. Replay the rest and the terminal frames, then go live.
    pub fn attach(&self, link: Link, received: u64) {
        let mut st = self.lock();
        if received < st.acked {
            tracing::warn!(
                session = self.session,
                received,
                acked = st.acked,
                "daemon resumes before its own ack; bytes before the ack are gone"
            );
        }
        trim_to(&mut st, received);
        let mut ok = true;
        for (stream, data) in &st.queue {
            let frame = DataFrame::new(self.session, *stream, data.clone());
            if link.send(Outgoing::Data(frame)).is_err() {
                ok = false;
                break;
            }
        }
        if ok && st.finished {
            for f in &st.finals {
                if link.send(Outgoing::Control(f.clone())).is_err() {
                    ok = false;
                    break;
                }
            }
        }
        st.link = ok.then_some(link);
        drop(st);
        self.room.notify_waiters();
    }

    /// The link went down. A writer waiting for room now sees the overflow
    /// if the buffer stays full.
    pub fn detach(&self) {
        self.lock().link = None;
        self.room.notify_waiters();
    }
}

fn trim_to(st: &mut State, bytes: u64) {
    let total = st.acked + st.queued as u64;
    let target = bytes.min(total);
    while st.acked < target {
        let Some((_, front)) = st.queue.front_mut() else {
            break;
        };
        let need = (target - st.acked) as usize;
        if front.len() <= need {
            let n = front.len();
            st.queue.pop_front();
            st.queued -= n;
            st.acked += n as u64;
        } else {
            front.drain(..need);
            st.queued -= need;
            st.acked += need as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ExitReason;
    use std::sync::Arc;
    use std::time::Duration;

    fn drain(rx: &mut mpsc::UnboundedReceiver<Outgoing>) -> Vec<Outgoing> {
        let mut v = Vec::new();
        while let Ok(o) = rx.try_recv() {
            v.push(o);
        }
        v
    }

    fn data_bytes(items: &[Outgoing]) -> Vec<u8> {
        items
            .iter()
            .filter_map(|o| match o {
                Outgoing::Data(d) => Some(d.payload.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[tokio::test]
    async fn replays_unacked_bytes_from_the_received_offset() {
        let ob = Outbox::new(1, 1024);
        let (tx, mut rx) = mpsc::unbounded_channel();
        ob.attach(tx, 0);
        ob.push(StreamTag::Stdout, b"hello ".to_vec())
            .await
            .unwrap();
        ob.push(StreamTag::Stderr, b"world".to_vec()).await.unwrap();
        assert_eq!(data_bytes(&drain(&mut rx)), b"hello world");
        ob.ack(3);
        assert_eq!(ob.buffered(), 8);
        ob.detach();
        ob.finish(vec![HostFrame::Exit {
            session: 1,
            code: Some(0),
            signal: None,
            reason: ExitReason::Exited,
        }]);
        // The daemon says it got 8 bytes ("hello wo"), more than it acked.
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        ob.attach(tx2, 8);
        let items = drain(&mut rx2);
        assert_eq!(data_bytes(&items), b"rld");
        assert!(matches!(items.last(), Some(Outgoing::Control(f)) if f.is_terminal()));
        assert_eq!(ob.sent(), 11);
    }

    #[tokio::test]
    async fn overflows_when_detached_and_full() {
        let ob = Outbox::new(1, 8);
        ob.push(StreamTag::Stdout, vec![0; 6]).await.unwrap();
        assert_eq!(ob.push(StreamTag::Stdout, vec![0; 6]).await, Err(Overflow));
    }

    #[tokio::test]
    async fn backpressures_while_attached_until_acked() {
        let ob = Arc::new(Outbox::new(1, 8));
        let (tx, _rx) = mpsc::unbounded_channel();
        ob.attach(tx, 0);
        ob.push(StreamTag::Stdout, vec![0; 6]).await.unwrap();
        let ob2 = ob.clone();
        let writer = tokio::spawn(async move { ob2.push(StreamTag::Stdout, vec![1; 6]).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!writer.is_finished(), "writer must wait for room");
        ob.ack(6);
        writer.await.unwrap().unwrap();
        assert_eq!(ob.sent(), 12);
    }

    #[tokio::test]
    async fn waiting_writer_overflows_when_the_link_drops() {
        let ob = Arc::new(Outbox::new(1, 8));
        let (tx, _rx) = mpsc::unbounded_channel();
        ob.attach(tx, 0);
        ob.push(StreamTag::Stdout, vec![0; 6]).await.unwrap();
        let ob2 = ob.clone();
        let writer = tokio::spawn(async move { ob2.push(StreamTag::Stdout, vec![1; 6]).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        ob.detach();
        assert_eq!(writer.await.unwrap(), Err(Overflow));
    }
}
