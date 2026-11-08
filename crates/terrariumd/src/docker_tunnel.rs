//! The VM key and SSH connection stay on the daemon. A trusted release
//! relay on the target sees only Docker byte streams, never a credential.
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::{ApiError, ApiResult};
use crate::executor::{Control, Ev, RemoteCmd, Stream};
use crate::secrets::{RunDir, RunDirs};
use crate::state::Shared;
use terrarium_core::config::TargetDef;

const KNOWN_HOST: &str = "[10.3.0.2]:2250 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGgTrtbsNPDR6lWHJVpgAh6tStS4C7O9F0ksb405ScXy\n";
const RELAY: &str = include_str!("docker_relay.py");

pub struct Tunnel {
    pub endpoint: String,
    ssh: tokio::process::Child,
    relay: mpsc::UnboundedSender<Control>,
    task: tokio::task::JoinHandle<()>,
    _dir: RunDir,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.relay.send(Control::Stdin(b"{\"stop\":true}\n".to_vec()));
        // The stream task drains the stop acknowledgement and closes all
        // connection tasks. SSH is daemon-owned and kill-on-drop as well.
        let _ = self.ssh.start_kill();
    }
}

impl Tunnel {
    pub async fn close(mut self) {
        let _ = self.relay.send(Control::Stdin(b"{\"stop\":true}\n".to_vec()));
        if tokio::time::timeout(Duration::from_secs(5), &mut self.task).await.is_err() {
            let _ = self.relay.send(Control::Kill);
            self.task.abort();
        }
        let _ = self.ssh.kill().await;
    }
}

pub async fn open(
    state: &Shared,
    target: &TargetDef,
    key: &str,
    work_dir: &str,
    timeout: u64,
) -> ApiResult<Tunnel> {
    let dir = RunDirs::new(&state.state_dir).run_dir()?;
    let keyfile = dir.write_private("identity", key.as_bytes())?;
    let hosts = dir.write_private("known_hosts", KNOWN_HOST.as_bytes())?;
    let socket = dir.path().join("docker.sock");
    let forward = format!("{}:/var/run/docker.sock", socket.display());
    let mut ssh = tokio::process::Command::new("ssh")
        .args(["-F", "/dev/null", "-N", "-T", "-p", "2250", "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=yes", "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none", "-o", "ExitOnForwardFailure=yes",
            "-o", "GlobalKnownHostsFile=/dev/null", "-o", "ServerAliveInterval=10",
            "-o", "ServerAliveCountMax=2"])
        .arg("-o").arg(format!("UserKnownHostsFile={}", hosts.display()))
        .arg("-i").arg(keyfile).arg("-L").arg(forward).arg("root@10.3.0.2")
        .env_remove("SSH_AUTH_SOCK")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .kill_on_drop(true).spawn()
        .map_err(|_| ApiError::internal("cannot start daemon Docker SSH tunnel"))?;
    let ready = async {
        loop {
            if ssh.try_wait().map_err(|_| ApiError::internal("cannot check Docker SSH tunnel"))?.is_some() {
                return Err(ApiError::conflict("Docker SSH tunnel failed (check grant, host key, and VM)"));
            }
            if socket.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(15), ready).await
        .map_err(|_| ApiError::conflict("Docker SSH tunnel did not become ready"))??;
    let target_dir = format!("{work_dir}/.git/terrarium/d-{}", ulid::Ulid::new());
    let cmd = RemoteCmd {
        argv: vec!["python3".into(), "-u".into(), "-c".into(), RELAY.into(), target_dir.clone(), (timeout + 30).to_string()],
        timeout_secs: Some(timeout + 30),
        ..Default::default()
    };
    let running = crate::executor::start_cmd(state, target, &cmd)?;
    let relay = running.ctl.clone();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(bridge(running, socket, tx));
    let mut tunnel = Tunnel { endpoint: format!("unix://{target_dir}/docker.sock"), ssh, relay, task, _dir: dir };
    if !matches!(tokio::time::timeout(Duration::from_secs(15), rx).await, Ok(Ok(()))) {
        let _ = tunnel.relay.send(Control::Kill);
        tunnel.task.abort();
        let _ = tunnel.ssh.kill().await;
        return Err(ApiError::conflict("Docker target relay did not become ready"));
    }
    Ok(tunnel)
}

fn send(ctl: &mpsc::UnboundedSender<Control>, event: Value) -> bool {
    let mut frame = event.to_string().into_bytes();
    frame.push(b'\n');
    ctl.send(Control::Stdin(frame)).is_ok()
}

async fn bridge(mut run: crate::executor::Running, socket: PathBuf, ready: oneshot::Sender<()>) {
    let mut ready = Some(ready);
    let mut buffer = Vec::new();
    let mut connections = HashMap::<u64, mpsc::Sender<Vec<u8>>>::new();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(event) = run.events.recv().await {
        let Ev::Data(Stream::Stdout, data) = event else {
            if matches!(event, Ev::End(_)) { break; }
            continue;
        };
        buffer.extend(data);
        if buffer.len() > 131072 { break; }
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(&buffer[..end]) else { return; };
            buffer.drain(..=end);
            if event["ready"] == true {
                if let Some(tx) = ready.take() { let _ = tx.send(()); }
                continue;
            }
            let Some(id) = event["id"].as_u64() else { return; };
            if event["open"] == true {
                if connections.len() >= 32 { send(&run.ctl, json!({"id":id,"close":true})); continue; }
                let (tx, rx) = mpsc::channel(16);
                connections.insert(id, tx);
                tasks.spawn(connection(id, socket.clone(), run.ctl.clone(), rx));
            } else if event["close"] == true {
                connections.remove(&id);
            } else if let Some(data) = event["data"].as_str() {
                let Ok(data) = STANDARD.decode(data) else { return; };
                if data.len() > 16384 { return; }
                if let Some(tx) = connections.get(&id) {
                    if tx.try_send(data).is_err() {
                        connections.remove(&id);
                        send(&run.ctl, json!({"id":id,"close":true}));
                    }
                }
            }
        }
    }
    tasks.abort_all();
}

async fn connection(id: u64, path: PathBuf, ctl: mpsc::UnboundedSender<Control>, mut input: mpsc::Receiver<Vec<u8>>) {
    if let Ok(mut socket) = tokio::net::UnixStream::connect(path).await {
        let mut data = vec![0; 16384];
        loop {
            tokio::select! {
                read = socket.read(&mut data) => {
                    match read {
                        Ok(n) if n > 0 => { if !send(&ctl, json!({"id":id,"data":STANDARD.encode(&data[..n])})) { break; } }
                        _ => break,
                    }
                }
                data = input.recv() => {
                    let Some(data) = data else { break; };
                    if socket.write_all(&data).await.is_err() { break; }
                }
            }
        }
    }
    send(&ctl, json!({"id":id,"close":true}));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn daemon_bridge_round_trips_without_any_secret_in_target_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4];
            socket.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"ping");
            socket.write_all(b"pong").await.unwrap();
        });
        let (events, rx) = mpsc::channel(8);
        let (ctl, mut controls) = mpsc::unbounded_channel();
        let (ready, ready_rx) = oneshot::channel();
        let task = tokio::spawn(bridge(crate::executor::Running { events: rx, ctl }, path, ready));
        events.send(Ev::Data(Stream::Stdout, b"{\"ready\":true}\n{\"id\":1,\"open\":true}\n{\"id\":1,\"data\":\"cGluZw==\"}\n".to_vec())).await.unwrap();
        ready_rx.await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), controls.recv()).await.unwrap().unwrap();
        let Control::Stdin(bytes) = response else { panic!("expected data"); };
        let event: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(event["id"], 1);
        assert_eq!(STANDARD.decode(event["data"].as_str().unwrap()).unwrap(), b"pong");
        server.await.unwrap();
        drop(events);
        task.await.unwrap();
    }
}
