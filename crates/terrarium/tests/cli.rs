//! The `terrarium` binary end to end:
//!
//! - operator mode over `HttpTransport`, against a stub daemon (axum) on a
//!   Unix socket and on TCP: pagination, error mapping, exit codes, SSE;
//! - agent mode over the runner tunnel, against a fake runner socket that
//!   records each request line.

#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("terrarium-cli-{tag}-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_terrarium"));
    for k in [
        "TERRARIUM_AGENT",
        "TERRARIUM_SOCKET",
        "TERRARIUM_URL",
        "TERRARIUM_TOKEN",
        "TERRARIUM_API_SOCKET",
        "COLUMNS",
    ] {
        c.env_remove(k);
    }
    // Never the stored sessions of the user who runs the tests.
    c.env(
        "XDG_CONFIG_HOME",
        std::env::temp_dir().join(format!("terrarium-cli-xdg-{}", std::process::id())),
    );
    c
}

#[test]
fn questions_require_card_even_when_urgent_or_critical() {
    for verb in ["ask", "bundle"] {
        for priority in [None, Some("--urgent"), Some("--critical")] {
            let mut command = bin();
            command.args(["question", verb, "Decision?"]);
            if verb == "bundle" {
                command.args(["--item", "First?", "--item", "Second?"]);
            }
            if let Some(priority) = priority {
                command.arg(priority);
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&output.stderr).contains("--card"));
        }
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A stub daemon: enough routes for pagination, errors, and SSE.
fn stub() -> axum::Router {
    async fn agents(
        Query(q): Query<std::collections::HashMap<String, String>>,
    ) -> impl IntoResponse {
        let mut h = HeaderMap::new();
        match q.get("cursor").map(String::as_str) {
            None => {
                h.insert("x-next-cursor", "A2".parse().unwrap());
                (
                    h,
                    Json(
                        json!([{"id": "A1", "name": "lead", "project": "terra", "harness": "claude", "status": "idle"}]),
                    ),
                )
            }
            Some(_) => (
                h,
                Json(
                    json!([{"id": "A2", "name": "worker", "project": "terra", "harness": "pi", "status": "working"}]),
                ),
            ),
        }
    }
    async fn refuse() -> impl IntoResponse {
        (
            StatusCode::FORBIDDEN,
            Json(
                json!({"error": {"code": "forbidden", "message": "`ops/dev` is outside project `terra`; write to `project:ops`"}}),
            ),
        )
    }
    async fn missing() -> impl IntoResponse {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"code": "not_found", "message": "no such API route"}})),
        )
    }
    async fn events(headers: HeaderMap) -> axum::response::Response {
        let sse = headers
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
        if !sse {
            return Json(json!([])).into_response();
        }
        let body = "id: 1\nevent: status\ndata: {\"seq\":1,\"kind\":\"status\",\"agent\":\"lead\",\"summary\":\"lead: starting -> idle\"}\n\n\
                    : keepalive\n\n\
                    id: 2\nevent: mail\ndata: {\"seq\":2,\"kind\":\"mail\",\"agent\":\"lead\",\"summary\":\"mail to lead\"}\n\n";
        ([("content-type", "text/event-stream")], body).into_response()
    }
    axum::Router::new()
        .route(
            "/api/health",
            get(|| async { Json(json!({"status": "ok", "version": "9.9.9", "uptime": 3700})) }),
        )
        .route("/api/agents", get(agents))
        .route("/api/mail", post(refuse))
        .route("/api/events", get(events))
        .route("/api/skills", post(skill_create))
        .route("/api/skills/{name}", axum::routing::put(skill_update))
        .route("/api/agents/{id}/interrupt", post(interrupt))
        .route("/api/agents/{owner}/spawn", post(spawn_post))
        .route("/api/cards/{id}", axum::routing::patch(card_patch))
        .route(
            "/api/cards/{id}/claim",
            post(|| async {
                Json(json!({"id":"TER-1", "title":"B", "column":"working",
                "eta_history":{"n":7,"median_ms":60000.0,"p70_ms":120000.0}}))
            }),
        )
        .fallback(missing)
}

/// `POST /api/agents/{owner}/spawn` of the stub: echoes the body in
/// `sent`, so that a test sees what the CLI sent.
async fn spawn_post(
    axum::extract::Path(owner): axum::extract::Path<String>,
    Json(b): Json<Value>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::CREATED,
        Json(json!({"id": "S1", "sent": b,
             "skipped": ["target:box",
                         "grant:1 hidden: the owner may not read their names"],
             "project": "terra", "name": "lead-worker-1", "template": b["template"],
             "harness": "claude", "owner": owner, "owner_name": "lead",
             "deadline_at": "2099-01-01T00:00:00.000Z"})),
    )
}

/// `PATCH /api/cards/{id}` of the stub: the card with the patch in
/// `sent`, so that a test sees the body the CLI sent.
async fn card_patch(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(b): Json<Value>,
) -> Json<Value> {
    Json(
        json!({"id": id, "title": "B", "column": "backlog", "sent": b,
        "eta_at": b["eta_at"],
        "eta_history":{"n":7,"median_ms":60000.0,"p70_ms":120000.0}}),
    )
}

/// `POST /api/agents/{id}/interrupt` of the stub: `busy` runs a turn,
/// any other agent runs none.
async fn interrupt(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if id == "busy" {
        (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": true, "interrupted": true})),
        )
            .into_response()
    } else {
        (
            StatusCode::CONFLICT,
            Json(json!({"error": {"code": "conflict", "message": "the agent runs no turn: there is nothing to interrupt"}})),
        )
            .into_response()
    }
}

/// `POST /api/skills` of the stub: `taken` exists, `bad` fails the
/// checks, any other name is created.
async fn skill_create(Json(b): Json<Value>) -> axum::response::Response {
    let files = b["files"].as_object().map_or(0, serde_json::Map::len) + 1;
    match b["name"].as_str().unwrap_or_default() {
        "taken" => (
            StatusCode::CONFLICT,
            Json(json!({"error": {"code": "conflict", "message": "the skill `taken` exists"}})),
        )
            .into_response(),
        "bad" => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(
                json!({"error": {"code": "invalid", "message": "the skill fails the checks",
                "details": {"problems": ["`x.exe`: the file type is not allowed"]}}}),
            ),
        )
            .into_response(),
        n => (
            StatusCode::CREATED,
            Json(json!({"name": n, "files": files})),
        )
            .into_response(),
    }
}

async fn skill_update(
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(b): Json<Value>,
) -> Json<Value> {
    assert!(b.get("name").is_none(), "a PUT has no name");
    let files = b["files"].as_object().map_or(0, serde_json::Map::len) + 1;
    Json(json!({"name": name, "files": files}))
}

fn serve_unix(dir: &Path) -> PathBuf {
    let socket = dir.join("api.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::UnixListener::from_std(listener).unwrap();
            axum::serve(l, stub()).await.unwrap();
        });
    });
    socket
}

fn run(socket: &Path, args: &[&str]) -> Output {
    bin()
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn claim_and_eta_print_similar_card_history() {
    let dir = temp_dir("eta-history");
    let socket = serve_unix(&dir);
    for args in [
        vec!["card", "claim", "TER-1"],
        vec!["card", "claim", "TER-1", "--eta", "25m"],
        vec!["card", "eta", "TER-1", "25m"],
        vec!["card", "eta", "TER-1", "--clear"],
    ] {
        let o = run(&socket, &args);
        assert!(o.status.success(), "{}", text(&o.stderr));
        let out = text(&o.stdout);
        assert!(
            out.contains("Similar cards (same project and type): median 1.0m, p70 2.0m, n=7"),
            "{out}"
        );
    }
    let o = run(&socket, &["--json", "card", "claim", "TER-1"]);
    let value: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(value["eta_history"]["n"], 7);
}

#[test]
fn operator_mode_over_the_unix_socket() {
    let dir = temp_dir("unix");
    let socket = serve_unix(&dir);

    let o = run(&socket, &["daemon", "status"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).starts_with("terrariumd 9.9.9 running at unix:"));
    assert!(text(&o.stdout).contains("(up 1h1m)"));

    // Pagination: both pages, in order.
    let o = run(&socket, &["--json", "agent", "ls"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["A1", "A2"]);
    let o = run(&socket, &["agent", "ls"]);
    let out = text(&o.stdout);
    assert!(out.starts_with("ID  "), "{out}");
    assert!(out.contains("lead") && out.contains("worker"), "{out}");

    // A refusal: exit 3, the message verbatim.
    let o = run(&socket, &["mail", "send", "ops/dev", "hi"]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(
        text(&o.stderr),
        "terrarium: `ops/dev` is outside project `terra`; write to `project:ops` [forbidden]\n"
    );
    let o = run(&socket, &["--json", "mail", "send", "ops/dev", "hi"]);
    assert_eq!(o.status.code(), Some(3));
    let e: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(e["error"]["code"], "forbidden");

    // A route the daemon does not serve yet: exit 1 and a hint.
    let o = run(&socket, &["-p", "terra", "mem", "search", "x"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("no such API route [not_found]"));
    assert!(text(&o.stderr).contains("awaiting server"));

    // Usage errors: exit 2.
    let o = run(&socket, &["exec", "box"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("name the command after `--`"));
    let o = run(&socket, &["board"]);
    assert_eq!(o.status.code(), Some(2));
    let o = run(&socket, &["card", "frobnicate"]);
    assert_eq!(o.status.code(), Some(2));

    // Issue #12: `agent interrupt` posts to the interrupt route.
    let o = run(&socket, &["agent", "interrupt", "busy"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "interrupted the turn of busy\n");
    let o = run(&socket, &["agent", "interrupt", "lead"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("nothing to interrupt [conflict]"),
        "{}",
        text(&o.stderr)
    );

    // SSE follow: each event as it comes; the command ends with the stream.
    let o = run(&socket, &["events", "--follow"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("lead: starting -> idle"), "{out}");
    assert!(out.contains("mail to lead"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

/// Issue #13: `card edit --depends-on none` (and `--label none`) sends
/// an empty list; `none` with other values is a usage error (exit 2) and
/// sends nothing.
#[test]
fn card_edit_none_clears_the_lists() {
    let dir = temp_dir("card-none");
    let socket = serve_unix(&dir);

    let o = run(
        &socket,
        &[
            "--json",
            "card",
            "edit",
            "TER-2",
            "--depends-on",
            "none",
            "--label",
            "none",
        ],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["sent"], json!({"depends_on": [], "labels": []}));

    let o = run(
        &socket,
        &["--json", "card", "edit", "TER-2", "--depends-on", "TER-1"],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["sent"], json!({"depends_on": ["TER-1"]}));

    let o = run(
        &socket,
        &[
            "card",
            "edit",
            "TER-2",
            "--depends-on",
            "none",
            "--depends-on",
            "TER-1",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o.stderr).contains("--depends-on none clears the list"),
        "{}",
        text(&o.stderr)
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// `skill import <dir>`: each child dir with a SKILL.md, with the result
/// of each (issue #2); a failure does not stop the others; `--update`
/// replaces a skill that exists.
#[test]
fn skill_import_reports_each_skill() {
    let dir = temp_dir("skill-import");
    let socket = serve_unix(&dir);
    let lib = dir.join("lib");
    for n in ["fresh", "taken", "bad"] {
        let d = lib.join(n);
        std::fs::create_dir_all(d.join("scripts/__pycache__")).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {n}\ndescription: d\n---\n"),
        )
        .unwrap();
        std::fs::write(d.join("scripts/run.py"), "print(1)\n").unwrap();
        std::fs::write(d.join("scripts/__pycache__/run.pyc"), b"\0").unwrap();
    }
    std::fs::create_dir_all(lib.join("not-a-skill")).unwrap();

    let o = run(&socket, &["skill", "import", lib.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("fresh: created (2 file(s))"), "{out}");
    assert!(
        out.contains("taken: exists: the skill exists; give --update"),
        "{out}"
    );
    assert!(
        out.contains("bad: failed: the skill fails the checks"),
        "{out}"
    );
    assert!(
        out.contains("  `x.exe`: the file type is not allowed"),
        "{out}"
    );
    assert!(out.contains("  skipped: scripts/__pycache__/"), "{out}");
    assert!(!out.contains("not-a-skill"), "{out}");
    assert!(text(&o.stderr).contains("2 of 3 skill(s) failed"));

    let o = run(
        &socket,
        &[
            "--json",
            "skill",
            "import",
            "--update",
            lib.to_str().unwrap(),
        ],
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["imported"], 2, "{v}");
    assert_eq!(v["failed"], 1);
    let taken = v["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "taken")
        .unwrap();
    assert_eq!(taken["result"], "updated");
    assert_eq!(taken["skipped"], json!(["scripts/__pycache__/"]));

    std::fs::remove_dir_all(&dir).ok();
}

/// `exec -i` against a stub daemon whose command ends before the stdin
/// comes: the `data` input gets `409 conflict`, the late `eof` is a no-op.
/// The CLI does not fail on the 409; the exit code and the output come
/// from the output stream.
#[test]
fn exec_stdin_after_the_command_ended_is_not_an_error() {
    let inputs: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = inputs.clone();
    let app = axum::Router::new()
        .route(
            "/api/targets/box/exec",
            post(|| async {
                (
                    StatusCode::CREATED,
                    Json(json!({"session_id": "S1", "target": "box", "kind": "exec"})),
                )
            }),
        )
        .route(
            "/api/targets/box/sessions/S1/input",
            post(move |Json(b): Json<Value>| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(b.clone());
                    if b.get("data").is_some() {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({"error": {"code": "conflict",
                                "message": "the session has ended; read its output for the end"}})),
                        )
                            .into_response();
                    }
                    Json(json!({"session_id": "S1", "accepted": false})).into_response()
                }
            }),
        )
        .route(
            "/api/targets/box/sessions/S1/output",
            get(|| async {
                Json(json!({"session_id": "S1", "since": 0, "next": 5,
                    "chunks": [{"stream": "stdout", "data": "ZmFzdAo="}],
                    "done": true, "end": {"type": "exit", "code": 4, "reason": "exited"}}))
            }),
        );
    let dir = temp_dir("exec409");
    let socket = dir.join("api.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::UnixListener::from_std(listener).unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });
    let mut child = bin()
        .arg("--socket")
        .arg(&socket)
        .args(["exec", "-i", "box", "--", "true"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"input that nobody reads\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "fast\n");
    assert!(!text(&o.stderr).contains("conflict"), "{}", text(&o.stderr));
    // One `data` (refused); no input after it: the session has ended.
    let got = inputs.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].get("data").is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn operator_mode_over_tcp_sends_the_token() {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    // The token is required, as on the daemon's TCP listener.
    let app = axum::Router::new().route(
        "/api/health",
        get(|h: HeaderMap| async move {
            if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer t0ken") {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"code": "unauthorized", "message": "a bearer token is needed"}})),
                )
                    .into_response();
            }
            Json(json!({"version": "9.9.9", "uptime": 5})).into_response()
        }),
    );
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });
    let o = bin()
        .env("TERRARIUM_URL", format!("http://{addr}"))
        .env("TERRARIUM_TOKEN", "t0ken")
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains(&format!("running at http://{addr}")));
    let o = bin()
        .args([
            "--url",
            &format!("http://{addr}"),
            "--token",
            "wrong",
            "daemon",
            "status",
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3));
    assert!(text(&o.stderr).contains("a bearer token is needed [unauthorized]"));
    let o = bin()
        .args(["--url", "ftp://nope", "daemon", "status"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("bad daemon URL"));
}

/// `terrarium login` and `logout` against a stub daemon, with a temp
/// `XDG_CONFIG_HOME`: the file modes, the stored token on later calls, the
/// env first, and the hint on a `401` with a stored session.
#[test]
fn login_and_logout_with_a_temp_xdg_dir() {
    use std::os::unix::fs::PermissionsExt;
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    // The token that the stub takes; logout clears it.
    let live: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("jwt-1".into())));
    let deleted: Arc<Mutex<Vec<String>>> = Arc::default();
    let (l1, l2, d1) = (live.clone(), live.clone(), deleted.clone());
    let app = axum::Router::new()
        .route(
            "/api/session",
            post(move |h: HeaderMap, Json(b): Json<Value>| {
                let live = l1.clone();
                async move {
                    assert!(h.get("authorization").is_none(), "the login sends no token");
                    if b["password"] != "the password" || b["client"] != "cli" {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": {"code": "unauthorized", "message": "login failed"}})),
                        )
                            .into_response();
                    }
                    *live.lock().unwrap() = Some("jwt-1".into());
                    (
                        StatusCode::CREATED,
                        Json(json!({"kind": "cli", "token": "jwt-1", "exp": 1,
                            "expires_at": "2027-01-25T10:00:00Z"})),
                    )
                        .into_response()
                }
            })
            .delete(move |h: HeaderMap| {
                let deleted = d1.clone();
                async move {
                    let a = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    deleted.lock().unwrap().push(a);
                    StatusCode::NO_CONTENT
                }
            }),
        )
        .route(
            "/api/health",
            get(move |h: HeaderMap| {
                let live = l2.clone();
                async move {
                    let want = live.lock().unwrap().clone().map(|t| format!("Bearer {t}"));
                    let got = h.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
                    if got.is_none() || got != want && got.as_deref() != Some("Bearer env-token") {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": {"code": "unauthorized", "message": "missing or wrong bearer token"}})),
                        )
                            .into_response();
                    }
                    Json(json!({"version": "9.9.9", "uptime": 5})).into_response()
                }
            }),
        );
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });
    let xdg = temp_dir("xdg");
    let url = format!("http://{addr}");
    let run = |args: &[&str], stdin: &str, env_token: Option<&str>| {
        let mut c = bin();
        c.env("XDG_CONFIG_HOME", &xdg).env("TERRARIUM_URL", &url);
        if let Some(t) = env_token {
            c.env("TERRARIUM_TOKEN", t);
        }
        let mut child = c
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    // A wrong password: nothing stored, no expiry hint.
    let o = run(&["login", "--password-stdin"], "nope\n", None);
    assert_eq!(o.status.code(), Some(3));
    assert!(text(&o.stderr).contains("login failed"));
    assert!(!text(&o.stderr).contains("session expired"));
    let file = xdg.join("terrarium").join("credentials.json");
    assert!(!file.exists());
    // Log in: the file and its dir are private.
    let o = run(
        &["login", "--password-stdin"],
        "the password\n",
        Some("ignored"),
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("2027-01-25T10:00:00Z"));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&xdg.join("terrarium")), 0o700);
    let stored: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored[&url]["token"], "jwt-1");
    // A later call uses the stored token.
    let o = run(&["daemon", "status"], "", None);
    assert!(o.status.success(), "{}", text(&o.stderr));
    // The env wins: a wrong env token fails, with no expiry hint.
    let o = run(&["daemon", "status"], "", Some("wrong"));
    assert_eq!(o.status.code(), Some(3));
    assert!(!text(&o.stderr).contains("session expired"));
    let o = run(&["daemon", "status"], "", Some("env-token"));
    assert!(o.status.success(), "{}", text(&o.stderr));
    // The daemon ends the session: a 401 with the hint.
    *live.lock().unwrap() = None;
    let o = run(&["daemon", "status"], "", None);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("session expired: run terrarium login"),
        "{}",
        text(&o.stderr)
    );
    // Log out: DELETE with the stored token, and the entry goes.
    let o = run(&["logout"], "", None);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(deleted.lock().unwrap().as_slice(), ["Bearer jwt-1"]);
    let stored: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert!(stored.get(&url).is_none());
    let o = run(&["logout"], "", None);
    assert!(text(&o.stdout).contains("not logged in"));
    std::fs::remove_dir_all(&xdg).ok();
}

/// A fake runner: answers `whoami` itself, answers `http` from `answer`,
/// and records every request line.
fn fake_runner(dir: &Path, answer: fn(&Value) -> Value) -> (PathBuf, Arc<Mutex<Vec<Value>>>) {
    let socket = dir.join("agent.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut w = stream.try_clone().unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Some(Ok(line)) = lines.next() {
                let req: Value = serde_json::from_str(&line).unwrap();
                log.lock().unwrap().push(req.clone());
                let result = match req["method"].as_str() {
                    Some("whoami") => json!({"agent": "01AGENT", "name": "worker",
                        "project": "terra", "harness": "pi", "status": "idle"}),
                    Some("http") => answer(&req["params"]),
                    _ => Value::Null,
                };
                let mut reply = serde_json::to_vec(
                    &json!({"type": "response", "id": req["id"], "result": result}),
                )
                .unwrap();
                reply.push(b'\n');
                w.write_all(&reply).unwrap();
            }
        }
    });
    (socket, seen)
}

#[test]
fn version_group_actions_use_project_scoped_api() {
    let dir = temp_dir("version-groups");
    let (socket, seen) = fake_runner(
        &dir,
        |_| json!({"status":200,"headers":{},"body":{"changed":true,"group":{"key":"version:cut-1"}}}),
    );
    for args in [
        vec![
            "group",
            "status",
            "version:cut-1",
            "--step",
            "version branch e2e",
            "--state",
            "running",
            "--eta",
            "10m",
        ],
        vec!["group", "remove", "version:cut-1", "TER-7"],
        vec!["group", "built", "version:cut-1", "version:cut-2", "v1.2.3"],
        vec!["group", "add", "version:cut-1", "TER-8"],
    ] {
        let o = bin()
            .env("TERRARIUM_AGENT", "01AGENT")
            .env("TERRARIUM_SOCKET", &socket)
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", text(&o.stderr));
        assert!(text(&o.stdout).contains("changed"));
    }
    let requests = seen.lock().unwrap();
    let http: Vec<_> = requests
        .iter()
        .filter(|r| r["method"] == "http")
        .map(|r| &r["params"])
        .collect();
    assert_eq!(http.len(), 5);
    for (r, (key, action)) in http.iter().zip([
        ("cut-1", "status"),
        ("cut-1", "remove"),
        ("cut-1", "built"),
        ("cut-2", "built"),
        ("cut-1", "add"),
    ]) {
        assert_eq!(r["method"], "POST");
        assert_eq!(
            r["path"],
            format!("/api/projects/terra/version-groups/version%3A{key}/{action}")
        );
    }
    assert_eq!(http[0]["body"]["step"], "version branch e2e");
    assert_eq!(http[0]["body"]["eta"], "10m");
    assert_eq!(http[1]["body"]["card"], "TER-7");
    assert_eq!(http[2]["body"]["version"], "v1.2.3");
    assert_eq!(http[3]["body"]["version"], "v1.2.3");
    assert_eq!(http[4]["body"]["card"], "TER-8");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn agent_mode_tunnels_every_call_over_the_runner_socket() {
    let dir = temp_dir("tunnel");
    let (socket, seen) = fake_runner(&dir, |p| {
        let cursor = p["query"]["cursor"].as_str();
        match (p["method"].as_str(), p["path"].as_str(), cursor) {
            (Some("GET"), Some("/api/projects/terra/cards"), None) => json!({
                "status": 200, "headers": {"x-next-cursor": "2"},
                "body": [{"id": "TER-1", "column": "ready", "title": "First"}]}),
            (Some("GET"), Some("/api/projects/terra/cards"), Some(_)) => json!({
                "status": 200, "headers": {},
                "body": [{"id": "TER-2", "column": "ready", "title": "Second"}]}),
            (Some("POST"), Some("/api/mail"), _) => json!({
                "status": 403, "headers": {},
                "body": {"error": {"code": "forbidden", "message": "outside your project"}}}),
            (Some("GET"), Some("/api/agents/01AGENT"), _) => json!({
                "status": 200, "headers": {},
                "body": {"id": "01AGENT", "name": "worker",
                         "roles": ["sysadmin", "architect", "project_admin"]}}),
            _ => json!({"status": 404, "headers": {},
                "body": {"error": {"code": "not_found", "message": "no such API route"}}}),
        }
    });
    let agent = |args: &[&str]| {
        bin()
            .env("TERRARIUM_AGENT", "01AGENT")
            .env("TERRARIUM_SOCKET", &socket)
            // Agent mode ignores the operator socket.
            .env("TERRARIUM_API_SOCKET", "/nonexistent/api.sock")
            .args(args)
            .output()
            .unwrap()
    };

    let o = agent(&["card", "ls", "--unassigned", "--column", "ready"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("TER-1") && out.contains("TER-2"), "{out}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["method"], "whoami");
        let request_id = seen[1]["params"]["request_id"].as_str().unwrap();
        assert!(ulid::Ulid::from_string(request_id).is_ok());
        assert_ne!(
            seen[1]["params"]["request_id"],
            seen[2]["params"]["request_id"]
        );
        assert_eq!(
            seen[1],
            json!({"type": "request", "id": 1, "method": "http", "params": {
                "method": "GET", "path": "/api/projects/terra/cards",
                "query": {"column": "ready", "unassigned": "true"}, "request_id": request_id}})
        );
        assert_eq!(seen[2]["params"]["query"]["cursor"], "2");
    }

    let o = agent(&["mail", "send", "ops/dev", "-s", "hi", "body text"]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(
        text(&o.stderr),
        "terrarium: outside your project [forbidden]\n"
    );
    {
        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        let request_id = last["params"]["request_id"].as_str().unwrap();
        assert!(ulid::Ulid::from_string(request_id).is_ok());
        assert_eq!(
            last["params"],
            json!({"method": "POST", "path": "/api/mail",
                   "request_id": request_id,
                   "body": {"to": "ops/dev", "subject": "hi", "body": "body text"}})
        );
    }

    // The runner's copy has no roles. whoami reads the daemon row, so the
    // global roles and the project role both show, as `agent show` shows them.
    let o = agent(&["--json", "whoami"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let me: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(me["project"], "terra");
    assert_eq!(
        me["roles"],
        json!(["sysadmin", "architect", "project_admin"])
    );
    let o = agent(&["whoami"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        text(&o.stdout).contains("roles: sysadmin,architect,project_admin"),
        "{}",
        text(&o.stdout)
    );

    // A dead runner socket is a transport error: exit 1.
    let o = bin()
        .env("TERRARIUM_AGENT", "01AGENT")
        .env("TERRARIUM_SOCKET", dir.join("gone.sock"))
        .args(["-p", "terra", "board"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("cannot connect to the runner"));

    std::fs::remove_dir_all(&dir).ok();
}

/// `card attach` and `card files get` over the runner tunnel (docs/api.md
/// §8.4): the upload is JSON with base64, the download asks
/// `format=json`, and the file lands in the dir.
#[test]
fn card_files_over_the_runner_tunnel() {
    let dir = temp_dir("card-files");
    let (socket, seen) = fake_runner(&dir, |p| match (p["method"].as_str(), p["path"].as_str()) {
        (Some("POST"), Some("/api/cards/TER-1/files")) => json!({
                "status": 201, "headers": {},
                "body": {"card": "TER-1", "name": p["body"]["name"], "version": 1, "size": 5,
                         "outcome": "added"}}),
        (Some("GET"), Some("/api/cards/TER-1/files/notes.txt")) => json!({
                "status": 200, "headers": {},
                "body": {"card": "TER-1", "name": "notes.txt", "version": 2,
                         "data_base64": "aGVsbG8="}}),
        _ => json!({"status": 404, "headers": {},
                "body": {"error": {"code": "not_found", "message": "no such API route"}}}),
    });
    let agent = |args: &[&str]| {
        bin()
            .env("TERRARIUM_AGENT", "01AGENT")
            .env("TERRARIUM_SOCKET", &socket)
            .current_dir(&dir)
            .args(args)
            .output()
            .unwrap()
    };
    std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
    let o = agent(&["card", "attach", "TER-1", "notes.txt"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        text(&o.stdout).contains("added notes.txt v1"),
        "{}",
        text(&o.stdout)
    );
    {
        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        let request_id = last["params"]["request_id"].as_str().unwrap();
        assert!(ulid::Ulid::from_string(request_id).is_ok());
        assert_eq!(
            last["params"],
            json!({"method": "POST", "path": "/api/cards/TER-1/files",
                   "request_id": request_id,
                   "body": {"name": "notes.txt", "data_base64": "aGVsbG8="}})
        );
    }
    let o = agent(&[
        "card",
        "files",
        "get",
        "TER-1",
        "notes.txt",
        "-o",
        "got.txt",
    ]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(std::fs::read(dir.join("got.txt")).unwrap(), b"hello");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.last().unwrap()["params"]["query"],
            json!({"format": "json"})
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// PLAN §16b.4, secret manager review S1 and S6: the harness gets the env
/// vars of its secret grants from `terrarium exec-env` inside the sandbox,
/// not from the launch spec. A var that the env holds already wins.
#[test]
fn exec_env_adds_the_granted_vars_and_execs() {
    let dir = temp_dir("exec-env");
    let (socket, seen) = fake_runner(&dir, |p| match (p["method"].as_str(), p["path"].as_str()) {
        (Some("POST"), Some("/api/agent/secrets/env")) => json!({
                "status": 200, "headers": {},
                "body": {"vars": {"DB_URL": "pg://granted", "GH_TOKEN": "ghp_granted"}}}),
        _ => json!({"status": 404, "headers": {},
                "body": {"error": {"code": "not_found", "message": "no such API route"}}}),
    });
    let o = bin()
        .env("TERRARIUM_AGENT", "01AGENT")
        .env("TERRARIUM_SOCKET", &socket)
        .env("GH_TOKEN", "from-the-harness")
        .args([
            "exec-env",
            "--",
            "sh",
            "-c",
            "echo \"$DB_URL|$GH_TOKEN|$1\"",
            "sh",
            "-x",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "pg://granted|from-the-harness|-x\n");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r["params"]["path"] == "/api/agent/secrets/env")
    );
    // No daemon: a warning, and the program still starts.
    let o = bin()
        .env("TERRARIUM_AGENT", "01AGENT")
        .env("TERRARIUM_SOCKET", dir.join("gone.sock"))
        .args(["exec-env", "--", "sh", "-c", "echo \"[$DB_URL]\""])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "[]\n");
    assert!(
        text(&o.stderr).contains("are not set"),
        "{}",
        text(&o.stderr)
    );
    // A program that does not exist: exit 127.
    let o = bin()
        .env("TERRARIUM_AGENT", "01AGENT")
        .env("TERRARIUM_SOCKET", &socket)
        .args(["exec-env", "--", "/nonexistent/prog"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(127));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn hook_stays_quiet_and_exits_zero() {
    let o = bin().args(["hook", "Stop"]).output().unwrap();
    assert!(o.status.success());
    assert!(text(&o.stderr).contains("TERRARIUM_SOCKET is not set"));
}

/// TER-391 (TER-89 review): `agent spawn --like` sends the copy flags, and
/// the skipped list prints, also the count entry for grants whose names
/// the owner may not read. `--like-skip-missing` without `--like` is a
/// usage error.
#[test]
fn agent_spawn_like_sends_the_flags_and_prints_skipped() {
    let dir = temp_dir("spawn-like");
    let socket = serve_unix(&dir);

    let o = run(
        &socket,
        &[
            "--json",
            "agent",
            "spawn",
            "worker",
            "--task",
            "do it",
            "--like",
            "w",
            "--like-skip-missing",
            "--for",
            "w",
        ],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["sent"]["template"], "worker");
    assert_eq!(v["sent"]["task"], "do it");
    assert_eq!(v["sent"]["like"], "w");
    assert_eq!(v["sent"]["like_skip_missing"], true);

    let o = run(
        &socket,
        &[
            "agent",
            "spawn",
            "worker",
            "--task",
            "do it",
            "--like",
            "w",
            "--like-skip-missing",
            "--for",
            "w",
        ],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("skipped copied right: target:box"), "{out}");
    assert!(
        out.contains("skipped copied right: grant:1 hidden: the owner may not read their names"),
        "{out}"
    );
    assert!(out.contains("spawned terra/lead-worker-1"), "{out}");

    // No like: the flag is not sent. Without one: a usage error.
    let o = run(
        &socket,
        &[
            "--json", "agent", "spawn", "worker", "--task", "t", "--for", "w",
        ],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["sent"].get("like").is_none(), "{}", v);
    assert!(v["sent"].get("like_skip_missing").is_none(), "{}", v);
    let o = run(
        &socket,
        &[
            "agent",
            "spawn",
            "worker",
            "--task",
            "t",
            "--like-skip-missing",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    std::fs::remove_dir_all(&dir).ok();
}
