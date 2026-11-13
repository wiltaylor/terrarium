//! Windows paths cross a real host link without using the Linux filesystem.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::time::Duration;
use terrarium_host::protocol::{
    CheckoutMode, DaemonFrame, GitCredential, HostFrame, decode_control,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

#[tokio::test]
async fn windows_exec_and_copy_round_trip() {
    windows_round_trip(false).await;
}

#[tokio::test]
async fn windows_commands_only_exec_and_copy_send_no_linux_probes() {
    windows_round_trip(true).await;
}

async fn windows_round_trip(no_workspaces: bool) {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let agent = td
        .agent("p", "worker", json!({"policy":{"targets":["p-win"]}}))
        .await;
    let aid = agent["id"].as_str().unwrap();
    let token = td.post("/api/hosts", json!({"name": "windows"})).await.ok()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (listener, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0")
        .await
        .unwrap();
    let mut req = format!("ws://{addr}/hosts/connect")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(
        json!({
            "type": "hello", "protocol": 1, "host": "windows",
            "version": terrarium_version::FULL, "os": "windows", "arch": "x86_64",
            "path_kind": "windows", "capabilities": {"no_workspaces": no_workspaces}, "max_sessions": 16
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        decode_control::<DaemonFrame>(ws.next().await.unwrap().unwrap().to_text().unwrap())
            .unwrap(),
        DaemonFrame::Welcome(_)
    ));
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let fake = tokio::spawn(async move {
        while let Some(Ok(message)) = ws.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: DaemonFrame = decode_control(&text).unwrap();
            if let DaemonFrame::Ping { nonce } = frame {
                ws.send(Message::Text(
                    serde_json::to_string(&HostFrame::Pong { nonce })
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            } else {
                sent.send(frame).unwrap();
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        while state.executor.resource_pending("windows") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    td.post(
        "/api/targets",
        json!({"name":"win", "kind":"host", "host":"windows", "project":"p"}),
    )
    .await
    .ok();
    let cwd = r"C:\Terrarium\work\project\agent";
    td.post(
        "/api/targets/p-win/exec",
        json!({"cmd":["cmd.exe", "/c", "echo"], "cwd":cwd}),
    )
    .await
    .ok();
    let frame = tokio::time::timeout(Duration::from_secs(30), received.recv())
        .await
        .unwrap()
        .unwrap();
    let DaemonFrame::Exec(exec) = frame else {
        panic!("{frame:?}")
    };
    assert_eq!(
        exec.cmd, "cmd.exe",
        "native Windows argv must not be wrapped in sh"
    );
    assert_eq!(exec.args, ["/c", "echo"]);
    assert_eq!(exec.cwd.unwrap().to_str(), Some(cwd));
    // Named runs use this same exec start path. Neither ordinary commands
    // nor explicit shell sessions may receive the POSIX kill wrapper.
    td.post("/api/targets/p-win/exec", json!({"shell":true}))
        .await
        .ok();
    let frame = tokio::time::timeout(Duration::from_secs(30), received.recv())
        .await
        .unwrap()
        .unwrap();
    let DaemonFrame::Exec(shell) = frame else {
        panic!("{frame:?}")
    };
    assert_eq!(shell.cmd, "cmd.exe");
    assert!(shell.args.is_empty());
    assert!(shell.pty.is_some());
    for (direction, path) in [
        ("push", r"C:\Terrarium\work\project\agent\file"),
        ("pull", r"\\server\share\dir\file"),
    ] {
        td.post(
            "/api/targets/p-win/cp",
            json!({"direction":direction, "path":path}),
        )
        .await
        .ok();
        let frame = tokio::time::timeout(Duration::from_secs(30), received.recv())
            .await
            .unwrap()
            .unwrap();
        let actual = match frame {
            DaemonFrame::CpPush(cp) => cp.path,
            DaemonFrame::CpPull(cp) => cp.path,
            other => panic!("{other:?}"),
        };
        assert_eq!(actual.to_str(), Some(path));
    }
    for path in [
        r"C:\Terrarium\work\project\agent\..\other",
        r"C:\NUL.txt",
        r"C:\bad.",
        "/posix",
    ] {
        td.post(
            "/api/targets/p-win/cp",
            json!({"direction":"pull", "path":path}),
        )
        .await
        .err(422, "invalid");
        td.post(
            "/api/targets/p-win/exec",
            json!({"cmd":["cmd.exe"], "cwd":path}),
        )
        .await
        .err(422, "invalid");
    }
    td.post_as(aid, "/api/targets/p-win/exec", json!({"cmd":["cmd.exe"]}))
        .await
        .ok();
    let frame = tokio::time::timeout(Duration::from_secs(30), received.recv())
        .await
        .unwrap()
        .unwrap();
    let DaemonFrame::Exec(exec) = frame else {
        panic!("{frame:?}")
    };
    assert_eq!(exec.cmd, "cmd.exe");
    if no_workspaces {
        assert!(exec.cwd.is_none());
        assert!(!exec.env.contains_key("RESULTS_DIR"));
        assert!(!exec.env.contains_key("CARGO_TARGET_DIR"));
    } else {
        assert_eq!(
            exec.cwd.unwrap().to_str(),
            Some(r"C:\Terrarium\work\p\worker")
        );
        assert_eq!(
            exec.env["RESULTS_DIR"],
            r"C:\Terrarium\work\p\worker\results"
        );
    }
    td.post_as(
        aid,
        "/api/targets/p-win/cp",
        json!({"direction":"pull", "path":"c:/TERRARIUM/work/P/WORKER/file"}),
    )
    .await
    .ok();
    let frame = tokio::time::timeout(Duration::from_secs(30), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(frame, DaemonFrame::CpPull(_)));
    for path in [
        "C:/Terrarium/work/p/worker2/file",
        "D:/Terrarium/work/p/worker/file",
    ] {
        td.post_as(
            aid,
            "/api/targets/p-win/cp",
            json!({"direction":"pull", "path":path}),
        )
        .await
        .err(422, "invalid");
        td.post_as(
            aid,
            "/api/targets/p-win/exec",
            json!({"cmd":["cmd.exe"], "cwd":path}),
        )
        .await
        .err(422, "invalid");
    }
    // TER-274 D6: the managed checkout is a POSIX sh script. It is refused
    // for a Windows host before any frame reaches it.
    let repo = td
        .post(
            "/api/projects/p/repos",
            json!({"url": "https://example.invalid/app.git", "name": "app"}),
        )
        .await;
    let checkout = td
        .post(
            &format!("/api/repos/{}/checkout", repo.ok()["id"].as_str().unwrap()),
            json!({"target": "p-win", "path": r"C:\Terrarium\work\p\worker\app"}),
        )
        .await;
    checkout.err(422, "invalid");
    let message = checkout.body["error"]["message"].as_str().unwrap();
    assert!(message.contains("not supported yet"), "{message}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        received.try_recv().is_err(),
        "no frame for a refused checkout"
    );
    fake.abort();
    listener.abort();
}

const TOKEN: &str = "ghp_ter274SentinelTokenQ7w2E9r4T6y8";
const LOGIN: &str = "ter274-login-sentinel-Z3x5C7v9B1n";

/// TER-274 D8, D16, D17, F2, F5: a host with `windows_git_checkout` gets one
/// typed `git.checkout` frame in managed mode, inside the agent's workspace,
/// with run-as. The token and the SSH key are only in the typed field: never
/// in events, audit or error answers.
#[tokio::test]
async fn windows_git_checkout_goes_only_to_a_capable_host_in_managed_mode() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let agent = td
        .agent(
            "p",
            "worker",
            json!({"policy":{"targets":["p-win", "p-plain"]}}),
        )
        .await;
    let aid = agent["id"].as_str().unwrap();
    td.put(
        "/api/secrets/p/win-login",
        json!({"kind": "login", "fields": {"username": "ignored", "password": LOGIN}}),
    )
    .await
    .ok();
    td.put(
        "/api/secrets/p/gh",
        json!({"kind": "token", "fields": {"value": TOKEN}}),
    )
    .await
    .ok();
    let generated = td.post("/api/secrets/p/deploy/generate", json!({})).await;
    assert_eq!(generated.status.as_u16(), 201, "{}", generated.body);
    let token = td.post("/api/hosts", json!({"name": "windows"})).await.ok()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (listener, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0")
        .await
        .unwrap();
    let mut req = format!("ws://{addr}/hosts/connect")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(
        json!({
            "type": "hello", "protocol": 1, "host": "windows",
            "version": terrarium_version::FULL, "os": "windows", "arch": "x86_64",
            "path_kind": "windows",
            "capabilities": {"no_workspaces": false, "windows_git_checkout": true,
                "windows_run_as": true},
            "max_sessions": 16
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        decode_control::<DaemonFrame>(ws.next().await.unwrap().unwrap().to_text().unwrap())
            .unwrap(),
        DaemonFrame::Welcome(_)
    ));
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let fake = tokio::spawn(async move {
        while let Some(Ok(message)) = ws.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: DaemonFrame = decode_control(&text).unwrap();
            let replies = match &frame {
                DaemonFrame::Ping { nonce } => vec![HostFrame::Pong { nonce: *nonce }],
                // A failing checkout: its error answer must not echo secrets.
                DaemonFrame::GitCheckout(r) if r.url.contains("fail") => vec![
                    HostFrame::Started {
                        session: r.session,
                        pid: None,
                    },
                    HostFrame::Exit {
                        session: r.session,
                        code: Some(2),
                        signal: None,
                        reason: terrarium_host::protocol::ExitReason::Exited,
                    },
                ],
                DaemonFrame::GitCheckout(r) => vec![
                    HostFrame::Started {
                        session: r.session,
                        pid: None,
                    },
                    HostFrame::Result {
                        session: r.session,
                        value: json!({"action":"cloned","head":"0123","branch":"main",
                            "path": r.dest, "note": null}),
                    },
                    HostFrame::Exit {
                        session: r.session,
                        code: Some(0),
                        signal: None,
                        reason: terrarium_host::protocol::ExitReason::Exited,
                    },
                ],
                _ => Vec::new(),
            };
            for reply in replies {
                ws.send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                    .await
                    .unwrap();
            }
            if !matches!(frame, DaemonFrame::Ping { .. }) {
                sent.send(frame).unwrap();
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        while state.executor.resource_pending("windows") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    td.post(
        "/api/targets",
        json!({"name":"win", "kind":"host", "host":"windows", "project":"p",
            "windows_run_as": {"account": "LAB\\worker", "password_secret": "win-login"}}),
    )
    .await
    .ok();
    td.post(
        "/api/targets",
        json!({"name":"plain", "kind":"host", "host":"windows", "project":"p"}),
    )
    .await
    .ok();
    let mut repos = Vec::new();
    for (name, url, credential) in [
        ("app", "https://example.invalid/app.git", "gh"),
        ("keyed", "https://example.invalid/keyed.git", "deploy"),
        ("broken", "https://example.invalid/fail.git", "gh"),
    ] {
        let repo = td
            .post(
                "/api/projects/p/repos",
                json!({"url": url, "name": name, "credential": credential}),
            )
            .await;
        repos.push(repo.ok()["id"].as_str().unwrap().to_owned());
    }
    let url = |id: &str| format!("/api/repos/{id}/checkout");
    // Operator: no workspace. Another agent's path. No run-as (F5).
    td.post(
        &url(&repos[0]),
        json!({"target": "p-win", "path": r"C:\Terrarium\work\p\worker\app"}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        aid,
        &url(&repos[0]),
        json!({"target": "p-win", "path": r"C:\Terrarium\work\p\other\app"}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        aid,
        &url(&repos[0]),
        json!({"target": "p-plain", "path": "app"}),
    )
    .await
    .err(422, "invalid");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(received.try_recv().is_err(), "no frame for a refusal");
    let mut next_frame = async || {
        let frame = tokio::time::timeout(Duration::from_secs(30), received.recv())
            .await
            .unwrap()
            .unwrap();
        let DaemonFrame::GitCheckout(r) = frame else {
            panic!("{frame:?}")
        };
        r
    };
    // Token branch.
    let answer = td
        .post_as(
            aid,
            &url(&repos[0]),
            json!({"target": "p-win", "path": "app"}),
        )
        .await;
    assert_eq!(answer.ok()["action"], "cloned", "{}", answer.body);
    let r = next_frame().await;
    assert_eq!(r.dest.to_str(), Some(r"C:\Terrarium\work\p\worker\app"));
    assert_eq!(r.url, "https://example.invalid/app.git");
    assert!(
        matches!(&r.credential, Some(GitCredential::Token { password, .. }) if password == TOKEN),
        "the token goes in its typed field"
    );
    assert!(
        r.windows_run_as
            .as_ref()
            .is_some_and(|w| w.password == LOGIN),
        "run-as is required (F5)"
    );
    let CheckoutMode::Managed(m) = &r.mode else {
        panic!("an agent checkout must be managed: {:?}", r.mode)
    };
    assert!(
        m.commit_hook
            .contains("# terrarium managed provenance hook")
    );
    terrarium_host::git_checkout::check_push_hook(&m.push_hook).unwrap();
    assert_eq!(m.repo_id, repos[0]);
    // SSH-key branch.
    td.post_as(
        aid,
        &url(&repos[1]),
        json!({"target": "p-win", "path": "keyed"}),
    )
    .await
    .ok();
    let r = next_frame().await;
    let Some(GitCredential::SshKey { key }) = &r.credential else {
        panic!("the SSH key goes in its typed field")
    };
    assert!(key.contains("PRIVATE KEY"));
    let key_line = key
        .lines()
        .find(|l| l.len() > 40 && !l.contains("PRIVATE KEY"))
        .unwrap()
        .to_owned();
    // A failing checkout answers without the secrets.
    let failed = td
        .post_as(
            aid,
            &url(&repos[2]),
            json!({"target": "p-win", "path": "broken"}),
        )
        .await;
    failed.err(422, "invalid");
    next_frame().await;
    let events = td.get("/api/events").await.body.to_string();
    let audit = td.get("/api/audit").await.body.to_string();
    for (place, text) in [
        ("events", &events),
        ("audit", &audit),
        ("error", &failed.body.to_string()),
        ("answer", &answer.body.to_string()),
    ] {
        for sentinel in [TOKEN, LOGIN, key_line.as_str()] {
            assert!(!text.contains(sentinel), "a secret in the {place}");
        }
    }
    assert!(
        events.contains("repo.checkout"),
        "checkout events were recorded"
    );
    fake.abort();
    listener.abort();
}
