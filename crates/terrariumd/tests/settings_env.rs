//! The web settings come from the env (or the flags) in one place,
//! `Settings::web`, and `DaemonConfig` carries them to the routers. The
//! env cannot change in a test (`unsafe_code` is forbidden), so the parent
//! test runs this binary again as a child with the env set.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::body::Body;
use clap::Parser;
use common::TestDaemon;
use http_body_util::BodyExt;
use hyper::{Request, StatusCode};
use serde_json::json;
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
