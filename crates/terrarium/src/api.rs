//! The API client the commands use: it sends over any [`Transport`], maps
//! errors to exit codes, and follows `X-Next-Cursor`.

use std::sync::Arc;

use serde_json::Value;

use crate::transport::{ApiRequest, ApiResponse, Method, Transport};

/// The exit codes of the CLI.
pub mod exit {
    pub const OK: u8 = 0;
    /// The daemon answered with an error, or the CLI cannot reach it.
    pub const SERVER: u8 = 1;
    /// A bad command line, or a command that is not available yet.
    pub const USAGE: u8 = 2;
    /// The daemon refused the action: `401` or `403`.
    pub const REFUSED: u8 = 3;
}

/// An error answer from the daemon (`docs/api.md` §2.5).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ServerError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
    /// The raw body, for `--json`.
    pub body: Value,
}

impl ServerError {
    pub fn from_response(resp: &ApiResponse) -> Self {
        let err = &resp.body["error"];
        let code = err["code"]
            .as_str()
            .map_or_else(|| default_code(resp.status).to_owned(), str::to_owned);
        let message = match err["message"].as_str() {
            Some(m) => m.to_owned(),
            None => match &resp.body {
                Value::String(s) if !s.trim().is_empty() => s.trim().to_owned(),
                Value::Null => format!("the daemon answered {}", resp.status),
                other => other.to_string(),
            },
        };
        Self {
            status: resp.status,
            code,
            message,
            details: err.get("details").cloned(),
            body: resp.body.clone(),
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.status {
            401 | 403 => exit::REFUSED,
            _ => exit::SERVER,
        }
    }

    /// The route does not exist in this daemon (as opposed to a missing
    /// object behind a route that exists).
    pub fn is_missing_route(&self) -> bool {
        (self.status == 404 && self.message == "no such API route") || self.status == 405
    }
}

fn default_code(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        409 => "conflict",
        422 => "invalid",
        _ => "internal",
    }
}

/// An error the CLI makes itself, with its exit code.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A bad command line: a missing project, a bad flag value.
    #[error("{0}")]
    Usage(String),
    /// A command that this build or this daemon does not have yet.
    #[error("{0}")]
    NotAvailable(String),
    /// `terrarium exec` passes on the exit code of the remote command.
    #[error("the remote command exited with {0}")]
    RemoteExit(u8),
    /// `test push --fail-on-failures`: the run has failed tests (exit 1).
    #[error("{0} tests failed or errored")]
    TestsFailed(i64),
}

impl CliError {
    pub fn usage(msg: impl Into<String>) -> anyhow::Error {
        Self::Usage(msg.into()).into()
    }

    pub fn not_available(msg: impl Into<String>) -> anyhow::Error {
        Self::NotAvailable(msg.into()).into()
    }
}

/// The exit code for an error from a command.
pub fn exit_code_of(err: &anyhow::Error) -> u8 {
    if let Some(e) = err.downcast_ref::<ServerError>() {
        return e.exit_code();
    }
    match err.downcast_ref::<CliError>() {
        Some(CliError::RemoteExit(code)) => return *code,
        Some(CliError::TestsFailed(_)) => return exit::SERVER,
        Some(_) => return exit::USAGE,
        None => {}
    }
    exit::SERVER
}

/// A page of a list.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub items: Vec<Value>,
    pub next: Option<String>,
}

#[derive(Clone)]
pub struct Api {
    transport: Arc<dyn Transport>,
}

/// Build a query from pairs, and drop the `None` values.
pub fn query<const N: usize>(pairs: [(&str, Option<String>); N]) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .filter_map(|(k, v)| Some((k.to_owned(), v?)))
        .collect()
}

impl Api {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }

    pub fn describe(&self) -> String {
        self.transport.describe()
    }

    /// Send a request. A non-2xx answer is a [`ServerError`].
    pub async fn send(&self, req: ApiRequest) -> anyhow::Result<ApiResponse> {
        let resp = self.transport.send(req).await?;
        if !(200..300).contains(&resp.status) {
            return Err(ServerError::from_response(&resp).into());
        }
        Ok(resp)
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        query: Vec<(String, String)>,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let req = ApiRequest {
            method,
            path: path.to_owned(),
            query,
            body,
        };
        Ok(self.send(req).await?.body)
    }

    pub async fn get(&self, path: &str, query: Vec<(String, String)>) -> anyhow::Result<Value> {
        self.call(Method::Get, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        self.call(Method::Post, path, Vec::new(), body).await
    }

    pub async fn put(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(Method::Put, path, Vec::new(), Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(Method::Patch, path, Vec::new(), Some(body)).await
    }

    pub async fn delete(&self, path: &str, query: Vec<(String, String)>) -> anyhow::Result<Value> {
        self.call(Method::Delete, path, query, None).await
    }

    /// One page of a list.
    pub async fn page(&self, path: &str, query: Vec<(String, String)>) -> anyhow::Result<Page> {
        let resp = self
            .send(ApiRequest {
                method: Method::Get,
                path: path.to_owned(),
                query,
                body: None,
            })
            .await?;
        let next = resp
            .header("x-next-cursor")
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        let items = match resp.body {
            Value::Array(items) => items,
            Value::Null => Vec::new(),
            other => vec![other],
        };
        Ok(Page { items, next })
    }

    /// A whole list: follow `X-Next-Cursor` until the end, or until `max`
    /// items.
    pub async fn list(
        &self,
        path: &str,
        query: Vec<(String, String)>,
        max: Option<usize>,
    ) -> anyhow::Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut q = query.clone();
            if let Some(c) = &cursor {
                q.push(("cursor".into(), c.clone()));
            }
            if let Some(max) = max
                && !q.iter().any(|(k, _)| k == "limit")
            {
                q.push(("limit".into(), (max - out.len()).clamp(1, 1000).to_string()));
            }
            let page = self.page(path, q).await?;
            out.extend(page.items);
            if let Some(max) = max
                && out.len() >= max
            {
                out.truncate(max);
                return Ok(out);
            }
            match page.next {
                Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
                _ => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::transport::mock::MockTransport;
    use serde_json::json;

    #[tokio::test]
    async fn follows_cursors() {
        let m = MockTransport::new();
        m.reply_with(200, &[("x-next-cursor", "c1")], json!([{"id": 1}]))
            .reply(200, json!([{"id": 2}]));
        let api = Api::new(Arc::new(m.clone()));
        let all = api.list("/api/agents", vec![], None).await.unwrap();
        assert_eq!(all, vec![json!({"id": 1}), json!({"id": 2})]);
        let reqs = m.requests();
        assert_eq!(reqs[0].query_value("cursor"), None);
        assert_eq!(reqs[1].query_value("cursor"), Some("c1"));
    }

    #[tokio::test]
    async fn stops_at_max() {
        let m = MockTransport::new();
        m.reply_with(200, &[("x-next-cursor", "c1")], json!([1, 2, 3]));
        let api = Api::new(Arc::new(m.clone()));
        let all = api.list("/api/audit", vec![], Some(2)).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(m.requests()[0].query_value("limit"), Some("2"));
    }

    #[tokio::test]
    async fn maps_errors() {
        let m = MockTransport::new();
        m.reply(
            403,
            json!({"error": {"code": "forbidden", "message": "needs project_admin"}}),
        )
        .reply(
            404,
            json!({"error": {"code": "not_found", "message": "no such API route"}}),
        )
        .reply(500, json!("boom"));
        let api = Api::new(Arc::new(m));
        let e = api.get("/api/x", vec![]).await.unwrap_err();
        let s = e.downcast_ref::<ServerError>().unwrap();
        assert_eq!(
            (s.code.as_str(), s.message.as_str()),
            ("forbidden", "needs project_admin")
        );
        assert_eq!(exit_code_of(&e), exit::REFUSED);
        let e = api.get("/api/x", vec![]).await.unwrap_err();
        assert!(e.downcast_ref::<ServerError>().unwrap().is_missing_route());
        assert_eq!(exit_code_of(&e), exit::SERVER);
        let e = api.get("/api/x", vec![]).await.unwrap_err();
        assert_eq!(e.to_string(), "boom");
        assert_eq!(exit_code_of(&CliError::usage("x")), exit::USAGE);
    }
}
