//! Integration tests: a stub daemon WebSocket server drives a real
//! `terrarium_host::Client` through docs/host-protocol.md.

// tungstenite's handshake callback fixes the large `ErrorResponse` type.
#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use terrarium_host::protocol::{
    CpPull, CpPush, DaemonFrame, DataFrame, ErrorCode, ExecRequest, ExitReason, Hello, HostFrame,
    PtySize, ResumeAccept, SessionId, StreamTag, Welcome, decode_control, encode_control,
};
use terrarium_host::{Client, ClientOptions};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

const TOKEN: &str = "host-token-1";
const WAIT: Duration = Duration::from_secs(15);

struct Stub {
    listener: TcpListener,
    auth: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug)]
enum In {
    Ctl(HostFrame),
    Data(DataFrame),
}

struct Conn {
    ws: WebSocketStream<TcpStream>,
}

impl Stub {
    async fn new() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            auth: Arc::default(),
        }
    }

    fn url(&self) -> String {
        format!("ws://{}/hosts/connect", self.listener.local_addr().unwrap())
    }

    /// Accept one connection. Refuses a wrong bearer token with 401.
    async fn accept(&self) -> Conn {
        loop {
            let (tcp, _) = tokio::time::timeout(WAIT, self.listener.accept())
                .await
                .expect("host agent did not dial")
                .unwrap();
            let auth = self.auth.clone();
            let cb = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                assert_eq!(req.uri().path(), "/hosts/connect");
                let h = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                auth.lock().unwrap().push(h.clone());
                if h == format!("Bearer {TOKEN}") {
                    Ok(resp)
                } else {
                    let mut r = ErrorResponse::new(None);
                    *r.status_mut() = StatusCode::UNAUTHORIZED;
                    Err(r)
                }
            };
            if let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await {
                return Conn { ws };
            }
        }
    }
}

impl Conn {
    /// The next frame. Answers pings and drops pongs.
    async fn next(&mut self) -> In {
        loop {
            let msg = tokio::time::timeout(WAIT, self.ws.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("connection closed")
                .unwrap();
            match msg {
                Message::Text(t) => match decode_control::<HostFrame>(&t).unwrap() {
                    HostFrame::Ping { nonce } => {
                        self.send(DaemonFrame::Pong { nonce }).await;
                    }
                    HostFrame::Pong { .. } => {}
                    f => return In::Ctl(f),
                },
                Message::Binary(b) => return In::Data(DataFrame::decode(&b).unwrap()),
                _ => {}
            }
        }
    }

    async fn next_ctl(&mut self) -> HostFrame {
        match self.next().await {
            In::Ctl(f) => f,
            In::Data(d) => panic!("expected a control frame, got data {d:?}"),
        }
    }

    async fn hello(&mut self) -> Hello {
        match self.next_ctl().await {
            HostFrame::Hello(h) => *h,
            f => panic!("expected hello, got {f:?}"),
        }
    }

    async fn welcome(&mut self, resume: Vec<ResumeAccept>) {
        self.send(DaemonFrame::Welcome(Welcome {
            host_id: "h-1".into(),
            heartbeat_secs: 1,
            resume_token: "resume-1".into(),
            resume,
            token: None,
            metrics: false,
            update: None,
        }))
        .await;
    }

    async fn send(&mut self, f: DaemonFrame) {
        self.ws
            .send(Message::Text(encode_control(&f).unwrap().into()))
            .await
            .unwrap();
    }

    async fn send_data(&mut self, session: SessionId, stream: StreamTag, data: &[u8]) {
        let f = DataFrame::new(session, stream, data.to_vec());
        self.ws
            .send(Message::Binary(f.encode().into()))
            .await
            .unwrap();
    }
}

/// Everything a session sent until its terminal frame.
#[derive(Debug, Default)]
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    file: Vec<u8>,
    frames: Vec<HostFrame>,
}

impl Collected {
    fn terminal(&self) -> &HostFrame {
        self.frames.last().expect("no terminal frame")
    }
}

async fn collect(conn: &mut Conn, session: SessionId) -> Collected {
    let mut c = Collected::default();
    loop {
        match conn.next().await {
            In::Data(d) => {
                assert_eq!(d.session, session);
                match d.stream {
                    StreamTag::Stdout => c.stdout.extend(d.payload),
                    StreamTag::Stderr => c.stderr.extend(d.payload),
                    StreamTag::File => c.file.extend(d.payload),
                    StreamTag::Stdin => panic!("host sent stdin"),
                }
            }
            In::Ctl(f) => {
                let done = f.is_terminal();
                c.frames.push(f);
                if done {
                    return c;
                }
            }
        }
    }
}

fn start_client(
    url: String,
    tweak: impl FnOnce(&mut ClientOptions),
) -> tokio::task::JoinHandle<()> {
    let mut opts = ClientOptions::new(url, TOKEN, "test-host");
    opts.backoff_min = Duration::from_millis(20);
    opts.backoff_max = Duration::from_millis(100);
    opts.handshake_timeout = Duration::from_secs(5);
    tweak(&mut opts);
    tokio::spawn(Client::new(opts).run())
}

fn exec(session: SessionId, cmd: &str, args: &[&str], pty: Option<PtySize>) -> DaemonFrame {
    DaemonFrame::Exec(ExecRequest {
        windows_run_as: None,
        session,
        cmd: cmd.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env: Default::default(),
        pty,
        timeout_secs: Some(30),
        limits: None,
    })
}

fn sha(data: &[u8]) -> String {
    terrarium_host::protocol::hex(&Sha256::digest(data))
}

#[tokio::test]
async fn exec_streams_stdout_stderr_and_exit_code() {
    let stub = Stub::new().await;
    let client = start_client(stub.url(), |_| {});
    let mut conn = stub.accept().await;
    let hello = conn.hello().await;
    assert_eq!(hello.host, "test-host");
    assert_eq!(hello.protocol, 1);
    assert!(hello.resume.is_none(), "first hello has no resume token");
    assert_eq!(
        stub.auth.lock().unwrap().as_slice(),
        [format!("Bearer {TOKEN}")]
    );
    conn.welcome(vec![]).await;

    // Heartbeat: the host agent answers a ping.
    conn.send(DaemonFrame::Ping { nonce: 42 }).await;
    loop {
        let msg = conn.ws.next().await.unwrap().unwrap();
        if let Message::Text(t) = msg
            && let HostFrame::Pong { nonce } = decode_control::<HostFrame>(&t).unwrap()
        {
            assert_eq!(nonce, 42);
            break;
        }
    }

    conn.send(exec(
        1,
        "sh",
        &["-c", "echo out; echo err 1>&2; exit 3"],
        None,
    ))
    .await;
    let c = collect(&mut conn, 1).await;
    assert!(matches!(
        c.frames[0],
        HostFrame::Started {
            session: 1,
            pid: Some(_)
        }
    ));
    assert_eq!(c.stdout, b"out\n");
    assert_eq!(c.stderr, b"err\n");
    assert_eq!(
        c.terminal(),
        &HostFrame::Exit {
            session: 1,
            code: Some(3),
            signal: None,
            reason: ExitReason::Exited
        }
    );
    conn.send(DaemonFrame::Release { session: 1 }).await;
    client.abort();
}

#[tokio::test]
async fn pty_exec_echoes_stdin() {
    let stub = Stub::new().await;
    let client = start_client(stub.url(), |_| {});
    let mut conn = stub.accept().await;
    conn.hello().await;
    conn.welcome(vec![]).await;

    // An empty PS1 keeps the interactive shell's prompt out of the output:
    // sh can print its first prompt while the terminal still echoes the
    // typed line, which splits "echo pty-..." (TER-568).
    let mut req = exec(5, "sh", &[], Some(PtySize { rows: 24, cols: 80 }));
    if let DaemonFrame::Exec(r) = &mut req {
        r.env.insert("PS1".into(), String::new());
    }
    conn.send(req).await;
    conn.send_data(5, StreamTag::Stdin, b"echo pty-$((40+2))\n")
        .await;
    conn.send_data(5, StreamTag::Stdin, b"exit 7\n").await;
    let c = collect(&mut conn, 5).await;
    let out = String::from_utf8_lossy(&c.stdout);
    // The terminal echoes the typed line, and the shell prints the result.
    assert!(out.contains("echo pty-$((40+2))"), "pty output: {out:?}");
    assert!(out.contains("pty-42"), "pty output: {out:?}");
    assert!(c.stderr.is_empty(), "a pty has one stream");
    assert!(matches!(
        c.terminal(),
        HostFrame::Exit {
            code: Some(7),
            reason: ExitReason::Exited,
            ..
        }
    ));
    client.abort();
}

#[tokio::test]
async fn cp_push_then_pull_round_trips_with_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/dir/blob.bin");
    let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();
    let roots = vec![dir.path().to_path_buf()];

    let stub = Stub::new().await;
    let client = start_client(stub.url(), move |o| {
        o.policy.roots = Some(roots);
    });
    let mut conn = stub.accept().await;
    conn.hello().await;
    conn.welcome(vec![]).await;

    // Push.
    conn.send(DaemonFrame::CpPush(CpPush {
        session: 10,
        path: path.clone(),
        mode: Some(0o640),
        mkdirs: true,
    }))
    .await;
    for chunk in data.chunks(50_000) {
        conn.send_data(10, StreamTag::File, chunk).await;
    }
    conn.send(DaemonFrame::CpEnd {
        session: 10,
        bytes: data.len() as u64,
        sha256: sha(&data),
    })
    .await;
    let c = collect(&mut conn, 10).await;
    assert_eq!(
        c.terminal(),
        &HostFrame::CpDone {
            session: 10,
            bytes: data.len() as u64,
            sha256: sha(&data)
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), data);
    conn.send(DaemonFrame::Release { session: 10 }).await;

    // Pull.
    conn.send(DaemonFrame::CpPull(CpPull {
        session: 11,
        path: path.clone(),
    }))
    .await;
    let c = collect(&mut conn, 11).await;
    assert_eq!(c.file, data);
    let HostFrame::CpDone { bytes, sha256, .. } = c.terminal() else {
        panic!("{:?}", c.terminal())
    };
    assert_eq!(*bytes, data.len() as u64);
    assert_eq!(sha256, &sha(&c.file));
    conn.send(DaemonFrame::Release { session: 11 }).await;

    // A bad digest leaves the old file in place.
    conn.send(DaemonFrame::CpPush(CpPush {
        session: 12,
        path: path.clone(),
        mode: None,
        mkdirs: false,
    }))
    .await;
    conn.send_data(12, StreamTag::File, b"tampered").await;
    conn.send(DaemonFrame::CpEnd {
        session: 12,
        bytes: 8,
        sha256: sha(b"expected"),
    })
    .await;
    let c = collect(&mut conn, 12).await;
    assert!(matches!(
        c.terminal(),
        HostFrame::Error {
            code: ErrorCode::ChecksumMismatch,
            ..
        }
    ));
    assert_eq!(std::fs::read(&path).unwrap(), data);

    // A path outside allowed_cwd_roots is refused before anything runs.
    conn.send(DaemonFrame::CpPull(CpPull {
        session: 13,
        path: "/etc/hostname".into(),
    }))
    .await;
    let c = collect(&mut conn, 13).await;
    assert!(matches!(
        c.terminal(),
        HostFrame::Error {
            code: ErrorCode::PathDenied,
            ..
        }
    ));
    client.abort();
}

#[tokio::test]
async fn reconnect_resumes_and_replays_unacked_bytes() {
    let stub = Stub::new().await;
    let client = start_client(stub.url(), |_| {});
    let mut conn = stub.accept().await;
    conn.hello().await;
    conn.welcome(vec![]).await;

    let script = "i=0; while [ $i -lt 30 ]; do echo line$i; i=$((i+1)); sleep 0.02; done";
    let expected: String = (0..30).map(|i| format!("line{i}\n")).collect();
    conn.send(exec(3, "sh", &["-c", script], None)).await;

    // Read some output, ack part of it, then drop the link.
    let mut got = Vec::new();
    while got.len() < 20 {
        match conn.next().await {
            In::Data(d) => got.extend(d.payload),
            In::Ctl(HostFrame::Started { .. }) => {}
            In::Ctl(f) => panic!("unexpected {f:?}"),
        }
    }
    conn.send(DaemonFrame::Ack {
        session: 3,
        bytes: 6,
    })
    .await;
    // The daemon keeps only what it has; say it lost the tail of the last
    // frame, to prove that replay starts at `received`, not at the ack.
    let received = got.len() as u64 - 3;
    got.truncate(received as usize);
    drop(conn);

    let mut conn = stub.accept().await;
    let hello = conn.hello().await;
    let resume = hello.resume.expect("second hello carries the resume token");
    assert_eq!(resume.token, "resume-1");
    assert_eq!(resume.sessions.len(), 1);
    assert_eq!(resume.sessions[0].session, 3);
    assert!(resume.sessions[0].sent >= received);
    conn.welcome(vec![ResumeAccept {
        session: 3,
        received,
    }])
    .await;

    let c = collect(&mut conn, 3).await;
    got.extend(&c.stdout);
    assert_eq!(String::from_utf8_lossy(&got), expected);
    assert!(matches!(
        c.terminal(),
        HostFrame::Exit {
            code: Some(0),
            reason: ExitReason::Exited,
            ..
        }
    ));
    // The tokens differ per connection only in the daemon; both dials used the host token.
    assert!(
        stub.auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a == &format!("Bearer {TOKEN}"))
    );
    client.abort();
}

#[tokio::test]
async fn overflow_while_disconnected_ends_with_a_reason() {
    let stub = Stub::new().await;
    let client = start_client(stub.url(), |o| o.buffer_limit = 4096);
    let mut conn = stub.accept().await;
    conn.hello().await;
    conn.welcome(vec![]).await;

    conn.send(exec(
        4,
        "sh",
        &[
            "-c",
            "echo start; sleep 0.3; head -c 100000 /dev/zero; sleep 30",
        ],
        None,
    ))
    .await;
    let mut got = Vec::new();
    while got.len() < 6 {
        if let In::Data(d) = conn.next().await {
            got.extend(d.payload);
        }
    }
    assert_eq!(got, b"start\n");
    drop(conn);
    // Let the process fill the buffer while no daemon is connected.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let mut conn = stub.accept().await;
    let hello = conn.hello().await;
    let offer = &hello.resume.unwrap().sessions[0];
    assert!(offer.finished, "the session ended while detached");
    conn.welcome(vec![ResumeAccept {
        session: 4,
        received: 6,
    }])
    .await;
    let c = collect(&mut conn, 4).await;
    assert!(c.stdout.len() <= 4096);
    assert!(matches!(
        c.terminal(),
        HostFrame::Exit {
            reason: ExitReason::OutputOverflow,
            signal: Some(9),
            ..
        }
    ));
    client.abort();
}

#[tokio::test]
async fn sessions_the_daemon_does_not_resume_are_ended() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("still-running");
    let stub = Stub::new().await;
    let client = start_client(stub.url(), |_| {});
    let mut conn = stub.accept().await;
    conn.hello().await;
    conn.welcome(vec![]).await;
    let script = format!("echo up; sleep 1; touch {}", marker.display());
    conn.send(exec(8, "sh", &["-c", &script], None)).await;
    while !matches!(conn.next().await, In::Data(_)) {}
    drop(conn);

    let mut conn = stub.accept().await;
    let hello = conn.hello().await;
    assert_eq!(hello.resume.unwrap().sessions.len(), 1);
    conn.welcome(vec![]).await; // The daemon forgot session 8.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!marker.exists(), "the abandoned session was killed");
    client.abort();
}

#[tokio::test]
async fn rejections_are_errors() {
    let stub = Stub::new().await;
    let client = start_client(stub.url(), |o| o.max_sessions = 1);
    let mut conn = stub.accept().await;
    conn.hello().await;
    conn.welcome(vec![]).await;

    // Docker is not present in these options.
    conn.send(decode_control(r#"{"type":"docker.ps","session":20}"#).unwrap())
        .await;
    assert!(matches!(
        conn.next_ctl().await,
        HostFrame::Error {
            session: 20,
            code: ErrorCode::Unsupported,
            ..
        }
    ));
    // Stdin for a session that does not exist.
    conn.send_data(21, StreamTag::Stdin, b"x").await;
    assert!(matches!(
        conn.next_ctl().await,
        HostFrame::Error {
            session: 21,
            code: ErrorCode::UnknownSession,
            ..
        }
    ));
    // A frame that does not parse.
    conn.ws
        .send(Message::Text(r#"{"type":"exec","session":22}"#.into()))
        .await
        .unwrap();
    assert!(matches!(
        conn.next_ctl().await,
        HostFrame::Error {
            session: 22,
            code: ErrorCode::BadRequest,
            ..
        }
    ));
    // max_sessions = 1: a second live session is refused; a duplicate ID too.
    conn.send(exec(23, "sleep", &["30"], None)).await;
    assert!(matches!(
        conn.next_ctl().await,
        HostFrame::Started { session: 23, .. }
    ));
    conn.send(exec(23, "true", &[], None)).await;
    assert!(matches!(
        conn.next_ctl().await,
        HostFrame::Error {
            session: 23,
            code: ErrorCode::DuplicateSession,
            ..
        }
    ));
    conn.send(exec(24, "true", &[], None)).await;
    assert!(matches!(
        conn.next_ctl().await,
        HostFrame::Error {
            session: 24,
            code: ErrorCode::Busy,
            ..
        }
    ));
    // A signal ends the live one.
    conn.send(DaemonFrame::Signal {
        session: 23,
        signal: "TERM".into(),
    })
    .await;
    let c = collect(&mut conn, 23).await;
    assert!(matches!(
        c.terminal(),
        HostFrame::Exit {
            signal: Some(15),
            reason: ExitReason::Signaled,
            ..
        }
    ));
    // A program that does not exist.
    conn.send(DaemonFrame::Release { session: 23 }).await;
    conn.send(exec(25, "/nonexistent/program", &[], None)).await;
    assert!(matches!(
        conn.next_ctl().await,
        HostFrame::Error {
            session: 25,
            code: ErrorCode::SpawnFailed,
            ..
        }
    ));
    client.abort();
}

#[tokio::test]
async fn wrong_token_is_refused_and_retried() {
    let stub = Stub::new().await;
    let mut opts = ClientOptions::new(stub.url(), "wrong", "test-host");
    opts.backoff_min = Duration::from_millis(20);
    opts.backoff_max = Duration::from_millis(40);
    let client = tokio::spawn(Client::new(opts).run());
    // Accept the TCP connections by hand; each upgrade gets a 401.
    for _ in 0..2 {
        let (tcp, _) = tokio::time::timeout(WAIT, stub.listener.accept())
            .await
            .unwrap()
            .unwrap();
        let auth = stub.auth.clone();
        let r = tokio_tungstenite::accept_hdr_async(tcp, move |req: &Request, _resp: Response| {
            let h = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            auth.lock().unwrap().push(h);
            let mut r = ErrorResponse::new(None);
            *r.status_mut() = StatusCode::UNAUTHORIZED;
            Err(r)
        })
        .await;
        assert!(r.is_err());
    }
    assert_eq!(stub.auth.lock().unwrap()[0], "Bearer wrong");
    client.abort();
}
