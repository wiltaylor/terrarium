//! The daemon side of the runner socket (`docs/agent-rpc.md`).
//!
//! The runner answers `ping` itself and sends every other
//! request here. Method `http` runs one HTTP request through the daemon's
//! own router, as the agent that owns the socket. The caller cannot choose
//! the actor: it comes from the socket, through [`RequestContext`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, header};
use http_body_util::BodyExt;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use terrarium_runner::{BoxFuture, RequestContext, RequestHandler, RpcError};
use tower::ServiceExt;

use crate::actor::Actor;
use crate::api::{self, AppState, NEXT_CURSOR};

/// The largest response body that the tunnel returns.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Admission and draining are shared by all runner connections of one daemon.
pub struct CallGate {
    draining: tokio::sync::watch::Sender<bool>,
    active: tokio::sync::RwLock<()>,
    keys: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl Default for CallGate {
    fn default() -> Self {
        Self {
            draining: tokio::sync::watch::Sender::new(false),
            active: Default::default(),
            keys: Default::default(),
        }
    }
}

impl CallGate {
    fn key(&self, key: String) -> Arc<tokio::sync::Mutex<()>> {
        let mut keys = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        keys.retain(|_, v| v.strong_count() != 0);
        let entry = keys.entry(key).or_default();
        if let Some(lock) = entry.upgrade() {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        *entry = Arc::downgrade(&lock);
        lock
    }
}

fn retry() -> RpcError {
    RpcError::new(
        "retry",
        "the daemon is restarting; retry this request with the same request ID",
    )
}

fn internal(e: crate::error::ApiError) -> RpcError {
    RpcError::new("internal", e.message)
}

/// Stop admission before holding runners, cancel reads/long polls, then wait
/// for admitted writes and their journal answers. Never cancel a write merely
/// because the normal 30-second drain interval has elapsed.
pub async fn drain(state: &Arc<AppState>, limit: Duration) -> crate::error::ApiResult<()> {
    let start = tokio::time::Instant::now();
    state.agent_calls.draining.send_replace(true);
    state.supervisor.hold_agent_calls().await;
    drain_active(&state.agent_calls, &state.db, start, limit).await
}

async fn drain_active(
    gate: &CallGate,
    db: &crate::db::Db,
    start: tokio::time::Instant,
    limit: Duration,
) -> crate::error::ApiResult<()> {
    let drained = gate.active.write();
    tokio::pin!(drained);
    if tokio::time::timeout_at(start + Duration::from_secs(30).min(limit), &mut drained)
        .await
        .is_ok()
    {
        return Ok(());
    }
    tracing::warn!("deferring daemon restart for admitted agent writes");
    db.write("system", |t| {
        t.audit("agent_rpc.drain_deferred", "daemon", None, json!({}))
    })
    .await?;
    if tokio::time::timeout_at(start + limit, &mut drained)
        .await
        .is_err()
    {
        // The NULL entries remain durable. The next daemon refuses to execute
        // them, even if their side effect happened before this process exited.
        db.write("system", |t| {
            let rows: Vec<(String, String)> = t
                .prepare("SELECT agent, request_id FROM agent_rpc_answers WHERE answer IS NULL")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (agent, id) in rows {
                tracing::warn!(
                    agent,
                    request_id = id,
                    "agent write indeterminate at drain cap"
                );
                t.audit(
                    "agent_rpc.indeterminate",
                    &id,
                    None,
                    json!({"agent": agent, "reason": "drain_cap"}),
                )?;
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// The `params` of method `http`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpParams {
    #[serde(default)]
    request_id: Option<String>,
    method: String,
    path: String,
    #[serde(default)]
    query: BTreeMap<String, Value>,
    #[serde(default)]
    body: Option<Value>,
}

impl HttpParams {
    fn is_read(&self) -> bool {
        self.method.eq_ignore_ascii_case("GET")
            || self.method.eq_ignore_ascii_case("HEAD")
            // These POST routes are audited reads, not effects to deduplicate.
            // Their no-store answers contain secrets and must never enter the
            // answer journal (including the harness launch credentials).
            || (self.method.eq_ignore_ascii_case("POST")
                && (self.path == "/api/agent/secrets/env"
                    || (self.path.starts_with("/api/agent/secrets/") && self.path.ends_with("/get"))
                    || (self.path.starts_with("/api/secrets/") && self.path.ends_with("/reveal"))))
    }
}

/// Answers the agent CLI on one runner socket.
pub struct AgentRpc {
    state: Weak<AppState>,
}

impl AgentRpc {
    pub fn new(state: Weak<AppState>) -> Self {
        Self { state }
    }
}

impl RequestHandler for AgentRpc {
    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>> {
        Box::pin(async move {
            match method {
                "whoami" => {
                    let state = self.state.upgrade().ok_or_else(retry)?;
                    let key = ctx.agent.to_string();
                    let agent = state
                        .db
                        .read(move |c| crate::store::agents::get(c, &key))
                        .await
                        .map_err(|e| RpcError::new("internal", e.message))?;
                    Ok(json!({
                        "agent": ctx.agent,
                        "name": agent.agent.name,
                        "project": agent.agent.project,
                        "harness": ctx.harness,
                        "roles": agent.agent.roles,
                        "status": ctx.status,
                    }))
                }
                "http" => self.http(ctx, params).await,
                "upload" | "upload.chunk" | "upload.finish" => Err(RpcError::new(
                    "not_implemented",
                    format!("method `{method}` is not in this daemon yet"),
                )),
                _ => Err(RpcError::method_not_found(method)),
            }
        })
    }
}

fn bad_params(m: impl Into<String>) -> RpcError {
    RpcError::new("invalid_params", m)
}

/// `a=1&b=x%20y`, with each value as text.
fn encode_query(query: &BTreeMap<String, Value>) -> String {
    query
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| {
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{}={}", percent_encode(k), percent_encode(&v))
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl AgentRpc {
    async fn http(&self, ctx: &RequestContext, params: Value) -> Result<Value, RpcError> {
        let state = self.state.upgrade().ok_or_else(retry)?;
        let mut draining = state.agent_calls.draining.subscribe();
        let _admitted = state.agent_calls.active.read().await;
        if *draining.borrow() {
            return Err(retry());
        }
        let p: HttpParams = serde_json::from_value(params.clone())
            .map_err(|e| bad_params(format!("bad `http` params: {e}")))?;
        if p.is_read() {
            return tokio::select! {
                result = self.execute_http(ctx, params) => result,
                _ = draining.wait_for(|v| *v) => Err(retry()),
            };
        }
        let Some(id) = p.request_id else {
            // Legacy clients cannot replay, but are still included in the drain.
            return self.execute_http(ctx, params).await;
        };
        if id.is_empty() || id.len() > 128 {
            return Err(bad_params("request_id must contain 1..128 bytes"));
        }
        let agent = ctx.agent.to_string();
        let lock = state.agent_calls.key(format!("{agent}:{id}"));
        let _unique = lock.lock().await;
        journaled(
            &state.db,
            ctx,
            id,
            &params,
            self.execute_http(ctx, params.clone()),
        )
        .await
    }

    async fn execute_http(&self, ctx: &RequestContext, params: Value) -> Result<Value, RpcError> {
        let p: HttpParams = serde_json::from_value(params)
            .map_err(|e| bad_params(format!("bad `http` params: {e}")))?;
        if !p.path.starts_with("/api/") {
            return Err(bad_params("`path` must start with /api/"));
        }
        if p.path.contains('?') || p.path.contains('#') {
            return Err(bad_params("put the query in `query`, not in `path`"));
        }
        let method = Method::from_bytes(p.method.to_ascii_uppercase().as_bytes())
            .map_err(|_| bad_params(format!("bad HTTP method `{}`", p.method)))?;
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| RpcError::new("unavailable", "the daemon is shutting down"))?;

        let key = ctx.agent.to_string();
        let actor = state
            .db
            .read(move |c| crate::store::agents::actor_for(c, &key))
            .await
            .map_err(|e| RpcError::new("internal", e.message))?
            .ok_or_else(|| RpcError::new("unauthorized", "this agent no longer exists"))?;

        let mut uri = p.path.clone();
        let q = encode_query(&p.query);
        if !q.is_empty() {
            uri.push('?');
            uri.push_str(&q);
        }
        let mut req = Request::builder().method(method).uri(uri);
        let body = match &p.body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let mut req = req
            .body(body)
            .map_err(|e| bad_params(format!("bad request: {e}")))?;
        req.extensions_mut().insert(Actor::Agent(actor));

        let resp = api::router(state)
            .oneshot(req)
            .await
            .map_err(|e| RpcError::new("internal", e.to_string()))?;
        let status = resp.status().as_u16();
        let mut headers = serde_json::Map::new();
        if let Some(v) = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
        {
            headers.insert("cache-control".into(), Value::String(v.to_owned()));
        }
        if let Some(v) = resp
            .headers()
            .get(NEXT_CURSOR)
            .and_then(|v| v.to_str().ok())
        {
            headers.insert(NEXT_CURSOR.into(), Value::String(v.to_owned()));
        }
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| RpcError::new("internal", e.to_string()))?
            .to_bytes();
        if bytes.len() > MAX_BODY {
            return Err(RpcError::new(
                "too_large",
                format!(
                    "the response has {} bytes; the tunnel takes {MAX_BODY}",
                    bytes.len()
                ),
            ));
        }
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        Ok(json!({"status": status, "headers": headers, "body": body}))
    }
}

/// Persist admission before polling the effect, and its answer before replying.
/// The caller serializes concurrent requests with the same agent and ID.
async fn journaled(
    db: &crate::db::Db,
    ctx: &RequestContext,
    id: String,
    params: &Value,
    effect: impl std::future::Future<Output = Result<Value, RpcError>>,
) -> Result<Value, RpcError> {
    let agent = ctx.agent.to_string();
    let fingerprint: String =
        Sha256::digest(serde_json::to_vec(params).map_err(|e| bad_params(e.to_string()))?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
    let key = agent.clone();
    let request_id = id.clone();
    let project = ctx.project.clone();
    let method = params["method"].as_str().unwrap_or_default().to_owned();
    let cached = db.write(format!("agent:{agent}"), move |t| {
            let old: Option<(String, Option<String>)> = t.query_row(
                "SELECT fingerprint, answer FROM agent_rpc_answers WHERE agent = ?1 AND request_id = ?2",
                rusqlite::params![key, request_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if let Some((hash, answer)) = old {
                if hash != fingerprint {
                    return Ok(Some(Err(bad_params("request_id was already used for a different request"))));
                }
                if let Some(answer) = answer {
                    let answer = serde_json::from_str::<Result<Value, RpcError>>(&answer)?;
                    if answer.as_ref().is_err_and(|e| e.code == "indeterminate") {
                        tracing::warn!(agent = key, request_id, "agent write answer cannot be replayed");
                        t.audit("agent_rpc.indeterminate", &request_id, Some(&project), json!({"agent": key, "reason": "no_store_answer"}))?;
                    }
                    return Ok(Some(answer));
                }
                tracing::warn!(agent = key, request_id, "agent write has an indeterminate outcome");
                t.audit("agent_rpc.indeterminate", &request_id, Some(&project), json!({"agent": key, "reason": "unresolved_journal"}))?;
                return Ok(Some(Err(RpcError::new("indeterminate", format!("request {request_id} ({method}): the call may or may not have happened; check before you retry")))));
            }
            t.execute("INSERT INTO agent_rpc_answers(agent, request_id, fingerprint) VALUES (?1, ?2, ?3)", rusqlite::params![key, request_id, fingerprint])?;
            Ok(None)
        }).await.map_err(internal)?;
    if let Some(answer) = cached {
        return answer;
    }
    let answer = effect.await;
    // Respect no-store even for a future write endpoint. Its effect is still
    // deduplicated, but a lost sensitive answer cannot be recovered by storing
    // plaintext credentials in the ordinary database.
    let no_store = answer.as_ref().is_ok_and(|v| {
        v["headers"]["cache-control"].as_str().is_some_and(|h| {
            h.split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("no-store"))
        })
    });
    let nonreplayable = Err(RpcError::new(
        "indeterminate",
        format!(
            "request {id} ({}): the answer cannot be stored; the call may or may not have happened; check before you retry",
            params["method"].as_str().unwrap_or_default()
        ),
    ));
    let stored = serde_json::to_string(if no_store { &nonreplayable } else { &answer })
        .map_err(|e| RpcError::new("internal", e.to_string()))?;
    db.write(format!("agent:{agent}"), move |t| {
        t.execute(
            "UPDATE agent_rpc_answers SET answer = ?3 WHERE agent = ?1 AND request_id = ?2",
            rusqlite::params![agent, id, stored],
        )?;
        Ok(())
    })
    .await
    .map_err(internal)?;
    answer
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn secret_fetch_posts_are_uncached_reads_but_secret_writes_are_not() {
        for (method, path, read) in [
            ("POST", "/api/agent/secrets/env", true),
            ("POST", "/api/agent/secrets/token/get", true),
            ("POST", "/api/secrets/demo/token/reveal", true),
            ("POST", "/api/secrets", false),
            ("PUT", "/api/secrets/demo/token", false),
            ("POST", "/api/secrets/demo/token/grants", false),
            ("GET", "/api/mail", true),
            ("POST", "/api/mail", false),
        ] {
            let p: HttpParams =
                serde_json::from_value(json!({"method":method,"path":path})).unwrap();
            assert_eq!(p.is_read(), read, "{method} {path}");
        }
    }

    fn crash_context() -> RequestContext {
        serde_json::from_value(json!({
            "agent":"00000000000000000000000001", "name":"tester", "project":"demo",
            "harness":"claude", "status":"idle"
        }))
        .unwrap()
    }

    fn crash_params() -> Value {
        json!({"method":"POST", "path":"/api/test-effect", "request_id":"crash-boundary"})
    }

    fn crash_db(path: &std::path::Path) -> crate::db::Db {
        let mut conn = rusqlite::Connection::open(path).unwrap();
        crate::db::migrate(&mut conn).unwrap();
        crate::db::Db::new(conn, crate::events::EventHub::default())
    }

    #[tokio::test]
    async fn no_store_write_answers_are_not_persisted_or_reexecuted() {
        let dir = tempfile::tempdir().unwrap();
        let db = crash_db(&dir.path().join("no-store.sqlite"));
        let answer = journaled(&db, &crash_context(), "sensitive".into(), &crash_params(), async {
            Ok(json!({"headers":{"cache-control":"private, no-store"},"body":"sensitive-test-fixture"}))
        }).await.unwrap();
        assert_eq!(answer["body"], "sensitive-test-fixture");
        let stored: String = db
            .read(|c| Ok(c.query_row("SELECT answer FROM agent_rpc_answers", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert!(!stored.contains("sensitive-test-fixture"));
        let replay = journaled(
            &db,
            &crash_context(),
            "sensitive".into(),
            &crash_params(),
            async {
                panic!("a no-store write must not execute twice");
            },
        )
        .await
        .unwrap_err();
        assert_eq!(replay.code, "indeterminate");
    }

    #[tokio::test]
    async fn drain_cap_keeps_started_writes_indeterminate_and_audits_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = crash_db(&dir.path().join("cap.sqlite"));
        let gate = Arc::new(CallGate::default());
        let started = Arc::new(tokio::sync::Notify::new());
        let task = {
            let gate = gate.clone();
            let db = db.clone();
            let started = started.clone();
            tokio::spawn(async move {
                let _admitted = gate.active.read().await;
                journaled(
                    &db,
                    &crash_context(),
                    "cap".into(),
                    &crash_params(),
                    async {
                        started.notify_one();
                        std::future::pending::<Result<Value, RpcError>>().await
                    },
                )
                .await
            })
        };
        started.notified().await;
        tokio::time::timeout(
            Duration::from_secs(2),
            drain_active(
                &gate,
                &db,
                tokio::time::Instant::now(),
                Duration::from_millis(20),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!task.is_finished());
        task.abort();
        let _ = task.await;
        let unfinished: i64 = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM agent_rpc_answers WHERE answer IS NULL",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(unfinished, 1);
        let audits: i64 = db.read(|c| Ok(c.query_row("SELECT count(*) FROM audit WHERE action IN ('agent_rpc.drain_deferred', 'agent_rpc.indeterminate')", [], |r| r.get(0))?)).await.unwrap();
        assert_eq!(audits, 2);
    }

    #[tokio::test]
    async fn drain_waits_until_the_write_answer_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let db = crash_db(&dir.path().join("drain.sqlite"));
        let gate = CallGate::default();
        let admitted = gate.active.read().await;
        let draining = drain_active(
            &gate,
            &db,
            tokio::time::Instant::now(),
            Duration::from_secs(5),
        );
        tokio::pin!(draining);
        tokio::select! {
            r = &mut draining => panic!("drained with an admitted write: {r:?}"),
            _ = tokio::task::yield_now() => {}
        }
        let answer = journaled(
            &db,
            &crash_context(),
            "finished".into(),
            &crash_params(),
            async { Ok(json!("done")) },
        )
        .await
        .unwrap();
        drop(admitted);
        draining.await.unwrap();
        let replay = journaled(
            &db,
            &crash_context(),
            "finished".into(),
            &crash_params(),
            async { panic!("must use durable answer") },
        )
        .await
        .unwrap();
        assert_eq!(replay, answer);
    }

    // Invoked in its own OS process by the test below. There is no production
    // failpoint: the journal polls this effect only after STARTED is durable.
    #[tokio::test]
    async fn journal_crash_child() {
        use std::io::Write;
        let Some(path) = std::env::var_os("TER_260_CRASH_DB") else {
            return;
        };
        let db = crash_db(std::path::Path::new(&path));
        let _ = journaled(&db, &crash_context(), "crash-boundary".into(), &crash_params(), async {
            db.write("test", |t| {
                t.execute_batch("CREATE TABLE crash_effect(value INTEGER); INSERT INTO crash_effect VALUES (1);")?;
                Ok(())
            }).await.unwrap();
            println!("effect durable");
            std::io::stdout().flush().unwrap();
            std::future::pending::<Result<Value, RpcError>>().await
        }).await;
    }

    #[tokio::test]
    async fn sigkill_between_started_and_done_never_repeats_the_effect() {
        use tokio::io::AsyncBufReadExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent_rpc::tests::journal_crash_child",
                "--nocapture",
            ])
            .env("TER_260_CRASH_DB", &path)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("child reached the durable effect");
                if line == "effect durable" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        // Tokio Child::kill sends SIGKILL on Unix and reaps only our child.
        child.kill().await.unwrap();
        let db = crash_db(&path);
        let result = journaled(
            &db,
            &crash_context(),
            "crash-boundary".into(),
            &crash_params(),
            async {
                panic!("an unresolved write must never be executed again");
            },
        )
        .await
        .unwrap_err();
        assert_eq!(result.code, "indeterminate");
        assert!(result.message.contains("crash-boundary (POST)"));
        assert!(result.message.contains("check before you retry"));
        let count: i64 = db
            .read(|c| Ok(c.query_row("SELECT count(*) FROM crash_effect", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 1);
        let audits: i64 = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM audit WHERE action = 'agent_rpc.indeterminate'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(audits, 1);
    }

    #[test]
    fn query_encoding() {
        let mut q = BTreeMap::new();
        q.insert("limit".to_owned(), json!(20));
        q.insert("to".to_owned(), json!("project:ops"));
        q.insert("thread".to_owned(), json!("a b&c"));
        q.insert("skip".to_owned(), Value::Null);
        assert_eq!(
            encode_query(&q),
            "limit=20&thread=a%20b%26c&to=project%3Aops"
        );
    }
}
