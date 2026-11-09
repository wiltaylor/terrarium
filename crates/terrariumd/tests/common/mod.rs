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
/// The longest time a helper waits for a state before the test fails.
pub const WAIT: Duration = Duration::from_secs(15);
/// The longest time daemon boot waits for job queue recovery.
pub const RECOVERY_WAIT: Duration = Duration::from_secs(90);

/// Each test gets its own database, but pays for schema creation only once
/// per test binary. Close the connection before copying so WAL is checkpointed.
pub fn seed_database(state: &Path) {
    static TEMPLATE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let path = state.join("terrarium.db");
    if path.exists() {
        return; // Restart tests must keep their existing data.
    }
    let template = TEMPLATE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let conn = terrariumd::db::open(&dir.path().join("template.db")).unwrap();
        conn.close().unwrap();
        std::fs::read(dir.path().join("template.db")).unwrap()
    });
    std::fs::create_dir_all(state).unwrap();
    std::fs::write(path, template).unwrap();
}

pub struct TestDaemon {
    pub daemon: Option<Daemon>,
    pub dir: tempfile::TempDir,
    pub socket: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub sup: Arc<InMemorySupervisor>,
    /// The web settings of the next [`TestDaemon::boot`].
    pub web: terrariumd::settings::WebConfig,
    /// The artifact listener of the next boot. `None`: the dashboard
    /// port serves `/a/...`.
    pub artifact_listen: Option<String>,
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

/// Re-stamp a recorded Claude transcript so that it reads as "just now".
///
/// The trap: a usage sample takes the transcript's own `timestamp` (the
/// correct behaviour), `GET /api/usage` defaults `since` to the last 24 h,
/// and a daily budget counts only today's (UTC) samples. A fixture recorded
/// on a fixed date passes on that day and fails from the next day on. So a
/// test that feeds a fixture to the collector and reads the default window or
/// a daily budget must re-stamp it first.
///
/// This moves every `timestamp` (RFC 3339) and every cost-state `startTime`
/// (epoch ms) by one offset. The relative order and gaps stay the same. The
/// last line lands one second before now. If that puts the first line before
/// UTC midnight, the first line lands on midnight instead (so a run in the
/// first seconds of a day can get stamps a few seconds in the future). Lines
/// that are not JSON stay as they are. The source fixture file is not changed.
pub fn restamp_transcript(text: &str) -> String {
    use chrono::{DateTime, Utc};
    let stamps: Vec<DateTime<Utc>> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        })
        .collect();
    let (Some(first), Some(last)) = (stamps.iter().min(), stamps.iter().max()) else {
        return text.to_owned();
    };
    let now = Utc::now();
    let midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
    let mut shift = (now - chrono::Duration::seconds(1)) - *last;
    if *first + shift < midnight {
        shift = midnight - *first;
    }
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(mut v) => {
                if let Some(t) = v
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                {
                    let t = (t.with_timezone(&Utc) + shift)
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    v["timestamp"] = Value::String(t);
                }
                if let Some(ms) = v.get("startTime").and_then(Value::as_i64) {
                    v["startTime"] = json!(ms + shift.num_milliseconds());
                }
                out.push_str(&v.to_string());
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// The agent's own Claude project dir for the sandbox cwd, on the host:
/// `<state>/agents/<ulid>/harness/claude/projects/-home-agent-work/`. The
/// sandbox binds it over the shared home's `projects/-home-agent-work/`,
/// so a session file that the harness reports there is in this dir.
#[allow(dead_code)]
pub fn claude_project_dir(state: &Path, agent: &str) -> PathBuf {
    let dir = terrarium_runner::harness_dir(state, agent.parse().expect("agent id"))
        .join("claude/projects/-home-agent-work");
    std::fs::create_dir_all(&dir).expect("project dir");
    dir
}

pub fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// TER-526: a shell loop for a fixture (a job recipe, a fake docker) that
/// waits until the file `gate` exists. A test that ended early left the old
/// open-ended loops running for good, with the open fds of the test run (a
/// card check's lock fd). This loop also ends when the test process is gone
/// (killed, so no drop ran) and after `limit`. Its status is 0 only when the
/// gate exists.
pub fn gate_wait(gate: &Path, limit: Duration) -> String {
    let rounds = limit.as_millis() / 50;
    format!(
        "i=0; while [ ! -f '{g}' ] && kill -0 {pid} 2>/dev/null && [ \"$i\" -lt {rounds} ]; \
         do sleep 0.05; i=$((i + 1)); done; [ -f '{g}' ]",
        g = gate.display(),
        pid = std::process::id(),
    )
}

/// The live processes, other than this one, whose command line names
/// `marker` (the temp dir of a test): the fixture processes of the test.
/// Linux only: it reads `/proc`.
pub fn processes_naming(marker: &Path) -> Vec<(i32, String)> {
    let marker = marker.to_string_lossy();
    let me = std::process::id().to_string();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if name == me {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if !cmdline.contains(marker.as_ref()) {
            continue;
        }
        // A zombie has ended: only its parent has not reaped it yet.
        let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        if stat
            .rsplit(')')
            .next()
            .is_some_and(|r| r.trim_start().starts_with('Z'))
        {
            continue;
        }
        found.push((pid, cmdline.trim_end().to_owned()));
    }
    found
}

/// TER-526: the fixture processes of one test, found by its temp dir in
/// their command line. On drop, also on a panic, it kills each one that is
/// still alive, by pid. [`FixtureGuard::assert_none_left`] is the check at
/// the end of a test that no fixture outlived it.
pub struct FixtureGuard {
    dir: PathBuf,
}

impl FixtureGuard {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Wait up to [`WAIT`] for the fixture processes to end, then fail
    /// with the ones that are still alive.
    pub async fn assert_none_left(&self) {
        let start = std::time::Instant::now();
        loop {
            let left = processes_naming(&self.dir);
            if left.is_empty() {
                return;
            }
            assert!(
                start.elapsed() < WAIT,
                "fixture processes outlived the test: {left:?}"
            );
            tokio::time::sleep(DELAY).await;
        }
    }
}

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        for (pid, _) in processes_naming(&self.dir) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
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
            web: Default::default(),
            artifact_listen: None,
        };
        td.boot(None, None).await;
        td
    }

    /// Start (or start again) the daemon on the same dirs.
    pub async fn boot(&mut self, listen: Option<&str>, token: Option<&str>) {
        self.boot_with_ssh(listen, token, None).await;
    }

    pub async fn boot_with_ssh(
        &mut self,
        listen: Option<&str>,
        token: Option<&str>,
        ssh: Option<&str>,
    ) {
        if let Some(d) = self.daemon.take() {
            d.shutdown();
        }
        seed_database(&self.state);
        let mut executor = terrariumd::executor::Executor::default();
        if let Some(ssh) = ssh {
            executor.ssh_bin = ssh.to_owned();
        }
        let daemon = terrariumd::start_with_executor(
            DaemonConfig {
                socket: self.socket.clone(),
                listen: listen.map(str::to_owned),
                token: token.map(str::to_owned),
                state_dir: self.state.clone(),
                config_dir: self.config.clone(),
                work_dir: self.state.clone().with_file_name("work"),
                max_targets_per_project: terrariumd::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
                repos_dir: std::path::PathBuf::from("/repos"),
                supervisor: self.sup.clone(),
                // The tests name the calling agent with the header.
                insecure_agent_header: true,
                artifact_listen: self.artifact_listen.clone(),
                web: self.web.clone(),
            },
            executor,
        )
        .await
        .unwrap();
        // TER-216: the job queue takes up the running jobs of the last
        // daemon in the background after the start, and puts them back in
        // the queue as lost (TER-211). A test that starts a job before that
        // write would see it requeued, so wait for it here, once for all.
        tokio::time::timeout(RECOVERY_WAIT, daemon.state.job_queue.recovered())
            .await
            .expect("the job queue takes up the jobs of the last daemon");
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
        // A deadline, not a count of tries: on a loaded machine each GET is
        // slow, and a start can take seconds. Only a failing test waits it out.
        let start = std::time::Instant::now();
        loop {
            let r = self.get(&format!("/api/agents/{agent}")).await;
            if r.body["status"] == status {
                return;
            }
            assert!(
                start.elapsed() < WAIT,
                "agent {agent} never reached {status}; last {}",
                r.body["status"]
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Poll until the agent `agent` ran a turn and is `idle`: a status
    /// event `working -> idle`, then the status `idle`. The `idle` of the
    /// start can come before the first turn (the task turn of an ephemeral
    /// agent), and the daemon writes `working` from the supervisor events
    /// after the prompt is typed, so `wait_status(idle)` alone may return
    /// while that turn is still to run (TER-452, TER-462). The status event
    /// is written in the same write as the status.
    pub async fn wait_task_done(&self, agent: &str) {
        let start = std::time::Instant::now();
        loop {
            let r = self
                .get(&format!("/api/agents/{agent}/events?kind=status"))
                .await;
            let ended = r
                .ok()
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["data"]["from"] == "working" && e["data"]["status"] == "idle");
            if ended {
                break;
            }
            // Longer than WAIT: under load a start alone took up to 16 s
            // (TER-108), and the task turn comes after it.
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "agent {agent} never ended a turn"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.wait_status(agent, "idle").await;
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
