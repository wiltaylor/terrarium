//! Resource limits with the real systemd user manager (docs/host-protocol.md,
//! "Resource limits"). The tests skip when `systemd-run --user` does not
//! work, or when the user manager lacks a controller that they need.
//!
//! Two sessions of one holder run in two scopes of one slice. Together
//! they ask for more memory than the slice cap, so the kernel throttles or
//! kills them, and the slice never goes past the cap.

use std::sync::Arc;
use std::time::{Duration, Instant};

use terrarium_host::limits::{self, apply_slice, slice_name, wrap};
use terrarium_host::outbox::Outbox;
use terrarium_host::protocol::Limits;
use terrarium_host::session::{self, ExecPlan, SessionInput};

const MIB: u64 = 1024 * 1024;

/// The hog: allocate and touch `$1` MiB, print `ok`, then sleep `$2` s.
const HOG: &str = "import sys,time\n\
b=bytearray(int(sys.argv[1])*1048576)\n\
for i in range(0,len(b),4096): b[i]=1\n\
print('ok',flush=True)\n\
time.sleep(float(sys.argv[2]))\n";

async fn ready(need: &[&str]) -> bool {
    let cap = limits::detect().await;
    if !cap.systemd {
        eprintln!("skip: {}", cap.reason.unwrap_or_default());
        return false;
    }
    if let Some(c) = need
        .iter()
        .find(|c| !cap.controllers.iter().any(|h| h == *c))
    {
        eprintln!("skip: the user manager has no {c} controller");
        return false;
    }
    true
}

fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_ok_and(|s| s.success())
}

/// `systemctl --user show -p <prop> --value <unit>`.
fn show(unit: &str, prop: &str) -> String {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", "-p", prop, "--value", "--", unit])
        .envs(limits::user_bus_env())
        .output()
        .expect("systemctl");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn systemctl(args: &[&str]) {
    let _ = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .envs(limits::user_bus_env())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// A slice of a target name of this test run, removed at the end with the
/// parent slice of the target.
struct TestSlice(String, String);

impl TestSlice {
    fn new(tag: &str) -> Self {
        let target = format!("itest-{tag}-{}", std::process::id());
        Self(
            slice_name(&target, "itest/agent"),
            format!("terrarium-{}.slice", limits::escape(&target)),
        )
    }

    fn cgroup_file(&self, file: &str) -> String {
        let cg = show(&self.0, "ControlGroup");
        std::fs::read_to_string(format!("/sys/fs/cgroup{cg}/{file}")).unwrap_or_default()
    }
}

impl Drop for TestSlice {
    fn drop(&mut self) {
        systemctl(&["revert", "--", &self.0]);
        systemctl(&["stop", "--", &self.1]);
    }
}

fn plan(slice: &str, n: u32, program: &str, args: &[&str]) -> ExecPlan {
    let mut p = ExecPlan {
        program: program.into(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        timeout: Some(Duration::from_secs(90)),
        capture_stdout: true,
        ..Default::default()
    };
    let scope = limits::scope_name("itest", std::process::id(), n);
    wrap(&mut p, slice, &scope);
    p
}

#[tokio::test]
async fn two_scopes_share_the_memory_cap_of_the_slice() {
    if !ready(&["cpu", "memory", "pids"]).await || !have_python() {
        return;
    }
    let slice = TestSlice::new("mem");
    let l = Limits {
        cpu_millis: Some(1500),
        memory_bytes: Some(100 * MIB),
        swap_bytes: None,
        pids: Some(64),
        io_weight: None,
    };
    apply_slice(&slice.0, &l).await.expect("set-property");
    // The properties read back from the user manager.
    assert_eq!(show(&slice.0, "MemoryMax"), (100 * MIB).to_string());
    assert_eq!(show(&slice.0, "MemoryHigh"), (90 * MIB).to_string());
    assert_eq!(show(&slice.0, "MemorySwapMax"), "0");
    assert_eq!(show(&slice.0, "CPUQuotaPerSecUSec"), "1.500000s");
    assert_eq!(show(&slice.0, "TasksMax"), "64");

    // A holds 70 MiB for 4 s. B then asks for 70 MiB more: 140 MiB in a
    // slice of 100 MiB.
    let a = session::spawn(
        plan(&slice.0, 1, "python3", &["-c", HOG, "70", "4"]),
        Arc::new(Outbox::new(1, 1 << 20)),
    )
    .expect("spawn a");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let started = Instant::now();
    let b = session::spawn(
        plan(&slice.0, 2, "python3", &["-c", HOG, "70", "0"]),
        Arc::new(Outbox::new(2, 1 << 20)),
    )
    .expect("spawn b");
    let (a, b) = (a.done.await.unwrap(), b.done.await.unwrap());
    let b_secs = started.elapsed().as_secs_f64();
    let events = slice.cgroup_file("memory.events");
    let oom_kills: u64 = events
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    let peak: u64 = show(&slice.0, "MemoryPeak").parse().unwrap_or(0);
    eprintln!(
        "a: {:?} {:?}, b: {:?} {:?} after {b_secs:.1}s, peak {} MiB, oom_kill {oom_kills}",
        a.code,
        a.signal,
        b.code,
        b.signal,
        peak / MIB
    );
    // The slice never went past its cap: the 140 MiB never lived at one time.
    assert!(peak <= 101 * MIB, "peak {peak} is past the cap");
    // So the kernel throttled B until A let go, or the OOM killer acted.
    assert!(
        oom_kills > 0 || b_secs >= 2.0,
        "b got its memory at once ({b_secs:.1}s) and nothing was killed: {events}"
    );
    assert_eq!(slice.cgroup_file("memory.swap.max").trim(), "0");
}

#[tokio::test]
async fn a_kill_ends_the_whole_scope() {
    if !ready(&["pids"]).await {
        return;
    }
    let slice = TestSlice::new("kill");
    apply_slice(
        &slice.0,
        &Limits {
            pids: Some(32),
            ..Default::default()
        },
    )
    .await
    .expect("set-property");
    // `setsid` takes a child out of the process group: only the scope
    // kill reaches it.
    let p = plan(
        &slice.0,
        3,
        "sh",
        &[
            "-c",
            "setsid sleep 300 </dev/null >/dev/null 2>&1 & echo up; sleep 300",
        ],
    );
    let scope = p.scope.clone().unwrap();
    let run = session::spawn(p, Arc::new(Outbox::new(3, 1 << 20))).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    // The shell may exec its last command: two tasks.
    while show(&scope, "TasksCurrent").parse::<u64>().unwrap_or(0) < 2 {
        assert!(
            Instant::now() < deadline,
            "the scope did not start its tasks"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    run.input
        .send(SessionInput::Kill(
            terrarium_host::protocol::ExitReason::Timeout,
        ))
        .unwrap();
    let info = run.done.await.unwrap();
    assert_eq!(info.signal, Some(9));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = show(&scope, "ActiveState");
        if state == "inactive" || state == "failed" || state.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the scope still runs: {state}, tasks {}",
            show(&scope, "TasksCurrent")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
