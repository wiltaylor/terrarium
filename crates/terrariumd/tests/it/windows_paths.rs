//! Windows paths cross a real host link without using the Linux filesystem.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::time::Duration;
use terrarium_host::protocol::{DaemonFrame, HostFrame, decode_control};
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

/// TER-274 D8, D16, D17: a host with `windows_git_checkout` gets one typed
/// `git.checkout` frame in managed mode, inside the agent's workspace, and
/// its `result` frame is the checkout answer.
#[tokio::test]
async fn windows_git_checkout_goes_only_to_a_capable_host_in_managed_mode() {
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
            "path_kind": "windows",
            "capabilities": {"no_workspaces": false, "windows_git_checkout": true},
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
        json!({"name":"win", "kind":"host", "host":"windows", "project":"p"}),
    )
    .await
    .ok();
    let repo = td
        .post(
            "/api/projects/p/repos",
            json!({"url": "https://example.invalid/app.git", "name": "app"}),
        )
        .await;
    let repo_id = repo.ok()["id"].as_str().unwrap().to_owned();
    let url = format!("/api/repos/{repo_id}/checkout");
    // An operator checkout has no workspace: refused, no frame.
    td.post(
        &url,
        json!({"target": "p-win", "path": r"C:\Terrarium\work\p\worker\app"}),
    )
    .await
    .err(422, "invalid");
    // A path outside the agent's own workspace: refused, no frame.
    td.post_as(
        aid,
        &url,
        json!({"target": "p-win", "path": r"C:\Terrarium\work\p\other\app"}),
    )
    .await
    .err(422, "invalid");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(received.try_recv().is_err(), "no frame for a refusal");
    let answer = td
        .post_as(aid, &url, json!({"target": "p-win", "path": "app"}))
        .await;
    assert_eq!(answer.ok()["action"], "cloned", "{}", answer.body);
    let frame = tokio::time::timeout(Duration::from_secs(30), received.recv())
        .await
        .unwrap()
        .unwrap();
    let DaemonFrame::GitCheckout(r) = frame else {
        panic!("{frame:?}")
    };
    assert_eq!(r.dest.to_str(), Some(r"C:\Terrarium\work\p\worker\app"));
    assert_eq!(r.url, "https://example.invalid/app.git");
    assert!(r.credential.is_none());
    let terrarium_host::protocol::CheckoutMode::Managed(m) = &r.mode else {
        panic!("an agent checkout must be managed: {:?}", r.mode)
    };
    assert!(
        m.commit_hook
            .contains("# terrarium managed provenance hook")
    );
    terrarium_host::git_checkout::check_push_hook(&m.push_hook).unwrap();
    assert_eq!(m.repo_id, repo_id);
    fake.abort();
    listener.abort();
}
