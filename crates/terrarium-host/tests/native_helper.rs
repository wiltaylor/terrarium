//! The native GPU helper over its real socket, with fake systemd, a fake
//! `nvidia-smi`, and fake device nodes (TER-345). The real
//! `terrarium-host native` shim relays a session through it.
//!
//! The fake `systemd-run` records its arguments and runs the command after
//! `--` as the test user; the fake `systemctl` records its calls. Real
//! device policy needs root and a GPU host: that is the hardware follow-up
//! of TER-345.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use terrarium_host::native::{
    self, Ctx, DeviceNodes, ErrorKind, Exit, Helper, Identity, Kind, Reply, Request, Right,
    Sessions, SliceRequest, StartRequest, WorkloadCheck,
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
    /// The fake re-check of the workload users passes while true.
    check_ok: Arc<AtomicBool>,
}

/// Write an executable fake. A child shell writes it, so this process
/// never holds a write fd on a file that a fork could exec (ETXTBSY under
/// load, TER-415).
fn script(path: &Path, body: &str) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("printf %s \"$1\" > \"$2\" && chmod 755 \"$2\"")
        .arg("sh")
        .arg(format!("#!/bin/sh\n{body}"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

struct Check(Arc<AtomicBool>);

impl WorkloadCheck for Check {
    fn check(&self, w: &Identity) -> Result<(), String> {
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(format!(
                "workload user {} must not be in group docker",
                w.name
            ))
        }
    }
}

fn pool() -> Vec<Identity> {
    (1..=2)
        .map(|i| Identity {
            name: format!("terrarium-w{i}"),
            uid: 2000 + i,
            gid: 2000 + i,
        })
        .collect()
}

impl Fake {
    async fn new(rights: Vec<Right>, agent_uid: u32) -> Self {
        Self::with(rights, agent_uid, Ok(())).await
    }

    async fn with(rights: Vec<Right>, agent_uid: u32, gpu: Result<(), String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let d = dir.path();
        script(
            &d.join("systemd-run"),
            &format!(
                "printf '%s\\n' \"$@\" >> '{run}'\n\
                 for a; do case \"$a\" in EnvironmentFile=*) cp \"${{a#EnvironmentFile=}}\" '{env}'; stat -c %a \"${{a#EnvironmentFile=}}\" > '{env}.mode';; esac; done\n\
                 while [ $# -gt 0 ]; do a=$1; shift; [ \"$a\" = -- ] && break; done\n\
                 exec \"$@\"\n",
                run = d.join("run.log").display(),
                env = d.join("env.copy").display(),
            ),
        );
        // `show` prints the unit state (file `state`, default inactive)
        // and its cgroup (file `cgroup`, default none); `list-units` prints
        // the file `units`, or fails with `list.fail`.
        script(
            &d.join("systemctl"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/ctl.log'\n\
                 case \"$1\" in\n\
                 show) case \"$2\" in\n\
                 *ControlGroup) cat '{d}/cgroup' 2>/dev/null ;;\n\
                 *) if [ -f '{d}/state' ]; then cat '{d}/state'; else echo inactive; fi ;;\n\
                 esac ;;\n\
                 list-units) [ -f '{d}/list.fail' ] && exit 1; cat '{d}/units' 2>/dev/null ;;\n\
                 esac\n\
                 exit 0\n",
                d = d.display()
            ),
        );
        script(
            &d.join("nvidia-smi"),
            &format!("printf '{A}, 0\\n{B}, 1\\n'\n"),
        );
        std::fs::create_dir(d.join("sessions")).unwrap();
        // A fake cgroup v2 tree: `cgroup` names a unit cgroup under it.
        std::fs::create_dir(d.join("cgroupfs")).unwrap();
        std::fs::write(d.join("cgroupfs/cgroup.controllers"), "cpu memory\n").unwrap();
        let check_ok = Arc::new(AtomicBool::new(true));
        let helper = Arc::new(Helper {
            ctx: Ctx {
                workspace_root: std::fs::canonicalize(root.path()).unwrap(),
                state_dir: d.join("sessions"),
                dev_dir: "/dev".into(),
                support_nodes: vec!["/dev/nvidiactl".into()],
            },
            agent_uid,
            rights,
            pool: pool(),
            checker: Box::new(Check(check_ok.clone())),
            systemd_run: d.join("systemd-run"),
            systemctl: d.join("systemctl"),
            nvidia_smi: d.join("nvidia-smi"),
            cgroup_root: d.join("cgroupfs"),
            gpu,
            nodes: Box::new(FakeNodes(HashMap::from([
                (PathBuf::from("/dev/nvidia0"), (195, 0)),
                (PathBuf::from("/dev/nvidia1"), (195, 1)),
                (PathBuf::from("/dev/nvidiactl"), (195, 255)),
            ]))),
            sessions: Mutex::new(Sessions::default()),
            stop_timeout: Duration::from_millis(400),
            stop_retry: Duration::from_millis(200),
        });
        let socket = d.join("helper.sock");
        let listener = native::bind(&socket, nix::unistd::getuid().as_raw()).unwrap();
        tokio::spawn(native::serve(helper.clone(), listener));
        Self {
            dir,
            root,
            socket,
            helper,
            check_ok,
        }
    }

    /// The helper answers `started` before it spawns `systemd-run`: wait
    /// for the fake's log to show the spawns.
    async fn wait_log(&self, name: &str, ok: impl Fn(&str) -> bool) -> String {
        for _ in 0..200 {
            let text = self.log(name);
            if ok(&text) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{name}: {}", self.log(name));
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir.path().join(name), text).unwrap();
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
async fn owned_roots_require_operator_scope_and_cleanup_runs_before_success() {
    if !nix::unistd::geteuid().is_root() { return; }
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let mut req = f.start("p/a", None, &[], &[], &["true"]);
    req.prepare_root = Some(native::OwnedRoot {
        cleanup: false,
        base: f.root.path().to_str().unwrap().into(),
        project: "p".into(),
        name: "a".into(),
        agent: "id-a".into(),
    });
    let (_, r) = send(&f.socket, &Request::Start(req.clone())).await;
    assert_eq!(code(&r), Some(ErrorKind::Invalid));
    assert!(!f.root.path().join("p").exists());
    req.holder = "operator".into();
    for cleanup in [false, true] {
        req.prepare_root.as_mut().unwrap().cleanup = cleanup;
        let (s, r) = send(&f.socket, &Request::Start(req.clone())).await;
        assert!(matches!(r, Reply::Started { .. }), "{r:?}");
        let (mut rd, _wr) = s.into_split();
        let exit = loop {
            match native::read_frame(&mut rd).await.unwrap() {
                Some((Kind::Exit, d)) => break serde_json::from_slice::<Exit>(&d).unwrap(),
                Some(_) => {}
                None => panic!("no exit"),
            }
        };
        assert_eq!(exit.code, Some(0));
        assert_eq!(f.root.path().join("p/a/.terrarium-owner").exists(), !cleanup);
        f.wait_idle().await;
    }
    assert!(f.root.path().join("p").is_dir());
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
    let meta = std::fs::metadata(&f.socket).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(meta.uid(), nix::unistd::getuid().as_raw());
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

/// TER-566: the daemon sends multi-line values (git trailers, known hosts,
/// a commit hook) on every exec. The helper accepts them and writes them
/// to the 0600 environment file in systemd's double-quoted form, never to
/// the `systemd-run` command line; NUL stays refused.
#[tokio::test]
async fn a_session_takes_multi_line_environment_values() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let trailers = "Card: TER-566\nAgent: p/a\n";
    let hook = "#!/bin/sh\nexec \"$1\" `x`\r\n";
    let mut req = f.start("p/a", None, &[], &[], &["true"]);
    req.env = BTreeMap::from([
        ("TERRARIUM_GIT_TRAILERS".into(), trailers.into()),
        ("TERRARIUM_COMMIT_HOOK".into(), hook.into()),
    ]);
    let (s, r) = send(&f.socket, &Request::Start(req.clone())).await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    let (mut rd, _wr) = s.into_split();
    let exit = loop {
        match native::read_frame(&mut rd).await.unwrap() {
            Some((Kind::Exit, d)) => break serde_json::from_slice::<Exit>(&d).unwrap(),
            Some(_) => {}
            None => panic!("no exit"),
        }
    };
    assert_eq!(exit.code, Some(0));
    assert_eq!(
        f.log("env.copy"),
        "TERRARIUM_COMMIT_HOOK=\"#!/bin/sh\nexec \\\"\\$1\\\" \\`x\\`\r\n\"\n\
         TERRARIUM_GIT_TRAILERS=\"Card: TER-566\nAgent: p/a\n\"\n"
    );
    assert_eq!(f.log("env.copy.mode").trim(), "600");
    let run = f.log("run.log");
    assert!(
        !run.contains("TER-566") && !run.contains("#!/bin/sh"),
        "{run}"
    );
    f.wait_idle().await;

    req.env = BTreeMap::from([("TERRARIUM_GIT_TRAILERS".into(), "a\n\0b".into())]);
    let (_, r) = send(&f.socket, &Request::Start(req)).await;
    assert_eq!(code(&r), Some(ErrorKind::Invalid), "{r:?}");
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
        std::fs::read_dir(f.dir.path().join("sessions"))
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
    let shared = f.start("p/b", None, &[A], &[], &["true"]);
    let (_, r) = send(&f.socket, &Request::Start(shared)).await;
    assert_eq!(code(&r), Some(ErrorKind::Busy), "{r:?}");
    assert!(
        matches!(&r, Reply::Error { message, .. } if message.contains("another reservation")),
        "{r:?}"
    );
    // A disjoint GPU is free.
    let other = f.start("p/b", Some("L2"), &[B], &[B], &["true"]);
    let (_s, r) = send(&f.socket, &Request::Start(other)).await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    f.wait_log("run.log", |l| l.matches("DevicePolicy=closed").count() == 2)
        .await;
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

/// A stop with `--user-slice` (the host agent sends it once the helper
/// enforced): the holder's user slice and its system slice both stop, and
/// the shim fails when either stop fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_shim_stop_also_stops_the_user_slice() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let bin = f.dir.path().join("userbin");
    std::fs::create_dir(&bin).unwrap();
    // The user manager's systemctl: exit 5 (not loaded) unless `user.code`.
    script(
        &bin.join("systemctl"),
        &format!(
            "printf '%s\\n' \"$*\" >> '{d}/user.log'\n\
             [ -f '{d}/user.code' ] && exit \"$(cat '{d}/user.code')\"\n\
             exit 5\n",
            d = f.dir.path().display()
        ),
    );
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let stop = Request::Stop(SliceRequest {
        target: "box".into(),
        holder: "p/a".into(),
    });
    let slice = terrarium_host::limits::slice_name("box", "p/a");
    let run = |socket: &Path| {
        std::process::Command::new(env!("CARGO_BIN_EXE_terrarium-host"))
            .args(["native", "--user-slice", "--socket"])
            .arg(socket)
            .env(native::REQUEST_ENV, serde_json::to_string(&stop).unwrap())
            .env("PATH", &path)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .code()
    };
    let stops = || {
        f.log("ctl.log")
            .matches(&format!("stop -- {slice}"))
            .count()
    };
    // A user slice that is not loaded has nothing to stop.
    assert_eq!(run(&f.socket), Some(0));
    assert_eq!(f.log("user.log"), format!("--user stop -- {slice}\n"));
    assert_eq!(stops(), 1);
    // The user stop fails: the helper stop still runs, the shim fails.
    f.write("user.code", "1");
    assert_eq!(run(&f.socket), Some(125));
    assert_eq!(stops(), 2);
    // The helper is gone: the user stop runs, the shim fails.
    f.write("user.code", "0");
    assert_eq!(run(&f.dir.path().join("gone.sock")), Some(125));
    assert_eq!(f.log("user.log").lines().count(), 3);
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

fn uids(run: &str) -> Vec<String> {
    run.lines()
        .filter(|l| l.starts_with("--uid="))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn each_holder_runs_under_its_own_workload_uid() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let (a, r) = send(
        &f.socket,
        &Request::Start(f.start("p/a", None, &[], &[], &["cat"])),
    )
    .await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    let (a2, r) = send(
        &f.socket,
        &Request::Start(f.start("p/a", None, &[], &[], &["cat"])),
    )
    .await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    let (b, r) = send(
        &f.socket,
        &Request::Start(f.start("p/b", None, &[], &[], &["cat"])),
    )
    .await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    let run = f
        .wait_log("run.log", |l| l.matches("--uid=").count() == 3)
        .await;
    let mut seen = uids(&run);
    seen.sort();
    assert_eq!(seen, ["--uid=2001", "--uid=2001", "--uid=2002"]);
    // The pool of two is used up by p/a and p/b.
    let (_, r) = send(
        &f.socket,
        &Request::Start(f.start("p/c", None, &[], &[], &["true"])),
    )
    .await;
    assert!(
        matches!(&r, Reply::Error { code: ErrorKind::Busy, message } if message.contains("no free workload user")),
        "{r:?}"
    );
    // The sessions run `cat`, so they hold their workload users until
    // their stdin ends, however slow the run (a `sleep` could end first).
    for s in [a, a2, b] {
        let (_, mut wr) = s.into_split();
        native::write_frame(&mut wr, Kind::StdinEof, b"")
            .await
            .unwrap();
    }
    f.wait_idle().await;
    // After both ended, p/c gets a UID.
    let (_c, r) = send(
        &f.socket,
        &Request::Start(f.start("p/c", None, &[], &[], &["true"])),
    )
    .await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
}

/// The kernel model behind per-holder UIDs: a process of one UID cannot
/// read the memory map, environment, or signal a process of another UID
/// (ptrace access mode checks the UID), while the same UID can. Needs
/// root to switch UIDs; it is skipped elsewhere.
#[test]
fn another_uid_cannot_reach_a_session_process() {
    use std::os::unix::process::CommandExt;
    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipped: needs root to run processes as two workload UIDs");
        return;
    }
    let mut a = std::process::Command::new("sleep")
        .arg("30")
        .uid(2001)
        .gid(2001)
        .current_dir("/")
        .spawn()
        .unwrap();
    let pid = a.id();
    let probe = |uid: u32, cmd: &str| {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .uid(uid)
            .gid(uid)
            .current_dir("/")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    for cmd in [
        format!("cat /proc/{pid}/environ"),
        format!("cat /proc/{pid}/maps"),
        format!("kill -0 {pid}"),
    ] {
        assert!(!probe(2002, &cmd), "uid 2002 reached uid 2001: {cmd}");
        assert!(
            probe(2001, &cmd),
            "uid 2001 could not reach its own process: {cmd}"
        );
    }
    a.kill().unwrap();
    a.wait().unwrap();
}

#[tokio::test]
async fn a_unit_not_confirmed_stopped_keeps_its_gpu() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    // systemd-run exits, but systemd still reports the unit active.
    f.write("state", "active\n");
    let (s, r) = send(
        &f.socket,
        &Request::Start(f.start("p/a", Some("L1"), &[A], &[A], &["true"])),
    )
    .await;
    let Reply::Started { unit } = r else {
        panic!("{r:?}")
    };
    drop(s);
    let mut stuck = false;
    for _ in 0..100 {
        if f.helper
            .sessions
            .lock()
            .unwrap()
            .claim(&unit)
            .is_some_and(|c| c.stuck)
        {
            stuck = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(stuck, "the claim was freed without a confirmed stop");
    let (_, r) = send(
        &f.socket,
        &Request::Start(f.start("p/b", Some("L2"), &[A], &[A], &["true"])),
    )
    .await;
    assert!(
        matches!(&r, Reply::Error { code: ErrorKind::Busy, message } if message.contains("not confirmed stopped")),
        "{r:?}"
    );
    // The unit stops: the retry frees the claim.
    f.write("state", "inactive\n");
    f.wait_idle().await;
    let (_s, r) = send(
        &f.socket,
        &Request::Start(f.start("p/b", Some("L2"), &[A], &[A], &["true"])),
    )
    .await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
}

/// A failed unit whose cgroup still holds a process is not stopped: its
/// claim stays stuck until the cgroup is empty.
#[tokio::test]
async fn a_failed_unit_with_a_populated_cgroup_keeps_its_gpu() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    let cg = f.dir.path().join("cgroupfs/system.slice/u.service");
    std::fs::create_dir_all(&cg).unwrap();
    std::fs::write(cg.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
    f.write("state", "failed\n");
    f.write("cgroup", "/system.slice/u.service\n");
    let (s, r) = send(
        &f.socket,
        &Request::Start(f.start("p/a", Some("L1"), &[A], &[A], &["true"])),
    )
    .await;
    let Reply::Started { unit } = r else {
        panic!("{r:?}")
    };
    drop(s);
    let mut stuck = false;
    for _ in 0..100 {
        if f.helper
            .sessions
            .lock()
            .unwrap()
            .claim(&unit)
            .is_some_and(|c| c.stuck)
        {
            stuck = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(stuck, "a failed unit with live processes freed its claim");
    assert!(f.log("ctl.log").contains("--property=ControlGroup"));
    // The last process ends: the retry frees the claim.
    std::fs::write(cg.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
    f.wait_idle().await;
}

#[tokio::test]
async fn leftover_units_are_stopped_before_serving_or_the_start_fails() {
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    f.write(
        "units",
        "terrarium-native-5-0000abcd.service loaded active running x\n\
         ssh.service loaded active running y\n",
    );
    std::fs::write(
        f.dir
            .path()
            .join("sessions/terrarium-native-5-0000abcd.service.env"),
        "A=\"b\"\n",
    )
    .unwrap();
    f.helper.kill_leftovers().await.unwrap();
    let ctl = f.log("ctl.log");
    assert!(
        ctl.contains("kill --signal=9 -- terrarium-native-5-0000abcd.service"),
        "{ctl}"
    );
    assert!(!ctl.contains("ssh.service"), "{ctl}");
    assert_eq!(
        std::fs::read_dir(f.dir.path().join("sessions"))
            .unwrap()
            .count(),
        0
    );
    // A leftover that does not stop: the helper must not serve.
    f.write("state", "active\n");
    let e = f.helper.kill_leftovers().await.unwrap_err();
    assert!(e.contains("not confirmed stopped"), "{e}");
    // The list fails: fail closed too.
    f.write("list.fail", "");
    let e = f.helper.kill_leftovers().await.unwrap_err();
    assert!(e.contains("cannot list"), "{e}");
}

#[tokio::test]
async fn gpu_sessions_need_a_verified_nvidia_smi_and_a_clean_workload_user() {
    let f = Fake::with(
        vec![Right::Session],
        nix::unistd::getuid().as_raw(),
        Err("/usr/bin/nvidia-smi is missing".into()),
    )
    .await;
    let (_, r) = send(
        &f.socket,
        &Request::Start(f.start("p/a", None, &[A], &[], &["true"])),
    )
    .await;
    assert!(
        matches!(&r, Reply::Error { code: ErrorKind::Unsupported, message } if message.contains("GPU sessions are off")),
        "{r:?}"
    );
    // Sessions without a GPU still run.
    let (_s, r) = send(
        &f.socket,
        &Request::Start(f.start("p/a", None, &[], &[], &["true"])),
    )
    .await;
    assert!(matches!(r, Reply::Started { .. }), "{r:?}");
    f.wait_idle().await;
    // A workload user that changed since the start (a new group, linger):
    // refused on the next start.
    f.check_ok.store(false, Ordering::SeqCst);
    let (_, r) = send(
        &f.socket,
        &Request::Start(f.start("p/a", None, &[], &[], &["true"])),
    )
    .await;
    assert!(
        matches!(&r, Reply::Error { code: ErrorKind::Unsupported, message } if message.contains("group docker")),
        "{r:?}"
    );
}

#[test]
fn the_socket_belongs_to_the_agent_user() {
    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipped: needs root to give the socket to another UID");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let listener = native::bind(&socket, 4242).unwrap();
        let meta = std::fs::metadata(&socket).unwrap();
        assert_eq!(meta.uid(), 4242);
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // A stale socket is replaced; any other file is refused.
        drop(listener);
        native::bind(&socket, 4242).unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(native::bind(&file, 4242).is_err());
    });
}

/// The process umask, from the Umask line of /proc/self/status. Reading it
/// there does not change it (umask(2) would, for a moment).
fn process_umask() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find_map(|l| l.strip_prefix("Umask:"))
        .expect("a Umask line");
    u32::from_str_radix(line.trim(), 8).unwrap()
}

/// `native::bind` gives the socket mode 0600 and the caller's owner without
/// a change of the process umask (TER-500): a second thread samples the
/// umask while bind runs many times and never sees another value. It also
/// leaves no temporary directory behind.
#[tokio::test]
async fn bind_sets_the_mode_without_the_process_umask() {
    use std::sync::atomic::AtomicUsize;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let uid = nix::unistd::getuid().as_raw();
    let before = process_umask();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let (stop, seen) = (stop.clone(), seen.clone());
        std::thread::spawn(move || {
            let mut other = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let m = process_umask();
                seen.fetch_add(1, Ordering::Relaxed);
                if m != before {
                    other.push(m);
                }
            }
            other
        })
    };
    // Bind until the sampler has read the umask often enough to see a
    // change (and at least 200 times).
    let mut binds = 0;
    while binds < 200 || (seen.load(Ordering::Relaxed) < 20_000 && binds < 20_000) {
        drop(native::bind(&socket, uid).unwrap());
        binds += 1;
    }
    let listener = native::bind(&socket, uid).unwrap();
    stop.store(true, Ordering::Relaxed);
    let other = sampler.join().unwrap();
    assert!(
        other.is_empty(),
        "the umask changed during bind: {:o} -> {:?} ({binds} binds)",
        before,
        other.iter().map(|m| format!("{m:o}")).collect::<Vec<_>>()
    );
    assert_eq!(process_umask(), before);
    let meta = std::fs::symlink_metadata(&socket).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(meta.uid(), uid);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("h.sock")]);
    // The socket accepts at the final path.
    let (_client, accepted) = tokio::join!(UnixStream::connect(&socket), listener.accept());
    accepted.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shim_relays_the_terminal_size_after_sigwinch() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let f = Fake::new(vec![Right::Session], nix::unistd::getuid().as_raw()).await;
    // The session prints a marker once it runs (the shim relays output
    // only after its signal handlers are in place), then waits for the new
    // size: no fixed sleep decides the order.
    let mut req = f.start(
        "p/a",
        None,
        &[],
        &[],
        &[
            "sh",
            "-c",
            "echo ready; i=0; while [ $i -lt 200 ]; do s=$(stty size); \
             if [ \"$s\" = '40 120' ]; then echo \"size $s\"; exit 0; fi; \
             i=$((i + 1)); sleep 0.1; done; echo \"size stayed $s\"; exit 1",
        ],
    );
    req.pty = Some((24, 80));
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_terrarium-host"));
    cmd.args(["native", "--socket"]);
    cmd.arg(&f.socket);
    cmd.env(
        native::REQUEST_ENV,
        serde_json::to_string(&Request::Start(req)).unwrap(),
    );
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = std::io::Read::read(&mut reader, &mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut out = Vec::new();
    let ready = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(d) = rx.recv().await {
            out.extend(d);
            if String::from_utf8_lossy(&out).contains("ready") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(ready, "{:?}", String::from_utf8_lossy(&out));
    pair.master
        .resize(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let found = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(d) = rx.recv().await {
            out.extend(d);
            if String::from_utf8_lossy(&out).contains("size 40 120") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(found, "{:?}", String::from_utf8_lossy(&out));
    assert!(f.log("run.log").lines().any(|l| l == "--pty"));
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
}
