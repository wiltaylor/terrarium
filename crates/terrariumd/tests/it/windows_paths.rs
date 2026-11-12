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
    let td = TestDaemon::start().await;
    let token = td.post("/api/hosts", json!({"name": "windows"})).await.ok()["token"]
        .as_str().unwrap().to_owned();
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (listener, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0").await.unwrap();
    let mut req = format!("ws://{addr}/hosts/connect").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(json!({
        "type": "hello", "protocol": 1, "host": "windows",
        "version": terrarium_version::FULL, "os": "windows", "arch": "x86_64",
        "path_kind": "windows", "capabilities": {}, "max_sessions": 16
    }).to_string().into())).await.unwrap();
    assert!(matches!(decode_control::<DaemonFrame>(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap(), DaemonFrame::Welcome(_)));
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let fake = tokio::spawn(async move {
        while let Some(Ok(message)) = ws.next().await {
            let Message::Text(text) = message else { continue };
            let frame: DaemonFrame = decode_control(&text).unwrap();
            if let DaemonFrame::Ping { nonce } = frame {
                ws.send(Message::Text(serde_json::to_string(&HostFrame::Pong { nonce }).unwrap().into())).await.unwrap();
            } else {
                sent.send(frame).unwrap();
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        while state.executor.resource_pending("windows") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    td.post("/api/targets", json!({"name":"win", "kind":"host", "host":"windows"})).await.ok();
    let cwd = r"C:\Terrarium\work\project\agent";
    td.post("/api/targets/win/exec", json!({"cmd":["cmd.exe", "/c", "echo"], "cwd":cwd})).await.ok();
    let frame = tokio::time::timeout(Duration::from_secs(30), received.recv()).await.unwrap().unwrap();
    let DaemonFrame::Exec(exec) = frame else { panic!("{frame:?}") };
    assert_eq!(exec.cwd.unwrap().to_str(), Some(cwd));
    for (direction, path) in [("push", r"C:\Terrarium\work\project\agent\file"), ("pull", r"\\server\share\dir\file")] {
        td.post("/api/targets/win/cp", json!({"direction":direction, "path":path})).await.ok();
        let frame = tokio::time::timeout(Duration::from_secs(30), received.recv()).await.unwrap().unwrap();
        let actual = match frame {
            DaemonFrame::CpPush(cp) => cp.path,
            DaemonFrame::CpPull(cp) => cp.path,
            other => panic!("{other:?}"),
        };
        assert_eq!(actual.to_str(), Some(path));
    }
    for path in [r"C:\Terrarium\work\project\agent\..\other", r"C:\NUL.txt", r"C:\bad.", "/posix"] {
        td.post("/api/targets/win/cp", json!({"direction":"pull", "path":path})).await.err(400, "invalid");
        td.post("/api/targets/win/exec", json!({"cmd":["cmd.exe"], "cwd":path})).await.err(400, "invalid");
    }
    fake.abort();
    listener.abort();
}
