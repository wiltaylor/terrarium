//! Test support (feature `test-support`): end the `terrarium-runner`
//! processes that a test started, also when the test panics, and sweep the
//! runners that an earlier, crashed test run left behind.
//!
//! A runner process calls `setsid`, so it does not end with the test
//! process. The harness of a runner has its own session too (the PTY). A
//! test that leaves a runner running leaves two processes: the runner and
//! its harness. [`RunnerProcessGuard`] ends both when it drops.
//!
//! The functions read `/proc`, so they work on Linux only. They remove no
//! files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant, SystemTime};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::{Pid, getpgrp};
use terrarium_core::AgentId;

use crate::config::{pid_path, runners_dir};
use crate::process::live_runner_pid;

/// The file name of the runner binary.
const RUNNER: &str = "terrarium-runner";

/// A completed delivery for a fake `RunnerHandle` in another crate.
pub fn delivered_turn() -> crate::Delivery {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = tx.send(Ok(()));
    crate::Delivery::from_parts(0, rx)
}

/// A failed delivery (`DeliveryError::Dropped`) for a fake `RunnerHandle`
/// in another crate.
pub fn failed_turn() -> crate::Delivery {
    let (_, rx) = tokio::sync::oneshot::channel();
    crate::Delivery::from_parts(0, rx)
}

/// How long [`RunnerProcessGuard`] waits after SIGTERM before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(2);

/// [`sweep_stale_runners_once`] ends a runner when its pid file is older
/// than this.
pub const STALE_AGE: Duration = Duration::from_secs(10 * 60);

/// Ends the runner processes of one runtime dir when it drops, also on a
/// panic. Put it before the temp dir in a struct (fields drop in order), or
/// after the temp dir in a function (locals drop in reverse order).
///
/// On drop it finds the runners of the dir: the pids given to
/// [`RunnerProcessGuard::track`], the live pid files in
/// `<run>/runners/`, and the `terrarium-runner` processes with
/// `--runtime-dir <run>` on their command line (a runner that has not
/// locked its pid file yet). It sends SIGTERM to each runner, waits up to
/// [`TERM_GRACE`] for the runners and their children to exit, then sends
/// SIGKILL to the process group of each one that is still alive.
pub struct RunnerProcessGuard {
    runtime_dir: PathBuf,
    tracked: Mutex<Vec<u32>>,
}

impl RunnerProcessGuard {
    pub fn new(runtime_dir: impl Into<PathBuf>) -> Self {
        Self {
            runtime_dir: runtime_dir.into(),
            tracked: Mutex::new(Vec::new()),
        }
    }

    /// Also end the runner with this pid.
    pub fn track(&self, pid: u32) {
        self.tracked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(pid);
    }

    /// The live runner processes of the runtime dir.
    pub fn runner_pids(&self) -> Vec<u32> {
        let mut pids: HashSet<u32> = self
            .tracked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .copied()
            .filter(|p| is_runner(*p))
            .collect();
        if let Ok(dir) = std::fs::read_dir(runners_dir(&self.runtime_dir)) {
            for e in dir.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_suffix(".pid")
                    && let Ok(agent) = id.parse::<AgentId>()
                    && let Some(pid) = live_runner_pid(&self.runtime_dir, agent)
                    && is_runner(pid)
                {
                    pids.insert(pid);
                }
            }
        }
        for pid in all_pids() {
            let argv = cmdline(pid);
            if is_runner_argv(&argv)
                && flag(&argv, "--runtime-dir").is_some_and(|d| Path::new(d) == self.runtime_dir)
            {
                pids.insert(pid);
            }
        }
        let mut pids: Vec<u32> = pids.into_iter().collect();
        pids.sort_unstable();
        pids
    }
}

impl Drop for RunnerProcessGuard {
    fn drop(&mut self) {
        let pids = self.runner_pids();
        if !pids.is_empty() {
            terminate(&pids, TERM_GRACE);
        }
    }
}

/// End the processes `runners` and all their descendants: SIGTERM to each
/// runner, a wait of up to `grace`, then SIGKILL to the process group of
/// each process that is still alive. Returns when all are gone, or after
/// one more second.
pub fn terminate(runners: &[u32], grace: Duration) {
    // Take the tree before the signals: a harness whose runner is gone has
    // a new parent.
    let tree = with_descendants(runners);
    for &pid in runners {
        let _ = kill(to_pid(pid), Signal::SIGTERM);
    }
    if wait_gone(&tree, grace) {
        return;
    }
    let own = getpgrp().as_raw();
    for &(pid, pgid) in &tree {
        if !alive(pid) {
            continue;
        }
        if pgid > 1 && pgid != own {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        }
        let _ = kill(to_pid(pid), Signal::SIGKILL);
    }
    let _ = wait_gone(&tree, Duration::from_secs(1));
}

/// End the `terrarium-runner` processes that an earlier test run left: the
/// processes of the binary `bin` whose `--state-dir` is in the system temp
/// dir, and whose state dir is gone or whose pid file is older than
/// `older_than`. Returns their pids.
pub fn sweep_stale_runners(bin: &Path, older_than: Duration) -> Vec<u32> {
    let want = canonical(bin);
    let temp = canonical(&std::env::temp_dir());
    let mut stale = Vec::new();
    for pid in all_pids() {
        let argv = cmdline(pid);
        if !is_runner_argv(&argv) || pid == std::process::id() {
            continue;
        }
        let Some(exe) = exe_path(pid) else { continue };
        if exe != want {
            continue;
        }
        let (Some(state), Some(run), Some(agent)) = (
            flag(&argv, "--state-dir"),
            flag(&argv, "--runtime-dir"),
            flag(&argv, "--agent").and_then(|a| a.parse::<AgentId>().ok()),
        ) else {
            continue;
        };
        let state = Path::new(state);
        if !(state.starts_with(&temp) || state.starts_with("/tmp")) {
            continue;
        }
        let old = std::fs::metadata(pid_path(Path::new(run), agent))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > older_than);
        if !state.exists() || old {
            stale.push(pid);
        }
    }
    if !stale.is_empty() {
        terminate(&stale, TERM_GRACE);
    }
    stale
}

/// [`sweep_stale_runners`] with [`STALE_AGE`], once for each test binary.
/// Call it at the start of each test that starts runner processes.
pub fn sweep_stale_runners_once(bin: &Path) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let pids = sweep_stale_runners(bin, STALE_AGE);
        if !pids.is_empty() {
            eprintln!("ended stale runner processes from an earlier test run: {pids:?}");
        }
    });
}

fn to_pid(pid: u32) -> Pid {
    Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX))
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The binary of `pid`. A rebuilt binary shows as `<path> (deleted)`.
fn exe_path(pid: u32) -> Option<PathBuf> {
    let link = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let s = link.to_string_lossy();
    Some(PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s)))
}

fn all_pids() -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn is_runner_argv(argv: &[String]) -> bool {
    argv.first()
        .and_then(|a| Path::new(a).file_name())
        .is_some_and(|n| n == RUNNER)
        && argv.iter().any(|a| a == "--agent")
}

fn is_runner(pid: u32) -> bool {
    alive(pid) && is_runner_argv(&cmdline(pid))
}

/// The value after `name` in `argv`.
fn flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    let i = argv.iter().position(|a| a == name)?;
    argv.get(i + 1).map(String::as_str)
}

/// The state, the parent pid, and the process group of `pid`.
fn stat(pid: u32) -> Option<(char, u32, i32)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid pgrp ...`; comm can hold spaces and `)`.
    let rest = &text[text.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    let pgid = f.next()?.parse().ok()?;
    Some((state, ppid, pgid))
}

/// A zombie counts as gone: it runs no code, and its parent reaps it.
fn alive(pid: u32) -> bool {
    stat(pid).is_some_and(|(s, _, _)| !matches!(s, 'Z' | 'X' | 'x'))
}

/// `roots` and all their descendants, each with its process group.
fn with_descendants(roots: &[u32]) -> Vec<(u32, i32)> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut pgids: HashMap<u32, i32> = HashMap::new();
    for pid in all_pids() {
        if let Some((_, ppid, pgid)) = stat(pid) {
            children.entry(ppid).or_default().push(pid);
            pgids.insert(pid, pgid);
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut todo: Vec<u32> = roots.to_vec();
    while let Some(pid) = todo.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(&pgid) = pgids.get(&pid) {
            out.push((pid, pgid));
        }
        if let Some(c) = children.get(&pid) {
            todo.extend(c);
        }
    }
    out
}

/// Wait until no process of `tree` is alive. False after `limit`.
fn wait_gone(tree: &[(u32, i32)], limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if tree.iter().all(|&(pid, _)| !alive(pid)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
