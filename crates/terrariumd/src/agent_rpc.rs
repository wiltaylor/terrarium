//! The daemon side of the runner socket (`docs/agent-rpc.md`).
//!
//! The runner answers `ping` and `whoami` itself and sends every other
//! request here. Method `http` runs one HTTP request through the daemon's
//! own router, as the agent that owns the socket. The caller cannot choose
//! the actor: it comes from the socket, through [`RequestContext`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, header};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::{Value, json};
use rusqlite::OptionalExtension;
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
        let mut keys = self.keys.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        keys.retain(|_, v| v.strong_count() != 0);
        let entry = keys.entry(key).or_default();
        if let Some(lock) = entry.upgrade() { return lock; }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        *entry = Arc::downgrade(&lock);
        lock
    }
}

fn retry() -> RpcError {
    RpcError::new("retry", "the daemon is restarting; retry this request with the same request ID")
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
    let drained = state.agent_calls.active.write();
    tokio::pin!(drained);
    if tokio::time::timeout_at(start + Duration::from_secs(30).min(limit), &mut drained).await.is_ok() {
        return Ok(());
    }
    tracing::warn!("deferring daemon restart for admitted agent writes");
    state.db.write("system", |t| t.audit("agent_rpc.drain_deferred", "daemon", None, json!({}))).await?;
    if tokio::time::timeout_at(start + limit, &mut drained).await.is_err() {
        // The NULL entries remain durable. The next daemon refuses to execute
        // them, even if their side effect happened before this process exited.
        state.db.write("system", |t| {
            let rows: Vec<(String, String)> = t.prepare("SELECT agent, request_id FROM agent_rpc_answers WHERE answer IS NULL")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            for (agent, id) in rows {
                tracing::warn!(agent, request_id = id, "agent write indeterminate at drain cap");
                t.audit("agent_rpc.indeterminate", &id, None, json!({"agent": agent, "reason": "drain_cap"}))?;
            }
            Ok(())
        }).await?;
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
        if *draining.borrow() { return Err(retry()); }
        let p: HttpParams = serde_json::from_value(params.clone())
            .map_err(|e| bad_params(format!("bad `http` params: {e}")))?;
        if p.method.eq_ignore_ascii_case("GET") || p.method.eq_ignore_ascii_case("HEAD") {
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
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&params).map_err(|e| bad_params(e.to_string()))?));
        let key = agent.clone();
        let request_id = id.clone();
        let project = ctx.project.clone();
        let cached = state.db.write(format!("agent:{agent}"), move |t| {
            let old: Option<(String, Option<String>)> = t.query_row(
                "SELECT fingerprint, answer FROM agent_rpc_answers WHERE agent = ?1 AND request_id = ?2",
                rusqlite::params![key, request_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if let Some((hash, answer)) = old {
                if hash != fingerprint {
                    return Ok(Some(Err(bad_params("request_id was already used for a different request"))));
                }
                if let Some(answer) = answer {
                    return Ok(Some(serde_json::from_str::<Result<Value, RpcError>>(&answer)?));
                }
                tracing::warn!(agent = key, request_id, "agent write has an indeterminate outcome");
                t.audit("agent_rpc.indeterminate", &request_id, Some(&project), json!({"agent": key, "reason": "unresolved_journal"}))?;
                return Ok(Some(Err(RpcError::new("indeterminate", "the previous daemon did not record this write's answer; it will not be executed again"))));
            }
            t.execute("INSERT INTO agent_rpc_answers(agent, request_id, fingerprint) VALUES (?1, ?2, ?3)", rusqlite::params![key, request_id, fingerprint])?;
            Ok(None)
        }).await.map_err(internal)?;
        if let Some(answer) = cached { return answer; }
        let answer = self.execute_http(ctx, params).await;
        let stored = serde_json::to_string(&answer).map_err(|e| RpcError::new("internal", e.to_string()))?;
        state.db.write(format!("agent:{agent}"), move |t| {
            t.execute("UPDATE agent_rpc_answers SET answer = ?3 WHERE agent = ?1 AND request_id = ?2", rusqlite::params![agent, id, stored])?;
            Ok(())
        }).await.map_err(internal)?;
        answer
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

#[cfg(test)]
mod tests {
    use super::*;

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
