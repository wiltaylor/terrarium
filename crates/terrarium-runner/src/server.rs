//! The control socket server of a runner process (`docs/runner.md`, "The
//! control socket"), and the agent RPC forwarding to the daemon.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use terrarium_core::HarnessId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinSet};

use crate::control::{
    AnswerKeysParams, AnswerParams, CancelParams, ClientFrame, EventsParams, Hello, HelloParams,
    PROTOCOL_VERSION, ResizeParams, ServerFrame, StoppedByParams, TypeTurnParams,
    WireDeliveryError, WireSnapshot, WriteParams, codes, decode, encode,
};
use crate::error::RunnerError;
use crate::events::read_since;
use crate::protocol::{BoxFuture, RequestContext, RequestHandler, RpcError};
use crate::runner::{OUTPUT_DRAIN, Runner};

/// The frames that wait for the writer of one connection.
const OUT_CAPACITY: usize = 1024;

type Out = mpsc::Sender<ServerFrame>;

#[cfg(test)]
mod replay_tests {
    use super::*;

    fn context() -> RequestContext {
        serde_json::from_value(json!({"agent":"00000000000000000000000001",
            "name":"tester", "project":"demo", "harness":"claude", "status":"idle"}))
        .unwrap()
    }

    #[tokio::test]
    async fn lost_answer_replays_the_same_request_after_hello() {
        let hub = Hub::new(Duration::from_secs(5));
        let (tx, mut first) = mpsc::channel(4);
        let (epoch, _) = hub.claim(tx);
        let params = json!({"method":"POST", "path":"/api/mail", "request_id":"stable"});
        let ctx = context();
        let (answer, ()) = tokio::join!(hub.forward(&ctx, "http", params.clone()), async {
            let ServerFrame::RpcRequest { params: sent, .. } = first.recv().await.unwrap() else {
                panic!("request");
            };
            assert_eq!(sent, params);
            hub.release(epoch);
            let (tx, mut second) = mpsc::channel(4);
            hub.claim(tx);
            let ServerFrame::RpcRequest {
                id,
                params: replayed,
                ..
            } = second.recv().await.unwrap()
            else {
                panic!("replay");
            };
            assert_eq!(replayed, params);
            hub.resolve(id, Ok(json!({"stored_answer":true})));
        });
        assert_eq!(answer.unwrap()["stored_answer"], true);
        assert!(lock(&hub.pending).is_empty());
    }

    #[tokio::test]
    async fn hold_does_not_cancel_an_admitted_call() {
        let hub = Hub::new(Duration::from_secs(5));
        let (tx, mut daemon) = mpsc::channel(4);
        let (epoch, _) = hub.claim(tx);
        let ctx = context();
        let (answer, ()) = tokio::join!(hub.forward(&ctx, "echo", json!({})), async {
            let ServerFrame::RpcRequest { id, .. } = daemon.recv().await.unwrap() else {
                panic!("request");
            };
            hub.hold(epoch);
            hub.resolve(id, Ok(json!("finished")));
        });
        assert_eq!(answer.unwrap(), "finished");
        assert!(hub.connected());
    }

    #[tokio::test]
    async fn held_queue_expires_without_forwarding() {
        let hub = Hub::new(Duration::from_millis(20));
        let (tx, mut daemon) = mpsc::channel(4);
        let (epoch, _) = hub.claim(tx);
        hub.hold(epoch);
        let err = hub
            .forward(&context(), "http", json!({"request_id":"held"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, "unavailable");
        assert!(err.message.contains("tunnel is down"));
        assert!(daemon.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_application_unavailable_answer_is_not_replayed() {
        let hub = Hub::new(Duration::from_secs(5));
        let (tx, mut daemon) = mpsc::channel(4);
        hub.claim(tx);
        let ctx = context();
        let (answer, ()) = tokio::join!(
            hub.forward(&ctx, "http", json!({"request_id":"once"})),
            async {
                let ServerFrame::RpcRequest { id, .. } = daemon.recv().await.unwrap() else {
                    panic!("request");
                };
                hub.resolve(id, Err(unavailable("application failure")));
            }
        );
        assert_eq!(answer.unwrap_err().message, "application failure");
        assert!(daemon.try_recv().is_err());
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn unavailable(m: impl Into<String>) -> RpcError {
    RpcError::new(codes::UNAVAILABLE, m)
}

/// The daemon connection of the moment.
#[derive(Clone)]
struct Link {
    epoch: u64,
    held: bool,
    out: Out,
    close: Arc<Notify>,
}

type PendingRpc = (u64, oneshot::Sender<Result<Value, RpcError>>);

/// Holds the one daemon connection, and the agent RPC requests that wait for
/// its answers.
pub(crate) struct Hub {
    link: watch::Sender<Option<Link>>,
    pending: Mutex<HashMap<u64, PendingRpc>>,
    next_rpc: AtomicU64,
    next_epoch: AtomicU64,
    queue_timeout: Duration,
}

impl Hub {
    pub(crate) fn new(queue_timeout: Duration) -> Self {
        Self {
            link: watch::Sender::new(None),
            pending: Mutex::new(HashMap::new()),
            next_rpc: AtomicU64::new(1),
            next_epoch: AtomicU64::new(1),
            queue_timeout,
        }
    }

    /// Make `out` the daemon connection. The old one closes.
    fn claim(&self, out: Out) -> (u64, Arc<Notify>) {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        let close = Arc::new(Notify::new());
        let old = self.link.send_replace(Some(Link {
            epoch,
            held: false,
            out,
            close: close.clone(),
        }));
        if let Some(old) = old {
            tracing::info!("a new daemon connection replaces the old one");
            old.close.notify_one();
            self.fail_epoch(
                old.epoch,
                "a new daemon connection replaced the one with the request",
            );
        }
        (epoch, close)
    }

    /// The connection `epoch` has closed.
    fn release(&self, epoch: u64) {
        self.link.send_if_modified(|l| {
            if l.as_ref().is_some_and(|l| l.epoch == epoch) {
                *l = None;
                true
            } else {
                false
            }
        });
        self.fail_epoch(epoch, "the daemon disconnected before it answered");
    }

    fn hold(&self, epoch: u64) {
        self.link.send_if_modified(|link| {
            if let Some(link) = link.as_mut().filter(|l| l.epoch == epoch) {
                link.held = true;
                true
            } else {
                false
            }
        });
    }

    fn fail_epoch(&self, epoch: u64, message: &str) {
        let failed: Vec<_> = {
            let mut pending = lock(&self.pending);
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, (g, _))| *g == epoch)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        };
        for (_, tx) in failed {
            let _ = tx.send(Err(RpcError::new("tunnel_lost", message)));
        }
    }

    fn resolve(&self, id: u64, outcome: Result<Value, RpcError>) {
        if let Some((_, tx)) = lock(&self.pending).remove(&id) {
            let _ = tx.send(outcome);
        }
    }

    pub(crate) fn connected(&self) -> bool {
        self.link.borrow().is_some()
    }

    /// Wait until no daemon is connected.
    pub(crate) async fn disconnected(&self) {
        let mut rx = self.link.subscribe();
        let _ = rx.wait_for(Option::is_none).await;
    }

    /// Send one agent RPC request to the daemon and wait for its answer.
    /// With no daemon, wait up to the queue timeout for one.
    async fn forward(
        &self,
        ctx: &RequestContext,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let mut rx = self.link.subscribe();
        // Only identified HTTP calls are safe to replay after losing an answer.
        // Older callers and other methods retain the fail-on-disconnect behavior.
        let replay = method == "http" && params["request_id"].is_string();
        let mut deadline = None;
        loop {
            if deadline.is_some_and(|until| tokio::time::Instant::now() >= until) {
                return Err(unavailable(
                    "the agent RPC tunnel is down: restart queue expired",
                ));
            }
            let ready = rx.borrow().clone().filter(|l| !l.held);
            let link = if ready.is_some() {
                ready
            } else {
                let until = *deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + self.queue_timeout);
                match tokio::time::timeout_at(
                    until,
                    rx.wait_for(|l| l.as_ref().is_some_and(|l| !l.held)),
                )
                .await
                {
                    Ok(Ok(l)) => l.clone(),
                    _ => None,
                }
            };
            let Some(link) = link else {
                return Err(unavailable(format!(
                    "the agent RPC tunnel is down: no daemon connected to the runner within {} ms; \
                 try again later",
                    self.queue_timeout.as_millis()
                )));
            };
            let id = self.next_rpc.fetch_add(1, Ordering::Relaxed);
            let (tx, answer) = oneshot::channel();
            lock(&self.pending).insert(id, (link.epoch, tx));
            let frame = ServerFrame::RpcRequest {
                id,
                method: method.to_owned(),
                params: params.clone(),
                context: ctx.clone(),
            };
            if link.out.send(frame).await.is_err() {
                lock(&self.pending).remove(&id);
                self.release(link.epoch);
                if replay {
                    deadline
                        .get_or_insert_with(|| tokio::time::Instant::now() + self.queue_timeout);
                    continue;
                }
                return Err(unavailable("the daemon disconnected"));
            }
            let outcome = tokio::select! {
                answer = answer => answer.unwrap_or_else(|_| Err(RpcError::new("tunnel_lost", "the daemon disconnected"))),
                _ = rx.wait_for(|l| l.as_ref().is_none_or(|l| l.epoch != link.epoch)) => Err(RpcError::new("tunnel_lost", "the daemon disconnected")),
            };
            lock(&self.pending).remove(&id);
            if outcome.as_ref().is_err_and(|e| e.code == "tunnel_lost") {
                if !replay {
                    return Err(unavailable("the daemon disconnected"));
                }
                // The queue budget starts when the connection is lost, not when a
                // potentially long-running command was first sent.
                deadline.get_or_insert_with(|| tokio::time::Instant::now() + self.queue_timeout);
                continue;
            }
            return outcome;
        }
    }
}

/// The `RequestHandler` of a runner process: it sends each request to the
/// daemon over the control socket.
pub(crate) struct ForwardHandler {
    pub(crate) hub: Arc<Hub>,
}

impl RequestHandler for ForwardHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>> {
        Box::pin(self.hub.forward(ctx, method, params))
    }
}

/// What every control connection shares.
pub(crate) struct ServerCtx {
    pub(crate) runner: Runner,
    pub(crate) hub: Arc<Hub>,
    pub(crate) harness: HarnessId,
    /// Set by the `set_stopped_by` call; goes into `runner-exit.json`.
    pub(crate) stopped_by: std::sync::Mutex<Option<crate::process::StoppedBy>>,
}

impl ServerCtx {
    pub(crate) fn stopped_by(&self) -> Option<crate::process::StoppedBy> {
        self.stopped_by
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn hello(&self) -> Hello {
        let r = &self.runner;
        Hello {
            protocol: PROTOCOL_VERSION,
            version: terrarium_version::FULL.to_owned(),
            agent: r.agent(),
            harness: self.harness,
            runner_pid: std::process::id(),
            harness_pid: r.harness_pid(),
            status: r.status(),
            session: r.session(),
            seq: r.last_seq(),
            exit: r.exit_status(),
            hook_socket: r.socket_path().to_path_buf(),
            events_path: r.events_path().to_path_buf(),
        }
    }
}

pub(crate) async fn serve(listener: UnixListener, ctx: Arc<ServerCtx>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(connection(stream, ctx.clone()));
            }
            Err(e) => {
                tracing::warn!("control socket accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn connection(stream: UnixStream, ctx: Arc<ServerCtx>) {
    let (read, mut write) = stream.into_split();
    let (out, mut out_rx) = mpsc::channel::<ServerFrame>(OUT_CAPACITY);
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let mut line = match serde_json::to_vec(&frame) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("serialise a control frame: {e}");
                    continue;
                }
            };
            line.push(b'\n');
            if write.write_all(&line).await.is_err() {
                break;
            }
        }
    });
    let mut conn = Conn {
        ctx,
        out,
        tasks: JoinSet::new(),
        streams: HashMap::new(),
        claim: None,
    };
    let mut lines = BufReader::new(read).lines();
    loop {
        let close = conn.claim.as_ref().map(|c| c.1.clone());
        tokio::select! {
            _ = async {
                match &close {
                    Some(n) => n.notified().await,
                    None => std::future::pending().await,
                }
            } => break,
            Some(_) = conn.tasks.join_next(), if !conn.tasks.is_empty() => {}
            line = lines.next_line() => match line {
                Ok(Some(line)) => conn.on_line(&line).await,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("control connection: {e}");
                    break;
                }
            },
        }
    }
    conn.tasks.abort_all();
    writer.abort();
    if let Some((epoch, _)) = conn.claim {
        tracing::info!("the daemon disconnected");
        conn.ctx.hub.release(epoch);
    }
}

struct Conn {
    ctx: Arc<ServerCtx>,
    out: Out,
    tasks: JoinSet<()>,
    streams: HashMap<u64, AbortHandle>,
    claim: Option<(u64, Arc<Notify>)>,
}

fn params<T: DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| RpcError::new(codes::INVALID_PARAMS, e.to_string()))
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::new(codes::FAILED, e.to_string()))
}

fn runner_err(e: RunnerError) -> RpcError {
    match e {
        RunnerError::Exited => RpcError::new(codes::EXITED, e.to_string()),
        e => RpcError::new(codes::FAILED, e.to_string()),
    }
}

impl Conn {
    async fn send(&self, frame: ServerFrame) {
        let _ = self.out.send(frame).await;
    }

    async fn on_line(&mut self, line: &str) {
        let text = line.trim();
        if text.is_empty() {
            return;
        }
        match serde_json::from_str::<ClientFrame>(text) {
            Ok(ClientFrame::Hold) => {
                if let Some((epoch, _)) = &self.claim {
                    self.ctx.hub.hold(*epoch);
                }
            }
            Ok(ClientFrame::Call { id, method, params }) => {
                if let Some(outcome) = self.call(id, &method, params).await {
                    self.send(ServerFrame::reply(id, outcome)).await;
                }
            }
            Ok(ClientFrame::RpcResponse { id, result, error }) => {
                let outcome = match (result, error) {
                    (_, Some(e)) => Err(e),
                    (Some(v), None) => Ok(v),
                    (None, None) => Ok(Value::Null),
                };
                self.ctx.hub.resolve(id, outcome);
            }
            Err(e) => {
                self.send(ServerFrame::reply(
                    0,
                    Err(RpcError::parse_error(e.to_string())),
                ))
                .await;
            }
        }
    }

    /// Run one call. `None`: the reply comes later, from a task.
    async fn call(&mut self, id: u64, method: &str, p: Value) -> Option<Result<Value, RpcError>> {
        if self.claim.is_none() && method != "hello" {
            return Some(Err(RpcError::new(
                codes::HELLO_REQUIRED,
                "send `hello` first",
            )));
        }
        let runner = self.ctx.runner.clone();
        let out = self.out.clone();
        Some(match method {
            "hello" => match params::<HelloParams>(p) {
                Ok(h) => {
                    if self.claim.is_none() {
                        tracing::info!(client = %h.client, protocol = h.protocol, "daemon connected");
                        self.claim = Some(self.ctx.hub.claim(self.out.clone()));
                    }
                    to_value(&self.ctx.hello())
                }
                Err(e) => Err(e),
            },
            "list_pending" => to_value(&runner.list_pending()),
            "cancel_pending" => match p.get("delivery").and_then(Value::as_u64) {
                Some(delivery) => Ok(json!(runner.cancel_pending(delivery))),
                None => Err(RpcError::new("invalid_params", "delivery must be an unsigned integer")),
            },
            "status" => Ok(json!({ "status": runner.status() })),
            "screen" => to_value(&WireSnapshot::from(&runner.screen())),
            "write" => match params::<WriteParams>(p)
                .and_then(|w| decode(&w.data).map(|bytes| (bytes, w.person)))
            {
                Ok((bytes, person)) => if person {
                    runner.write_person(&bytes).await
                } else {
                    runner.write(&bytes).await
                }
                .map(|()| Value::Null)
                .map_err(runner_err),
                Err(e) => Err(e),
            },
            "resize" => match params::<ResizeParams>(p) {
                Ok(r) => runner
                    .resize(r.rows, r.cols)
                    .map(|()| Value::Null)
                    .map_err(runner_err),
                Err(e) => Err(e),
            },
            "claim_idle_stop" => Ok(json!({ "claimed": runner.claim_idle_stop() })),
            "release_idle_stop" => {
                runner.release_idle_stop();
                Ok(Value::Null)
            }
            "type_turn" => match params::<TypeTurnParams>(p) {
                Ok(t) => {
                    let delivery = runner.type_turn(t.text);
                    let d = delivery.id();
                    // The reply goes out before the outcome: both use `out`.
                    self.send(ServerFrame::reply(id, Ok(json!({ "delivery": d }))))
                        .await;
                    self.tasks.spawn(async move {
                        let frame = match delivery.wait().await {
                            Ok(()) => ServerFrame::Delivery {
                                delivery: d,
                                ok: true,
                                error: None,
                            },
                            Err(e) => ServerFrame::Delivery {
                                delivery: d,
                                ok: false,
                                error: Some(WireDeliveryError::from(&e)),
                            },
                        };
                        let _ = out.send(frame).await;
                    });
                    return None;
                }
                Err(e) => Err(e),
            },
            "answer" => match params::<AnswerParams>(p) {
                Ok(a) => runner
                    .answer(&a.request_id, &a.answer)
                    .await
                    .map(|()| Value::Null)
                    .map_err(runner_err),
                Err(e) => Err(e),
            },
            "answer_keys" => match params::<AnswerKeysParams>(p) {
                Ok(a) => runner
                    .answer_keys(&a.keys)
                    .await
                    .map(|()| Value::Null)
                    .map_err(runner_err),
                Err(e) => Err(e),
            },
            "interrupt" => runner
                .interrupt()
                .await
                .map(|sent| json!({ "sent": sent }))
                .map_err(runner_err),
            "kill" => runner.kill().map(|()| Value::Null).map_err(runner_err),
            "set_stopped_by" => match params::<StoppedByParams>(p) {
                Ok(b) => {
                    *self
                        .ctx
                        .stopped_by
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(b.stopped_by);
                    Ok(Value::Null)
                }
                Err(e) => Err(e),
            },
            "wait" => {
                self.tasks.spawn(async move {
                    let status = runner.wait().await;
                    let _ = out.send(ServerFrame::reply(id, to_value(&status))).await;
                });
                return None;
            }
            "watch_status" => {
                let h = self.tasks.spawn(status_stream(runner, out, id));
                self.streams.insert(id, h);
                return None;
            }
            "events" => match params::<EventsParams>(p) {
                Ok(e) => {
                    let h = self.tasks.spawn(events_stream(runner, out, id, e.since));
                    self.streams.insert(id, h);
                    return None;
                }
                Err(e) => Err(e),
            },
            "attach" => {
                let h = self.tasks.spawn(attach_stream(runner, out, id));
                self.streams.insert(id, h);
                return None;
            }
            "cancel" => match params::<CancelParams>(p) {
                Ok(c) => {
                    if let Some(h) = self.streams.remove(&c.call) {
                        h.abort();
                        self.send(ServerFrame::End {
                            id: c.call,
                            error: Some(RpcError::new(codes::CANCELLED, "cancelled")),
                        })
                        .await;
                    }
                    Ok(Value::Null)
                }
                Err(e) => Err(e),
            },
            other => Err(RpcError::method_not_found(other)),
        })
    }
}

async fn item(out: &Out, id: u64, item: Value) -> bool {
    out.send(ServerFrame::Stream { id, item }).await.is_ok()
}

async fn end(out: &Out, id: u64, error: Option<RpcError>) {
    let _ = out.send(ServerFrame::End { id, error }).await;
}

async fn status_stream(runner: Runner, out: Out, id: u64) {
    let mut rx = runner.watch_status();
    let mut last = *rx.borrow_and_update();
    if !item(&out, id, json!({ "status": last })).await {
        return;
    }
    let exited = runner.wait();
    tokio::pin!(exited);
    loop {
        tokio::select! {
            r = rx.changed() => {
                if r.is_err() {
                    break;
                }
                let now = *rx.borrow_and_update();
                if now != last {
                    last = now;
                    if !item(&out, id, json!({ "status": now })).await {
                        return;
                    }
                }
            }
            _ = &mut exited => {
                let now = *rx.borrow_and_update();
                if now != last && !item(&out, id, json!({ "status": now })).await {
                    return;
                }
                break;
            }
        }
    }
    end(&out, id, None).await;
}

/// Send the records of `events.jsonl` after `last`. False: the connection
/// is gone.
async fn replay(path: &Path, last: &mut u64, out: &Out, id: u64) -> bool {
    let p = path.to_path_buf();
    let since = *last;
    let records = match tokio::task::spawn_blocking(move || read_since(&p, since)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!("read {}: {e}", path.display());
            Vec::new()
        }
        Err(e) => {
            tracing::warn!("the events read failed: {e}");
            Vec::new()
        }
    };
    for rec in records {
        let seq = rec.seq.unwrap_or(0);
        if seq <= *last {
            continue;
        }
        *last = seq;
        match serde_json::to_value(&rec) {
            Ok(v) => {
                if !item(out, id, v).await {
                    return false;
                }
            }
            Err(e) => tracing::warn!("serialise an event: {e}"),
        }
    }
    true
}

async fn events_stream(runner: Runner, out: Out, id: u64, since: Option<u64>) {
    let path = runner.events_path().to_path_buf();
    // Take `last` before the subscribe, and replay the file after it: a
    // record that comes in between is in the file, and the receiver skips
    // the doubles by seq.
    let mut last = since.unwrap_or_else(|| runner.last_seq());
    let mut rx = runner.subscribe_events();
    if !replay(&path, &mut last, &out, id).await {
        return;
    }
    let exited = runner.wait();
    tokio::pin!(exited);
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(rec) => {
                    let seq = rec.seq.unwrap_or(0);
                    if seq <= last {
                        continue;
                    }
                    last = seq;
                    let Ok(v) = serde_json::to_value(&rec) else { continue };
                    if !item(&out, id, v).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    if !replay(&path, &mut last, &out, id).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            _ = &mut exited => {
                // The exit event and the last status change are in the
                // file before `wait` returns.
                if !replay(&path, &mut last, &out, id).await {
                    return;
                }
                break;
            }
        }
    }
    end(&out, id, None).await;
}

async fn attach_stream(runner: Runner, out: Out, id: u64) {
    let (snapshot, mut rx) = runner.attach();
    if !item(
        &out,
        id,
        json!({ "snapshot": WireSnapshot::from(&snapshot) }),
    )
    .await
    {
        return;
    }
    let exited = runner.wait();
    tokio::pin!(exited);
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(chunk) => {
                    if !item(&out, id, json!({ "output": encode(&chunk) })).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    end(&out, id, Some(RpcError::new(
                        codes::LAGGED,
                        format!("the attach client fell {n} chunks behind; attach again"),
                    ))).await;
                    return;
                }
                Err(RecvError::Closed) => break,
            },
            _ = &mut exited => {
                // The reader thread can still hold the last bytes.
                runner.output_drained(OUTPUT_DRAIN).await;
                loop {
                    match rx.try_recv() {
                        Ok(chunk) => {
                            if !item(&out, id, json!({ "output": encode(&chunk) })).await {
                                return;
                            }
                        }
                        Err(TryRecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                break;
            }
        }
    }
    end(&out, id, None).await;
}
