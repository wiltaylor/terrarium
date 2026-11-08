//! A kill of a run ends its whole process tree on the target (TER-99).
//!
//! Killing the local client of a run does not end the command on the
//! target: the `docker exec` CLI and the `ssh` client die, and the process
//! in the container or on the remote host runs on with its children. So a
//! run on a `host`, `docker`, or `ssh` target starts through a small shell
//! wrapper ([`WRAP`]) that writes its PID to a file on the target and then
//! `exec`s the command. The command keeps that PID. `docker exec` (runc),
//! `sshd`, and the host agent each start it as the leader of its own
//! process group, and for docker and ssh of its own session.
//!
//! A kill runs a second command on the same target ([`KILL`]). It sends
//! `TERM` to each process of that group or session, waits up to
//! [`GRACE_SECS`], sends `KILL` to the ones left, and prints the PIDs. Only
//! then does the client of the run get its own kill. The result is a
//! [`report`] that `DELETE /api/waits/{id}?kill=true` returns as `kill`.
//! A run that hits its own timeout (the client's, which leaves the command
//! on a docker or ssh target) gets the same kill before its end goes out
//! (TER-115).
//!
//! The steps of a job of the job queue use one PID file per job
//! ([`job_pidfile`]), not a new one per run. A daemon that restarts
//! while a job runs has lost the run, but it can still find that file
//! and run the same kill on it ([`kill_file`], TER-123).
//!
//! A process that leaves both the group and the session (`setsid` in the
//! command) is not found. A kill in the moment before the wrapper wrote its
//! file finds no file and falls back to the client kill.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use terrarium_core::config::{TargetDef, TargetKind};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::api::Shared;
use crate::error::{ApiError, ApiResult, Code};
use crate::executor::{Control, End, Ev, RemoteCmd, Running, start_cmd};

/// The time between `TERM` and `KILL`.
pub const GRACE_SECS: u64 = 5;

/// The directory of the PID files on the target.
const PID_DIR: &str = "/tmp";

/// `sh -c WRAP terrarium-run <pidfile> <cmd> <args>...`: write the PID
/// and its start time (field 22 of `/proc/<pid>/stat`, empty without
/// `/proc`), then become the command. The file goes in through a rename,
/// so the clean-up of stale files in the same step never sees a
/// half-written one. The clean-up removes the files of the runs that
/// ended, and the ones whose PID now names a process with another start
/// time (TER-123).
pub const WRAP: &str = r#"f=$1; shift
st() { s=; { read -r s < "/proc/$1/stat"; } 2>/dev/null || return 0; s=${s##*) }; set -- $s; echo "${20}"; }
for o in /tmp/terrarium-run-*.pid; do
  [ -f "$o" ] || continue
  q= t=
  read -r q t < "$o" 2>/dev/null
  case $q in ''|*[!0-9]*) continue ;; esac
  if ! kill -0 "$q" 2>/dev/null || { [ -n "$t" ] && [ "$(st "$q")" != "$t" ]; }; then
    rm -f "$o" 2>/dev/null
  fi
done
{ echo "$$ $(st $$)" > "$f.tmp" && mv -f "$f.tmp" "$f"; } 2>/dev/null
exec "$@""#;

/// `sh -c KILL terrarium-kill <pidfile> <grace>`: end the group and the
/// session of the PID in the file. Prints `pid`, `term`, `kill`, and
/// `left` lines, each with the PIDs after the word. Without `/proc` it
/// signals the process group only.
///
/// A file whose PID now names a process with another start time is
/// stale: the PID was reused, so KILL removes the file, prints `stale`
/// and the PID, and signals nothing (TER-123). When the PID names no
/// process, the members of its group and session (if any) are still the
/// run's: Linux does not reuse a PID while it is a group or session ID.
pub const KILL: &str = r#"f=$1; g=${2:-5}; p= t=
[ -f "$f" ] && read -r p t < "$f"
case $p in ''|*[!0-9]*) echo none; exit 0 ;; esac
rm -f "$f"
if [ -n "$t" ]; then
  s=
  { read -r s < "/proc/$p/stat"; } 2>/dev/null
  if [ -n "$s" ]; then
    s=${s##*) }
    set -- $s
    if [ "${20}" != "$t" ]; then echo stale "$p"; exit 0; fi
  fi
fi
members() {
  if [ -d /proc/self ]; then
    for d in /proc/[0-9]*; do
      s=
      { read -r s < "$d/stat"; } 2>/dev/null || continue
      s=${s##*) }
      set -- $s
      [ "$1" = Z ] && continue
      if [ "$3" = "$p" ] || [ "$4" = "$p" ] || [ "${d#/proc/}" = "$p" ]; then
        echo "${d#/proc/}"
      fi
    done
  elif kill -0 "$p" 2>/dev/null; then
    echo "$p"
  fi
}
sig() {
  for x in $1; do kill -"$2" "$x" 2>/dev/null; done
  kill -"$2" -"$p" 2>/dev/null
}
all=$(members)
sig "$all" TERM
i=0; left=$(members)
while [ -n "$left" ] && [ "$i" -lt "$g" ]; do
  sleep 1; i=$((i+1)); left=$(members)
done
if [ -n "$left" ]; then sig "$left" KILL; sleep 1; fi
rest=$(members)
echo pid "$p"
echo term $all
echo kill $left
echo left $rest
"#;

/// `sh -c CLEAR terrarium-clear <pidfile>`: remove the PID file of a run
/// that ended by itself, so no later kill finds it (TER-123).
pub const CLEAR: &str = r#"rm -f "$1""#;

/// How long a kill from a PID file alone ([`kill_file`]) tries to start
/// while its host agent is offline or busy: after a daemon restart the
/// host agents connect again only after a while (TER-123).
pub const HOST_WAIT_SECS: u64 = 180;

/// The time between two tries to start a kill while its host agent is
/// offline or busy.
const START_RETRY: Duration = Duration::from_secs(1);

/// The report of a kill, once it is there. `Some(Null)` when the run ended
/// without a kill.
pub type Report = watch::Receiver<Option<Value>>;

/// Does a run of `cmd` on `def` go through the wrapper? An interactive
/// login shell (no argv) and a vmlab target do not.
pub fn wraps(def: &TargetDef, cmd: &RemoteCmd) -> bool {
    kind_wraps(def) && !cmd.argv.is_empty()
}

/// Do runs with an argv on `def` go through the wrapper?
pub fn kind_wraps(def: &TargetDef) -> bool {
    matches!(
        def.kind,
        TargetKind::Host | TargetKind::Docker | TargetKind::Ssh
    )
}

/// `cmd` behind the wrapper with this PID file.
pub fn wrap(cmd: &RemoteCmd, pidfile: &str) -> RemoteCmd {
    let mut argv: Vec<String> = ["sh", "-c", WRAP, "terrarium-run", pidfile]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    argv.extend(cmd.argv.iter().cloned());
    RemoteCmd {
        argv,
        ..cmd.clone()
    }
}

/// The kill command for the run of `cmd`: the same target instance and
/// budget, no env, no cwd.
pub fn kill_cmd(cmd: &RemoteCmd, pidfile: &str, grace: u64) -> RemoteCmd {
    RemoteCmd {
        argv: vec![
            "sh".into(),
            "-c".into(),
            KILL.into(),
            "terrarium-kill".into(),
            pidfile.into(),
            grace.to_string(),
        ],
        timeout_secs: Some(grace + 30),
        agent: cmd.agent.clone(),
        holder: cmd.holder.clone(),
        ..RemoteCmd::default()
    }
}

/// The command that removes the PID file of the run of `cmd` after it
/// ended by itself ([`CLEAR`]).
pub fn clear_cmd(cmd: &RemoteCmd, pidfile: &str) -> RemoteCmd {
    RemoteCmd {
        argv: vec![
            "sh".into(),
            "-c".into(),
            CLEAR.into(),
            "terrarium-clear".into(),
            pidfile.into(),
        ],
        timeout_secs: Some(15),
        agent: cmd.agent.clone(),
        holder: cmd.holder.clone(),
        ..RemoteCmd::default()
    }
}

/// A new PID file path on the target.
pub fn new_pidfile() -> String {
    format!("{PID_DIR}/terrarium-run-{}.pid", ulid::Ulid::new())
}

/// The PID file of the steps of job `job` on its target (TER-123). The
/// steps of a job run one at a time, so one file serves them all, and a
/// daemon that restarts finds the file of a job that was running. Only
/// the letters and digits of the ID go into the path.
pub fn job_pidfile(job: &str) -> String {
    let id: String = job.chars().filter(char::is_ascii_alphanumeric).collect();
    format!("{PID_DIR}/terrarium-run-job-{id}.pid")
}

/// Parse the output of [`KILL`].
pub fn parse(stdout: &str) -> Value {
    let pids = |rest: &str| -> Vec<u32> {
        rest.split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    let mut r = json!({"pid": null, "term": [], "kill": [], "left": []});
    for line in stdout.lines() {
        let (key, rest) = line.split_once(' ').unwrap_or((line.trim(), ""));
        match key {
            "none" => r["pid"] = Value::Null,
            "stale" => {
                r["pid"] = Value::Null;
                r["stale"] = json!(pids(rest).first());
            }
            "pid" => r["pid"] = json!(pids(rest).first()),
            "term" | "kill" | "left" => r[key] = json!(pids(rest)),
            _ => {}
        }
    }
    r
}

/// Start `cmd` on `def`. A wrapped run (see [`wraps`]) comes with a
/// [`Report`] of its kill.
pub fn start(
    state: &Shared,
    def: &TargetDef,
    cmd: &RemoteCmd,
) -> ApiResult<(Running, Option<Report>)> {
    start_named(state, def, cmd, new_pidfile())
}

/// [`start`] with this PID file on the target (a job step:
/// [`job_pidfile`]). A run that ends by itself removes the file before
/// its end goes out, so a later kill from the file ([`kill_file`]) finds
/// nothing (TER-123).
pub fn start_named(
    state: &Shared,
    def: &TargetDef,
    cmd: &RemoteCmd,
    pidfile: String,
) -> ApiResult<(Running, Option<Report>)> {
    if !wraps(def, cmd) {
        return Ok((start_cmd(state, def, cmd)?, None));
    }
    let weak = Arc::downgrade(state);
    let d = def.clone();
    let starter = move |c: RemoteCmd| {
        let state = weak
            .upgrade()
            .ok_or_else(|| ApiError::internal("the daemon is shutting down"))?;
        start_cmd(&state, &d, &c)
    };
    start_with_file(starter, cmd, GRACE_SECS, pidfile, true)
}

/// [`start`] with any way to start a command on the target.
pub fn start_with<F>(start: F, cmd: &RemoteCmd, grace: u64) -> ApiResult<(Running, Option<Report>)>
where
    F: Fn(RemoteCmd) -> ApiResult<Running> + Send + Sync + 'static,
{
    start_with_file(start, cmd, grace, new_pidfile(), false)
}

/// [`start_with`] with this PID file on the target. With `clear`, a run
/// that ends by itself removes the file ([`clear_cmd`]) before its end
/// goes out.
pub fn start_with_file<F>(
    start: F,
    cmd: &RemoteCmd,
    grace: u64,
    pidfile: String,
    clear: bool,
) -> ApiResult<(Running, Option<Report>)>
where
    F: Fn(RemoteCmd) -> ApiResult<Running> + Send + Sync + 'static,
{
    let inner = start(wrap(cmd, &pidfile))?;
    let (tx, rx) = watch::channel(None);
    let killer = kill_cmd(cmd, &pidfile, grace);
    let clear = clear.then(|| clear_cmd(cmd, &pidfile));
    Ok((guard(inner, start, killer, clear, tx), Some(rx)))
}

/// How the forward of a guarded run stopped.
enum Stop {
    /// The run ended by itself, with its end event when there was one
    /// (it goes out after the clear).
    Ended(Option<Ev>),
    /// A `Kill`, or nobody reads the run any more.
    Killed,
    /// The run hit its own timeout: the client is gone, the command on
    /// the target may not be. The held end goes out after the kill.
    TimedOut(Ev),
}

/// Is `ev` the end of a run that hit its timeout?
fn is_timeout(ev: &Ev) -> bool {
    matches!(ev, Ev::End(End::Exit { reason, .. }) if reason == "timeout")
}

/// Did the run end by itself? Not when no end came, or an error end with
/// no exit code (the host agent no longer holds the session, the host is
/// offline), or the host agent ended it for a down daemon link: its
/// processes may still run. Such a run keeps its PID file, so a later
/// kill from the file finds them (TER-196, as after a daemon restart).
fn ended_by_itself(end: Option<&Ev>) -> bool {
    match end {
        Some(Ev::End(End::Exit { reason, .. })) => {
            reason != "detached_timeout" && reason != "output_overflow"
        }
        Some(Ev::End(End::Cp { .. })) => true,
        _ => false,
    }
}

/// Forward `inner`. A `Kill` (or the drop of the returned run) runs
/// `killer` on the target first, then kills `inner`. A run that hits its
/// own timeout runs `killer` too, and its end goes out only after the
/// kill, with the report ready (TER-115). A run that ends by itself
/// ([`ended_by_itself`]) runs `clear` (when given) before its end goes
/// out (TER-123); a lost run keeps its PID file (TER-196).
fn guard<F>(
    inner: Running,
    start: F,
    killer: RemoteCmd,
    clear: Option<RemoteCmd>,
    report: watch::Sender<Option<Value>>,
) -> Running
where
    F: Fn(RemoteCmd) -> ApiResult<Running> + Send + Sync + 'static,
{
    let (etx, erx) = mpsc::channel(16);
    let (ctx, mut crx) = mpsc::unbounded_channel::<Control>();
    tokio::spawn(async move {
        let Running { mut events, ctl } = inner;
        let stop = loop {
            tokio::select! {
                ev = events.recv() => {
                    let Some(ev) = ev else { break Stop::Ended(None) };
                    if is_timeout(&ev) {
                        break Stop::TimedOut(ev);
                    }
                    if matches!(ev, Ev::End(_)) {
                        break Stop::Ended(Some(ev));
                    }
                    if etx.send(ev).await.is_err() {
                        // Nobody reads the run any more: end it.
                        break Stop::Killed;
                    }
                }
                c = crx.recv() => match c {
                    None | Some(Control::Kill) => break Stop::Killed,
                    Some(c) => { let _ = ctl.send(c); }
                },
            }
        };
        let held = match stop {
            Stop::Ended(end) => {
                if let Some(c) = clear.as_ref().filter(|_| ended_by_itself(end.as_ref())) {
                    let ok = match start(c.clone()) {
                        Ok(r) => {
                            let wait = Duration::from_secs(c.timeout_secs.unwrap_or(15) + 5);
                            tokio::time::timeout(wait, r.collect(4096))
                                .await
                                .ok()
                                .and_then(|c| c.end)
                                .is_some_and(|e| e.success())
                        }
                        Err(_) => false,
                    };
                    if !ok {
                        debug!("cannot remove the PID file of a run that ended");
                    }
                }
                report.send_replace(Some(Value::Null));
                if let Some(ev) = end {
                    let _ = etx.send(ev).await;
                }
                return;
            }
            Stop::Killed => {
                drop(etx);
                None
            }
            Stop::TimedOut(ev) => Some((etx, ev)),
        };
        // Read the run's output meanwhile: a host session that nobody
        // reads is released, and that kills it at once, without TERM.
        let run_kill = run_killer(&start, &killer, Duration::ZERO);
        tokio::pin!(run_kill);
        let mut open = true;
        let r = loop {
            tokio::select! {
                r = &mut run_kill => break r,
                ev = events.recv(), if open => if ev.is_none() { open = false },
            }
        };
        let _ = ctl.send(Control::Kill);
        if r["left"].as_array().is_some_and(|l| !l.is_empty()) {
            warn!(report = %r, "a killed run left processes on the target");
        } else if r.get("error").is_some() {
            warn!(report = %r, "the kill of a run on its target failed");
        } else {
            debug!(report = %r, "killed a run on its target");
        }
        report.send_replace(Some(r));
        if let Some((etx, ev)) = held {
            let _ = etx.send(ev).await;
        }
    });
    Running {
        events: erx,
        ctl: ctx,
    }
}

/// Can a start that failed with `e` work later? A host agent that is
/// offline (a conflict) or has no free session (rate limited).
fn may_start_later(e: &ApiError) -> bool {
    matches!(e.code, Code::Conflict | Code::RateLimited)
}

/// Run `killer` (a [`kill_cmd`]) through `start` and read its report.
/// A start that fails as offline or busy is tried again each second for
/// `host_wait`. An error report when it cannot start, fails, or times
/// out.
async fn run_killer<F>(start: &F, killer: &RemoteCmd, host_wait: Duration) -> Value
where
    F: Fn(RemoteCmd) -> ApiResult<Running>,
{
    let until = tokio::time::Instant::now() + host_wait;
    let started = loop {
        match start(killer.clone()) {
            Err(e) if may_start_later(&e) && tokio::time::Instant::now() < until => {
                debug!("the kill command cannot start yet: {}", e.message);
                tokio::time::sleep(START_RETRY.min(until - tokio::time::Instant::now())).await;
            }
            r => break r,
        }
    };
    match started {
        Ok(r) => {
            let wait = Duration::from_secs(killer.timeout_secs.unwrap_or(60) + 5);
            match tokio::time::timeout(wait, r.collect(64 * 1024)).await {
                Ok(c) => {
                    let out = String::from_utf8_lossy(&c.stdout).into_owned();
                    if c.end.as_ref().is_some_and(crate::executor::End::success) {
                        parse(&out)
                    } else {
                        json!({"error": format!("the kill command failed: {:?} {}",
                            c.end, String::from_utf8_lossy(&c.stderr).trim())})
                    }
                }
                Err(_) => json!({"error": "the kill command timed out"}),
            }
        }
        Err(e) => json!({"error": format!("cannot start the kill command: {}", e.message)}),
    }
}

/// End what still runs of an earlier run on `def` from its PID file,
/// without the run itself: the run of a daemon that restarted (TER-123).
/// `holder` as in the run. A host agent that is offline (it has not
/// connected to this daemon yet) or busy gets [`HOST_WAIT_SECS`]. The
/// report as from a kill; its `pid` is null when the file is not there,
/// and `stale` holds the PID of a file whose PID was reused.
pub async fn kill_file(
    state: &Shared,
    def: &TargetDef,
    holder: Option<String>,
    pidfile: &str,
) -> Value {
    let (state, d) = (state.clone(), def.clone());
    kill_file_with(
        move |c: RemoteCmd| start_cmd(&state, &d, &c),
        holder,
        pidfile,
        GRACE_SECS,
        Duration::from_secs(HOST_WAIT_SECS),
    )
    .await
}

/// [`kill_file`] with any way to start a command on the target, a grace,
/// and a wait for an offline or busy host agent.
pub async fn kill_file_with<F>(
    start: F,
    holder: Option<String>,
    pidfile: &str,
    grace: u64,
    host_wait: Duration,
) -> Value
where
    F: Fn(RemoteCmd) -> ApiResult<Running>,
{
    let base = RemoteCmd {
        holder,
        ..RemoteCmd::default()
    };
    run_killer(&start, &kill_cmd(&base, pidfile, grace), host_wait).await
}

/// Wait for the report of a kill, at most until the kill command times
/// out. `None` when the run ended without a kill or no report came.
pub async fn report(mut rx: Report) -> Option<Value> {
    let wait = Duration::from_secs(GRACE_SECS + 40);
    let got = tokio::time::timeout(wait, rx.wait_for(Option::is_some))
        .await
        .ok()?
        .ok()?
        .clone();
    got.filter(|v| !v.is_null())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::executor::{pump_process, ssh_plan};

    /// TER-196: only a run that ended by itself clears its PID file. A
    /// lost run keeps it for the kill of a lost run.
    #[test]
    fn a_lost_run_keeps_its_pid_file() {
        let exit = |code: Option<i32>, reason: &str| {
            Ev::End(End::Exit {
                code,
                signal: None,
                reason: reason.into(),
            })
        };
        for ev in [
            exit(Some(0), "exited"),
            exit(Some(1), "exited"),
            exit(None, "signaled"),
        ] {
            assert!(ended_by_itself(Some(&ev)));
        }
        let lost = Ev::End(End::Error {
            code: "lost".into(),
            message: "the host agent no longer holds the session".into(),
        });
        for ev in [
            lost,
            exit(None, "detached_timeout"),
            exit(None, "output_overflow"),
        ] {
            assert!(!ended_by_itself(Some(&ev)));
        }
        assert!(!ended_by_itself(None));
    }

    #[test]
    fn the_wrapper_keeps_the_command_and_names_the_file() {
        let cmd = RemoteCmd {
            argv: vec!["cargo".into(), "test".into()],
            cwd: Some("/src".into()),
            agent: Some("a".into()),
            holder: Some("p/a".into()),
            ..RemoteCmd::default()
        };
        let w = wrap(&cmd, "/tmp/terrarium-run-X.pid");
        assert_eq!(w.argv[..2], ["sh", "-c"]);
        assert_eq!(
            w.argv[3..],
            ["terrarium-run", "/tmp/terrarium-run-X.pid", "cargo", "test"]
        );
        assert_eq!(w.cwd.as_deref(), Some("/src"));
        let k = kill_cmd(&cmd, "/tmp/terrarium-run-X.pid", 5);
        assert_eq!(
            k.argv[3..],
            ["terrarium-kill", "/tmp/terrarium-run-X.pid", "5"]
        );
        assert_eq!(k.cwd, None);
        assert_eq!(k.agent.as_deref(), Some("a"));
        assert_eq!(k.holder.as_deref(), Some("p/a"));
        assert!(new_pidfile().starts_with("/tmp/terrarium-run-"));
    }

    #[test]
    fn the_report_parses() {
        let r = parse("pid 10\nterm 10 11 12\nkill 12\nleft\n");
        assert_eq!(
            r,
            json!({"pid": 10, "term": [10, 11, 12], "kill": [12], "left": []})
        );
        assert_eq!(parse("none\n")["pid"], Value::Null);
    }

    #[test]
    fn only_runs_with_an_argv_on_host_docker_and_ssh_wrap() {
        let def =
            |kind: &str| -> TargetDef { toml::from_str(&format!("kind = \"{kind}\"\n")).unwrap() };
        let cmd = RemoteCmd {
            argv: vec!["true".into()],
            ..RemoteCmd::default()
        };
        for k in ["host", "docker", "ssh"] {
            assert!(wraps(&def(k), &cmd), "{k}");
        }
        assert!(!wraps(&def("vmlab"), &cmd));
        let shell = RemoteCmd {
            shell: true,
            ..RemoteCmd::default()
        };
        assert!(!wraps(&def("ssh"), &shell));
    }

    fn alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => !s.rsplit(") ").next().unwrap_or("").starts_with('Z'),
            Err(_) => false,
        }
    }

    async fn read_pid(file: &Path) -> u32 {
        for _ in 0..500 {
            if let Some(p) = std::fs::read_to_string(file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no pid in {}", file.display());
    }

    /// An ssh target: a fake `ssh` that runs the remote command in a new
    /// session, as sshd does. Killing the client leaves that session; the
    /// kill command on the target ends it, children too.
    #[tokio::test]
    async fn a_kill_over_ssh_ends_the_children_on_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor a; do last=$a; done\n( exec setsid sh -c \"$last\" ) &\nwait $!\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let known = dir.path().join("known_hosts");
        let mut def: TargetDef =
            toml::from_str("kind = \"ssh\"\naddress = \"build@box\"\n").unwrap();
        def.name = "remote".into();
        let ssh_bin = ssh.display().to_string();
        let starter = move |c: RemoteCmd| {
            let plan = ssh_plan(&ssh_bin, &def, None, &known, &c)?;
            let mut p = tokio::process::Command::new(&plan.program);
            p.args(&plan.args);
            Ok(pump_process(p, plan.stdin_prefix, None, None))
        };
        let child_file = dir.path().join("child");
        let cmd = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "trap '' TERM; sleep 300 & echo $! > \"$1\"; wait".into(),
                "run".into(),
                child_file.display().to_string(),
            ],
            ..RemoteCmd::default()
        };
        let (run, report) = start_with(starter, &cmd, 1).unwrap();
        let child = read_pid(&child_file).await;
        assert!(alive(child));
        let _ = run.ctl.send(Control::Kill);
        let r = super::report(report.unwrap()).await.expect("a kill report");
        assert!(r["term"].as_array().unwrap().contains(&json!(child)), "{r}");
        assert_eq!(r["left"], json!([]), "{r}");
        // The shell ignored TERM: it needed the KILL after the grace.
        assert!(!r["kill"].as_array().unwrap().is_empty(), "{r}");
        assert!(!alive(child), "the child {child} still runs: {r}");
        assert!(!alive(r["pid"].as_u64().unwrap() as u32), "{r}");
    }

    /// TER-115: a run over ssh that hits its own timeout, with no `Kill`:
    /// the client dies, the session on the target runs on, and the guard
    /// runs the kill command before the end goes out.
    #[tokio::test]
    async fn a_timeout_over_ssh_ends_the_children_on_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor a; do last=$a; done\n( exec setsid sh -c \"$last\" ) &\nwait $!\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let known = dir.path().join("known_hosts");
        let mut def: TargetDef =
            toml::from_str("kind = \"ssh\"\naddress = \"build@box\"\n").unwrap();
        def.name = "remote".into();
        let ssh_bin = ssh.display().to_string();
        let starter = move |c: RemoteCmd| {
            let plan = ssh_plan(&ssh_bin, &def, None, &known, &c)?;
            let mut p = tokio::process::Command::new(&plan.program);
            p.args(&plan.args);
            let t = c.timeout_secs.map(Duration::from_secs);
            Ok(pump_process(p, plan.stdin_prefix, t, None))
        };
        let child_file = dir.path().join("child");
        let cmd = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "trap '' TERM; sleep 300 & echo $! > \"$1\"; wait".into(),
                "run".into(),
                child_file.display().to_string(),
            ],
            timeout_secs: Some(2),
            ..RemoteCmd::default()
        };
        let (run, report) = start_with(starter, &cmd, 1).unwrap();
        let child = read_pid(&child_file).await;
        assert!(alive(child));
        let c = run.collect(1024).await;
        assert!(
            matches!(&c.end, Some(End::Exit { reason, .. }) if reason == "timeout"),
            "{:?}",
            c.end
        );
        // The end came after the kill: the report is there, nothing is left.
        let report = report.unwrap();
        assert!(report.borrow().is_some(), "the end came before the kill");
        let r = super::report(report).await.expect("a kill report");
        assert!(r["term"].as_array().unwrap().contains(&json!(child)), "{r}");
        assert_eq!(r["left"], json!([]), "{r}");
        assert!(!alive(child), "the child {child} still runs: {r}");
    }

    /// TER-123: a job step over ssh whose daemon is gone (its run is
    /// forgotten: no kill, no timeout, nobody reads it). A new daemon
    /// knows only the job ID: the kill on the job's PID file ends the
    /// session on the target, children too. A second kill finds no file.
    #[tokio::test]
    async fn a_lost_job_run_is_killed_from_its_job_pidfile() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor a; do last=$a; done\n( exec setsid sh -c \"$last\" ) &\nwait $!\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let known = dir.path().join("known_hosts");
        let mut def: TargetDef =
            toml::from_str("kind = \"ssh\"\naddress = \"build@box\"\n").unwrap();
        def.name = "remote".into();
        assert!(kind_wraps(&def));
        let ssh_bin = ssh.display().to_string();
        let starter = move |c: RemoteCmd| {
            let plan = ssh_plan(&ssh_bin, &def, None, &known, &c)?;
            let mut p = tokio::process::Command::new(&plan.program);
            p.args(&plan.args);
            Ok(pump_process(p, plan.stdin_prefix, None, None))
        };
        let child_file = dir.path().join("child");
        let cmd = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "trap '' TERM; sleep 300 & echo $! > \"$1\"; wait".into(),
                "run".into(),
                child_file.display().to_string(),
            ],
            holder: Some("p/jobs".into()),
            ..RemoteCmd::default()
        };
        let job = ulid::Ulid::new().to_string();
        let pidfile = job_pidfile(&job);
        assert_eq!(pidfile, format!("/tmp/terrarium-run-job-{job}.pid"));
        assert_eq!(job_pidfile("a/../b c"), "/tmp/terrarium-run-job-abc.pid");
        let (run, report) =
            start_with_file(starter.clone(), &cmd, 1, pidfile.clone(), true).unwrap();
        let child = read_pid(&child_file).await;
        assert!(alive(child));
        // The daemon is gone: its run and the report are never used again.
        std::mem::forget(run);
        std::mem::forget(report);
        let r = kill_file_with(
            starter.clone(),
            Some("p/jobs".into()),
            &job_pidfile(&job),
            1,
            Duration::ZERO,
        )
        .await;
        assert!(r.get("error").is_none(), "{r}");
        assert!(r["term"].as_array().unwrap().contains(&json!(child)), "{r}");
        assert_eq!(r["left"], json!([]), "{r}");
        assert!(!alive(child), "the child {child} still runs: {r}");
        assert!(!Path::new(&pidfile).exists(), "{r}");
        let again = kill_file_with(starter, None, &pidfile, 1, Duration::ZERO).await;
        assert_eq!(again["pid"], Value::Null, "{again}");
    }

    /// A run that ends by itself reports no kill.
    #[tokio::test]
    async fn a_run_that_ends_reports_no_kill() {
        let starter = |c: RemoteCmd| {
            let mut p = tokio::process::Command::new(&c.argv[0]);
            p.args(&c.argv[1..]);
            Ok(pump_process(p, None, None, None))
        };
        let cmd = RemoteCmd {
            argv: vec!["true".into()],
            ..RemoteCmd::default()
        };
        let (run, report) = start_with(starter, &cmd, 1).unwrap();
        let c = run.collect(1024).await;
        assert!(c.end.unwrap().success());
        assert_eq!(super::report(report.unwrap()).await, None);
    }

    /// A starter through a fake `ssh` that runs the remote command in a
    /// new session, as sshd does.
    fn fake_ssh(
        dir: &Path,
    ) -> impl Fn(RemoteCmd) -> ApiResult<Running> + Clone + Send + Sync + 'static {
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor a; do last=$a; done\n( exec setsid sh -c \"$last\" ) &\nwait $!\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let known = dir.join("known_hosts");
        let mut def: TargetDef =
            toml::from_str("kind = \"ssh\"\naddress = \"build@box\"\n").unwrap();
        def.name = "remote".into();
        let ssh_bin = ssh.display().to_string();
        move |c: RemoteCmd| {
            let plan = ssh_plan(&ssh_bin, &def, None, &known, &c)?;
            let mut p = tokio::process::Command::new(&plan.program);
            p.args(&plan.args);
            Ok(pump_process(p, plan.stdin_prefix, None, None))
        }
    }

    /// The start time of `pid` (field 22 of its stat).
    fn start_time(pid: u32) -> String {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let rest = s.rsplit(") ").next().unwrap();
        rest.split_whitespace().nth(19).unwrap().to_owned()
    }

    /// TER-123 review, blocker 1: after a daemon restart the host agent
    /// of a `host` or `docker` target is offline until it connects to the
    /// new daemon. The kill from the PID file tries again until it
    /// connects, then ends the lost run. Without a connect in its wait it
    /// reports the error.
    #[tokio::test]
    async fn a_lost_job_kill_waits_for_its_host_agent() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(dir.path());
        let child_file = dir.path().join("child");
        let cmd = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "sleep 300 & echo $! > \"$1\"; wait".into(),
                "run".into(),
                child_file.display().to_string(),
            ],
            ..RemoteCmd::default()
        };
        let pidfile = job_pidfile(&ulid::Ulid::new().to_string());
        let (run, report) = start_with_file(ssh.clone(), &cmd, 1, pidfile.clone(), true).unwrap();
        let child = read_pid(&child_file).await;
        std::mem::forget(run);
        std::mem::forget(report);
        // The host agent connects after the third try.
        let tries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let t = tries.clone();
        let late = move |c: RemoteCmd| {
            if t.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
                return Err(crate::hosts::HostError::Offline("lab1".into()).into());
            }
            ssh(c)
        };
        let r = kill_file_with(late, None, &pidfile, 1, Duration::from_secs(30)).await;
        assert!(r.get("error").is_none(), "{r}");
        assert!(tries.load(std::sync::atomic::Ordering::SeqCst) >= 4, "{r}");
        assert!(r["term"].as_array().unwrap().contains(&json!(child)), "{r}");
        assert!(!alive(child), "the child {child} still runs: {r}");
        // A host agent that never comes: an error after the wait.
        let never = |_: RemoteCmd| -> ApiResult<Running> {
            Err(crate::hosts::HostError::Offline("lab1".into()).into())
        };
        let r = kill_file_with(never, None, &pidfile, 1, Duration::from_secs(2)).await;
        assert!(r["error"].as_str().unwrap().contains("offline"), "{r}");
    }

    /// TER-123 review, major 2: a job step that ends by itself removes its
    /// PID file before its end goes out. The file holds the PID and its
    /// start time, and a kill from a file whose PID now names another
    /// process (a reused PID: another start time) signals nothing.
    #[tokio::test]
    async fn a_stale_job_pidfile_kills_no_other_process() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(dir.path());
        let pidfile = job_pidfile(&ulid::Ulid::new().to_string());
        let copy = dir.path().join("copy");
        let cmd = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "cp \"$1\" \"$2\"".into(),
                "run".into(),
                pidfile.clone(),
                copy.display().to_string(),
            ],
            ..RemoteCmd::default()
        };
        let (run, _report) = start_with_file(ssh.clone(), &cmd, 1, pidfile.clone(), true).unwrap();
        let c = run.collect(1024).await;
        assert!(c.end.unwrap().success());
        let written = std::fs::read_to_string(&copy).unwrap();
        let fields: Vec<&str> = written.split_whitespace().collect();
        assert_eq!(fields.len(), 2, "no start time in {written:?}");
        assert!(
            fields.iter().all(|f| f.parse::<u64>().is_ok()),
            "{written:?}"
        );
        assert!(!Path::new(&pidfile).exists(), "the step left its PID file");

        // An unrelated process group: its leader got the PID of the file.
        use std::os::unix::process::CommandExt;
        let mut other = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = other.id();
        let real = start_time(pid);
        let wrong = (real.parse::<u64>().unwrap() + 1).to_string();
        // Not under /tmp/terrarium-run-*.pid: the WRAP clean-up of a run of
        // a test beside this one would remove a stale file there first.
        let pidfile = dir.path().join("stale.pid").display().to_string();
        std::fs::write(&pidfile, format!("{pid} {wrong}\n")).unwrap();
        let r = kill_file_with(ssh.clone(), None, &pidfile, 1, Duration::ZERO).await;
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(r["stale"], json!(pid), "{r}");
        assert_eq!(r["pid"], Value::Null, "{r}");
        assert!(alive(pid), "a stale file killed {pid}: {r}");
        assert!(!Path::new(&pidfile).exists(), "{r}");
        // With its own start time the same file is the run's: killed.
        std::fs::write(&pidfile, format!("{pid} {real}\n")).unwrap();
        let r = kill_file_with(ssh, None, &pidfile, 1, Duration::ZERO).await;
        assert_eq!(r["pid"], json!(pid), "{r}");
        assert!(r["term"].as_array().unwrap().contains(&json!(pid)), "{r}");
        let _ = other.kill();
        let _ = other.wait();
    }
}
