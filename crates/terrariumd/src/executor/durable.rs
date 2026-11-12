//! Restartable, noninteractive SSH commands. The encrypted journal binds a
//! caller's identity to one remote claim directory before any SSH is started.
use super::*;

const RETAIN: u64 = 86400;
const MAX_RUN: u64 = 7 * 86400;
const PATHS: &str = include_str!("durable_paths.py");

#[derive(Clone, Serialize, Deserialize)]
struct Spool {
    root: String,
    owner: String,
    card: bool,
}

fn spool(state: &Shared, def: &TargetDef, cmd: &RemoteCmd) -> ApiResult<Spool> {
    if let Some((project, name)) = cmd
        .holder
        .as_deref()
        .and_then(|holder| holder.split_once('/'))
    {
        let (project, name, target) = (project.to_owned(), name.to_owned(), def.name.clone());
        let owned = state.db.read_now(move |c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row("SELECT base, project, name, agent FROM agent_target_roots WHERE project=?1 AND name=?2 AND target=?3", rusqlite::params![project, name, target], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))).optional()?)
        })?;
        if let Some((base, project, name, agent)) = owned {
            let base = base
                .as_deref()
                .or(def.work_root.as_deref())
                .unwrap_or(crate::target_roots::DEFAULT_BASE);
            return Ok(Spool {
                root: format!("{base}/{project}/{name}"),
                owner: agent,
                card: true,
            });
        }
    }
    let journal = journal(state);
    let key = format!("spool-owner/{}", def.name);
    let owner = match journal.load::<String>(&key)? {
        Some(owner) => owner,
        None => {
            let owner = ulid::Ulid::new().to_string();
            journal.save(&key, &owner)?;
            owner
        }
    };
    Ok(Spool {
        root: def
            .work_root
            .as_deref()
            .unwrap_or(crate::target_roots::DEFAULT_BASE)
            .to_owned(),
        owner,
        card: false,
    })
}

fn checked_paths(spool: &Spool, token: &str, mode: &str, script: &str) -> String {
    format!(
        "command -v python3 >/dev/null 2>&1 || {{ printf 'Bunsupported: python3 missing\\n'; exit 1; }}; exec python3 -S -c {} {} {} {} {} {} {}",
        sh_quote(PATHS),
        sh_quote(&spool.root),
        sh_quote(&spool.owner),
        if spool.card { "1" } else { "0" },
        sh_quote(token),
        sh_quote(mode),
        sh_quote(script)
    )
}

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
    #[serde(default)]
    spool: Option<Spool>,
    cmd: RemoteCmd,
    token: String,
    created: i64,
}

fn journal(state: &Shared) -> crate::hosts::resume::Journal {
    crate::hosts::resume::Journal::with_db(&state.state_dir, state.db.clone())
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
                spool: Some(spool(state, def, cmd)?),
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
    let spool = saved
        .spool
        .as_ref()
        .ok_or_else(|| ApiError::conflict("durable checkpoint has no owned spool binding"))?;
    crate::target_roots::check_base(&spool.root)?;
    let path = format!("{}/.terrarium-runs/{}", spool.root, saved.token);
    let script = script_for_spool(&path, &saved.cmd, saved.created, RETAIN, spool);
    let cancel_script = checked_paths(spool, &saved.token, "cancel", "");
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
                            argv: vec!["sh".into(), "-c".into(), cancel_script.clone()],
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
                    Some(Ev::Data(Stream::Stderr, bytes)) => { let _ = tx.send(Ev::Data(Stream::Stderr, bytes)).await; }
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

#[cfg(test)]
fn script(path: &str, cmd: &RemoteCmd, created: i64) -> String {
    script_with_retention(path, cmd, created, RETAIN)
}

#[cfg(test)]
fn script_with_retention(path: &str, cmd: &RemoteCmd, created: i64, retain: u64) -> String {
    let (root, _) = path.rsplit_once("/.terrarium-runs/").unwrap_or((path, ""));
    script_for_spool(
        path,
        cmd,
        created,
        retain,
        &Spool {
            root: root.into(),
            owner: "test-owner".into(),
            card: false,
        },
    )
}

fn script_for_spool(
    path: &str,
    cmd: &RemoteCmd,
    created: i64,
    retain: u64,
    spool: &Spool,
) -> String {
    let command = format!(
        "{}exec {}",
        cmd.cwd
            .as_ref()
            .map(|cwd| format!("cd {} && ", sh_quote(cwd)))
            .unwrap_or_else(|| "cd \"$TERRARIUM_ORIGINAL_CWD\" && ".into()),
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
    let group = format!(
        r#"
p=.
trap ':' TERM
sh -c {command} &
f=$!
# The coordinator anchors the group identity, not the output streams.
exec >/dev/null 2>&1
wait "$f"; code=$?
printf '%s\n' "$code" > "$p/foreground.part"
mv "$p/foreground.part" "$p/foreground"
while :; do sleep 60; done
"#,
        command = sh_quote(&command)
    );
    let janitor = format!(
        r#"
p=.
echo $$ > "$p/janitor"
while :; do
  test ! -f "$p/exit" || break
  sleep 1
done
sleep {retain}
exec 9> "$p/live"
flock -x 9
: > "$p/expired"
rm -f "$p/o" "$p/e" "$p/out" "$p/err" "$p/pid" "$p/exit" "$p/exit.part" "$p/cancel" "$p/ready" "$p/startup-error" "$p/foreground" "$p/foreground.part" "$p/out-done" "$p/err-done"
left=$(({expires}-$(date +%s)+120))
test "$left" -le 0 || sleep "$left"
rm -f "$p/expired" "$p/janitor" "$p/live" "$p/.terrarium-owner"
python3 -S -c {remove_claim} {token}
"#,
        remove_claim = sh_quote(
            "import os,sys; a=os.stat(sys.argv[1],dir_fd=8,follow_symlinks=False); b=os.fstat(7); assert (a.st_dev,a.st_ino)==(b.st_dev,b.st_ino), 'claim identity changed'; os.rmdir(sys.argv[1],dir_fd=8)"
        ),
        token = sh_quote(path.rsplit('/').next().unwrap_or(""))
    );
    // Each logger retains at most 64 MiB and then drains the rest. Output
    // limits do not impose a file-size limit on the user's other files.
    let supervisor = format!(
        r#"
umask 077
p=.
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
setsid sh -c 'head -c 67108864 > "$1/out"; cat > /dev/null; : > "$1/out-done"; while :; do sleep 60; done' sh "$p" < "$p/o" 9>&- &
o=$!
setsid sh -c 'head -c 67108864 > "$1/err"; cat > /dev/null; : > "$1/err-done"; while :; do sleep 60; done' sh "$p" < "$p/e" 9>&- &
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
# Most loggers finish during TERM grace. Give a KILL-closed pipe one bounded
# final drain interval before stopping its logger, rather than truncating a
# buffered final write in the same scheduling turn as KILL.
n=0
while test ! -f "$p/out-done" || test ! -f "$p/err-done"; do
  test "$n" -lt 10 || break
  sleep 0.1
  n=$((n+1))
done
# Logger groups cannot retain the slot beyond teardown, even if a child
# escaped the command group while keeping inherited stream descriptors.
kill -TERM "-$o" "-$e" 2>/dev/null
wait "$o" "$e" 2>/dev/null
printf '%s\n' "$code" > "$p/exit.part"
mv "$p/exit.part" "$p/exit"
setsid nohup sh -c {janitor} </dev/null >/dev/null 2>&1 &
"#,
        group = sh_quote(&group),
        janitor = sh_quote(&janitor)
    );
    let env = if cmd.env.is_empty() {
        ""
    } else {
        "IFS= read -r __terrarium_env && eval \"$(printf '%s' \"$__terrarium_env\" | base64 -d)\" && unset __terrarium_env;"
    };
    let reader = format!(
        r#"
umask 077
p=.
fail() {{ printf 'B%s\n' "$1"; exit 1; }}
for tool in setsid nohup flock sh mkfifo timeout date head tail base64 tr wc cat mkdir mv rm rmdir sleep; do
  command -v "$tool" >/dev/null 2>&1 || fail "unsupported: $tool missing"
done
if test "$TERRARIUM_NEW_CLAIM" = 1; then
  setsid nohup sh -c {supervisor} </dev/null >/dev/null 2>&1 &
fi
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
        supervisor = sh_quote(&supervisor)
    );
    // Check prerequisites before invoking the path helper, preserving clear
    // failures even when PATH lacks both the helper and framing tools.
    format!(
        "{env}\nfor tool in setsid nohup flock sh mkfifo timeout date head tail base64 tr wc cat mkdir mv rm rmdir sleep; do command -v \"$tool\" >/dev/null 2>&1 || {{ printf 'Bunsupported: %s missing\\n' \"$tool\"; exit 1; }}; done\n{}",
        checked_paths(spool, path.rsplit('/').next().unwrap_or(""), "run", &reader)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn secure_root(card: bool) -> (tempfile::TempDir, Spool) {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("owned");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        if card {
            std::fs::write(root.join(".terrarium-owner"), "test-owner").unwrap();
        }
        (
            temp,
            Spool {
                root: root.to_str().unwrap().into(),
                owner: "test-owner".into(),
                card,
            },
        )
    }

    async fn path_refusal(attack: &str) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        for card in [false, true] {
            let (temp, spool) = secure_root(card);
            let parent = std::path::Path::new(&spool.root).join(".terrarium-runs");
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("keep"), "untouched").unwrap();
            if attack == "symlink" {
                std::os::unix::fs::symlink(&outside, &parent).unwrap();
            } else {
                std::fs::create_dir(&parent).unwrap();
                if attack == "permissions" {
                    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o770))
                        .unwrap();
                } else {
                    // Foreign ownership is exercised by the root card check.
                    // An unprivileged extra gate cannot manufacture that owner.
                    if std::fs::metadata(&parent).unwrap().uid() != 0 {
                        continue;
                    }
                    std::os::unix::fs::chown(&parent, Some(65534), None).unwrap();
                }
            }
            let cmd = RemoteCmd {
                argv: vec![
                    "touch".into(),
                    outside.join("executed").to_str().unwrap().into(),
                ],
                ..RemoteCmd::default()
            };
            let path = format!("{}/.terrarium-runs/claim", spool.root);
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(script_for_spool(
                    &path,
                    &cmd,
                    chrono::Utc::now().timestamp(),
                    1,
                    &spool,
                ))
                .output()
                .await
                .unwrap();
            assert!(!output.status.success());
            let message = String::from_utf8_lossy(&output.stdout);
            assert!(message.starts_with("Bspool error:"), "{message}");
            if attack == "permissions" {
                assert!(message.contains("group/other writable"));
            }
            if attack == "owner" {
                assert!(message.contains("foreign owner"));
            }
            assert_eq!(
                std::fs::read_to_string(outside.join("keep")).unwrap(),
                "untouched"
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn durable_refuses_symlink_parent_for_both_root_kinds() {
        path_refusal("symlink").await;
    }

    #[tokio::test]
    async fn durable_refuses_writable_parent_for_both_root_kinds() {
        path_refusal("permissions").await;
    }

    #[tokio::test]
    async fn durable_refuses_foreign_parent_for_both_root_kinds() {
        path_refusal("owner").await;
    }

    #[tokio::test]
    async fn durable_refuses_swapped_claim_for_both_root_kinds() {
        for card in [false, true] {
            let (temp, spool) = secure_root(card);
            let path = format!("{}/.terrarium-runs/claim", spool.root);
            let cmd = RemoteCmd {
                argv: vec!["true".into()],
                ..RemoteCmd::default()
            };
            let script =
                script_for_spool(&path, &cmd, chrono::Utc::now().timestamp(), RETAIN, &spool);
            let output = tokio::time::timeout(
                Duration::from_secs(12),
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&script)
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(String::from_utf8_lossy(&output.stdout).contains("X0\n"));
            let claim = std::path::Path::new(&path);
            for _ in 0..100 {
                if let Ok(pid) = std::fs::read_to_string(claim.join("janitor")) {
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
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("keep"), "untouched").unwrap();
            std::fs::rename(claim, claim.with_file_name("old-claim")).unwrap();
            std::os::unix::fs::symlink(&outside, claim).unwrap();
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .output()
                .await
                .unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).starts_with("Bspool error:"));
            assert_eq!(
                std::fs::read_to_string(outside.join("keep")).unwrap(),
                "untouched"
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
            // Cancellation also goes through no-follow descriptors.
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(checked_paths(&spool, "claim", "cancel", ""))
                .output()
                .await
                .unwrap();
            assert!(!output.status.success());
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        }
    }

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
                    format!(
                        "sleep 600 & echo $! > '{}'; {}",
                        child.display(),
                        if deadline { "wait" } else { "exit 0" }
                    )
                } else {
                    format!("echo $$ > '{}'; exec sleep 60", child.display())
                },
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
        assert!(
            String::from_utf8_lossy(&result.stdout).contains(if deadline {
                "X124\n"
            } else {
                "X0\n"
            })
        );
        let pid = std::fs::read_to_string(child).unwrap();
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
            // An orphan may await the target init's reap, but cannot execute
            // or retain stream descriptors once it has become a zombie.
            assert_eq!(
                status
                    .rsplit_once(") ")
                    .unwrap()
                    .1
                    .split_whitespace()
                    .next(),
                Some("Z")
            );
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
