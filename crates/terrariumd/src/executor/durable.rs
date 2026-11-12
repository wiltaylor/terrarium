//! Restartable, noninteractive SSH commands. The encrypted journal binds a
//! caller's identity to one remote claim directory before any SSH is started.
use super::*;

const RETAIN: u64 = 86400;
const MAX_RUN: u64 = 7 * 86400;

/// A launch can be journaled just before the daemon dies, without acquiring
/// its exec owner. Drop only expired, unowned checkpoints, never a job's
/// expired binding (which must continue to prohibit relaunch).
pub(crate) fn spawn_cleanup(state: &Shared) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else { return };
            if let Err(error) = prune(&state) {
                warn!(%error, "cannot prune durable SSH checkpoints");
            }
            drop(state);
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    })
}

fn prune(state: &Shared) -> ApiResult<()> {
    #[derive(Deserialize)]
    struct Owner {
        #[serde(default)]
        remote_key: Option<String>,
    }
    #[derive(Deserialize)]
    struct Created {
        created: i64,
    }
    let j = journal(state);
    let mut owned: std::collections::HashSet<_> = j
        .scan::<Owner>("exec/")?
        .into_iter()
        .filter_map(|(_, owner)| owner.remote_key.map(|key| format!("ssh/{key}")))
        .collect();
    owned.extend(
        j.scan::<Value>("recipe/")?
            .into_iter()
            .map(|(key, _)| format!("ssh/{key}")),
    );
    let before = chrono::Utc::now().timestamp() - (MAX_RUN + RETAIN) as i64;
    let old: Vec<_> = j
        .scan::<Created>("ssh/")?
        .into_iter()
        .filter(|(key, value)| value.created < before && !owned.contains(key))
        .map(|(key, _)| key)
        .collect();
    j.replace(&[], &old, &[])
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    def: TargetDef,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    vm: Option<String>,
    #[serde(default)]
    host_keys: Vec<String>,
    #[serde(default)]
    host_trust: String,
    cmd: RemoteCmd,
    token: String,
    created: i64,
}

fn journal(state: &Shared) -> crate::hosts::resume::Journal {
    crate::hosts::resume::Journal::new(&state.state_dir)
}

fn host_trust(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let host = def
        .address
        .as_deref()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = match def.port {
        Some(port) => format!("[{host}]:{port}"),
        None => host.to_owned(),
    };
    let found = std::process::Command::new("ssh-keygen")
        .arg("-F")
        .arg(host)
        .arg("-f")
        .arg(known_hosts(&state.state_dir)?)
        .output()
        .map_err(|e| ApiError::internal(format!("cannot read SSH host identity: {e}")))?;
    Ok(String::from_utf8_lossy(&found.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn host_keys(state: &Shared, def: &TargetDef) -> ApiResult<Vec<String>> {
    let trust = host_trust(state, def)?;
    let mut keys = Vec::new();
    for line in trust.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 3 {
            let key = ssh_key::PublicKey::from_openssh(&format!("{} {}", fields[1], fields[2]))
                .map_err(|e| ApiError::internal(format!("invalid SSH host identity: {e}")))?;
            keys.push(key.fingerprint(ssh_key::HashAlg::Sha256).to_string());
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

pub(crate) fn exists(state: &Shared, key: &str) -> ApiResult<bool> {
    Ok(journal(state)
        .load::<Checkpoint>(&format!("ssh/{key}"))?
        .is_some())
}

pub(crate) fn resume(state: &Shared, key: &str) -> ApiResult<Running> {
    let saved = journal(state)
        .load::<Checkpoint>(&format!("ssh/{key}"))?
        .ok_or_else(|| ApiError::internal("missing durable SSH checkpoint"))?;
    let name = saved.def.name.clone();
    let current = state
        .db
        .read_now(move |c| crate::store::targets::get(c, &name))?;
    if saved.identity.as_deref() != Some(crate::job_logs::target_identity(&current).as_str()) {
        return Err(ApiError::conflict(
            "interrupted (target changed): target incarnation differs",
        ));
    }
    let name = current.def.name.clone();
    let vm = state
        .db
        .read_now(move |c| Ok(crate::exedev::record_of(c, &name)?.map(|vm| vm.name)))?;
    if saved.vm != vm {
        return Err(ApiError::conflict(
            "interrupted (target changed): exe.dev VM differs",
        ));
    }
    if saved.vm.is_none()
        && (saved.host_keys.is_empty() || saved.host_keys != host_keys(state, &saved.def)?)
    {
        return Err(ApiError::conflict(
            "interrupted (target changed): SSH host identity differs or was not recorded",
        ));
    }
    start_cmd_locked(state, &current.def, &saved.cmd)
}

pub(super) fn start(
    state: &Shared,
    def: &TargetDef,
    cmd: &RemoteCmd,
    mut plan: SshPlan,
    guard: Option<Box<dyn Send>>,
) -> ApiResult<Running> {
    crate::target_roots::check_base(
        def.work_root
            .as_deref()
            .unwrap_or(crate::target_roots::DEFAULT_BASE),
    )?;
    let key = cmd
        .resume_key
        .as_deref()
        .ok_or_else(|| ApiError::invalid("durable execution needs a resume identity"))?;
    let j = journal(state);
    let id = format!("ssh/{key}");
    let existing = j.load::<Checkpoint>(&id)?;
    let recovering = existing.is_some();
    let mut saved = match existing {
        Some(saved) => saved,
        None => {
            let name = def.name.clone();
            let (identity, vm) = state.db.read_now(move |c| {
                let row = crate::store::targets::get(c, &name)?;
                Ok((
                    crate::job_logs::target_identity(&row),
                    crate::exedev::record_of(c, &name)?.map(|vm| vm.name),
                ))
            })?;
            let saved = Checkpoint {
                def: def.clone(),
                identity: Some(identity),
                vm,
                host_keys: Vec::new(),
                host_trust: String::new(),
                cmd: cmd.clone(),
                token: ulid::Ulid::new().to_string(),
                created: chrono::Utc::now().timestamp(),
            };
            j.save(&id, &saved)?;
            saved
        }
    };
    let name = def.name.clone();
    let current = state
        .db
        .read_now(move |c| crate::store::targets::get(c, &name))?;
    if saved.identity.as_deref() != Some(crate::job_logs::target_identity(&current).as_str()) {
        return Err(ApiError::conflict(
            "interrupted (target changed): target incarnation differs",
        ));
    }
    if recovering {
        let name = current.def.name.clone();
        let vm = state
            .db
            .read_now(move |c| Ok(crate::exedev::record_of(c, &name)?.map(|vm| vm.name)))?;
        if vm != saved.vm
            || (vm.is_none()
                && (saved.host_keys.is_empty() || saved.host_keys != host_keys(state, &saved.def)?))
        {
            return Err(ApiError::conflict(
                "interrupted (target changed): remote identity differs or was not recorded",
            ));
        }
    }
    // Never launch from an expired checkpoint, even if remote GC has removed
    // its claim. This is the exactly-once boundary after retention expires.
    if chrono::Utc::now().timestamp() - saved.created > (MAX_RUN + RETAIN) as i64 {
        return Err(ApiError::conflict("durable SSH checkpoint expired"));
    }
    let root = saved
        .def
        .work_root
        .as_deref()
        .unwrap_or(crate::target_roots::DEFAULT_BASE);
    crate::target_roots::check_base(root)?;
    let path = format!("{root}/.terrarium-runs/{}", saved.token);
    let script = script(&path, &saved.cmd, saved.created);
    *plan
        .args
        .last_mut()
        .ok_or_else(|| ApiError::internal("empty SSH plan"))? = script;
    let (tx, events) = mpsc::channel(EVENT_QUEUE);
    let (ctl, mut controls) = mpsc::unbounded_channel();
    let state = state.clone();
    let def = saved.def.clone();
    tokio::spawn(async move {
        // Authenticate before detaching anything. A crash before this checkpoint
        // remains unbound and is refused on recovery rather than guessing a host.
        if saved.vm.is_none() && saved.host_keys.is_empty() {
            let mut probe = tokio::process::Command::new(&plan.program);
            probe.args(&plan.args[..plan.args.len() - 1]).arg("true");
            probe.kill_on_drop(true);
            let authenticated = tokio::time::timeout(Duration::from_secs(20), probe.output()).await;
            match authenticated {
                Ok(Ok(output)) if output.status.success() => {}
                _ => {
                    let _ = tx
                        .send(Ev::End(End::error(
                            "interrupted",
                            "SSH identity probe failed",
                        )))
                        .await;
                    return;
                }
            }
            match host_keys(&state, &def) {
                Ok(keys) if !keys.is_empty() => saved.host_keys = keys,
                _ => {
                    let _ = tx
                        .send(Ev::End(End::error(
                            "interrupted",
                            "SSH identity was not recorded",
                        )))
                        .await;
                    return;
                }
            }
            match host_trust(&state, &def) {
                Ok(trust) => saved.host_trust = trust,
                Err(_) => {
                    let _ = tx
                        .send(Ev::End(End::error(
                            "interrupted",
                            "cannot pin SSH host identity",
                        )))
                        .await;
                    return;
                }
            }
            if journal(&state).save(&id, &saved).is_err() {
                let _ = tx
                    .send(Ev::End(End::error(
                        "interrupted",
                        "cannot save SSH identity",
                    )))
                    .await;
                return;
            }
        }
        // Never allow TOFU on a bound SSH run. The saved fingerprint was checked
        // against this trust store before recovery starts a connection.
        let mut pinned = None;
        if saved.vm.is_none() {
            let pin = (|| -> ApiResult<_> {
                let run = RunDirs::new(&state.state_dir).run_dir()?;
                let path = run.write_private("known_hosts", saved.host_trust.as_bytes())?;
                Ok((run, path))
            })();
            let (run, path) = match pin {
                Ok(pin) => pin,
                Err(_) => {
                    let _ = tx
                        .send(Ev::End(End::error(
                            "interrupted",
                            "cannot pin SSH host identity",
                        )))
                        .await;
                    return;
                }
            };
            for arg in &mut plan.args {
                if arg == "StrictHostKeyChecking=accept-new" {
                    *arg = "StrictHostKeyChecking=yes".into();
                }
                if arg.starts_with("UserKnownHostsFile=") {
                    *arg = format!("UserKnownHostsFile={}", path.display());
                }
            }
            plan.args.splice(
                0..0,
                [
                    "-o".into(),
                    "GlobalKnownHostsFile=/dev/null".into(),
                    "-o".into(),
                    "VerifyHostKeyDNS=no".into(),
                ],
            );
            pinned = Some(run);
        }
        if state.executor.hosts.is_stopped() {
            return;
        }
        let mut process = tokio::process::Command::new(&plan.program);
        process.args(&plan.args);
        let mut raw = pump_process(process, plan.stdin_prefix, None, guard);
        let _pinned_host_identity = pinned;
        // Keep the local SSH client's control owner alive. Capturing just
        // raw.events would drop raw.ctl and pump_process would kill SSH.
        let _transport_control = raw.ctl;
        let mut pending = Vec::new();
        let mut ended = false;
        let startup = tokio::time::Instant::now();
        let mut ready = false;
        loop {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    if state.executor.hosts.is_stopped() { break; }
                    if !ready && startup.elapsed() >= Duration::from_secs(30) {
                        let _ = tx.send(Ev::End(End::error("unsupported", "durable SSH startup handshake timed out"))).await;
                        break;
                    }
                }
                control = controls.recv() => match control {
                    Some(Control::Kill | Control::Signal(_)) => {
                        if state.executor.hosts.is_stopped() { break; }
                        // Ask the still-live supervisor to signal its own
                        // child. A stale PID file can never kill a reused PID.
                        let kill = RemoteCmd {
                            argv: vec!["sh".into(), "-c".into(), format!(
                                "p={}; test -f \"$p/exit\" || : > \"$p/cancel\"", sh_quote(&path))],
                            ..RemoteCmd::default()
                        };
                        if let Ok(run) = start_cmd_locked(&state, &def, &kill) {
                            let _ = run.collect(4096).await;
                        }
                    }
                    None => break, // daemon shutdown: detach, do not kill remote
                    Some(_) => {} // stdin is rejected at the public boundary
                },
                ev = raw.events.recv() => match ev {
                    Some(Ev::Started) => { let _ = tx.send(Ev::Started).await; }
                    Some(Ev::Data(Stream::Stdout, bytes)) => {
                        pending.extend(bytes);
                        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                            let line: Vec<_> = pending.drain(..=end).collect();
                            let payload = &line[1..line.len()-1];
                            if line[0] == b'R' { ready = true; }
                            let event = match line[0] {
                                b'B' => Some(Ev::End(End::error("unsupported", String::from_utf8_lossy(payload).to_string()))),
                                b'O' | b'E' => base64::engine::general_purpose::STANDARD.decode(payload).ok()
                                    .map(|bytes| Ev::Data(if line[0] == b'O' { Stream::Stdout } else { Stream::Stderr }, bytes)),
                                b'X' => std::str::from_utf8(payload).ok().and_then(|s| s.parse::<i32>().ok())
                                    .map(|code| Ev::End(End::Exit { code: Some(code), signal: None, reason: "exited".into() })),
                                b'L' => Some(Ev::End(End::error("durable_expired", "durable SSH output retention expired"))),
                                _ => None,
                            };
                            if let Some(event) = event {
                                ended |= matches!(event, Ev::End(_));
                                if tx.send(event).await.is_err() { return; }
                            }
                        }
                    }
                    Some(Ev::End(_)) | None => {
                        if !ended { let _ = tx.send(Ev::End(End::error("interrupted", "durable SSH reader disconnected; remote result retained"))).await; }
                        break;
                    }
                    _ => {}
                }
            }
        }
    });
    Ok(Running {
        events,
        ctl,
        host_session: None,
    })
}

fn script(path: &str, cmd: &RemoteCmd, created: i64) -> String {
    script_with_retention(path, cmd, created, RETAIN)
}

fn script_with_retention(path: &str, cmd: &RemoteCmd, created: i64, retain: u64) -> String {
    let command = format!(
        "{}exec {}",
        cmd.cwd
            .as_ref()
            .map(|cwd| format!("cd {} && ", sh_quote(cwd)))
            .unwrap_or_default(),
        cmd.argv
            .iter()
            .map(|arg| sh_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let timeout = cmd
        .timeout_secs
        .filter(|n| *n > 0)
        .unwrap_or(86400)
        .min(MAX_RUN);
    let deadline = created + timeout as i64;
    let expires = created + (MAX_RUN + RETAIN) as i64;
    // Keep a leader in the command's group until the supervisor kills it.
    // Otherwise a foreground exit can strand inherited FIFO descriptors in
    // grandchildren, or release the group ID before teardown uses it.
    let group = format!(r#"
p={path}
trap ':' TERM
sh -c {command} &
f=$!
wait "$f"; code=$?
printf '%s\n' "$code" > "$p/foreground.part"
mv "$p/foreground.part" "$p/foreground"
while :; do sleep 60; done
"#, path = sh_quote(path), command = sh_quote(&command));
    let janitor = format!(
        r#"
p={path}
echo $$ > "$p/janitor"
while :; do
  test ! -f "$p/exit" || break
  sleep 1
done
sleep {retain}
exec 9> "$p/live"
flock -x 9
: > "$p/expired"
rm -f "$p/o" "$p/e" "$p/out" "$p/err" "$p/pid" "$p/exit" "$p/exit.part" "$p/cancel" "$p/ready" "$p/startup-error" "$p/foreground" "$p/foreground.part"
left=$(({expires}-$(date +%s)+120))
test "$left" -le 0 || sleep "$left"
rm -f "$p/expired" "$p/janitor" "$p/live"
rmdir "$p"
"#,
        path = sh_quote(path)
    );
    // Each logger retains at most 64 MiB and then drains the rest. Output
    // limits do not impose a file-size limit on the user's other files.
    let supervisor = format!(
        r#"
umask 077
p={path}
mkdir "$p" 2>/dev/null || exit 0
exec 9> "$p/live"
flock -x 9 || exit 1
echo $$ > "$p/pid"
bad_spool() {{
  printf '%s' "$1" > "$p/startup-error"
  : > "$p/out"; : > "$p/err"
  printf '127\n' > "$p/exit"
  : > "$p/ready"
  setsid nohup sh -c {janitor} </dev/null >/dev/null 2>&1 &
  exit 1
}}
mkfifo "$p/o" "$p/e" || bad_spool 'spool error: mkfifo failed'
: > "$p/out"; : > "$p/err"
left=$(({deadline}-$(date +%s)))
if test "$left" -le 0; then
  printf '124\n' > "$p/exit"
  : > "$p/ready"
  setsid nohup sh -c {janitor} </dev/null >/dev/null 2>&1 &
  exit
fi
: > "$p/ready"
setsid sh -c 'head -c 67108864 > "$1/out"; cat > /dev/null; while :; do sleep 60; done' sh "$p" < "$p/o" 9>&- &
o=$!
setsid sh -c 'head -c 67108864 > "$1/err"; cat > /dev/null; while :; do sleep 60; done' sh "$p" < "$p/e" 9>&- &
e=$!
setsid sh -c {group} > "$p/o" 2> "$p/e" 9>&- &
c=$!
trap 'code=143' TERM
while :; do
  if test -f "$p/foreground"; then code=$(cat "$p/foreground"); break; fi
  if test -f "$p/cancel"; then code=143; break; fi
  if test "$(date +%s)" -ge {deadline}; then code=124; break; fi
  sleep 0.1
done
kill -TERM "-$c" 2>/dev/null
sleep 5
kill -KILL "-$c" 2>/dev/null
wait "$c" 2>/dev/null
# Logger groups cannot retain the slot beyond teardown, even if a child
# escaped the command group while keeping inherited stream descriptors.
kill -TERM "-$o" "-$e" 2>/dev/null
wait "$o" "$e" 2>/dev/null
printf '%s\n' "$code" > "$p/exit.part"
mv "$p/exit.part" "$p/exit"
setsid nohup sh -c {janitor} </dev/null >/dev/null 2>&1 &
"#,
        path = sh_quote(path),
        group = sh_quote(&group),
        janitor = sh_quote(&janitor)
    );
    let env = if cmd.env.is_empty() {
        ""
    } else {
        "IFS= read -r __terrarium_env && eval \"$(printf '%s' \"$__terrarium_env\" | base64 -d)\" && unset __terrarium_env;"
    };
    format!(
        r#"{env}
umask 077
p={path}
fail() {{ printf 'B%s\n' "$1"; exit 1; }}
for tool in setsid nohup flock sh mkfifo timeout date head tail base64 tr wc cat mkdir mv rm rmdir sleep; do
  command -v "$tool" >/dev/null 2>&1 || fail "unsupported: $tool missing"
done
mkdir -p {parent} || fail 'spool error: cannot create parent'
test -w {parent} || fail 'spool error: parent is not writable'
setsid nohup sh -c {supervisor} </dev/null >/dev/null 2>&1 &
startup=$(date +%s)
until test -f "$p/ready"; do
  if test -f "$p/expired"; then printf 'L\n'; exit; fi
  test ! -f "$p/startup-error" || fail "$(cat "$p/startup-error")"
  test $(($(date +%s)-startup)) -lt 30 || fail 'spool error: supervisor did not become ready'
  sleep 0.1
done
test ! -f "$p/startup-error" || fail "$(cat "$p/startup-error")"
printf 'R\n'
o=0; e=0
while :; do
  if test -f "$p/expired"; then printf 'L\n'; exit; fi
  for stream in out err; do
    case "$stream" in out) offset=$o; tag=O;; err) offset=$e; tag=E;; esac
    if test -f "$p/$stream"; then
      data=$(tail -c +$((offset+1)) "$p/$stream" | head -c 32768 | base64 | tr -d '\n')
      if test -n "$data"; then
        printf '%s%s\n' "$tag" "$data"
        n=$(printf '%s' "$data" | base64 -d | wc -c)
        case "$stream" in out) o=$((o+n));; err) e=$((e+n));; esac
      fi
    fi
  done
  if test -f "$p/exit" && test "$o" -eq "$(wc -c < "$p/out")" && test "$e" -eq "$(wc -c < "$p/err")"; then
    printf X; cat "$p/exit"; exit
  fi
  sleep 0.1
done"#,
        path = sh_quote(path),
        parent = sh_quote(&format!(
            "{}/.terrarium-runs",
            path.rsplit_once("/.terrarium-runs/")
                .map(|(root, _)| root)
                .unwrap_or("")
        )),
        supervisor = sh_quote(&supervisor)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_missing_tool_is_terminal() {
        let cmd = RemoteCmd {
            argv: vec!["true".into()],
            ..RemoteCmd::default()
        };
        let output = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script(
                "/unused/.terrarium-runs/test",
                &cmd,
                chrono::Utc::now().timestamp(),
            ))
            .env("PATH", "/nonexistent")
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"Bunsupported: setsid missing\n");
    }

    #[tokio::test]
    async fn durable_spool_failure_is_terminal() {
        let cmd = RemoteCmd {
            argv: vec!["true".into()],
            ..RemoteCmd::default()
        };
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script(
                "/dev/null/.terrarium-runs/test",
                &cmd,
                chrono::Utc::now().timestamp(),
            ))
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).starts_with("B"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("spool error"));
    }

    async fn late_launch(age: i64, timeout: u64) -> (tempfile::TempDir, std::process::Output) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".terrarium-runs/late");
        let command = format!(
            "echo child-started; sleep 60; touch '{}'",
            root.path().join("escaped").display()
        );
        let cmd = RemoteCmd {
            argv: vec!["sh".into(), "-c".into(), command],
            timeout_secs: Some(timeout),
            ..RemoteCmd::default()
        };
        let output = tokio::time::timeout(
            Duration::from_secs(12),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(script(
                    path.to_str().unwrap(),
                    &cmd,
                    chrono::Utc::now().timestamp() - age,
                ))
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        // Kill only this test's detached janitor group, after it records its ID.
        for _ in 0..100 {
            if let Ok(pid) = std::fs::read_to_string(path.join("janitor")) {
                assert!(
                    std::process::Command::new("kill")
                        .args(["-KILL", "--", &format!("-{}", pid.trim())])
                        .status()
                        .unwrap()
                        .success()
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(path.join("exit").exists());
        assert!(!path.join("expired").exists());
        assert!(!root.path().join("escaped").exists());
        (root, output)
    }

    #[tokio::test]
    async fn durable_late_launch_keeps_original_deadline() {
        let start = tokio::time::Instant::now();
        let (root, output) = late_launch(28, 30).await;
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(String::from_utf8_lossy(&output.stdout).contains("X124\n"));
        let path = root.path().join(".terrarium-runs/late");
        assert!(
            std::fs::read_to_string(path.join("out"))
                .unwrap()
                .contains("child-started")
        );
        // The output/claim are retained after the child exits, not removed at
        // the original deadline while a fresh full-timeout child still runs.
        assert_eq!(std::fs::read_to_string(path.join("exit")).unwrap(), "124\n");
    }

    #[tokio::test]
    async fn durable_expired_first_launch_never_starts_child() {
        let (root, output) = late_launch((MAX_RUN + RETAIN - 1) as i64, MAX_RUN).await;
        assert!(String::from_utf8_lossy(&output.stdout).contains("X124\n"));
        assert!(
            std::fs::read(root.path().join(".terrarium-runs/late/out"))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn durable_cleanup_follows_child_exit() {
        cleanup_case(false, true).await;
    }

    #[tokio::test]
    async fn durable_foreground_exit_terminates_background_child() {
        cleanup_case(true, false).await;
    }

    #[tokio::test]
    async fn durable_deadline_terminates_background_children() {
        cleanup_case(true, true).await;
    }

    async fn cleanup_case(background: bool, deadline: bool) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".terrarium-runs/cleanup");
        let child = root.path().join("child");
        let cmd = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                if background {
                    format!("sleep 600 & echo $! > '{}'; {}", child.display(), if deadline { "wait" } else { "exit 0" })
                } else { format!("echo $$ > '{}'; exec sleep 60", child.display()) },
            ],
            timeout_secs: Some(30),
            ..RemoteCmd::default()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(script_with_retention(
                    path.to_str().unwrap(),
                    &cmd,
                    chrono::Utc::now().timestamp() - 28,
                    1,
                ))
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(String::from_utf8_lossy(&result.stdout).contains(if deadline { "X124\n" } else { "X0\n" }));
        let pid = std::fs::read_to_string(child).unwrap();
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
            // An orphan may await the target init's reap, but cannot execute
            // or retain stream descriptors once it has become a zombie.
            assert_eq!(status.rsplit_once(") ").unwrap().1.split_whitespace().next(), Some("Z"));
        }
        assert!(path.join("out").exists());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.join("expired").exists() || path.join("out").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // The tombstone outlives output, and the original child is already gone.
        assert!(path.exists());
        let janitor = std::fs::read_to_string(path.join("janitor")).unwrap();
        assert!(
            std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", janitor.trim())])
                .status()
                .unwrap()
                .success()
        );
    }
}
