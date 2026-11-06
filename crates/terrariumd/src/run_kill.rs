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
use crate::error::{ApiError, ApiResult};
use crate::executor::{Control, Ev, RemoteCmd, Running, start_cmd};

/// The time between `TERM` and `KILL`.
pub const GRACE_SECS: u64 = 5;

/// The directory of the PID files on the target.
const PID_DIR: &str = "/tmp";

/// `sh -c WRAP terrarium-run <pidfile> <cmd> <args>...`: write the PID,
/// then become the command. The file goes in through a rename, so the
/// clean-up of stale files in the same step never sees a half-written
/// one. The clean-up removes the files of the runs that ended.
pub const WRAP: &str = r#"f=$1; shift
for o in /tmp/terrarium-run-*.pid; do
  [ -f "$o" ] || continue
  read -r q < "$o" 2>/dev/null
  case $q in ''|*[!0-9]*) continue ;; esac
  kill -0 "$q" 2>/dev/null || rm -f "$o" 2>/dev/null
done
{ echo $$ > "$f.tmp" && mv -f "$f.tmp" "$f"; } 2>/dev/null
exec "$@""#;

/// `sh -c KILL terrarium-kill <pidfile> <grace>`: end the group and the
/// session of the PID in the file. Prints `pid`, `term`, `kill`, and
/// `left` lines, each with the PIDs after the word. Without `/proc` it
/// signals the process group only.
pub const KILL: &str = r#"f=$1; g=${2:-5}; p=
[ -f "$f" ] && read -r p < "$f"
case $p in ''|*[!0-9]*) echo none; exit 0 ;; esac
rm -f "$f"
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

/// The report of a kill, once it is there. `Some(Null)` when the run ended
/// without a kill.
pub type Report = watch::Receiver<Option<Value>>;

/// Does a run of `cmd` on `def` go through the wrapper? An interactive
/// login shell (no argv) and a vmlab target do not.
pub fn wraps(def: &TargetDef, cmd: &RemoteCmd) -> bool {
    matches!(
        def.kind,
        TargetKind::Host | TargetKind::Docker | TargetKind::Ssh
    ) && !cmd.argv.is_empty()
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

/// A new PID file path on the target.
pub fn new_pidfile() -> String {
    format!("{PID_DIR}/terrarium-run-{}.pid", ulid::Ulid::new())
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
    start_with(starter, cmd, GRACE_SECS)
}

/// [`start`] with any way to start a command on the target.
pub fn start_with<F>(start: F, cmd: &RemoteCmd, grace: u64) -> ApiResult<(Running, Option<Report>)>
where
    F: Fn(RemoteCmd) -> ApiResult<Running> + Send + Sync + 'static,
{
    let pidfile = new_pidfile();
    let inner = start(wrap(cmd, &pidfile))?;
    let (tx, rx) = watch::channel(None);
    let killer = kill_cmd(cmd, &pidfile, grace);
    Ok((guard(inner, start, killer, tx), Some(rx)))
}

/// Forward `inner`. A `Kill` (or the drop of the returned run) runs
/// `killer` on the target first, then kills `inner`.
fn guard<F>(
    inner: Running,
    start: F,
    killer: RemoteCmd,
    report: watch::Sender<Option<Value>>,
) -> Running
where
    F: Fn(RemoteCmd) -> ApiResult<Running> + Send + Sync + 'static,
{
    let (etx, erx) = mpsc::channel(16);
    let (ctx, mut crx) = mpsc::unbounded_channel::<Control>();
    tokio::spawn(async move {
        let Running { mut events, ctl } = inner;
        let killed = loop {
            tokio::select! {
                ev = events.recv() => {
                    let Some(ev) = ev else { break false };
                    let last = matches!(ev, Ev::End(_));
                    if etx.send(ev).await.is_err() {
                        // Nobody reads the run any more: end it.
                        break !last;
                    }
                    if last {
                        break false;
                    }
                }
                c = crx.recv() => match c {
                    None | Some(Control::Kill) => break true,
                    Some(c) => { let _ = ctl.send(c); }
                },
            }
        };
        drop(etx);
        if !killed {
            report.send_replace(Some(Value::Null));
            return;
        }
        // Read the run's output meanwhile: a host session that nobody
        // reads is released, and that kills it at once, without TERM.
        let run_kill = async {
            match start(killer.clone()) {
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
        };
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
    });
    Running {
        events: erx,
        ctl: ctx,
    }
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
}
