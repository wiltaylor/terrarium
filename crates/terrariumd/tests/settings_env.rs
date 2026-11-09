//! The web settings come from the env (or the flags) in one place,
//! `Settings::web`, and `DaemonConfig` carries them to the routers. The
//! env cannot change in a test (`unsafe_code` is forbidden), so the parent
//! test runs this binary again as a child with the env set.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use axum::body::Body;
use clap::Parser;
use common::TestDaemon;
use http_body_util::BodyExt;
use hyper::{Request, StatusCode};
use terrariumd::settings::Settings;

const CHILD: &str = "TERRARIUM_TEST_SETTINGS_CHILD";

#[test]
fn the_env_reaches_the_routers() {
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args(["--exact", "child_reads_the_env", "--nocapture"])
        .env(CHILD, "1")
        .env("TERRARIUM_PUBLIC_URL", "https://t.example.com/")
        .env("TERRARIUM_ARTIFACT_PUBLIC_URL", "https://art.example.com")
        .env("TERRARIUM_ARTIFACT_LISTEN", "")
        .env("TERRARIUM_WS_QUERY_TOKEN", "1")
        .env("TERRARIUM_SESSION_TTL", "30m")
        .env("TERRARIUM_CLI_SESSION_TTL", "2d")
        // Removed: the daemon ignores it, and the start does not fail.
        .env("TERRARIUM_HARNESS_VERSION_POLICY", "refuse")
        .env("TERRARIUM_HARNESS_UPDATE_MAIL", "on")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the child test ran: {text}");
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    h: &[(&str, &str)],
) -> axum::response::Response {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in h {
        req = req.header(*k, *v);
    }
    tower::ServiceExt::oneshot(app, req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn child_reads_the_env() {
    if std::env::var(CHILD).is_err() {
        return;
    }
    let s = Settings::try_parse_from(["terrariumd"]).unwrap();
    let web = s.web();
    assert_eq!(web.public_url.as_deref(), Some("https://t.example.com"));
    assert_eq!(
        web.artifact_public_url.as_deref(),
        Some("https://art.example.com")
    );
    assert!(web.ws_query_token);
    assert_eq!(web.session_ttl.as_secs(), 1800);
    assert_eq!(web.cli_session_ttl.as_secs(), 2 * 86_400);
    // `TERRARIUM_ARTIFACT_LISTEN=` (empty): no listener of its own.
    assert_eq!(s.artifact_listen(), None);
    assert_eq!(
        s.harness_update_mail,
        terrariumd::harness_auto_update::UpdateMail::On
    );

    let mut td = TestDaemon::start().await;
    td.web = web;
    td.artifact_listen = s.artifact_listen();
    td.boot(None, None).await;
    let daemon = td.daemon.as_ref().unwrap();
    assert!(daemon.artifact_addr.is_none());
    let state = daemon.state.clone();
    let app = || terrariumd::api::tcp_router(state.clone(), "tok".into()).unwrap();

    // The dashboard port serves /a/ again (a signed-URL refusal, not the
    // SPA page), and the CSP frames artifacts from 'self'.
    let r = call(app(), "GET", "/a/ART-1/x?exp=1&sig=00", &[]).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = call(app(), "GET", "/", &[("host", "t.lan:8080")]).await;
    let csp = r.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.contains("frame-src 'self'"), "{csp}");
    // The session TTL.
    let r = call(
        app(),
        "POST",
        "/api/session",
        &[("authorization", "Bearer tok")],
    )
    .await;
    let set = r.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(set.contains("Max-Age=1800"), "{set}");
    // `?token=` is on.
    let r = call(
        app(),
        "GET",
        "/api/events?token=tok",
        &[
            ("host", "localhost"),
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
    )
    .await;
    assert_ne!(r.status(), StatusCode::UNAUTHORIZED);
    // The cookie WebSocket Origin check takes the public origin.
    let r = call(
        app(),
        "POST",
        "/api/session",
        &[("authorization", "Bearer tok")],
    )
    .await;
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let _ = r.into_body().collect().await.unwrap();
    let r = call(
        app(),
        "GET",
        "/api/events",
        &[
            ("host", "inner:8080"),
            ("origin", "https://t.example.com"),
            ("cookie", &cookie),
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
    )
    .await;
    assert_ne!(r.status(), StatusCode::FORBIDDEN);
    assert_ne!(r.status(), StatusCode::UNAUTHORIZED);
}

/// Without the env var, an installed harness update sends no mail.
#[test]
fn the_harness_update_mail_defaults_to_off() {
    if std::env::var(CHILD).is_ok() || std::env::var("TERRARIUM_HARNESS_UPDATE_MAIL").is_ok() {
        return;
    }
    let s = Settings::try_parse_from(["terrariumd"]).unwrap();
    assert_eq!(
        s.harness_update_mail,
        terrariumd::harness_auto_update::UpdateMail::Off
    );
    let s = Settings::try_parse_from(["terrariumd", "--harness-update-mail", "on"]).unwrap();
    assert_eq!(
        s.harness_update_mail,
        terrariumd::harness_auto_update::UpdateMail::On
    );
    // The removed flag is an error on the command line; the env var is
    // ignored (`the_env_reaches_the_routers`).
    assert!(Settings::try_parse_from(["terrariumd", "--harness-version-policy", "warn"]).is_err());
}

/// The `Settings` env keys that compose.yaml does not pass to the
/// `terrarium` service on purpose, each with its reason. Everything else
/// that settings.rs reads must be in the service `environment` list with
/// the daemon's default (an unset `.env` value must keep the default:
/// most value parsers refuse an empty string), so a deployment can set
/// every knob from `.env` (TER-428: `TERRARIUM_QUOTA_SAMPLE_KEEP` was
/// missing, so the quota sample retention could not be set).
const NOT_PASSED: &[(&str, &str)] = &[
    (
        "TERRARIUM_API_SOCKET",
        "the operator CLI socket, fixed in the container",
    ),
    (
        "TERRARIUM_ARTIFACT_LISTEN",
        "the published port 8081 is fixed in the container",
    ),
    ("TERRARIUM_CLI", "the image puts the CLI next to terrariumd"),
    ("TERRARIUM_CONFIG_DIR", "the /etc/terrarium bind mount"),
    ("TERRARIUM_CONFIG_REPO", "the /config/terrarium bind mount"),
    (
        "TERRARIUM_DEFAULTS_DIR",
        "the image fills /opt/terrarium/defaults",
    ),
    ("TERRARIUM_HARNESS_HOME", "the /auth/<harness> bind mounts"),
    (
        "TERRARIUM_HOST_DIST_DIR",
        "the image fills /opt/terrarium/dist",
    ),
    ("TERRARIUM_INSECURE_AGENT_HEADER", "tests only"),
    (
        "TERRARIUM_LISTEN",
        "the published port 8080 is fixed in the container",
    ),
    ("TERRARIUM_NPM", "the npm of the image"),
    ("TERRARIUM_NO_HARNESS_INSTALL", "development only"),
    (
        "TERRARIUM_PASSWORD_FILE",
        "a deployment sets TERRARIUM_PASSWORD instead",
    ),
    ("TERRARIUM_PROMPTS_DIR", "the prompts of the config repo"),
    ("TERRARIUM_REPOS_DIR", "the /repos bind mount"),
    (
        "TERRARIUM_RUNNER",
        "the image puts the runner next to terrariumd",
    ),
    (
        "TERRARIUM_RUNNER_MODE",
        "the container runs the default process mode",
    ),
    ("TERRARIUM_RUNTIME_DIR", "the /run of the container"),
    ("TERRARIUM_SANDBOX", "bwrap, the sandbox of the image"),
    ("TERRARIUM_SANDBOX_NEW_SESSION", "development only"),
    ("TERRARIUM_STATE_DIR", "the /state bind mount"),
    ("TERRARIUM_TOOLS_DIR", "the /opt/terrarium/tools bind mount"),
    ("TERRARIUM_WORK_DIR", "the /work bind mount"),
];

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The keys of the `environment` mapping of the `terrarium` service.
fn compose_env_keys(compose: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut in_service = false;
    let mut in_env = false;
    for line in compose.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent <= 2 {
            in_service = trimmed == "terrarium:";
            in_env = false;
        } else if in_service && indent == 4 {
            in_env = trimmed == "environment:";
        } else if in_env && indent == 6 {
            let entry = trimmed.strip_prefix("- ").unwrap_or(trimmed);
            let Some(key) = entry.split(':').next() else {
                continue;
            };
            let key = key.trim();
            if key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
                && !key.is_empty()
            {
                keys.push(key.to_string());
            }
        }
    }
    assert!(
        !keys.is_empty(),
        "no environment keys parsed from compose.yaml"
    );
    keys
}

/// The env key of one `env = ...` token of settings.rs: a string literal,
/// or a const path that this resolves in its module (`crate::a::b::C`,
/// `paths::C` from terrarium-core, or a const of settings.rs itself).
fn resolve_env_token(token: &str, manifest: &Path) -> String {
    let token = token.trim();
    if let Some(name) = token.strip_prefix('"') {
        return name.trim_end_matches('"').to_string();
    }
    let (file, name) = match token.strip_prefix("crate::") {
        Some(rest) => {
            let mut parts: Vec<&str> = rest.split("::").collect();
            let name = parts.pop().unwrap_or_default().to_string();
            (daemon_module(manifest, &parts), name)
        }
        None if token.contains("::") => {
            let alias = token.split("::").next().unwrap_or_default();
            let name = token.rsplit("::").next().unwrap_or_default().to_string();
            let file = match alias {
                "paths" => manifest.join("../terrarium-core/src/paths.rs"),
                _ => panic!("extend this test: the env const {token} of another crate"),
            };
            (file, name)
        }
        None => (manifest.join("src/settings.rs"), token.to_string()),
    };
    let text = read(&file);
    let prefix = format!("pub const {name}: &str");
    // The const line may wrap; join it with the next line.
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if !line.trim_start().starts_with(&prefix) {
            continue;
        }
        let mut decl = line.to_string();
        if !decl.contains('"') {
            decl.push(' ');
            decl.push_str(lines.next().unwrap_or(""));
        }
        let value = decl.split('=').nth(1).unwrap_or("");
        return value
            .split('"')
            .nth(1)
            .unwrap_or_else(|| panic!("no string in {token} at {}", file.display()))
            .to_string();
    }
    panic!("{prefix} not found for {token} at {}", file.display());
}

/// The file of a `crate::<a>::<b>` module path of the daemon.
fn daemon_module(manifest: &Path, parts: &[&str]) -> PathBuf {
    let rel = parts.join("/");
    let file = manifest.join(format!("src/{rel}.rs"));
    if file.exists() {
        return file;
    }
    manifest.join(format!("src/{rel}/mod.rs"))
}

/// TER-428: every env key of settings.rs is passed to the `terrarium`
/// service of compose.yaml, or listed in `NOT_PASSED` with a reason.
#[test]
fn every_settings_env_key_reaches_the_compose_service() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..");
    let passed = compose_env_keys(&read(&root.join("compose.yaml")));
    let src = read(&manifest.join("src/settings.rs"));
    let mut keys = BTreeSet::new();
    for line in src.lines() {
        let Some(at) = line.find("env = ") else {
            continue;
        };
        let rest = &line[at + "env = ".len()..];
        let end = rest.find(&[',', ')', ']'][..]).unwrap_or(rest.len());
        let token = rest[..end].trim();
        let plain = token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
        assert!(
            token.starts_with('"') || plain,
            "cannot read the env key of `{token}` in settings.rs"
        );
        keys.insert(resolve_env_token(token, manifest));
    }
    let mut problems: Vec<String> = Vec::new();
    for key in &keys {
        if !passed.contains(key) && !NOT_PASSED.iter().any(|(n, _)| n == key) {
            problems.push(format!(
                "{key}: pass it to the compose `terrarium` environment with the daemon default, \
                 or add it to NOT_PASSED with its reason"
            ));
        }
    }
    for (name, _) in NOT_PASSED {
        if passed.contains(*name) {
            problems.push(format!(
                "{name}: passed by compose now, remove it from NOT_PASSED"
            ));
        }
        if !keys.contains(*name) {
            problems.push(format!(
                "{name}: not a Settings env key, remove it from NOT_PASSED"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "settings.rs env keys and compose.yaml are out of sync:\n{}",
        problems.join("\n")
    );
}
