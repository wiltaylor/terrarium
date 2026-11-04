//! The git proxy (issue #11, PLAN §9 "The git proxy"): a fake forge
//! (`git http-backend` behind a small HTTP server that asks for the repo
//! token) and the daemon's proxy in front of it. The agent's git runs on
//! the host with the credential that `terrarium repo credential` would
//! give it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::Engine as _;
use common::TestDaemon;
use http_body_util::BodyExt;
use serde_json::json;

/// The forge token: the agent must never see it.
const FORGE_TOKEN: &str = "forge-s3cret-T0ken-0123456789abcdef";

/// What the fake forge saw.
#[derive(Default)]
struct Seen {
    /// `<method> <path>?<query>` and whether the token was right.
    requests: Vec<(String, bool)>,
}

#[derive(Clone)]
struct Forge {
    root: PathBuf,
    seen: Arc<Mutex<Seen>>,
}

/// `git http-backend` as a CGI program. The request needs the repo token
/// (Basic `bot:<token>`).
async fn forge(State(f): State<Forge>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("bot:{FORGE_TOKEN}"))
    );
    let ok = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(&want);
    let line = format!(
        "{} {}{}",
        parts.method,
        parts.uri.path(),
        parts
            .uri
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    f.seen.lock().unwrap().requests.push((line, ok));
    if !ok {
        return Response::builder()
            .status(401)
            .header("www-authenticate", "Basic realm=\"forge\"")
            .body(Body::from("no"))
            .unwrap();
    }
    let body = body.collect().await.unwrap().to_bytes();
    let h = |n: &str| -> String {
        parts
            .headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("GIT_PROJECT_ROOT", &f.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("REMOTE_USER", "bot")
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("PATH_INFO", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
        .env("CONTENT_TYPE", h("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_CONTENT_ENCODING", h("content-encoding"))
        .env("GIT_PROTOCOL", h("git-protocol"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(&body).unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    // The CGI answer: headers, a blank line, the body.
    let raw = out.stdout;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)))
        .unwrap();
    let head = String::from_utf8_lossy(&raw[..split.0]).into_owned();
    let mut r = Response::builder();
    let mut status = 200;
    for l in head.lines() {
        let (k, v) = l.split_once(':').unwrap();
        let v = v.trim();
        if k.eq_ignore_ascii_case("status") {
            status = v.split(' ').next().unwrap().parse().unwrap();
        } else {
            r = r.header(k, v);
        }
    }
    r.status(status)
        .body(Body::from(Bytes::copy_from_slice(
            &raw[split.0 + split.1..],
        )))
        .unwrap()
}

async fn start_forge(root: &Path) -> (SocketAddr, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = axum::Router::new().fallback(forge).with_state(Forge {
        root: root.to_owned(),
        seen: seen.clone(),
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, seen)
}

/// Run git with the env of a test: no global config, no prompt.
fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap()
}

/// [`git`] that must pass; its stdout.
fn git_ok(dir: &Path, args: &[&str]) -> String {
    let o = git(dir, args);
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_owned()
}

/// git as the agent: the helper of the checkout is replaced by a script
/// that prints the agent's proxy credential, as `terrarium repo
/// credential` does in the sandbox.
fn agent_git(dir: &Path, helper: &Path, args: &[&str]) -> Output {
    let h = format!("credential.helper={}", helper.display());
    let mut all = vec!["-c", "credential.helper=", "-c", &h];
    all.extend_from_slice(args);
    git(dir, &all)
}

/// A helper script that answers `password`.
fn helper_script(dir: &Path, name: &str, password: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(
        &p,
        format!("#!/bin/sh\ntest \"$1\" = get || exit 0\necho username=terrarium\necho password={password}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn forge_head(root: &Path, branch: &str) -> Option<String> {
    let o = git(
        &root.join("app.git"),
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{branch}"),
        ],
    );
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Each file under `dir` that holds `needle`.
fn files_with(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file()
                && std::fs::read(&p).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
            {
                hits.push(p);
            }
        }
    }
    hits
}

async fn proxy_get(url: &str, token: Option<&str>) -> (u16, HeaderMap, String) {
    let c = reqwest::Client::new();
    let mut r = c.get(url);
    if let Some(t) = token {
        r = r.basic_auth("terrarium", Some(t));
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_proxy_enforces_push_deny_and_keeps_the_token() {
    // The forge: a bare repo with one commit on main.
    let forge_dir = tempfile::tempdir().unwrap();
    let root = forge_dir.path();
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git_ok(root, &["init", "-q", "--bare", "-b", "main", "app.git"]);
    git_ok(
        &root.join("app.git"),
        &["config", "http.receivepack", "true"],
    );
    git_ok(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git_ok(&seed, &["add", "a.txt"]);
    git_ok(&seed, &["commit", "-q", "-m", "one"]);
    git_ok(&seed, &["push", "-q", "../app.git", "main"]);
    let (forge_addr, seen) = start_forge(root).await;
    let repo_url = format!("http://{forge_addr}/app.git");

    // The daemon, with the proxy on a free port.
    let mut td = TestDaemon::start().await;
    td.web.git_proxy_listen = Some("127.0.0.1:0".into());
    td.boot(None, None).await;
    let proxy_addr = td.daemon.as_ref().unwrap().git_proxy_addr.unwrap();
    td.post(
        "/api/credentials",
        json!({"name": "forge", "kind": "token", "value": FORGE_TOKEN, "username": "bot"}),
    )
    .await
    .ok();
    td.project("p").await;
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"name": "app", "url": repo_url, "credential": "forge",
                   "push_deny": ["op/*"], "push_via": "proxy"}),
        )
        .await;
    assert_eq!(r.ok()["push_via"], "proxy");
    let agent = td.agent("p", "w", json!({})).await;
    td.project("other").await;
    td.agent("other", "x", json!({})).await;

    // The checkout: the daemon clones from the forge with the token, and
    // origin is the proxy.
    let r = td
        .post_as("p/w", "/api/repos/app/checkout", json!({}))
        .await;
    assert_eq!(r.ok()["action"], "cloned", "{:?}", r.body);
    assert_eq!(r.body["push_via"], "proxy");
    let ws = PathBuf::from(agent["workspace"].as_str().unwrap()).join("app");
    let proxy_url = format!("http://{proxy_addr}/git/p/app.git");
    assert_eq!(
        git_ok(&ws, &["config", "--get", "remote.origin.url"]),
        proxy_url
    );

    // The credential helper: the proxy host gives the agent's proxy
    // token; the forge URL of the repo gives nothing (use only).
    let r = td
        .post_as(
            "p/w",
            "/api/repos/credential",
            json!({"protocol": "http", "host": proxy_addr.to_string(), "path": "git/p/app.git"}),
        )
        .await;
    let token = r.ok()["password"].as_str().unwrap().to_owned();
    assert_eq!(r.body["username"], "terrarium");
    assert!(token.starts_with("tgp."));
    let r = td
        .post_as(
            "p/w",
            "/api/repos/credential",
            json!({"protocol": "http", "host": forge_addr.to_string(), "path": "app.git"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 409, "{:?}", r.body);
    assert!(r.body.to_string().contains("git proxy"));
    assert!(!r.body.to_string().contains(FORGE_TOKEN));
    let other_token = td
        .post_as(
            "other/x",
            "/api/repos/credential",
            json!({"protocol": "http", "host": proxy_addr.to_string()}),
        )
        .await
        .ok()["password"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(other_token, token);

    let helpers = tempfile::tempdir().unwrap();
    let helper = helper_script(helpers.path(), "agent", &token);

    // An allowed push goes through.
    std::fs::write(ws.join("b.txt"), "two\n").unwrap();
    git_ok(&ws, &["add", "b.txt"]);
    git_ok(&ws, &["commit", "-q", "-m", "two"]);
    let o = agent_git(&ws, &helper, &["push", "origin", "main"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        forge_head(root, "main"),
        Some(git_ok(&ws, &["rev-parse", "HEAD"]))
    );

    // A denied branch: the pre-push hook refuses it first.
    git_ok(&ws, &["checkout", "-q", "-b", "op/x"]);
    std::fs::write(ws.join("c.txt"), "three\n").unwrap();
    git_ok(&ws, &["add", "c.txt"]);
    git_ok(&ws, &["commit", "-q", "-m", "three"]);
    let o = agent_git(&ws, &helper, &["push", "origin", "op/x"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("pre-push"));
    // With --no-verify the hook does not run: the proxy refuses it.
    let o = agent_git(&ws, &helper, &["push", "--no-verify", "origin", "op/x"]);
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(!o.status.success(), "{err}");
    assert!(err.contains("push_deny"), "{err}");
    assert!(err.contains("op/x"), "{err}");
    assert!(!err.contains(FORGE_TOKEN));
    assert_eq!(forge_head(root, "op/x"), None);
    // Under another name of the same ref, and with an allowed ref in the
    // same push: nothing changes on the forge.
    let main_before = forge_head(root, "main");
    git_ok(&ws, &["checkout", "-q", "main"]);
    std::fs::write(ws.join("d.txt"), "four\n").unwrap();
    git_ok(&ws, &["add", "d.txt"]);
    git_ok(&ws, &["commit", "-q", "-m", "four"]);
    let o = agent_git(
        &ws,
        &helper,
        &[
            "push",
            "--no-verify",
            "origin",
            "main",
            "main:refs/heads/op/y",
        ],
    );
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(!o.status.success(), "{err}");
    assert!(err.contains("op/y"), "{err}");
    assert_eq!(forge_head(root, "op/y"), None);
    assert_eq!(forge_head(root, "main"), main_before);
    // A delete of a denied branch is refused too (the forge has one).
    git_ok(&seed, &["push", "-q", "../app.git", "main:op/keep"]);
    let o = agent_git(&ws, &helper, &["push", "--no-verify", "origin", ":op/keep"]);
    assert!(!o.status.success());
    assert!(forge_head(root, "op/keep").is_some());

    // Fetch and clone through the proxy.
    git_ok(
        &seed,
        &["commit", "-q", "--allow-empty", "-m", "from the forge"],
    );
    git_ok(
        &seed,
        &["push", "-q", "-f", "../app.git", "HEAD:refs/heads/feature"],
    );
    let o = agent_git(&ws, &helper, &["fetch", "origin"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        git_ok(&ws, &["rev-parse", "origin/feature"]),
        forge_head(root, "feature").unwrap()
    );
    let clones = tempfile::tempdir().unwrap();
    let o = agent_git(clones.path(), &helper, &["clone", "-q", &proxy_url, "copy"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(clones.path().join("copy/b.txt").exists());

    // No credential: 401 with a Basic challenge. A bad token: 401. An
    // agent of another project: 403. A repo that is not there: 404.
    let refs = format!("{proxy_url}/info/refs?service=git-upload-pack");
    let (s, h, _) = proxy_get(&refs, None).await;
    assert_eq!(s, 401);
    assert!(h.get("www-authenticate").is_some());
    let forged = format!("{}00", &token[..token.len() - 2]);
    assert_eq!(proxy_get(&refs, Some(&forged)).await.0, 401);
    assert_eq!(proxy_get(&refs, Some(FORGE_TOKEN)).await.0, 401);
    let (s, _, body) = proxy_get(&refs, Some(&other_token)).await;
    assert_eq!(s, 403, "{body}");
    let other_helper = helper_script(helpers.path(), "other", &other_token);
    let o = agent_git(clones.path(), &other_helper, &["ls-remote", &proxy_url]);
    assert!(!o.status.success());
    let missing = format!("http://{proxy_addr}/git/p/nope.git/info/refs?service=git-upload-pack");
    assert_eq!(proxy_get(&missing, Some(&token)).await.0, 404);
    // Only smart HTTP: no dumb file reads, no other service.
    assert_eq!(
        proxy_get(&format!("{proxy_url}/HEAD"), Some(&token))
            .await
            .0,
        404
    );
    assert_eq!(
        proxy_get(&format!("{proxy_url}/info/refs"), Some(&token))
            .await
            .0,
        403
    );

    // The forge saw the token on each request that the proxy sent (the
    // first request is the daemon's own clone, which git sends with no
    // credential first). Only the allowed push reached its receive-pack.
    let seen = seen.lock().unwrap().requests.clone();
    assert!(!seen[0].1, "{seen:?}");
    assert!(seen[1..].iter().all(|(_, ok)| *ok), "{seen:?}");
    let pushes = seen
        .iter()
        .filter(|(l, _)| l.starts_with("POST /app.git/git-receive-pack"))
        .count();
    assert_eq!(pushes, 1, "{seen:?}");

    // The audit has the push and the refusals, with no token.
    let audit = td.get("/api/audit?action=repo.push").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert!(
        rows.iter().any(|r| r["detail"]["result"] == "forwarded"),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|r| r["detail"]["result"] == "refused"),
        "{rows:?}"
    );
    assert!(!audit.body.to_string().contains(FORGE_TOKEN));

    // The forge token is nowhere on disk in plain text: not in the
    // workspace (its .git/config too), the agent dirs, or the state dir.
    let hits = files_with(td.dir.path(), FORGE_TOKEN.as_bytes());
    assert!(hits.is_empty(), "{hits:?}");
    assert!(!git_ok(&ws, &["config", "--list"]).contains(FORGE_TOKEN));

    // A direct repo with the same URL, checked out into the same dir:
    // the update puts the forge URL back into origin (a repo that moves
    // from proxy to direct).
    td.post(
        "/api/projects/p/repos",
        json!({"name": "app2", "url": repo_url, "credential": "forge",
               "push_via": "direct"}),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "p/w",
            "/api/repos/app2/checkout",
            json!({"branch": "main", "path": "app"}),
        )
        .await;
    assert_eq!(r.ok()["push_via"], "direct", "{:?}", r.body);
    assert_eq!(
        git_ok(&ws, &["config", "--get", "remote.origin.url"]),
        repo_url
    );
    // The proxy does not serve a direct repo.
    let direct = format!("http://{proxy_addr}/git/p/app2.git/info/refs?service=git-upload-pack");
    assert_eq!(proxy_get(&direct, Some(&token)).await.0, 404);
}

#[tokio::test]
async fn push_via_rules_at_attach_and_with_the_proxy_off() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    // push_via = proxy needs an http(s) URL with no user in it.
    for url in [
        "git@github.com:a/b.git",
        "file:///repos/b.git",
        "https://u:p@x/b.git",
    ] {
        td.post(
            "/api/projects/p/repos",
            json!({"url": url, "push_via": "proxy"}),
        )
        .await
        .err(422, "invalid");
    }
    td.post(
        "/api/projects/p/repos",
        json!({"url": "https://x/b.git", "push_via": "ssh"}),
    )
    .await
    .err(400, "bad_request");
    // The proxy is off in this daemon: a checkout of a proxy repo fails
    // closed, and the helper does not give its token.
    td.post(
        "/api/credentials",
        json!({"name": "tok", "kind": "token", "value": FORGE_TOKEN}),
    )
    .await
    .ok();
    td.post(
        "/api/projects/p/repos",
        json!({"name": "app", "url": "https://git.example.com/a/app.git",
               "credential": "tok", "push_via": "proxy"}),
    )
    .await
    .ok();
    td.agent("p", "w", json!({})).await;
    let r = td
        .post_as("p/w", "/api/repos/app/checkout", json!({}))
        .await;
    assert_eq!(r.status.as_u16(), 409, "{:?}", r.body);
    assert!(r.body.to_string().contains("TERRARIUM_GIT_PROXY_LISTEN"));
    let r = td
        .post_as(
            "p/w",
            "/api/repos/credential",
            json!({"protocol": "https", "host": "git.example.com", "path": "a/app.git"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 409);
    assert!(!r.body.to_string().contains(FORGE_TOKEN));
    // With no push_via and the proxy off, an https repo with push_deny is
    // direct: the helper answers as before.
    td.post(
        "/api/projects/p/repos",
        json!({"name": "hub", "url": "https://git.example.com/a/hub.git",
               "credential": "tok", "push_deny": ["op/*"]}),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "p/w",
            "/api/repos/credential",
            json!({"protocol": "https", "host": "git.example.com", "path": "a/hub.git"}),
        )
        .await;
    assert_eq!(r.ok()["password"], FORGE_TOKEN);
}

#[tokio::test]
async fn the_default_rule_proxies_https_repos_with_push_deny() {
    let mut td = TestDaemon::start().await;
    td.web.git_proxy_listen = Some("127.0.0.1:0".into());
    td.boot(None, None).await;
    td.post(
        "/api/credentials",
        json!({"name": "tok", "kind": "token", "value": FORGE_TOKEN}),
    )
    .await
    .ok();
    td.project("p").await;
    for (name, deny) in [("guarded", json!(["op/*"])), ("open", json!([]))] {
        td.post(
            "/api/projects/p/repos",
            json!({"name": name, "url": format!("https://git.example.com/a/{name}.git"),
                   "credential": "tok", "push_deny": deny}),
        )
        .await
        .ok();
    }
    td.agent("p", "w", json!({})).await;
    let ask = |name: &str| json!({"protocol": "https", "host": "git.example.com", "path": format!("a/{name}.git")});
    // With push_deny: proxied, the token stays in the daemon.
    let r = td
        .post_as("p/w", "/api/repos/credential", ask("guarded"))
        .await;
    assert_eq!(r.status.as_u16(), 409);
    // With no push_deny: direct, as before the proxy.
    let r = td
        .post_as("p/w", "/api/repos/credential", ask("open"))
        .await;
    assert_eq!(r.ok()["password"], FORGE_TOKEN);
}
