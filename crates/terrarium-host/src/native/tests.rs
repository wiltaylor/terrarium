//! Unit tests of the native GPU helper with fake device nodes. The
//! socket, the shim, and fake systemd are in `tests/native_helper.rs`.

use std::collections::HashMap;

use super::*;

const A: &str = "GPU-aaaaaaaa-0000-0000-0000-000000000001";
const B: &str = "GPU-bbbbbbbb-0000-0000-0000-000000000002";

/// Fake device nodes: a path and its major:minor.
pub struct FakeNodes(pub HashMap<PathBuf, (u64, u64)>);

impl DeviceNodes for FakeNodes {
    fn char_device(&self, path: &Path) -> Result<(u64, u64), String> {
        self.0
            .get(path)
            .copied()
            .ok_or_else(|| format!("device node {}: missing", path.display()))
    }
}

fn nodes() -> FakeNodes {
    FakeNodes(HashMap::from([
        (PathBuf::from("/dev/nvidia0"), (195, 0)),
        (PathBuf::from("/dev/nvidia1"), (195, 1)),
        (PathBuf::from("/dev/nvidiactl"), (195, 255)),
        (PathBuf::from("/dev/nvidia-uvm"), (508, 0)),
    ]))
}

fn inventory() -> Vec<(String, u32)> {
    vec![(A.into(), 0), (B.into(), 1)]
}

fn ctx(root: &Path) -> Ctx {
    Ctx {
        workload: Identity {
            name: "terrarium-work".into(),
            uid: 2001,
            gid: 2001,
        },
        workspace_root: std::fs::canonicalize(root).unwrap(),
        state_dir: "/run/terrarium-host-helper/sessions".into(),
        dev_dir: "/dev".into(),
        support_nodes: vec!["/dev/nvidiactl".into(), "/dev/nvidia-uvm".into()],
    }
}

fn request(root: &Path) -> StartRequest {
    StartRequest {
        session: 7,
        target: "box".into(),
        holder: "terra/w-1".into(),
        lease: Some("01RESERVATION".into()),
        gpus: vec![A.into()],
        exclusive_gpus: vec![A.into()],
        cwd: root.to_string_lossy().into_owned(),
        argv: vec!["python3".into(), "-c".into(), "print('$HOME %h')".into()],
        env: BTreeMap::from([("TOKEN".into(), "a\"b\\c$d`e".into())]),
        ..Default::default()
    }
}

fn props(args: &[String]) -> Vec<&str> {
    args.windows(2)
        .filter(|w| w[0] == "-p")
        .map(|w| w[1].as_str())
        .collect()
}

#[tokio::test]
async fn frames_round_trip_and_refuse_oversized_frames() {
    let mut buf = Vec::new();
    write_frame(&mut buf, Kind::Stdin, b"hello").await.unwrap();
    write_frame(&mut buf, Kind::StdinEof, b"").await.unwrap();
    let mut r = buf.as_slice();
    assert_eq!(
        read_frame(&mut r).await.unwrap(),
        Some((Kind::Stdin, b"hello".to_vec()))
    );
    assert_eq!(
        read_frame(&mut r).await.unwrap(),
        Some((Kind::StdinEof, vec![]))
    );
    assert_eq!(read_frame(&mut r).await.unwrap(), None);
    let big = vec![0u8; MAX_FRAME + 1];
    assert!(write_frame(&mut Vec::new(), Kind::Stdin, &big).await.is_err());
    let mut forged = vec![2u8];
    forged.extend_from_slice(&u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes());
    assert!(read_frame(&mut forged.as_slice()).await.is_err());
    assert!(read_frame(&mut [99u8, 0, 0, 0, 0].as_slice()).await.is_err());
}

#[test]
fn requests_are_typed_and_refuse_unknown_fields() {
    let req = Request::Start(StartRequest {
        session: 1,
        holder: "operator".into(),
        cwd: "/w".into(),
        argv: vec!["true".into()],
        ..Default::default()
    });
    let text = serde_json::to_string(&req).unwrap();
    assert!(text.contains("\"op\":\"start\""), "{text}");
    assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), req);
    // No way to smuggle a unit property, a unit name, or a device.
    for forged in [
        r#"{"op":"start","session":1,"target":"","holder":"h","cwd":"/w","argv":["true"],"properties":["DevicePolicy=auto"]}"#,
        r#"{"op":"start","session":1,"target":"","holder":"h","cwd":"/w","argv":["true"],"unit":"ssh.service"}"#,
        r#"{"op":"stop","target":"t","holder":"h","slice":"-.slice"}"#,
        r#"{"op":"exec_as_root","argv":["sh"]}"#,
        r#"{"op":"hello","version":1,"right":"session"}"#,
    ] {
        assert!(serde_json::from_str::<Request>(forged).is_err(), "{forged}");
    }
}

#[test]
fn config_needs_absolute_paths_and_distinct_users() {
    let ok = "agent_user = \"terrarium\"\nworkload_user = \"terrarium-work\"\nworkspace_root = \"/work\"\nrights = [\"session\"]\n";
    let cfg = HelperConfig::parse(ok).unwrap();
    assert_eq!(cfg.socket, PathBuf::from(DEFAULT_SOCKET));
    assert_eq!(cfg.rights, [Right::Session]);
    assert_eq!(cfg.gpu.support_nodes.len(), 3);
    assert!(HelperConfig::parse(&ok.replace("/work", "work")).is_err());
    assert!(HelperConfig::parse(&ok.replace("terrarium-work", "terrarium")).is_err());
    assert!(HelperConfig::parse(&format!("{ok}systemctl = \"systemctl\"\n")).is_err());
    assert!(HelperConfig::parse(&format!("{ok}run_as_root = true\n")).is_err());
    assert!(HelperConfig::parse(&ok.replace("\"session\"", "\"quota\"")).is_err());
    // No rights by default: only hello.
    let none = HelperConfig::parse(&ok.replace("rights = [\"session\"]\n", "")).unwrap();
    assert!(none.rights.is_empty());
}

#[test]
fn workload_identity_is_checked() {
    let w = Identity {
        name: "terrarium-work".into(),
        uid: 2001,
        gid: 2001,
    };
    let groups = vec![(2001, "terrarium-work".to_owned()), (44, "video".to_owned())];
    check_workload(1000, &w, &groups, false).unwrap();
    assert!(check_workload(2001, &w, &groups, false).is_err());
    assert!(check_workload(0, &w, &groups, false).is_err());
    let root = Identity { uid: 0, ..w.clone() };
    assert!(check_workload(1000, &root, &groups, false).is_err());
    for g in ["docker", "sudo", "wheel", "root"] {
        let mut more = groups.clone();
        more.push((999, g.to_owned()));
        let e = check_workload(1000, &w, &more, false).unwrap_err();
        assert!(e.contains(g), "{e}");
    }
    let e = check_workload(1000, &w, &groups, true).unwrap_err();
    assert!(e.contains("linger"), "{e}");
}

#[test]
fn inventory_is_strict() {
    assert_eq!(
        parse_minors(&format!("{A}, 0\n{B}, 1\n")).unwrap(),
        inventory()
    );
    for bad in [
        format!("{A}, x"),
        format!("{A}, 0\n{A}, 1"),
        format!("{A}, 0\n{B}, 0"),
        format!("{A}, 255"),
        "MIG-abcd, 0".to_owned(),
        A.to_owned(),
    ] {
        assert!(parse_minors(&bad).is_err(), "{bad}");
    }
    assert_eq!(parse_minors("").unwrap(), vec![]);
}

#[test]
fn a_gpu_session_allows_only_its_gpu_and_the_support_nodes() {
    let root = tempfile::tempdir().unwrap();
    let ctx = ctx(root.path());
    let plan = plan_start(&ctx, &request(root.path()), &inventory(), &nodes(), 0xab).unwrap();
    assert_eq!(plan.unit, "terrarium-native-7-000000ab.service");
    assert_eq!(plan.slice, "terrarium-box-terra-w\\x2d1.slice");
    let p = props(&plan.args);
    assert!(p.contains(&"DevicePolicy=closed"));
    assert!(p.contains(&"NoNewPrivileges=yes"));
    assert!(p.contains(&"CapabilityBoundingSet="));
    assert!(p.contains(&"KillMode=control-group"));
    let allowed: Vec<_> = p
        .iter()
        .filter(|x| x.starts_with("DeviceAllow="))
        .copied()
        .collect();
    assert_eq!(
        allowed,
        [
            "DeviceAllow=char-pts rw",
            "DeviceAllow=/dev/nvidia0 rw",
            "DeviceAllow=/dev/nvidiactl rw",
            "DeviceAllow=/dev/nvidia-uvm rw"
        ]
    );
    // Never the other GPU, never the whole NVIDIA major.
    assert!(!plan.args.iter().any(|a| a.contains("nvidia1")));
    assert!(!plan.args.iter().any(|a| a.contains("char-nvidia")));
    assert!(plan.args.contains(&"--uid=2001".to_owned()));
    assert!(plan.args.contains(&"--gid=2001".to_owned()));
    assert!(plan.args.contains(&"--pipe".to_owned()));
    assert!(plan.args.contains(&"--expand-environment=no".to_owned()));
    // The command comes last, after `--`, unchanged.
    let dashes = plan.args.iter().position(|a| a == "--").unwrap();
    assert_eq!(&plan.args[dashes + 1..], request(root.path()).argv.as_slice());
    assert_eq!(plan.env, "TOKEN=\"a\\\"b\\\\c\\$d\\`e\"\n");
    assert_eq!(
        plan.env_file,
        PathBuf::from("/run/terrarium-host-helper/sessions/terrarium-native-7-000000ab.service.env")
    );
    assert!(plan.slice_args.is_none());
    assert_eq!(plan.claim.exclusive, [A]);
}

#[test]
fn a_session_without_gpus_gets_no_gpu_node() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path());
    req.gpus.clear();
    req.exclusive_gpus.clear();
    req.pty = Some((24, 80));
    // No inventory and no device nodes needed.
    let plan = plan_start(
        &ctx(root.path()),
        &req,
        &[],
        &FakeNodes(HashMap::new()),
        1,
    )
    .unwrap();
    let p = props(&plan.args);
    assert!(p.contains(&"DevicePolicy=closed"));
    assert_eq!(
        p.iter().filter(|x| x.starts_with("DeviceAllow=")).count(),
        1,
        "{p:?}"
    );
    assert!(plan.args.contains(&"--pty".to_owned()));
}

#[test]
fn limits_go_on_the_slice_from_typed_values_only() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path());
    req.limits = Some(RuntimeLimits {
        limits: terrarium_core::limits::Limits {
            cpu_millis: Some(2000),
            memory_bytes: Some(1 << 30),
            ..Default::default()
        },
        cpus: Some(vec![2, 3]),
        ..Default::default()
    });
    let plan = plan_start(&ctx(root.path()), &req, &inventory(), &nodes(), 1).unwrap();
    let a = plan.slice_args.unwrap();
    assert_eq!(&a[..4], ["set-property", "--runtime", "--", plan.slice.as_str()]);
    assert!(a.contains(&"CPUQuota=200%".to_owned()));
    assert!(a.contains(&"AllowedCPUs=2,3".to_owned()));
    let mut bad = request(root.path());
    bad.limits = Some(RuntimeLimits {
        cpus: Some(vec![]),
        ..Default::default()
    });
    assert_eq!(
        plan_start(&ctx(root.path()), &bad, &inventory(), &nodes(), 1)
            .unwrap_err()
            .code,
        ErrorKind::Invalid
    );
}

#[test]
fn unsafe_requests_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    std::fs::create_dir(root.path().join("50%")).unwrap();
    let ctx = ctx(root.path());
    let base = request(root.path());
    let cases: Vec<(&str, StartRequest, ErrorKind)> = vec![
        (
            "cwd outside the root",
            StartRequest {
                cwd: outside.path().to_string_lossy().into_owned(),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "symlink out of the root",
            StartRequest {
                cwd: root.path().join("escape").to_string_lossy().into_owned(),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "specifier in cwd",
            StartRequest {
                cwd: root.path().join("50%").to_string_lossy().into_owned(),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "relative cwd",
            StartRequest {
                cwd: "work".into(),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "exclusive GPU without lease",
            StartRequest {
                lease: None,
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "exclusive GPU not granted",
            StartRequest {
                gpus: vec![],
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "unknown GPU",
            StartRequest {
                gpus: vec!["GPU-cccc".into()],
                exclusive_gpus: vec![],
                ..base.clone()
            },
            ErrorKind::Unsupported,
        ),
        (
            "not a GPU UUID",
            StartRequest {
                gpus: vec!["/dev/sda".into()],
                exclusive_gpus: vec![],
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "GPU twice",
            StartRequest {
                gpus: vec![A.into(), A.into()],
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "bad lease",
            StartRequest {
                lease: Some("../x".into()),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "empty holder",
            StartRequest {
                holder: String::new(),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "control character in holder",
            StartRequest {
                holder: "a\nb".into(),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "empty argv",
            StartRequest {
                argv: vec![],
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "NUL in argv",
            StartRequest {
                argv: vec!["a\0b".into()],
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "bad env name",
            StartRequest {
                env: BTreeMap::from([("A=B".into(), "x".into())]),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "newline in env value",
            StartRequest {
                env: BTreeMap::from([("A".into(), "x\nB=y".into())]),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "zero PTY",
            StartRequest {
                pty: Some((0, 80)),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
    ];
    for (what, req, code) in cases {
        let e = plan_start(&ctx, &req, &inventory(), &nodes(), 1).unwrap_err();
        assert_eq!(e.code, code, "{what}: {e}");
    }
}

#[test]
fn device_nodes_must_match_the_inventory() {
    let root = tempfile::tempdir().unwrap();
    let req = request(root.path());
    // The node of minor 0 is another device.
    let mut wrong = nodes();
    wrong.0.insert("/dev/nvidia0".into(), (195, 1));
    let e = plan_start(&ctx(root.path()), &req, &inventory(), &wrong, 1).unwrap_err();
    assert_eq!(e.code, ErrorKind::Unsupported, "{e}");
    let mut other_major = nodes();
    other_major.0.insert("/dev/nvidia0".into(), (1, 0));
    assert!(plan_start(&ctx(root.path()), &req, &inventory(), &other_major, 1).is_err());
    // A missing support node fails closed.
    let mut missing = nodes();
    missing.0.remove(Path::new("/dev/nvidia-uvm"));
    let e = plan_start(&ctx(root.path()), &req, &inventory(), &missing, 1).unwrap_err();
    assert_eq!(e.code, ErrorKind::Unsupported, "{e}");
}

fn claim(holder: &str, lease: Option<&str>, gpus: &[&str], exclusive: &[&str]) -> Claim {
    Claim {
        holder: holder.into(),
        lease: lease.map(str::to_owned),
        gpus: gpus.iter().map(|g| (*g).to_owned()).collect(),
        exclusive: exclusive.iter().map(|g| (*g).to_owned()).collect(),
    }
}

#[test]
fn an_exclusive_gpu_is_never_shared_across_reservations() {
    let mut s = Sessions::default();
    s.admit("u1", claim("p/a", Some("L1"), &[A], &[A])).unwrap();
    // The same reservation and holder may open more sessions.
    s.admit("u2", claim("p/a", Some("L1"), &[A], &[A])).unwrap();
    // Another holder, a shared claim, an unreserved session, or a stale
    // lease of the same holder: busy.
    for (unit, c) in [
        ("u3", claim("p/b", Some("L2"), &[A], &[A])),
        ("u4", claim("p/b", Some("L2"), &[A], &[])),
        ("u5", claim("p/b", None, &[A], &[])),
        ("u6", claim("p/a", Some("L0"), &[A], &[A])),
    ] {
        assert_eq!(s.admit(unit, c).unwrap_err().code, ErrorKind::Busy, "{unit}");
    }
    // A forged holder on the live lease.
    assert_eq!(
        s.admit("u7", claim("p/b", Some("L1"), &[A], &[A]))
            .unwrap_err()
            .code,
        ErrorKind::Forbidden
    );
    // A disjoint GPU is free.
    s.admit("u8", claim("p/b", Some("L2"), &[B], &[B])).unwrap();
    // Capacity is not reassigned until every session of L1 ended.
    s.end("u1");
    assert!(s.admit("u9", claim("p/c", Some("L3"), &[A], &[A])).is_err());
    s.end("u2");
    s.admit("u9", claim("p/c", Some("L3"), &[A], &[A])).unwrap();
}

#[test]
fn a_live_shared_session_blocks_an_exclusive_claim() {
    let mut s = Sessions::default();
    s.admit("u1", claim("p/a", Some("L1"), &[A], &[])).unwrap();
    s.admit("u2", claim("p/b", Some("L2"), &[A], &[])).unwrap();
    assert_eq!(
        s.admit("u3", claim("p/c", Some("L3"), &[A], &[A]))
            .unwrap_err()
            .code,
        ErrorKind::Busy
    );
    s.end("u1");
    s.end("u2");
    assert!(s.is_empty());
    s.admit("u3", claim("p/c", Some("L3"), &[A], &[A])).unwrap();
    assert_eq!(s.len(), 1);
}

#[test]
fn small_parsers() {
    assert_eq!(parse_stty_size("40 120\n"), Some((40, 120)));
    assert_eq!(parse_stty_size("0 0\n"), None);
    assert_eq!(parse_stty_size("x"), None);
    assert_eq!(
        parse_systemd_version("systemd 255 (255.4-1ubuntu8)\n+PAM +AUDIT"),
        Some(255)
    );
    assert_eq!(parse_systemd_version("elogind 252"), None);
    assert_eq!(
        parse_units(
            "terrarium-native-7-000000ab.service loaded active running x\n\
             terrarium-native-x.service loaded active running y\n\
             ssh.service loaded active running z\n\
             terrarium-native-9-00000001.scope loaded active running w\n"
        ),
        ["terrarium-native-7-000000ab.service"]
    );
    assert_eq!(env_line("A", "plain"), "A=\"plain\"\n");
}

#[test]
fn stop_never_takes_a_shared_parent_slice() {
    assert_eq!(
        holder_slice("box", "p/a", true).unwrap(),
        crate::limits::slice_name("box", "p/a")
    );
    assert!(holder_slice("", "", true).is_err());
    assert!(holder_slice("box", "", true).is_err());
    // Apply may set the shared slice (CPU isolation of exclusive holders).
    assert_eq!(holder_slice("", "", false).unwrap(), "terrarium.slice");
}

#[test]
fn an_exec_becomes_a_shim_call_with_the_request_in_the_environment() {
    let plan = crate::session::ExecPlan {
        program: "nvidia-smi".into(),
        args: vec!["-L".into()],
        cwd: Some("/work/terra/w-1".into()),
        env: BTreeMap::from([("SECRET".into(), "s3".into())]),
        pty: Some(crate::protocol::PtySize { rows: 30, cols: 100 }),
        ..Default::default()
    };
    let l = crate::protocol::ExecLimits {
        target: "box".into(),
        holder: "terra/w-1".into(),
        limits: terrarium_core::limits::Limits::default(),
        reservation: Some(RuntimeLimits {
            gpu_devices: vec![A.into()],
            ..Default::default()
        }),
        native_gpu: Some(crate::protocol::NativeGpu {
            lease: Some("01RES".into()),
            exclusive: vec![A.into()],
        }),
    };
    let req = start_request(&plan, 5, Some(&l), Path::new("/work"));
    assert_eq!(req.argv, ["nvidia-smi", "-L"]);
    assert_eq!(req.gpus, [A]);
    assert_eq!(req.exclusive_gpus, [A]);
    assert_eq!(req.lease.as_deref(), Some("01RES"));
    assert_eq!(req.pty, Some((30, 100)));
    assert_eq!(req.cwd, "/work/terra/w-1");
    assert_eq!(req.env["TERM"], "xterm-256color");
    assert_eq!(req.limits, l.reservation);
    let opts = NativeOptions {
        socket: "/run/h.sock".into(),
        shim: "/usr/local/bin/terrarium-host".into(),
    };
    let shim = shim_plan(&opts, &Request::Start(req.clone()), plan);
    assert_eq!(shim.program, "/usr/local/bin/terrarium-host");
    assert_eq!(shim.args, ["native", "--socket", "/run/h.sock"]);
    // The secret is not on the command line.
    assert!(!shim.args.iter().any(|a| a.contains("s3")));
    assert_eq!(
        serde_json::from_str::<Request>(&shim.env[REQUEST_ENV]).unwrap(),
        Request::Start(req)
    );
    assert!(shim.pty.is_some() && shim.scope.is_none());
    // No limits and no reservation: no GPU, the operator's slice.
    let bare = start_request(&crate::session::ExecPlan::default(), 1, None, Path::new("/w"));
    assert!(bare.gpus.is_empty() && bare.limits.is_none());
    assert_eq!(bare.holder, "operator");
    assert_eq!(bare.cwd, "/w");
}
