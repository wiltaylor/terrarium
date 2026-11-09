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
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{Control, Ev, RemoteCmd, Stream};
use crate::secrets::{RunDir, RunDirs};
use terrarium_core::config::{DockerCiDef, TargetDef};

const RELAY: &str = include_str!("docker_relay.py");

/// The env name of the job socket in the recipe. Not `DOCKER_HOST`: on a
/// `docker` target the host agent runs `docker exec -e <name>`, and the
/// host's own docker CLI reads `DOCKER_HOST` from that env, so it dialed
/// the in-container relay path on the host and the recipe never started
/// (TER-477). `scripts/docker_ci.py` sets `DOCKER_HOST` inside the gate.
pub const ENDPOINT_ENV: &str = "TERRARIUM_DOCKER_HOST";

/// Time for one reset call. Resets take up to about 60 s under load.
const RESET_TIMEOUT: Duration = Duration::from_secs(180);
/// Time for the VM's Docker daemon to answer through the tunnel after a reset.
const PING_DEADLINE: Duration = Duration::from_secs(90);

/// META-104 forced command. No recipe code or remote command is involved.
pub async fn reset(state: &Shared, key: &str, config: &DockerCiDef) -> ApiResult<()> {
    let dir = RunDirs::new(&state.state_dir).run_dir()?;
    let identity = dir.write_private("reset-identity", key.as_bytes())?;
    let hosts = dir.write_private("reset_known_hosts", config.reset_known_hosts.as_bytes())?;
    for attempt in 0..3 {
        info!(
            destination = %config.reset_destination,
            attempt = attempt + 1,
            "docker ci: reset start"
        );
        let started = std::time::Instant::now();
        let child = tokio::process::Command::new(&state.executor.ssh_bin)
            .args(reset_args(&identity, &hosts, config))
            .env_remove("SSH_AUTH_SOCK")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| ApiError::internal("cannot start daemon Docker CI reset"))?;
        let Ok(output) = tokio::time::timeout(RESET_TIMEOUT, child.wait_with_output()).await
        else {
            warn!(
                secs = RESET_TIMEOUT.as_secs(),
                "docker ci: reset timed out; tunnel not opened"
            );
            return Err(ApiError::conflict(
                "Docker CI reset timed out; tunnel not opened",
            ));
        };
        let output = output.map_err(|_| ApiError::internal("cannot wait for Docker CI reset"))?;
        let code = output.status.code();
        let line = last_line(&output.stdout);
        let secs = started.elapsed().as_secs();
        if code == Some(0) {
            info!(?code, secs, stdout = %line, "docker ci: reset done");
        } else {
            warn!(?code, secs, stdout = %line, "docker ci: reset failed");
        }
        if code != Some(75) || attempt == 2 {
            return reset_status(code);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    unreachable!("reset loop always returns")
}

/// The last non-empty line of the reset output, bounded and printable.
/// reset.sh prints one result line and no key material (ops2).
fn last_line(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    line.chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

fn reset_args(
    identity: &std::path::Path,
    hosts: &std::path::Path,
    config: &DockerCiDef,
) -> Vec<String> {
    vec![
        "-F".into(),
        "/dev/null".into(),
        "-T".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        "-o".into(),
        "IdentityAgent=none".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "GlobalKnownHostsFile=/dev/null".into(),
        "-o".into(),
        format!("UserKnownHostsFile={}", hosts.display()),
        "-i".into(),
        identity.display().to_string(),
        "-p".into(),
        config.reset_port.to_string(),
        config.reset_destination.clone(),
    ]
}

fn reset_status(code: Option<i32>) -> ApiResult<()> {
    match code {
        Some(0) => Ok(()),
        Some(75) => Err(ApiError::conflict(
            "Docker CI reset lock busy (exit 75); tunnel not opened",
        )),
        code => Err(ApiError::conflict(format!(
            "Docker CI reset failed (exit {code:?}); tunnel not opened"
        ))),
    }
}

pub struct Tunnel {
    pub endpoint: String,
    ssh: tokio::process::Child,
    relay: mpsc::UnboundedSender<Control>,
    task: tokio::task::JoinHandle<()>,
    _dir: RunDir,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self
            .relay
            .send(Control::Stdin(b"{\"stop\":true}\n".to_vec()));
        // The stream task drains the stop acknowledgement and closes all
        // connection tasks. SSH is daemon-owned and kill-on-drop as well.
        let _ = self.ssh.start_kill();
    }
}

impl Tunnel {
    pub async fn close(mut self) {
        let _ = self
            .relay
            .send(Control::Stdin(b"{\"stop\":true}\n".to_vec()));
        if tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .is_err()
        {
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
    config: &DockerCiDef,
) -> ApiResult<Tunnel> {
    let dir = RunDirs::new(&state.state_dir).run_dir()?;
    let keyfile = dir.write_private("identity", key.as_bytes())?;
    let hosts = dir.write_private("known_hosts", config.known_hosts.as_bytes())?;
    let socket = dir.path().join("docker.sock");
    let forward = format!("{}:/var/run/docker.sock", socket.display());
    let mut ssh = tokio::process::Command::new(&state.executor.ssh_bin)
        .args([
            "-F",
            "/dev/null",
            "-N",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg("-p")
        .arg(config.port.to_string())
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", hosts.display()))
        .arg("-i")
        .arg(keyfile)
        .arg("-L")
        .arg(forward)
        .arg(&config.destination)
        .env_remove("SSH_AUTH_SOCK")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ApiError::internal("cannot start daemon Docker SSH tunnel"))?;
    let ready = async {
        loop {
            if ssh
                .try_wait()
                .map_err(|_| ApiError::internal("cannot check Docker SSH tunnel"))?
                .is_some()
            {
                return Err(ApiError::conflict(
                    "Docker SSH tunnel failed (check grant, host key, and VM)",
                ));
            }
            if socket.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let up = tokio::time::timeout(Duration::from_secs(15), ready)
        .await
        .map_err(|_| ApiError::conflict("Docker SSH tunnel did not become ready"))
        .and_then(|r| r);
    if let Err(e) = up {
        warn!(destination = %config.destination, "docker ci: tunnel failed: {}", e.message);
        return Err(e);
    }
    // A bound forward is not a Docker daemon that answers: right after a
    // reset the VM's dockerd can still be starting. The recipe starts only
    // after a `/_ping` through the tunnel returned 200.
    if let Err(e) = wait_for_ping(&socket, &mut ssh, PING_DEADLINE).await {
        warn!(destination = %config.destination, "docker ci: tunnel failed: {}", e.message);
        let _ = ssh.kill().await;
        return Err(e);
    }
    info!(destination = %config.destination, "docker ci: tunnel up, Docker answers /_ping");
    let target_dir = format!("{work_dir}/.git/terrarium/d-{}", ulid::Ulid::new());
    let cmd = RemoteCmd {
        argv: vec![
            "python3".into(),
            "-I".into(),
            "-u".into(),
            "-c".into(),
            RELAY.into(),
            target_dir.clone(),
            (timeout + 30).to_string(),
        ],
        timeout_secs: Some(timeout + 30),
        ..Default::default()
    };
    let running = crate::executor::start_cmd(state, target, &cmd)?;
    let relay = running.ctl.clone();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(bridge(running, socket, tx));
    let mut tunnel = Tunnel {
        endpoint: format!("unix://{target_dir}/docker.sock"),
        ssh,
        relay,
        task,
        _dir: dir,
    };
    if !matches!(
        tokio::time::timeout(Duration::from_secs(15), rx).await,
        Ok(Ok(()))
    ) {
        let _ = tunnel.relay.send(Control::Kill);
        tunnel.task.abort();
        let _ = tunnel.ssh.kill().await;
        warn!(target = %target.name, "docker ci: relay failed: the target relay did not become ready");
        return Err(ApiError::conflict(
            "Docker target relay did not become ready",
        ));
    }
    info!(target = %target.name, socket = %format!("{target_dir}/docker.sock"), "docker ci: relay socket created");
    Ok(tunnel)
}

/// Retry `GET /_ping` on the daemon-side forward until Docker answers 200,
/// the SSH client ends, or `deadline` passes.
async fn wait_for_ping(
    socket: &std::path::Path,
    ssh: &mut tokio::process::Child,
    deadline: Duration,
) -> ApiResult<()> {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        if matches!(ping(socket).await, Ok(true)) {
            return Ok(());
        }
        if ssh
            .try_wait()
            .map_err(|_| ApiError::internal("cannot check Docker SSH tunnel"))?
            .is_some()
        {
            return Err(ApiError::conflict(
                "Docker SSH tunnel ended before Docker answered /_ping",
            ));
        }
        if tokio::time::Instant::now() >= end {
            return Err(ApiError::conflict(format!(
                "Docker did not answer /_ping through the tunnel in {} s",
                deadline.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One Docker `/_ping` over a Unix socket: true on an HTTP 200 answer.
async fn ping(socket: &std::path::Path) -> std::io::Result<bool> {
    let attempt = async {
        let mut stream = tokio::net::UnixStream::connect(socket).await?;
        stream
            .write_all(b"GET /_ping HTTP/1.0\r\nHost: docker\r\n\r\n")
            .await?;
        let mut answer = Vec::new();
        let mut chunk = [0; 512];
        while answer.len() < 4096 {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            answer.extend_from_slice(&chunk[..n]);
            if answer.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok(ping_ok(&answer))
    };
    tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .unwrap_or(Ok(false))
}

fn ping_ok(answer: &[u8]) -> bool {
    let status = answer.split(|b| *b == b'\n').next().unwrap_or_default();
    let status = String::from_utf8_lossy(status);
    let mut parts = status.split_whitespace();
    matches!(
        (parts.next(), parts.next()),
        (Some(v), Some("200")) if v.starts_with("HTTP/1.")
    )
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
    // One data frame in flight per connection. Acknowledgement comes only
    // after the target socket has accepted it, not after enqueueing stdin.
    let mut credits = HashMap::<u64, mpsc::Sender<()>>::new();
    while let Some(event) = run.events.recv().await {
        let Ev::Data(Stream::Stdout, data) = event else {
            if matches!(event, Ev::End(_)) {
                break;
            }
            continue;
        };
        buffer.extend(data);
        if buffer.len() > 131072 {
            break;
        }
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(&buffer[..end]) else {
                return;
            };
            buffer.drain(..=end);
            if event["ready"] == true {
                if let Some(tx) = ready.take() {
                    let _ = tx.send(());
                }
                continue;
            }
            let Some(id) = event["id"].as_u64() else {
                return;
            };
            if event["open"] == true {
                if connections.len() >= 32 {
                    send(&run.ctl, json!({"id":id,"close":true}));
                    continue;
                }
                let (tx, rx) = mpsc::channel(16);
                let (credit, credit_rx) = mpsc::channel(1);
                credits.insert(id, credit);
                connections.insert(id, tx);
                tasks.spawn(connection(
                    id,
                    socket.clone(),
                    run.ctl.clone(),
                    rx,
                    credit_rx,
                ));
            } else if event["ack"] == true {
                if let Some(credit) = credits.get(&id)
                    && credit.send(()).await.is_err()
                {
                    credits.remove(&id);
                }
            } else if event["close"] == true {
                connections.remove(&id);
                credits.remove(&id);
            } else if let Some(data) = event["data"].as_str() {
                let Ok(data) = STANDARD.decode(data) else {
                    return;
                };
                if data.len() > 16384 {
                    return;
                }
                if let Some(tx) = connections.get(&id)
                    && tx.send(data).await.is_err()
                {
                    connections.remove(&id);
                    send(&run.ctl, json!({"id":id,"close":true}));
                }
            }
        }
    }
    tasks.abort_all();
}

async fn connection(
    id: u64,
    path: PathBuf,
    ctl: mpsc::UnboundedSender<Control>,
    mut input: mpsc::Receiver<Vec<u8>>,
    mut credit: mpsc::Receiver<()>,
) {
    if let Ok(socket) = tokio::net::UnixStream::connect(path).await {
        let (mut reader, mut writer) = socket.into_split();
        let write_ctl = ctl.clone();
        let write = async move {
            while let Some(data) = input.recv().await {
                if writer.write_all(&data).await.is_err() {
                    break;
                }
                if !send(&write_ctl, json!({"id":id,"ack":true})) {
                    break;
                }
            }
        };
        let read = async {
            let mut data = vec![0; 16384];
            loop {
                match reader.read(&mut data).await {
                    Ok(n) if n > 0 => {
                        if !send(&ctl, json!({"id":id,"data":STANDARD.encode(&data[..n])})) {
                            break;
                        }
                        if credit.recv().await.is_none() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        };
        tokio::select! { _ = write => {}, _ = read => {} }
    }
    send(&ctl, json!({"id":id,"close":true}));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn forced_reset_has_no_remote_command_and_fails_closed() {
        let args = reset_args(
            std::path::Path::new("/private/reset"),
            std::path::Path::new("/pinned/hosts"),
            &DockerCiDef {
                destination: "root@vm".into(),
                port: 2250,
                known_hosts: "pin".into(),
                reset_destination: "reset@host".into(),
                reset_port: 22,
                reset_known_hosts: "reset-host ssh-ed25519 pin".into(),
            },
        );
        assert_eq!(args.last().map(String::as_str), Some("reset@host"));
        assert!(args.contains(&"IdentitiesOnly=yes".to_owned()));
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_owned()));
        // Without this the pinned UserKnownHostsFile is not the only trust
        // source: /etc/ssh/ssh_known_hosts could vouch for a host again.
        assert!(args.contains(&"GlobalKnownHostsFile=/dev/null".to_owned()));
        assert!(reset_status(Some(0)).is_ok());
        for code in [Some(1), Some(75), None] {
            assert!(reset_status(code).is_err());
        }
    }

    #[test]
    fn ping_needs_an_http_200_and_reset_line_is_bounded() {
        assert!(ping_ok(b"HTTP/1.1 200 OK\r\nApi-Version: 1.47\r\n\r\nOK"));
        assert!(ping_ok(b"HTTP/1.0 200 OK\r\n\r\n"));
        assert!(!ping_ok(b"HTTP/1.1 500 Internal Server Error\r\n\r\n"));
        assert!(!ping_ok(b""));
        assert!(!ping_ok(b"garbage 200"));
        assert_eq!(last_line(b"booting\nreset ok rc=0 secs=21\n\n"), "reset ok rc=0 secs=21");
        assert_eq!(last_line(b""), "");
        assert_eq!(last_line(&[b'x'; 500]).len(), 200);
        assert_eq!(last_line(b"a\x1b[31mb\n"), "a[31mb");
    }

    #[tokio::test]
    async fn ping_waits_until_docker_answers_through_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker.sock");
        // Nothing listens yet: no answer, so the recipe must not start.
        assert!(!ping(&path).await.unwrap_or(false));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            // First connection: closed without an answer (dockerd not up).
            drop(listener.accept().await.unwrap());
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 256];
            let n = socket.read(&mut request).await.unwrap();
            assert!(request[..n].starts_with(b"GET /_ping HTTP/1.0\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                .await
                .unwrap();
        });
        let mut ssh = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        wait_for_ping(&path, &mut ssh, Duration::from_secs(10))
            .await
            .unwrap();
        server.await.unwrap();
        // A tunnel whose SSH client ended fails, it does not wait the deadline.
        let mut gone = tokio::process::Command::new("true").spawn().unwrap();
        let _ = gone.wait().await;
        let started = std::time::Instant::now();
        let err = wait_for_ping(&path, &mut gone, Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.message.contains("ended"), "{}", err.message);
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = ssh.kill().await;
    }

    #[tokio::test]
    async fn large_stream_obeys_credits_without_drops() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("docker.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let size = 8 * 1024 * 1024;
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; size];
                socket.read_exact(&mut request).await.unwrap();
                assert!(request.iter().all(|b| *b == 42));
                socket.write_all(&request).await.unwrap();
            });
            let (events, rx) = mpsc::channel(2);
            let (ctl, mut controls) = mpsc::unbounded_channel();
            let (ready, _) = oneshot::channel();
            let bridge_task = tokio::spawn(bridge(
                crate::executor::Running { events: rx, ctl },
                path,
                ready,
            ));
            let frame = |event: Value| Ev::Data(Stream::Stdout, format!("{event}\n").into_bytes());
            events
                .send(frame(json!({"id":1,"open":true})))
                .await
                .unwrap();
            let sender = events.clone();
            let producer = tokio::spawn(async move {
                // More than the channel capacity, while the server deliberately
                // waits for the entire upload before returning its response.
                for _ in 0..size / 16384 {
                    sender
                        .send(Ev::Data(
                            Stream::Stdout,
                            format!(
                                "{}\n",
                                json!({"id":1,"data":STANDARD.encode(vec![42;16384])})
                            )
                            .into_bytes(),
                        ))
                        .await
                        .unwrap();
                }
            });
            let mut received = 0;
            while received < size {
                let Control::Stdin(bytes) = controls.recv().await.unwrap() else {
                    panic!("expected stdin")
                };
                let event: Value = serde_json::from_slice(&bytes).unwrap();
                if let Some(data) = event["data"].as_str() {
                    let data = STANDARD.decode(data).unwrap();
                    assert!(data.iter().all(|b| *b == 42));
                    received += data.len();
                    // Simulate a slow target accepting each Docker response.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    events
                        .send(frame(json!({"id":1,"ack":true})))
                        .await
                        .unwrap();
                }
            }
            producer.await.unwrap();
            server.await.unwrap();
            drop(events);
            bridge_task.await.unwrap();
        })
        .await
        .unwrap();
    }

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
        let task = tokio::spawn(bridge(
            crate::executor::Running { events: rx, ctl },
            path,
            ready,
        ));
        events
            .send(Ev::Data(
                Stream::Stdout,
                b"{\"ready\":true}\n{\"id\":1,\"open\":true}\n{\"id\":1,\"data\":\"cGluZw==\"}\n"
                    .to_vec(),
            ))
            .await
            .unwrap();
        ready_rx.await.unwrap();
        let response = loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), controls.recv())
                .await
                .unwrap()
                .unwrap();
            if let Control::Stdin(bytes) = &frame {
                let event: Value = serde_json::from_slice(bytes).unwrap();
                if event["ack"] == true {
                    continue;
                }
            }
            break frame;
        };
        let Control::Stdin(bytes) = response else {
            panic!("expected data");
        };
        let event: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(event["id"], 1);
        assert_eq!(
            STANDARD.decode(event["data"].as_str().unwrap()).unwrap(),
            b"pong"
        );
        events
            .send(Ev::Data(
                Stream::Stdout,
                b"{\"id\":1,\"ack\":true}\n".to_vec(),
            ))
            .await
            .unwrap();
        server.await.unwrap();
        drop(events);
        task.await.unwrap();
    }
}
