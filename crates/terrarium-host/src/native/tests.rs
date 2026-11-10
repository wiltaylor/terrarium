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

const W: Identity = Identity {
    name: String::new(),
    uid: 2001,
    gid: 2001,
};

fn ctx(root: &Path) -> Ctx {
    Ctx {
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
    assert!(
        write_frame(&mut Vec::new(), Kind::Stdin, &big)
            .await
            .is_err()
    );
    let mut forged = vec![2u8];
    forged.extend_from_slice(&u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes());
    assert!(read_frame(&mut forged.as_slice()).await.is_err());
    assert!(
        read_frame(&mut [99u8, 0, 0, 0, 0].as_slice())
            .await
            .is_err()
    );
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
    let ok = "agent_user = \"terrarium\"\nworkload_users = [\"terrarium-w1\", \"terrarium-w2\"]\nworkspace_root = \"/work\"\nrights = [\"session\"]\n";
    let cfg = HelperConfig::parse(ok).unwrap();
    assert_eq!(cfg.socket, PathBuf::from(DEFAULT_SOCKET));
    assert_eq!(cfg.rights, [Right::Session]);
    assert_eq!(cfg.gpu.support_nodes.len(), 3);
    assert!(HelperConfig::parse(&ok.replace("/work", "work")).is_err());
    assert!(HelperConfig::parse(&ok.replace("terrarium-w2", "terrarium")).is_err());
    assert!(HelperConfig::parse(&ok.replace("terrarium-w2", "terrarium-w1")).is_err());
    assert!(
        HelperConfig::parse(&ok.replace("[\"terrarium-w1\", \"terrarium-w2\"]", "[]")).is_err()
    );
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
    let groups = vec![
        (2001, "terrarium-work".to_owned()),
        (44, "video".to_owned()),
    ];
    check_workload(1000, &w, &groups, false).unwrap();
    assert!(check_workload(2001, &w, &groups, false).is_err());
    assert!(check_workload(0, &w, &groups, false).is_err());
    let root = Identity {
        uid: 0,
        ..w.clone()
    };
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
    let plan = plan_start(
        &ctx,
        &request(root.path()),
        &inventory(),
        &nodes(),
        0xab,
        &W,
    )
    .unwrap();
    assert_eq!(plan.unit, "terrarium-native-7-000000ab.service");
    assert_eq!(plan.slice, "terrarium-box-terra-w\\x2d1.slice");
    let p = props(&plan.args);
    assert!(p.contains(&"DevicePolicy=closed"));
    assert!(p.contains(&"NoNewPrivileges=yes"));
    assert!(p.contains(&"CapabilityBoundingSet="));
    assert!(p.contains(&"KillMode=control-group"));
    assert!(p.contains(&"PrivateTmp=yes"));
    assert!(p.contains(&"PrivateIPC=yes"));
    assert!(p.contains(&"RemoveIPC=yes"));
    assert!(p.contains(&"ProtectSystem=strict"));
    // /home, /root and /run/user are an empty tmpfs: git trusts every
    // repo (safe.directory=*), so none of them may be visible (TER-659).
    assert!(p.contains(&"ProtectHome=tmpfs"));
    assert!(
        !p.iter()
            .any(|x| x.starts_with("ProtectHome=") && *x != "ProtectHome=tmpfs")
    );
    // No holder workspace yet: the workspace root is an empty read-only
    // tmpfs, nothing is bound in, nothing is writable.
    let real = std::fs::canonicalize(root.path()).unwrap();
    let tmpfs = format!("TemporaryFileSystem={}:ro", real.display());
    assert!(p.contains(&tmpfs.as_str()), "{p:?}");
    assert!(!p.iter().any(|x| x.starts_with("BindPaths=")), "{p:?}");
    assert!(!p.iter().any(|x| x.starts_with("ReadWritePaths=")), "{p:?}");
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
    assert_eq!(
        &plan.args[dashes + 1..],
        request(root.path()).argv.as_slice()
    );
    assert_eq!(plan.env, "TOKEN=\"a\\\"b\\\\c\\$d\\`e\"\n");
    assert_eq!(
        plan.env_file,
        PathBuf::from(
            "/run/terrarium-host-helper/sessions/terrarium-native-7-000000ab.service.env"
        )
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
        &W,
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
    let plan = plan_start(&ctx(root.path()), &req, &inventory(), &nodes(), 1, &W).unwrap();
    let a = plan.slice_args.unwrap();
    assert_eq!(
        &a[..4],
        ["set-property", "--runtime", "--", plan.slice.as_str()]
    );
    assert!(a.contains(&"CPUQuota=200%".to_owned()));
    assert!(a.contains(&"AllowedCPUs=2,3".to_owned()));
    let mut bad = request(root.path());
    bad.limits = Some(RuntimeLimits {
        cpus: Some(vec![]),
        ..Default::default()
    });
    assert_eq!(
        plan_start(&ctx(root.path()), &bad, &inventory(), &nodes(), 1, &W)
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
            "NUL in env value",
            StartRequest {
                env: BTreeMap::from([("A".into(), "x\0y".into())]),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "NUL after a newline in env value",
            StartRequest {
                env: BTreeMap::from([("A".into(), "x\n\0".into())]),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "noncharacter in env value (systemd refuses it)",
            StartRequest {
                env: BTreeMap::from([("A".into(), "x\u{FDD0}".into())]),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "U+FFFE in env value (systemd refuses it)",
            StartRequest {
                env: BTreeMap::from([("A".into(), "\u{FFFE}".into())]),
                ..base.clone()
            },
            ErrorKind::Invalid,
        ),
        (
            "U+10FFFF in env value (systemd refuses it)",
            StartRequest {
                env: BTreeMap::from([("A".into(), "\u{10FFFF}".into())]),
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
        let e = plan_start(&ctx, &req, &inventory(), &nodes(), 1, &W).unwrap_err();
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
    let e = plan_start(&ctx(root.path()), &req, &inventory(), &wrong, 1, &W).unwrap_err();
    assert_eq!(e.code, ErrorKind::Unsupported, "{e}");
    let mut other_major = nodes();
    other_major.0.insert("/dev/nvidia0".into(), (1, 0));
    assert!(plan_start(&ctx(root.path()), &req, &inventory(), &other_major, 1, &W).is_err());
    // A missing support node fails closed.
    let mut missing = nodes();
    missing.0.remove(Path::new("/dev/nvidia-uvm"));
    let e = plan_start(&ctx(root.path()), &req, &inventory(), &missing, 1, &W).unwrap_err();
    assert_eq!(e.code, ErrorKind::Unsupported, "{e}");
}

/// A claim; the holder `p/a` has UID 2001, `p/b` 2002, and so on.
fn claim(holder: &str, lease: Option<&str>, gpus: &[&str], exclusive: &[&str]) -> Claim {
    Claim {
        holder: holder.into(),
        lease: lease.map(str::to_owned),
        gpus: gpus.iter().map(|g| (*g).to_owned()).collect(),
        exclusive: exclusive.iter().map(|g| (*g).to_owned()).collect(),
        uid: 2000 + u32::from(holder.as_bytes()[holder.len() - 1] - b'a' + 1),
        stuck: false,
    }
}

fn pool(n: u32) -> Vec<Identity> {
    (1..=n)
        .map(|i| Identity {
            name: format!("w{i}"),
            uid: 2000 + i,
            gid: 2000 + i,
        })
        .collect()
}

#[test]
fn each_holder_gets_its_own_workload_user() {
    let mut s = Sessions::default();
    let pool = pool(2);
    let a = s.pick("p/a", &pool).unwrap();
    s.admit(
        "u1",
        Claim {
            uid: a.uid,
            ..claim("p/a", None, &[], &[])
        },
    )
    .unwrap();
    // The same holder keeps its UID; another holder gets another one.
    assert_eq!(s.pick("p/a", &pool).unwrap(), a);
    let b = s.pick("p/b", &pool).unwrap();
    assert_ne!(b.uid, a.uid);
    s.admit(
        "u2",
        Claim {
            uid: b.uid,
            ..claim("p/b", None, &[], &[])
        },
    )
    .unwrap();
    // The pool is used up: a third holder is refused with the reason.
    let e = s.pick("p/c", &pool).unwrap_err();
    assert_eq!(e.code, ErrorKind::Busy);
    assert!(e.message.contains("no free workload user"), "{e}");
    // A UID never serves two holders at once, even if the caller forges it.
    let e = s
        .admit(
            "u3",
            Claim {
                uid: a.uid,
                ..claim("p/c", None, &[], &[])
            },
        )
        .unwrap_err();
    assert_eq!(e.code, ErrorKind::Forbidden);
    // Once A's sessions ended, its UID is free for another holder.
    s.end("u1");
    assert_eq!(s.pick("p/c", &pool).unwrap(), a);
}

#[test]
fn a_stuck_unit_keeps_its_gpu_and_its_uid() {
    let mut s = Sessions::default();
    let pool = pool(2);
    s.admit("u1", claim("p/a", Some("L1"), &[A], &[A])).unwrap();
    s.mark_stuck("u1");
    assert!(s.claim("u1").unwrap().stuck);
    // Even the same reservation cannot use the GPU of a stuck unit, nor a
    // shared claim.
    for (unit, c) in [
        ("u2", claim("p/a", Some("L1"), &[A], &[A])),
        ("u3", claim("p/b", None, &[A], &[])),
    ] {
        let e = s.admit(unit, c).unwrap_err();
        assert_eq!(e.code, ErrorKind::Busy);
        assert!(e.message.contains("not confirmed stopped"), "{e}");
    }
    // Its UID stays with p/a.
    assert_eq!(s.pick("p/b", &pool).unwrap().uid, 2002);
    s.end("u1");
    s.admit("u3", claim("p/b", None, &[A], &[])).unwrap();
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
        assert_eq!(
            s.admit(unit, c).unwrap_err().code,
            ErrorKind::Busy,
            "{unit}"
        );
    }
    // Another agent on the same reservation (a target reservation is used
    // by every agent of the target) shares the GPU, under its own UID.
    s.admit("u7", claim("p/d", Some("L1"), &[A], &[A])).unwrap();
    s.end("u7");
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

/// Multi-line values in [`env_line`] need systemd 248 or later: older
/// `env_value_is_valid` drops values with control characters (TER-573).
#[test]
fn min_systemd_keeps_control_characters_in_env_values() {
    const { assert!(MIN_SYSTEMD >= 248) };
}

/// A port of `parse_env_file_internal` of systemd v254
/// (`src/basic/env-file.c`), the parser that PID 1 runs on an
/// `EnvironmentFile=`, state for state and byte for byte. It returns the
/// assignments in file order, as `load_env_file_push` pushes them.
fn systemd_parse_env_file(contents: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    #[derive(Clone, Copy, PartialEq)]
    enum S {
        PreKey,
        Key,
        PreValue,
        Value,
        ValueEscape,
        SingleQuoteValue,
        DoubleQuoteValue,
        DoubleQuoteValueEscape,
        Comment,
        CommentEscape,
    }
    const WHITESPACE: &[u8] = b" \t\n\r";
    const NEWLINE: &[u8] = b"\n\r";
    const COMMENTS: &[u8] = b"#;";
    const SHELL_NEED_ESCAPE: &[u8] = b"\"\\`$";
    fn finish(key: &mut Vec<u8>, last_key_ws: Option<usize>) -> Vec<u8> {
        let mut k = std::mem::take(key);
        if let Some(n) = last_key_ws {
            k.truncate(n);
        }
        k
    }
    let mut out = Vec::new();
    let (mut key, mut value) = (Vec::new(), Vec::new());
    let (mut last_value_ws, mut last_key_ws): (Option<usize>, Option<usize>) = (None, None);
    let mut state = S::PreKey;
    // `*p` stops at the first NUL, like the C loop.
    for &c in contents.iter().take_while(|&&c| c != 0) {
        match state {
            S::PreKey => {
                if COMMENTS.contains(&c) {
                    state = S::Comment;
                } else if !WHITESPACE.contains(&c) {
                    state = S::Key;
                    last_key_ws = None;
                    key.push(c);
                }
            }
            S::Key => {
                if NEWLINE.contains(&c) {
                    state = S::PreKey;
                    key.clear();
                } else if c == b'=' {
                    state = S::PreValue;
                    last_value_ws = None;
                } else {
                    if !WHITESPACE.contains(&c) {
                        last_key_ws = None;
                    } else if last_key_ws.is_none() {
                        last_key_ws = Some(key.len());
                    }
                    key.push(c);
                }
            }
            S::PreValue => {
                if NEWLINE.contains(&c) {
                    state = S::PreKey;
                    out.push((finish(&mut key, last_key_ws), std::mem::take(&mut value)));
                } else if c == b'\'' {
                    state = S::SingleQuoteValue;
                } else if c == b'"' {
                    state = S::DoubleQuoteValue;
                } else if c == b'\\' {
                    state = S::ValueEscape;
                } else if !WHITESPACE.contains(&c) {
                    state = S::Value;
                    value.push(c);
                }
            }
            S::Value => {
                if NEWLINE.contains(&c) {
                    state = S::PreKey;
                    if let Some(n) = last_value_ws {
                        value.truncate(n);
                    }
                    out.push((finish(&mut key, last_key_ws), std::mem::take(&mut value)));
                } else if c == b'\\' {
                    state = S::ValueEscape;
                    last_value_ws = None;
                } else {
                    if !WHITESPACE.contains(&c) {
                        last_value_ws = None;
                    } else if last_value_ws.is_none() {
                        last_value_ws = Some(value.len());
                    }
                    value.push(c);
                }
            }
            S::ValueEscape => {
                state = S::Value;
                if !NEWLINE.contains(&c) {
                    value.push(c);
                }
            }
            S::SingleQuoteValue => {
                if c == b'\'' {
                    state = S::PreValue;
                } else {
                    value.push(c);
                }
            }
            S::DoubleQuoteValue => {
                if c == b'"' {
                    state = S::PreValue;
                } else if c == b'\\' {
                    state = S::DoubleQuoteValueEscape;
                } else {
                    value.push(c);
                }
            }
            S::DoubleQuoteValueEscape => {
                state = S::DoubleQuoteValue;
                if SHELL_NEED_ESCAPE.contains(&c) {
                    value.push(c);
                } else if c != b'\n' {
                    value.push(b'\\');
                    value.push(c);
                }
            }
            S::Comment => {
                if c == b'\\' {
                    state = S::CommentEscape;
                } else if NEWLINE.contains(&c) {
                    state = S::PreKey;
                }
            }
            S::CommentEscape => {
                state = if NEWLINE.contains(&c) {
                    S::PreKey
                } else {
                    S::Comment
                };
            }
        }
    }
    if matches!(
        state,
        S::PreValue
            | S::Value
            | S::ValueEscape
            | S::SingleQuoteValue
            | S::DoubleQuoteValue
            | S::DoubleQuoteValueEscape
    ) {
        if state == S::Value
            && let Some(n) = last_value_ws
        {
            value.truncate(n);
        }
        out.push((finish(&mut key, last_key_ws), value));
    }
    out
}

/// The port above reads systemd's own examples as systemd does, so that
/// the round trip below proves something about systemd, not about itself.
#[test]
fn the_systemd_parser_port_matches_systemd() {
    let got = systemd_parse_env_file(
        b"# c\nA=plain  \nB = \"q \\\"x\\\" \\n\"\nC='s\\n'\nD=a\\\nb\n;x\nE=\"l1\nl2\"\n",
    );
    let want: Vec<(&[u8], &[u8])> = vec![
        (b"A", b"plain"),
        (b"B", b"q \"x\" \\n"),
        (b"C", b"s\\n"),
        (b"D", b"ab"),
        (b"E", b"l1\nl2"),
    ];
    let got: Vec<(&[u8], &[u8])> = got
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    assert_eq!(got, want);
}

/// TER-566: a multi-line value (the daemon's git trailers, known hosts,
/// commit hook) is accepted, and systemd reads the exact bytes back from
/// the environment file, with no other assignment.
#[test]
fn multi_line_env_values_reach_the_unit_byte_for_byte() {
    let values: &[&str] = &[
        "Card: TER-566\nAgent: ter-566\n",
        "host1 ssh-ed25519 AAAA\nhost2 ssh-rsa BBBB\n",
        "#!/bin/sh\nset -e\nexec git interpret-trailers \"$1\" `x` \\\n",
        "x\nB=y",
        "a\r\nb\rc\n\n",
        "\n#comment\n;semi\n",
        "trailing\\",
        "back\\\nslash",
        "  lead and trail  \t",
        "\"\"''\\\\$$``",
        "ctl \x1b[0m \x07 \x7f \u{85} tab\t",
        "unicode \u{e9} \u{1F600} \u{FFFD}",
        "",
    ];
    let root = tempfile::tempdir().unwrap();
    let ctx = ctx(root.path());
    let mut req = request(root.path());
    req.env = values
        .iter()
        .enumerate()
        .map(|(i, v)| (format!("V{i:02}"), (*v).to_owned()))
        .collect();
    req.env
        .insert("TERRARIUM_GIT_TRAILERS".into(), "A: 1\nB: 2\n".into());
    let plan = plan_start(&ctx, &req, &inventory(), &nodes(), 1, &W).unwrap();
    let got: Vec<(String, Vec<u8>)> = systemd_parse_env_file(plan.env.as_bytes())
        .into_iter()
        .map(|(k, v)| (String::from_utf8(k).unwrap(), v))
        .collect();
    let want: Vec<(String, Vec<u8>)> = req
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
        .collect();
    assert_eq!(got, want, "{:?}", plan.env);
    // The values are in the root-only environment file, never on the
    // systemd-run command line.
    for v in values.iter().filter(|v| v.len() > 2) {
        assert!(!plan.args.iter().any(|a| a.contains(v)), "{v:?}");
    }
}

#[test]
fn trusted_paths_need_root_owned_parents() {
    // This test runs as root on the build targets and as a user elsewhere:
    // a temporary directory is never trusted (owner or a 1777 parent).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "").unwrap();
    assert!(check_root_chain(&dir.path().join("config.toml")).is_err());
    assert!(check_root_chain(&dir.path().join("missing")).is_err());
    if Path::new("/usr/bin/env").exists() {
        check_root_chain(Path::new("/usr/bin/env")).unwrap();
    }
    assert!(check_root_chain(Path::new("relative/bin")).is_err());
}

/// Each symlink on the way is checked, not only the path as written and
/// the end: a root-owned link whose next hop sits in a writable directory,
/// or is owned by another user, is not trusted, and neither is a directory
/// symlink in the middle of the path. Needs root for a trusted directory;
/// it is skipped elsewhere.
#[test]
fn every_hop_of_a_symlink_chain_is_checked() {
    use std::os::unix::fs::symlink;
    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipped: needs root for a root-owned trusted directory");
        return;
    }
    let Ok(env) = std::fs::canonicalize("/usr/bin/env") else {
        eprintln!("skipped: no /usr/bin/env");
        return;
    };
    let trusted = tempfile::tempdir_in("/").unwrap();
    let t = trusted.path();
    let writable = tempfile::tempdir().unwrap();
    let w = writable.path();
    // Two hops, the middle link in a writable directory, the end trusted:
    // the path as written and its end both pass, the middle hop does not.
    symlink(&env, w.join("link2")).unwrap();
    symlink(w.join("link2"), t.join("nvidia-smi")).unwrap();
    let e = check_root_chain(&t.join("nvidia-smi")).unwrap_err();
    assert!(e.contains("must not be writable"), "{e}");
    // The middle link is in a trusted directory but owned by another user.
    symlink(&env, t.join("theirs")).unwrap();
    std::os::unix::fs::lchown(t.join("theirs"), Some(4242), None).unwrap();
    symlink(t.join("theirs"), t.join("via-theirs")).unwrap();
    let e = check_root_chain(&t.join("via-theirs")).unwrap_err();
    assert!(e.contains("must be owned by root"), "{e}");
    // A directory symlink in the middle of the path.
    std::fs::write(w.join("bin"), "").unwrap();
    symlink(w, t.join("dir")).unwrap();
    let e = check_root_chain(&t.join("dir/bin")).unwrap_err();
    assert!(e.contains("must not be writable"), "{e}");
    // Root-owned links in trusted directories, absolute and relative:
    // trusted, and the result is the real path.
    symlink(&env, t.join("hop2")).unwrap();
    symlink("hop2", t.join("hop1")).unwrap();
    assert_eq!(check_root_chain(&t.join("hop1")).unwrap(), env);
    assert_eq!(
        check_root_chain(&t.join("../").join(t.file_name().unwrap()).join("hop1")).unwrap(),
        env
    );
    // A loop ends at the hop limit.
    symlink("loop", t.join("loop")).unwrap();
    let e = check_root_chain(&t.join("loop")).unwrap_err();
    assert!(e.contains("symlinks"), "{e}");
}

#[test]
fn a_workspace_root_that_cannot_be_a_systemd_path_is_refused() {
    let root = tempfile::Builder::new().prefix("a b").tempdir().unwrap();
    let e = plan_start(
        &ctx(root.path()),
        &request(root.path()),
        &inventory(),
        &nodes(),
        1,
        &W,
    )
    .err()
    .unwrap();
    assert_eq!(e.code, ErrorKind::Unsupported);
    assert!(e.message.contains("systemd property"), "{}", e.message);
}

/// A pool UID passes from holder A to holder B. B's unit gets only B's
/// workspace: A's workspace, with a 0600 file that A's session left, is
/// not in any property of B's unit, so it does not exist under B's empty
/// workspace tmpfs, whatever its mode. A working directory in A's
/// workspace is refused for B. (The mounts themselves need systemd: the
/// hardware validation TER-460 opens the file from a real unit.)
#[test]
fn a_session_sees_only_its_own_holders_workspace() {
    let root = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(root.path()).unwrap();
    let a = real.join("terra/w-1");
    let b = real.join("terra/w-2");
    std::fs::create_dir_all(a.join("repo")).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let secret = a.join("repo/.git-credentials");
    std::fs::write(&secret, "token").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let ctx = ctx(root.path());
    let plan_for = |holder: &str, cwd: &Path| {
        let mut req = request(root.path());
        req.holder = holder.into();
        req.gpus.clear();
        req.exclusive_gpus.clear();
        req.lease = None;
        req.cwd = cwd.to_string_lossy().into_owned();
        plan_start(&ctx, &req, &inventory(), &nodes(), 1, &W)
    };
    // A: its own workspace bound read-write, its cwd there.
    let pa = plan_for("terra/w-1", &a.join("repo")).unwrap();
    let p = props(&pa.args);
    assert!(
        p.contains(&format!("BindPaths={}", a.display()).as_str()),
        "{p:?}"
    );
    assert!(p.contains(&format!("ReadWritePaths={}", a.display()).as_str()));
    // B, same pool UID later: only B's workspace; A's path is nowhere.
    let pb = plan_for("terra/w-2", &b).unwrap();
    let p = props(&pb.args);
    assert!(p.contains(&format!("TemporaryFileSystem={}:ro", real.display()).as_str()));
    assert!(
        p.contains(&format!("BindPaths={}", b.display()).as_str()),
        "{p:?}"
    );
    let a_text = a.display().to_string();
    assert!(
        !pb.args.iter().any(|x| x.contains(&a_text)),
        "{:?}",
        pb.args
    );
    // B may not start in A's workspace.
    let e = plan_for("terra/w-2", &a.join("repo")).err().unwrap();
    assert!(e.message.contains("outside the workspace"), "{}", e.message);
    // A holder that is neither an agent, a project service, nor the
    // operator gets the empty root only.
    let pn = plan_for("terra", &real).unwrap();
    assert!(!props(&pn.args).iter().any(|x| x.starts_with("BindPaths=")));
    assert!(plan_for("terra", &a).is_err());
    // A path with ':' is fine for WorkingDirectory= (only the mount
    // properties refuse it).
    let colon = a.join("repo/a:b");
    std::fs::create_dir_all(&colon).unwrap();
    plan_for("terra/w-1", &colon).unwrap();
    // A symlinked workspace is refused.
    std::os::unix::fs::symlink(&a, real.join("terra/w-3")).unwrap();
    assert!(holder_workspace(&real, "terra/w-3").is_err());
    assert!(holder_workspace(&real, "terra/..").is_err());
    assert_eq!(holder_workspace(&real, "terra/missing").unwrap(), None);
}

/// The bind scope follows the holder's trust domain: a project service
/// (`<project>/@jobs`, `@pipelines`, `@monitors`, `@schedules`) gets its
/// project's directory, never another project's; an agent named like a
/// service gets only its own directory; the operator gets the whole
/// workspace root.
#[test]
fn the_workspace_scope_follows_the_holders_trust_domain() {
    let root = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(root.path()).unwrap();
    let terra = real.join("terra");
    let other = real.join("other");
    let slot = terra.join("slot-3");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::create_dir_all(terra.join("w-1")).unwrap();
    std::fs::create_dir_all(other.join("slot-1")).unwrap();
    let ctx = ctx(root.path());
    let plan_for = |holder: &str, cwd: &Path| {
        let mut req = request(root.path());
        req.holder = holder.into();
        req.gpus.clear();
        req.exclusive_gpus.clear();
        req.lease = None;
        req.cwd = cwd.to_string_lossy().into_owned();
        plan_start(&ctx, &req, &inventory(), &nodes(), 1, &W)
    };
    let bind = |plan: &Plan| -> Vec<String> {
        props(&plan.args)
            .into_iter()
            .filter(|x| x.starts_with("BindPaths=") || x.starts_with("TemporaryFileSystem="))
            .map(str::to_owned)
            .collect()
    };
    // A job in a slot of its project (or a project schedule, pipeline, or
    // monitor in the project): the project's directory is bound.
    for service in PROJECT_SERVICES {
        let holder = service_holder("terra", service);
        assert_eq!(holder, format!("terra/@{service}"));
        let plan = plan_for(&holder, &slot).unwrap();
        assert_eq!(
            bind(&plan),
            [
                format!("TemporaryFileSystem={}:ro", real.display()),
                format!("BindPaths={}", terra.display()),
            ],
            "{holder}"
        );
        assert!(
            props(&plan.args).contains(&format!("ReadWritePaths={}", terra.display()).as_str())
        );
        // Also an agent's directory of the same project (one trust domain).
        plan_for(&holder, &terra.join("w-1")).unwrap();
        // Never another project's.
        let e = plan_for(&holder, &other.join("slot-1")).err().unwrap();
        assert!(e.message.contains("outside the workspace"), "{}", e.message);
        assert!(
            !plan
                .args
                .iter()
                .any(|x| x.contains(&*other.to_string_lossy()))
        );
    }
    // An agent of the project still gets only its own directory, also an
    // agent named like a service.
    assert!(plan_for("terra/w-1", &slot).is_err());
    std::fs::create_dir_all(terra.join("jobs")).unwrap();
    for name in PROJECT_SERVICES {
        let agent = format!("terra/{name}");
        assert!(plan_for(&agent, &slot).is_err(), "{agent}");
        std::fs::create_dir_all(terra.join(name)).unwrap();
        let plan = plan_for(&agent, &terra.join(name)).unwrap();
        assert_eq!(
            bind(&plan),
            [
                format!("TemporaryFileSystem={}:ro", real.display()),
                format!("BindPaths={}", terra.join(name).display()),
            ],
            "{agent}"
        );
    }
    // An `@` holder that is no known service: the empty root only.
    assert_eq!(holder_workspace(&real, "terra/@other").unwrap(), None);
    // The operator: the whole root, writable, no tmpfs over it, any cwd.
    let plan = plan_for("operator", &other.join("slot-1")).unwrap();
    let p = props(&plan.args);
    assert!(bind(&plan).is_empty(), "{p:?}");
    assert!(p.contains(&format!("ReadWritePaths={}", real.display()).as_str()));
}

#[test]
fn a_workspace_root_under_a_private_tmp_is_refused_at_start() {
    for bad in ["/tmp/work", "/var/tmp/w", "/srv/a b"] {
        assert!(check_workspace_root(Path::new(bad)).is_err(), "{bad}");
    }
    let e = check_workspace_root(Path::new("/tmp/work")).unwrap_err();
    assert!(e.contains("PrivateTmp"), "{e}");
    check_workspace_root(Path::new("/srv/terrarium/work")).unwrap();
    check_workspace_root(Path::new("/tmpfoo/work")).unwrap();
    // ProtectHome=tmpfs hides these in each session (TER-659).
    for bad in ["/home/op/work", "/root/w", "/run/user/1000/w"] {
        let e = check_workspace_root(Path::new(bad)).unwrap_err();
        assert!(e.contains("ProtectHome"), "{bad}: {e}");
    }
    check_workspace_root(Path::new("/homes/work")).unwrap();
}

#[test]
fn the_state_dir_is_checked_before_it_is_made() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("sessions");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let e = check_state_dir(&link).unwrap_err();
    assert!(e.contains("must not be a symlink"), "{e}");
    let file = dir.path().join("file");
    std::fs::write(&file, "").unwrap();
    assert!(check_state_dir(&file).unwrap_err().contains("directory"));
    // A missing state_dir under a writable parent: refused before it is
    // made.
    assert!(check_state_dir(&dir.path().join("a/b")).is_err());
    assert!(check_state_dir(Path::new("relative")).is_err());
    if nix::unistd::geteuid().is_root() {
        let trusted = tempfile::tempdir_in("/").unwrap();
        check_state_dir(&trusted.path().join("a/b")).unwrap();
    }
}

#[test]
fn a_failed_unit_is_stopped_only_with_an_empty_cgroup() {
    let root = tempfile::tempdir().unwrap();
    let cg = root.path().join("system.slice/u.service");
    std::fs::create_dir_all(&cg).unwrap();
    // Not cgroup v2: never empty.
    assert!(cgroup_empty(root.path(), "/system.slice/gone.service").is_err());
    assert!(check_cgroup_v2(root.path()).is_err());
    std::fs::write(root.path().join("cgroup.controllers"), "cpu memory\n").unwrap();
    check_cgroup_v2(root.path()).unwrap();
    // No cgroup, or a cgroup that is gone: empty.
    assert!(cgroup_empty(root.path(), "").unwrap());
    assert!(cgroup_empty(root.path(), "/system.slice/gone.service").unwrap());
    // A cgroup directory without cgroup.events: not empty.
    assert!(cgroup_empty(root.path(), "/system.slice/u.service").is_err());
    std::fs::write(cg.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
    assert!(!cgroup_empty(root.path(), "/system.slice/u.service").unwrap());
    std::fs::write(cg.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
    assert!(cgroup_empty(root.path(), "/system.slice/u.service").unwrap());
    std::fs::write(cg.join("cgroup.events"), "frozen 0\n").unwrap();
    assert!(cgroup_empty(root.path(), "/system.slice/u.service").is_err());
    for bad in ["system.slice", "/system.slice/../x"] {
        assert!(cgroup_empty(root.path(), bad).is_err(), "{bad}");
    }
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
        pty: Some(crate::protocol::PtySize {
            rows: 30,
            cols: 100,
        }),
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
    let shim = shim_plan(&opts, &Request::Start(req.clone()), plan.clone()).unwrap();
    assert_eq!(shim.program, "/usr/local/bin/terrarium-host");
    assert_eq!(shim.args, ["native", "--socket", "/run/h.sock"]);
    // The secret is not on the command line.
    assert!(!shim.args.iter().any(|a| a.contains("s3")));
    assert_eq!(
        serde_json::from_str::<Request>(&shim.env[REQUEST_ENV]).unwrap(),
        Request::Start(req.clone())
    );
    assert!(shim.pty.is_some() && shim.scope.is_none());
    // A request that does not fit in one environment string is refused.
    let mut big = req.clone();
    big.env.insert("BIG".into(), "x".repeat(MAX_SHIM_REQUEST));
    let e = shim_plan(&opts, &Request::Start(big), plan).unwrap_err();
    assert!(e.contains("too large"), "{e}");
    // No limits and no reservation: no GPU, the operator's slice.
    let bare = start_request(
        &crate::session::ExecPlan::default(),
        1,
        None,
        Path::new("/w"),
    );
    assert!(bare.gpus.is_empty() && bare.limits.is_none());
    assert_eq!(bare.holder, "operator");
    assert_eq!(bare.cwd, "/w");
}
