//! Start the daemon on a temp Unix socket with a temp state dir, and talk
//! to it over HTTP and WebSocket.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::HeaderMap;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use terrariumd::supervisor::InMemorySupervisor;
use terrariumd::{Daemon, DaemonConfig};
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const DELAY: Duration = Duration::from_millis(20);

pub struct TestDaemon {
    pub daemon: Option<Daemon>,
    pub dir: tempfile::TempDir,
    pub socket: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub sup: Arc<InMemorySupervisor>,
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Resp {
    pub fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
    pub fn next_cursor(&self) -> Option<String> {
        self.headers
            .get("x-next-cursor")
            .map(|v| v.to_str().unwrap().to_owned())
    }
    #[track_caller]
    pub fn ok(&self) -> &Value {
        assert!(
            self.status.is_success(),
            "expected success, got {}: {}",
            self.status,
            self.body
        );
        &self.body
    }
    #[track_caller]
    pub fn err(&self, status: u16, code: &str) {
        assert_eq!(self.status.as_u16(), status, "{}", self.body);
        assert_eq!(self.code(), code, "{}", self.body);
        assert!(self.body["error"]["message"].is_string(), "{}", self.body);
    }
}

pub fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

impl TestDaemon {
    pub async fn start() -> Self {
        Self::with_config(|_| {}).await
    }

    pub async fn with_config(f: impl FnOnce(&Path)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        f(&config);
        let state = dir.path().join("state");
        let socket = dir.path().join("api.sock");
        let sup = Arc::new(InMemorySupervisor::new(DELAY));
        let mut td = Self {
            daemon: None,
            dir,
            socket,
            config,
            state,
            sup,
        };
        td.boot(None, None).await;
        td
    }

    /// Start (or start again) the daemon on the same dirs.
    pub async fn boot(&mut self, listen: Option<&str>, token: Option<&str>) {
        if let Some(d) = self.daemon.take() {
            d.shutdown();
        }
        let daemon = terrariumd::start(DaemonConfig {
            socket: self.socket.clone(),
            listen: listen.map(str::to_owned),
            token: token.map(str::to_owned),
            state_dir: self.state.clone(),
            config_dir: self.config.clone(),
            supervisor: self.sup.clone(),
            // The tests name the calling agent with the header.
            insecure_agent_header: true,
            artifact_listen: None,
        })
        .await
        .unwrap();
        self.daemon = Some(daemon);
    }

    pub async fn req(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        agent: Option<&str>,
    ) -> Resp {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        if let Some(a) = agent {
            req = req.header("x-terrarium-agent", a);
        }
        let bytes = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Bytes::from(b.to_string())
            }
            None => Bytes::new(),
        };
        let resp = sender
            .send_request(req.body(Full::new(bytes)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let raw = resp.into_body().collect().await.unwrap().to_bytes();
        let body = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&raw)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into()))
        };
        Resp {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&self, path: &str) -> Resp {
        self.req(Method::GET, path, None, None).await
    }
    pub async fn post(&self, path: &str, body: Value) -> Resp {
        self.req(Method::POST, path, Some(body), None).await
    }
    pub async fn post_empty(&self, path: &str) -> Resp {
        self.req(Method::POST, path, None, None).await
    }
    pub async fn patch(&self, path: &str, body: Value) -> Resp {
        self.req(Method::PATCH, path, Some(body), None).await
    }
    pub async fn put(&self, path: &str, body: Value) -> Resp {
        self.req(Method::PUT, path, Some(body), None).await
    }
    pub async fn delete(&self, path: &str) -> Resp {
        self.req(Method::DELETE, path, None, None).await
    }

    pub async fn get_as(&self, agent: &str, path: &str) -> Resp {
        self.req(Method::GET, path, None, Some(agent)).await
    }
    pub async fn post_as(&self, agent: &str, path: &str, body: Value) -> Resp {
        self.req(Method::POST, path, Some(body), Some(agent)).await
    }
    pub async fn post_empty_as(&self, agent: &str, path: &str) -> Resp {
        self.req(Method::POST, path, None, Some(agent)).await
    }
    pub async fn patch_as(&self, agent: &str, path: &str, body: Value) -> Resp {
        self.req(Method::PATCH, path, Some(body), Some(agent)).await
    }
    pub async fn put_as(&self, agent: &str, path: &str, body: Value) -> Resp {
        self.req(Method::PUT, path, Some(body), Some(agent)).await
    }

    /// Open a WebSocket on the Unix socket.
    pub async fn ws(
        &self,
        path: &str,
        agent: Option<&str>,
    ) -> Result<WebSocketStream<UnixStream>, tokio_tungstenite::tungstenite::Error> {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let mut req = format!("ws://localhost{path}")
            .into_client_request()
            .unwrap();
        if let Some(a) = agent {
            req.headers_mut()
                .insert("x-terrarium-agent", a.parse().unwrap());
        }
        tokio_tungstenite::client_async(req, stream)
            .await
            .map(|(ws, _)| ws)
    }

    pub async fn project(&self, name: &str) -> Value {
        self.post("/api/projects", json!({"name": name}))
            .await
            .ok()
            .clone()
    }

    /// Make an agent through the API, with optional roles and policy.
    pub async fn agent(&self, project: &str, name: &str, extra: Value) -> Value {
        let mut def = json!({"name": name, "project": project, "harness": "claude"});
        if let (Some(d), Some(e)) = (def.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                d.insert(k.clone(), v.clone());
            }
        }
        self.post("/api/agents", def).await.ok().clone()
    }

    /// Poll until the agent has `status`.
    pub async fn wait_status(&self, agent: &str, status: &str) {
        for _ in 0..200 {
            let r = self.get(&format!("/api/agents/{agent}")).await;
            if r.body["status"] == status {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("agent {agent} never reached {status}");
    }

    /// The mail of an agent, as that agent sees it.
    pub async fn mail_of(&self, agent: &str) -> Vec<Value> {
        self.get_as(agent, "/api/mail")
            .await
            .ok()
            .as_array()
            .unwrap()
            .clone()
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown();
        }
    }
}
