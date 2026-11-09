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
use regex::Regex;
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

/// The daemon env keys that compose.yaml does not pass to the `terrarium`
/// service on purpose, each with its reason. Everything else that
/// settings.rs or a `std::env` read in src/ reads (TER-439) must be in the
/// service `environment` list with
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
        "TERRARIUM_HARNESS_VERSION_POLICY",
        "removed: the daemon ignores it with a warning",
    ),
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
    ("TERRARIUM_SSH", "the ssh of the image"),
    ("TERRARIUM_STATE_DIR", "the /state bind mount"),
    ("TERRARIUM_TOOLS_DIR", "the /opt/terrarium/tools bind mount"),
    ("TERRARIUM_WORK_DIR", "the /work bind mount"),
];

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The keys of the `environment` of the `terrarium` service: the mapping
/// form (`KEY: value`) or the list form (`- KEY=value`, `- KEY`). An entry
/// that is neither fails with the line.
fn compose_env_keys(compose: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut in_service = false;
    let mut in_env = false;
    for line in compose.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent <= 2 {
            in_service = trimmed == "terrarium:";
            in_env = false;
        } else if in_service && indent == 4 {
            in_env = trimmed == "environment:";
        } else if in_env && indent == 6 {
            let key = match trimmed.strip_prefix("- ") {
                Some(item) => item
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .split('=')
                    .next(),
                None => trimmed.split(':').next(),
            }
            .unwrap_or_default()
            .trim();
            assert!(
                !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()),
                "compose.yaml: cannot read the env key of `{trimmed}` in the `terrarium` \
                 environment; write `KEY: value` or `- KEY=value`"
            );
            keys.push(key.to_string());
        }
    }
    assert!(
        !keys.is_empty(),
        "no environment keys parsed from compose.yaml"
    );
    keys
}

#[test]
fn compose_env_keys_reads_the_mapping_and_the_list_form() {
    let mapping = "services:\n  terrarium:\n    environment:\n      # a note\n      \
                   TERRARIUM_A: ${TERRARIUM_A:-x}\n      TERRARIUM_B: \"1\"\n    volumes:\n      \
                   - /a:/a\n";
    assert_eq!(compose_env_keys(mapping), ["TERRARIUM_A", "TERRARIUM_B"]);
    let list = "services:\n  terrarium:\n    environment:\n      - TERRARIUM_A=${TERRARIUM_A:-x}\n      \
                - \"TERRARIUM_B=1\"\n      - TERRARIUM_C\n  other:\n    environment:\n      \
                - TERRARIUM_D=1\n";
    assert_eq!(
        compose_env_keys(list),
        ["TERRARIUM_A", "TERRARIUM_B", "TERRARIUM_C"]
    );
    let bad = "services:\n  terrarium:\n    environment:\n      - TERRARIUM_A: 1\n";
    let err = std::panic::catch_unwind(|| compose_env_keys(bad)).unwrap_err();
    let msg = err.downcast_ref::<String>().unwrap();
    assert!(msg.contains("cannot read the env key"), "{msg}");
}

/// The env key of one token: a string literal, or a const path that this
/// resolves in its module (`crate::a::b::C` or `terrariumd::a::b::C`,
/// `paths::C` from terrarium-core, `terrarium_updater::a::C`, or a const
/// of `local`, the file of the token).
fn resolve_env_token(token: &str, manifest: &Path, local: &Path) -> String {
    let token = token.trim();
    if let Some(name) = token.strip_prefix('"') {
        return name.trim_end_matches('"').to_string();
    }
    let (file, name) = match token.rsplit_once("::") {
        None => (local.to_path_buf(), token),
        Some((module, name)) => {
            let parts: Vec<&str> = module.split("::").collect();
            let file = match parts[0] {
                "crate" | "terrariumd" => daemon_module(manifest, &parts[1..]),
                "paths" => manifest.join("../terrarium-core/src/paths.rs"),
                "terrarium_updater" => manifest.join(format!(
                    "../terrarium-updater/src/{}.rs",
                    parts[1..].join("/")
                )),
                _ => panic!("extend this test: the env const {token} of another crate"),
            };
            (file, name)
        }
    };
    let text = read(&file);
    let prefix = format!("const {name}: &str");
    // The const line may wrap; join it with the next line.
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let decl = line.trim_start();
        let decl = decl
            .strip_prefix("pub(crate) ")
            .or_else(|| decl.strip_prefix("pub "))
            .unwrap_or(decl);
        if !decl.starts_with(&prefix) {
            continue;
        }
        let mut decl = decl.to_string();
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

/// The files whose env reads go through a closure that the file hands to
/// another function, with the names of the lookups that the file calls
/// with the key. An empty list: the closure reads no `TERRARIUM_` key.
const INDIRECT_READS: &[(&str, &[&str])] = &[
    // `HostListen::from_vars`.
    ("hosts.rs", &["set"]),
    // `hook_url_from`.
    ("ingest.rs", &["set"]),
    // `Schedule::parse`.
    ("backup/schedule.rs", &["get"]),
    // `password::SECRET_ENV`: only whether it is set, to scrub it.
    ("main.rs", &[]),
    // The GitHub token env of the Copilot probe.
    ("probes/github.rs", &[]),
    // The npm registry env.
    ("harness_versions.rs", &[]),
    // The locale env that a harness inherits.
    ("runner_supervisor.rs", &[]),
];

/// The text of a source file without its `#[cfg(test)] mod ...` block.
fn without_test_module(text: &str) -> &str {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut offset = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_end() == "#[cfg(test)]" {
            let next = lines[i + 1..].iter().find(|l| !l.starts_with("#["));
            if next.is_some_and(|l| l.starts_with("mod ")) {
                return &text[..offset];
            }
        }
        offset += line.len();
    }
    text
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// TER-439: the `TERRARIUM_` keys that the daemon reads with
/// `std::env::var` or `std::env::var_os` anywhere in src/, outside the
/// test modules. A read with a key from a parameter follows the lookup
/// that passes it: a closure (`let var = |k| ...`), a small function
/// (`fn cap(name: &str ...)`), or the names in `INDIRECT_READS`. The key
/// at each call is a string literal or a const.
fn daemon_env_reads(manifest: &Path) -> BTreeSet<String> {
    let src = manifest.join("src");
    let call = Regex::new(r"env::var(?:_os)?\(([^)]*)\)").unwrap();
    let closure = Regex::new(r"let (\w+) = \|").unwrap();
    let func = Regex::new(r"fn (\w+)\(").unwrap();
    let param = Regex::new(r"^[a-z_][a-z0-9_]*$").unwrap();
    let key_token = Regex::new(r#"^("[^"]*"|(\w+::)*[A-Z_][A-Z0-9_]*)$"#).unwrap();
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let mut keys = BTreeSet::new();
    for file in files {
        let rel = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let full = read(&file);
        let text = without_test_module(&full);
        let lines: Vec<&str> = text.lines().collect();
        let mut lookups: BTreeSet<String> = BTreeSet::new();
        for (i, line) in lines.iter().enumerate() {
            for c in call.captures_iter(line) {
                let arg = c[1].trim();
                if !param.is_match(arg) {
                    continue;
                }
                if let Some((_, names)) = INDIRECT_READS.iter().find(|(f, _)| *f == rel) {
                    lookups.extend(names.iter().map(|n| (*n).to_string()));
                } else if let Some(m) = closure.captures(line) {
                    lookups.insert(m[1].to_string());
                } else if let Some(m) = lines[i.saturating_sub(3)..=i]
                    .iter()
                    .rev()
                    .find_map(|l| func.captures(l))
                {
                    lookups.insert(m[1].to_string());
                } else {
                    panic!(
                        "src/{rel}:{}: an env read with the key `{arg}` from a closure; \
                         add the file to INDIRECT_READS with its lookups",
                        i + 1
                    );
                }
            }
        }
        let mut tokens: Vec<String> = call
            .captures_iter(text)
            .map(|c| c[1].trim().to_string())
            .collect();
        for name in &lookups {
            let at = Regex::new(&format!(
                r"(?m)(?:^|[^\w.:])({})\(([^),]*)",
                regex::escape(name)
            ))
            .unwrap();
            for c in at.captures_iter(text) {
                if text[..c.get(1).unwrap().start()].ends_with("fn ") {
                    continue;
                }
                tokens.push(c[2].trim().to_string());
            }
        }
        for token in tokens {
            if !key_token.is_match(&token) {
                continue;
            }
            let key = resolve_env_token(&token, manifest, &file);
            if key.starts_with("TERRARIUM_") {
                keys.insert(key);
            }
        }
    }
    keys
}

/// The env keys of settings.rs (`env = ...` of the clap args).
fn settings_env_keys(manifest: &Path) -> BTreeSet<String> {
    let settings = manifest.join("src/settings.rs");
    let src = read(&settings);
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
        keys.insert(resolve_env_token(token, manifest, &settings));
    }
    keys
}

/// The keys that the scan must find: the TER-439 keys read outside
/// settings.rs. A scan that misses one is broken.
#[test]
fn the_env_scan_finds_the_reads_outside_settings() {
    let keys = daemon_env_reads(Path::new(env!("CARGO_MANIFEST_DIR")));
    for key in [
        "TERRARIUM_CARD_REMINDER",
        "TERRARIUM_COMPACT_MAX_AGE",
        "TERRARIUM_WAIT_DEFAULT_TIMEOUT",
        "TERRARIUM_RESEARCH_HELPERS_PER_DAY",
        "TERRARIUM_UPDATE_RELEASES_URL",
        "TERRARIUM_HOST_TLS_CERT",
        "TERRARIUM_BACKUP_KEEP",
    ] {
        assert!(keys.contains(key), "{key} not found in {keys:?}");
    }
}

/// TER-428, TER-439: every env key that the daemon reads (settings.rs and
/// each `std::env` read in src/) is passed to the `terrarium` service of
/// compose.yaml, or listed in `NOT_PASSED` with a reason.
#[test]
fn every_daemon_env_key_reaches_the_compose_service() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..");
    let passed = compose_env_keys(&read(&root.join("compose.yaml")));
    let mut keys = settings_env_keys(manifest);
    keys.extend(daemon_env_reads(manifest));
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
        if passed.iter().any(|p| p == *name) {
            problems.push(format!(
                "{name}: passed by compose now, remove it from NOT_PASSED"
            ));
        }
        if !keys.contains(*name) {
            problems.push(format!(
                "{name}: the daemon does not read it, remove it from NOT_PASSED"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "the daemon env keys and compose.yaml are out of sync:\n{}",
        problems.join("\n")
    );
}
