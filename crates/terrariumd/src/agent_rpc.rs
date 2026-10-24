//! The daemon side of the runner socket (`docs/agent-rpc.md`).
//!
//! The runner answers `ping` and `whoami` itself and sends every other
//! request here. Method `http` runs one HTTP request through the daemon's
//! own router, as the agent that owns the socket. The caller cannot choose
//! the actor: it comes from the socket, through [`RequestContext`].

use std::collections::BTreeMap;
use std::sync::Weak;

use axum::body::Body;
use axum::http::{Method, Request, header};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_runner::{BoxFuture, RequestContext, RequestHandler, RpcError};
use tower::ServiceExt;

use crate::actor::Actor;
use crate::api::{self, AppState, NEXT_CURSOR};

/// The largest response body that the tunnel returns.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// The `params` of method `http`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpParams {
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
