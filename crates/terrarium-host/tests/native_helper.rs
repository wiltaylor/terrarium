//! The native GPU helper over its real socket, with fake systemd, a fake
//! `nvidia-smi`, and fake device nodes (TER-345). The real
//! `terrarium-host native` shim relays a session through it.
//!
//! The fake `systemd-run` records its arguments and runs the command after
//! `--` as the test user; the fake `systemctl` records its calls. Real
//! device policy needs root and a GPU host: that is the hardware follow-up
//! of TER-345.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use terrarium_host::native::{
    self, Ctx, DeviceNodes, ErrorKind, Exit, Helper, Identity, Kind, Reply, Request, Right,
    Sessions, SliceRequest, StartRequest,
};
use tokio::net::UnixStream;

const A: &str = "GPU-aaaaaaaa-0000-0000-0000-000000000001";
const B: &str = "GPU-bbbbbbbb-0000-0000-0000-000000000002";

struct FakeNodes(HashMap<PathBuf, (u64, u64)>);

impl DeviceNodes for FakeNodes {
    fn char_device(&self, path: &Path) -> Result<(u64, u64), String> {
        self.0
            .get(path)
            .copied()
            .ok_or_else(|| format!("{} missing", path.display()))
    }
}

struct Fake {
    dir: tempfile::TempDir,
    root: tempfile::TempDir,
    socket: PathBuf,
    helper: Arc<Helper>,
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fake {
    async fn new(rights: Vec<Right>, agent_uid: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let d = dir.path();
        script(
            &d.join("systemd-run"),
            &format!(
                "printf '%s\\n' \"$@\" >> '{run}'\n\
                 for a; do case \"$a\" in EnvironmentFile=*) cp \"${{a#EnvironmentFile=}}\" '{env}';; esac; done\n\
                 while [ $# -gt 0 ]; do a=$1; shift; [ \"$a\" = -- ] && break; done\n\
                 exec \"$@\"\n",
                run = d.join("run.log").display(),
                env = d.join("env.copy").display(),
            ),
        );
        script(
            &d.join("systemctl"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'\n",
                d.join("ctl.log").display()
            ),
        );
        script(
            &d.join("nvidia-smi"),
            &format!("printf '{A}, 0\\n{B}, 1\\n'\n"),
        );
        std::fs::create_dir(d.join("state")).unwrap();
        let helper = Arc::new(Helper {
            ctx: Ctx {
                workload: Identity {
                    name: "terrarium-work".into(),
                    uid: 2001,
                    gid: 2001,
                },
                workspace_root: std::fs::canonicalize(root.path()).unwrap(),
                state_dir: d.join("state"),
                dev_dir: "/dev".into(),
                support_nodes: vec!["/dev/nvidiactl".into()],
            },
            agent_uid,
            rights,
            systemd_run: d.join("systemd-run"),
            systemctl: d.join("systemctl"),
            nvidia_smi: d.join("nvidia-smi"),
            nodes: Box::new(FakeNodes(HashMap::from([
                (PathBuf::from("/dev/nvidia0"), (195, 0)),
                (PathBuf::from("/dev/nvidia1"), (195, 1)),
                (PathBuf::from("/dev/nvidiactl"), (195, 255)),
            ]))),
            sessions: Mutex::new(Sessions::default()),
        });
        let socket = d.join("helper.sock");
        let listener = native::bind(&socket, nix::unistd::getuid().as_raw()).unwrap();
        tokio::spawn(native::serve(helper.clone(), listener));
        Self {
            dir,
            root,
            socket,
            helper,
        }
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    fn start(
        &self,
        holder: &str,
        lease: Option<&str>,
        gpus: &[&str],
        exclusive: &[&str],
        argv: &[&str],
    ) -> StartRequest {
        StartRequest {
            session: 3,
            target: "box".into(),
            holder: holder.into(),
            lease: lease.map(str::to_owned),
            gpus: gpus.iter().map(|g| (*g).to_owned()).collect(),
            exclusive_gpus: exclusive.iter().map(|g| (*g).to_owned()).collect(),
            cwd: self.root.path().to_string_lossy().into_owned(),
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            env: BTreeMap::from([("TOKEN".into(), "t$1".into())]),
            ..Default::default()
        }
    }

    async fn live(&self) -> usize {
        self.helper.sessions.lock().unwrap().len()
    }

    async fn wait_idle(&self) {
        for _ in 0..100 {
            if self.live().await == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("sessions did not end");
    }
}

async fn send(socket: &Path, req: &Request) -> (UnixStream, Reply) {
    let mut s = UnixStream::connect(socket).await.unwrap();
    native::write_frame(&mut s, Kind::Request, &serde_json::to_vec(req).unwrap())
        .await
        .unwrap();
    let reply = match native::read_frame(&mut s).await.unwrap() {
        Some((Kind::Reply, body)) => serde_json::from_slice(&body).unwrap(),
        other => panic!("no reply: {other:?}"),
    };
    (s, reply)
}

fn code(reply: &Reply) -> Option<ErrorKind> {
    match reply {
        Reply::Error { code, .. } => Some(*code),
        _ => None,
    }
}

#[tokio::test]
async fn the_version_handshake_fails_closed() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    assert_eq!(native::hello(&f.socket).await.unwrap(), [Right::Session]);
    assert!(native::capability(&f.socket).await.enforced);
    let (_, r) = send(
        &f.socket,
        &Request::Hello(native::HelloRequest { version: 99 }),
    )
    .await;
    assert_eq!(code(&r), Some(ErrorKind::Unsupported), "{r:?}");
    let missing = native::capability(&f.dir.path().join("none.sock")).await;
    assert!(!missing.enforced && missing.reason.is_some());
}

#[tokio::test]
async fn only_the_agent_user_may_call() {
    let other = nix::unistd::getuid().as_raw().wrapping_add(1);
    let f = Fake::new(vec![Right::Session], other).await;
    let e = native::hello(&f.socket).await.unwrap_err();
    assert!(e.contains("not the agent user"), "{e}");
    assert!(!native::capability(&f.socket).await.enforced);
    let req = f.start("p/a", None, &[], &[], &["true"]);
    let (_, r) = send(&f.socket, &Request::Start(req)).await;
    assert_eq!(code(&r), Some(ErrorKind::Unauthorized));
    assert!(f.log("run.log").is_empty());
    // The socket is the agent user's alone.
    let mode = std::fs::metadata(&f.socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn without_the_session_right_nothing_starts() {
    let f = Fake::new(vec![], nix::unistd::getuid().as_raw()).await;
    assert!(native::hello(&f.socket).await.unwrap().is_empty());
    let cap = native::capability(&f.socket).await;
    assert!(!cap.enforced, "{cap:?}");
    let req = f.start("p/a", None, &[], &[], &["true"]);
    let (_, r) = send(&f.socket, &Request::Start(req)).await;
    assert_eq!(code(&r), Some(ErrorKind::Forbidden));
    let stop = Request::Stop(SliceRequest {
        target: "box".into(),
        holder: "p/a".into(),
    });
    assert_eq!(
        code(&send(&f.socket, &stop).await.1),
        Some(ErrorKind::Forbidden)
    );
    assert!(f.log("run.log").is_empty() && f.log("ctl.log").is_empty());
}

#[tokio::test]
async fn a_session_starts_in_its_device_policy_and_relays_stdio() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let req = f.start(
        "p/a",
        Some("L1"),
        &[A],
        &[A],
        &["sh", "-c", "read x; echo out:$x; echo err >&2; exit 3"],
    );
    let (s, r) = send(&f.socket, &Request::Start(req)).await;
    let Reply::Started { unit } = r else {
        panic!("{r:?}")
    };
    assert!(unit.starts_with("terrarium-native-3-"), "{unit}");
    let (mut rd, mut wr) = s.into_split();
    native::write_frame(&mut wr, Kind::Stdin, b"hi\n")
        .await
        .unwrap();
    native::write_frame(&mut wr, Kind::StdinEof, b"")
        .await
        .unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = loop {
        match native::read_frame(&mut rd).await.unwrap() {
            Some((Kind::Stdout, d)) => out.extend(d),
            Some((Kind::Stderr, d)) => err.extend(d),
            Some((Kind::Exit, d)) => break serde_json::from_slice::<Exit>(&d).unwrap(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(exit.code, Some(3));
    assert_eq!(String::from_utf8_lossy(&out), "out:hi\n");
    assert_eq!(String::from_utf8_lossy(&err), "err\n");
    let run = f.log("run.log");
    let args: Vec<&str> = run.lines().collect();
    for want in [
        "DevicePolicy=closed",
        "DeviceAllow=/dev/nvidia0 rw",
        "DeviceAllow=/dev/nvidiactl rw",
        "NoNewPrivileges=yes",
        "--uid=2001",
        "--pipe",
    ] {
        assert!(args.contains(&want), "{want} not in {args:?}");
    }
    assert!(!run.contains("nvidia1"), "{run}");
    assert_eq!(f.log("env.copy"), "TOKEN=\"t\\$1\"\n");
    f.wait_idle().await;
    // The unit is killed at the end, so no descendant keeps a GPU handle,
    // and its environment file is gone.
    assert!(
        f.log("ctl.log")
            .contains(&format!("kill --signal=9 -- {unit}"))
    );
    assert_eq!(
        std::fs::read_dir(f.dir.path().join("state"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn a_caller_that_goes_away_kills_the_unit() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let req = f.start("p/a", None, &[], &[], &["sleep", "1"]);
    let (s, r) = send(&f.socket, &Request::Start(req)).await;
    let Reply::Started { unit } = r else {
        panic!("{r:?}")
    };
    drop(s);
    for _ in 0..40 {
        if f.log("ctl.log")
            .contains(&format!("kill --signal=9 -- {unit}"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        f.log("ctl.log")
            .contains(&format!("kill --signal=9 -- {unit}"))
    );
    f.wait_idle().await;
}

#[tokio::test]
async fn two_reservations_never_hold_one_exclusive_gpu() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let a = f.start("p/a", Some("L1"), &[A], &[A], &["sleep", "1"]);
    let (held, r) = send(&f.socket, &Request::Start(a)).await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    // Holder B on A's GPU: busy, nothing starts.
    let b = f.start("p/b", Some("L2"), &[A], &[A], &["true"]);
    let (_, r) = send(&f.socket, &Request::Start(b.clone())).await;
    assert_eq!(code(&r), Some(ErrorKind::Busy), "{r:?}");
    // A session with no grant on that GPU: busy too.
    let shared = f.start("p/c", None, &[A], &[], &["true"]);
    let (_, r) = send(&f.socket, &Request::Start(shared)).await;
    assert_eq!(code(&r), Some(ErrorKind::Busy), "{r:?}");
    // A forged holder on A's lease.
    let forged = f.start("p/b", Some("L1"), &[A], &[A], &["true"]);
    let (_, r) = send(&f.socket, &Request::Start(forged)).await;
    assert_eq!(code(&r), Some(ErrorKind::Forbidden), "{r:?}");
    // A disjoint GPU is free.
    let other = f.start("p/b", Some("L2"), &[B], &[B], &["true"]);
    let (_s, r) = send(&f.socket, &Request::Start(other)).await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    assert_eq!(f.log("run.log").matches("DevicePolicy=closed").count(), 2);
    // After A's session ended, B may take the GPU.
    drop(held);
    f.wait_idle().await;
    let (_s, r) = send(&f.socket, &Request::Start(b)).await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
}

#[tokio::test]
async fn apply_and_stop_use_slices_that_the_helper_derives() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let apply = Request::Apply(native::ApplyRequest {
        target: "box".into(),
        holder: "p/a".into(),
        limits: terrarium_core::resources::RuntimeLimits {
            limits: terrarium_core::limits::Limits {
                cpu_millis: Some(1500),
                ..Default::default()
            },
            ..Default::default()
        },
    });
    assert_eq!(send(&f.socket, &apply).await.1, Reply::Ok);
    let slice = terrarium_host::limits::slice_name("box", "p/a");
    let ctl = f.log("ctl.log");
    assert!(
        ctl.contains(&format!("set-property --runtime -- {slice} CPUQuota=150%")),
        "{ctl}"
    );
    let shared = Request::Stop(SliceRequest {
        target: String::new(),
        holder: String::new(),
    });
    assert_eq!(
        code(&send(&f.socket, &shared).await.1),
        Some(ErrorKind::Invalid)
    );
    let stop = Request::Stop(SliceRequest {
        target: "box".into(),
        holder: "p/a".into(),
    });
    assert_eq!(send(&f.socket, &stop).await.1, Reply::Ok);
    assert!(f.log("ctl.log").contains(&format!("stop -- {slice}")));
}

fn shim(f: &Fake, req: &Request) -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_terrarium-host"));
    c.args(["native", "--socket"])
        .arg(&f.socket)
        .env(native::REQUEST_ENV, serde_json::to_string(req).unwrap());
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shim_relays_a_session_and_its_exit_code() {
    use std::io::Write;
    use std::process::Stdio;
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let req = Request::Start(f.start(
        "p/a",
        None,
        &[],
        &[],
        &["sh", "-c", "read x; echo out:$x; echo err >&2; exit 3"],
    ));
    let mut c = shim(&f, &req);
    let out = tokio::task::spawn_blocking(move || {
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"hi\n").unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out:hi\n");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("err"),
        "{out:?}"
    );

    // A refusal is exit 125 with the reason.
    let refused = Request::Start(f.start("p/a", None, &["GPU-cccc"], &[], &["true"]));
    let mut c = shim(&f, &refused);
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(125), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not on this host"),
        "{out:?}"
    );

    // `stop` through the shim: exit 0.
    let stop = Request::Stop(SliceRequest {
        target: "box".into(),
        holder: "p/a".into(),
    });
    let mut c = shim(&f, &stop);
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shim_forwards_a_signal_to_the_unit() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let req = Request::Start(f.start("p/a", None, &[], &[], &["sleep", "2"]));
    let mut c = shim(&f, &req);
    let child = c
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if !f.log("run.log").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    #[allow(clippy::cast_possible_wrap)]
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    let mut seen = false;
    for _ in 0..50 {
        if f.log("ctl.log")
            .contains("kill --signal=15 -- terrarium-native-3-")
        {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen, "{}", f.log("ctl.log"));
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    // The fake systemctl does not kill, so `sleep` ends by itself.
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    f.wait_idle().await;
}
