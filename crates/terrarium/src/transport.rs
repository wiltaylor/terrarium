//! One way to send an API request, over two transports.
//!
//! - [`HttpTransport`]: HTTP/1.1 over the daemon's Unix socket, or over TCP
//!   with a bearer token. The operator uses it.
//! - [`RunnerTunnelTransport`]: the `http` method of the runner socket
//!   (`docs/agent-rpc.md`). An agent inside a sandbox uses it.
//!
//! Both give the same [`ApiResponse`]. The CLI maps the status to its own
//! errors in `api.rs`. Neither transport checks policy: the daemon does.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub use crate::client::{Endpoint, HttpTransport};

/// A boxed future, so that `Transport` can be a trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The HTTP methods the CLI uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// One API request. `path` starts with `/api/` and has no query string.
/// Path segments that come from the user are percent-encoded already.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiRequest {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl ApiRequest {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
        }
    }

    /// The path and the encoded query, for an HTTP request line.
    pub fn path_and_query(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        let q: Vec<String> = self
            .query
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect();
        format!("{}?{}", self.path, q.join("&"))
    }

    /// The value of one query key, for tests.
    #[cfg(test)]
    pub fn query_value(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// One API response. `headers` have lower-case names. A body that is not
/// JSON becomes a JSON string. An empty body is `null`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

impl ApiResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Decode a response body.
pub fn decode_body(bytes: &[u8]) -> Value {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Value::Null;
    }
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

/// Send API requests. An `Err` means the transport failed: the daemon did
/// not answer. Every HTTP status, 4xx and 5xx too, is an `Ok`.
pub trait Transport: Send + Sync {
    fn send(&self, req: ApiRequest) -> BoxFuture<'_, anyhow::Result<ApiResponse>>;
    /// Where the requests go, for messages.
    fn describe(&self) -> String;
}

/// Percent-encode one path segment or query value.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `http` method of the runner socket.
#[derive(Debug)]
pub struct RunnerTunnelTransport {
    socket: PathBuf,
    next_id: AtomicU64,
}

impl RunnerTunnelTransport {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            next_id: AtomicU64::new(1),
        }
    }

    /// The runner request line for one API request.
    pub fn frame(id: u64, req: &ApiRequest) -> Value {
        let mut params = json!({
            "method": req.method.as_str(),
            "path": req.path,
        });
        if !req.query.is_empty() {
            let q: serde_json::Map<String, Value> = req
                .query
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            params["query"] = Value::Object(q);
        }
        if let Some(body) = &req.body {
            params["body"] = body.clone();
        }
        json!({"type": "request", "id": id, "method": "http", "params": params})
    }

    /// Turn the runner's answer into a response.
    pub fn unframe(reply: &Value) -> anyhow::Result<ApiResponse> {
        if let Some(err) = reply.get("error") {
            anyhow::bail!(
                "the runner tunnel failed: {}: {}",
                err["code"].as_str().unwrap_or("error"),
                err["message"].as_str().unwrap_or_default()
            );
        }
        let result = reply
            .get("result")
            .context("the runner answered without `result`")?;
        let status = result["status"]
            .as_u64()
            .and_then(|s| u16::try_from(s).ok())
            .context("the runner answered without `result.status`")?;
        let headers = result["headers"]
            .as_object()
            .map(|h| {
                h.iter()
                    .map(|(k, v)| {
                        let v = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                        (k.to_ascii_lowercase(), v)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ApiResponse {
            status,
            headers,
            body: result.get("body").cloned().unwrap_or(Value::Null),
        })
    }

    async fn round_trip(&self, req: ApiRequest) -> anyhow::Result<ApiResponse> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = Self::frame(id, &req);
        frame["params"]["request_id"] = json!(ulid::Ulid::new().to_string());
        let mut line = serde_json::to_vec(&frame)?;
        line.push(b'\n');
        let mut retry_until = None;
        'retry: loop {
            let stream = tokio::net::UnixStream::connect(&self.socket)
                .await
                .with_context(|| {
                    format!("cannot connect to the runner at {}", self.socket.display())
                })?;
            let (read, mut write) = stream.into_split();
            write
                .write_all(&line)
                .await
                .context("cannot send to the runner")?;
            let mut reader = BufReader::new(read);
            loop {
                let mut reply = String::new();
                let n = reader
                    .read_line(&mut reply)
                    .await
                    .context("cannot read from the runner")?;
                if n == 0 {
                    anyhow::bail!("the runner closed the socket before it answered");
                }
                let reply: Value =
                    serde_json::from_str(reply.trim()).context("bad answer from the runner")?;
                // Skip lines that answer another request.
                if reply.get("id").and_then(Value::as_u64) == Some(id)
                    || reply.get("id").is_none_or(Value::is_null)
                {
                    if reply["error"]["code"] == "retry" {
                        let until = *retry_until.get_or_insert_with(|| {
                            tokio::time::Instant::now() + std::time::Duration::from_secs(15 * 60)
                        });
                        if tokio::time::Instant::now() >= until {
                            anyhow::bail!(
                                "the agent RPC tunnel is down: restart exceeded 15 minutes"
                            );
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue 'retry;
                    }
                    return Self::unframe(&reply);
                }
            }
        }
    }
}

impl Transport for RunnerTunnelTransport {
    fn send(&self, req: ApiRequest) -> BoxFuture<'_, anyhow::Result<ApiResponse>> {
        Box::pin(self.round_trip(req))
    }

    fn describe(&self) -> String {
        format!("runner:{}", self.socket.display())
    }
}

/// A transport for tests: it records each request and answers from a queue.
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default, Clone)]
    pub struct MockTransport {
        pub requests: Arc<Mutex<Vec<ApiRequest>>>,
        responses: Arc<Mutex<VecDeque<ApiResponse>>>,
    }

    impl MockTransport {
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue an answer with this status and body.
        pub fn reply(&self, status: u16, body: Value) -> &Self {
            self.reply_with(status, &[], body)
        }

        pub fn reply_with(&self, status: u16, headers: &[(&str, &str)], body: Value) -> &Self {
            #[allow(clippy::unwrap_used)]
            self.responses.lock().unwrap().push_back(ApiResponse {
                status,
                headers: headers
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                body,
            });
            self
        }

        #[allow(clippy::unwrap_used)]
        pub fn requests(&self) -> Vec<ApiRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Transport for MockTransport {
        fn send(&self, req: ApiRequest) -> BoxFuture<'_, anyhow::Result<ApiResponse>> {
            #[allow(clippy::unwrap_used)]
            let resp = {
                self.requests.lock().unwrap().push(req);
                self.responses.lock().unwrap().pop_front()
            };
            Box::pin(async move {
                Ok(resp.unwrap_or(ApiResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Value::Array(Vec::new()),
                }))
            })
        }

        fn describe(&self) -> String {
            "mock".into()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retries_long_poll_with_the_same_request_id() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("runner.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let mut first = None;
            for attempt in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.unwrap();
                let frame: Value = serde_json::from_str(&line).unwrap();
                let id = frame["params"]["request_id"].as_str().unwrap();
                assert!(ulid::Ulid::from_string(id).is_ok());
                if let Some(previous) = &first {
                    assert_eq!(previous, &frame);
                }
                first = Some(frame.clone());
                let response = if attempt == 0 {
                    json!({"type":"response", "id":frame["id"], "error":{"code":"retry", "message":"restarting"}})
                } else {
                    json!({"type":"response", "id":frame["id"], "result":{"status":200, "body":[]}})
                };
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let response = RunnerTunnelTransport::new(socket)
            .send(ApiRequest::new(Method::Get, "/api/mail"))
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        server.await.unwrap();
    }

    #[test]
    fn encodes_segments() {
        assert_eq!(encode("terrarium/worker"), "terrarium%2Fworker");
        assert_eq!(encode("my proj"), "my%20proj");
        assert_eq!(encode("TER-42"), "TER-42");
    }

    #[test]
    fn builds_the_query() {
        let mut r = ApiRequest::new(Method::Get, "/api/mail");
        assert_eq!(r.path_and_query(), "/api/mail");
        r.query.push(("to".into(), "project:ops".into()));
        r.query.push(("limit".into(), "5".into()));
        assert_eq!(r.path_and_query(), "/api/mail?to=project%3Aops&limit=5");
        assert_eq!(r.query_value("limit"), Some("5"));
    }

    #[test]
    fn decodes_bodies() {
        assert_eq!(decode_body(b""), Value::Null);
        assert_eq!(decode_body(b"{\"a\":1}"), json!({"a": 1}));
        assert_eq!(decode_body(b"oops"), json!("oops"));
    }

    #[test]
    fn frames_a_tunnel_request() {
        let mut r = ApiRequest::new(Method::Post, "/api/mail");
        r.query.push(("limit".into(), "20".into()));
        r.body = Some(json!({"to": "bob", "body": "hi"}));
        assert_eq!(
            RunnerTunnelTransport::frame(7, &r),
            json!({"type":"request","id":7,"method":"http","params":{
                "method":"POST","path":"/api/mail","query":{"limit":"20"},
                "body":{"to":"bob","body":"hi"}}})
        );
        let bare = RunnerTunnelTransport::frame(1, &ApiRequest::new(Method::Get, "/api/health"));
        assert!(bare["params"].get("query").is_none());
        assert!(bare["params"].get("body").is_none());
    }

    #[test]
    fn unframes_a_tunnel_answer() {
        let r = RunnerTunnelTransport::unframe(&json!({"type":"response","id":7,"result":{
            "status":201,"headers":{"X-Next-Cursor":"abc"},"body":{"id":"m1"}}}))
        .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.header("x-next-cursor"), Some("abc"));
        assert_eq!(r.body["id"], "m1");
        let err = RunnerTunnelTransport::unframe(&json!({"type":"response","id":7,
            "error":{"code":"method_not_found","message":"unknown method `http`"}}))
        .unwrap_err();
        assert!(err.to_string().contains("method_not_found"));
    }
}
