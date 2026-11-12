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
            if let Err(error) = prune(&state) { warn!(%error, "cannot prune durable SSH checkpoints"); }
            drop(state);
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    })
}

fn prune(state: &Shared) -> ApiResult<()> {
    #[derive(Deserialize)]
    struct Owner { #[serde(default)] remote_key: Option<String> }
    #[derive(Deserialize)]
    struct Created { created: i64 }
    let j = journal(state);
    let mut owned: std::collections::HashSet<_> = j.scan::<Owner>("exec/")?.into_iter()
        .filter_map(|(_, owner)| owner.remote_key.map(|key| format!("ssh/{key}"))).collect();
    owned.extend(j.scan::<Value>("recipe/")?.into_iter().map(|(key, _)| format!("ssh/{key}")));
    let before = chrono::Utc::now().timestamp() - (MAX_RUN + RETAIN) as i64;
    let old: Vec<_> = j.scan::<Created>("ssh/")?.into_iter()
        .filter(|(key, value)| value.created < before && !owned.contains(key)).map(|(key, _)| key).collect();
    j.replace(&[], &old, &[])
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    def: TargetDef,
    cmd: RemoteCmd,
    token: String,
    created: i64,
}

fn journal(state: &Shared) -> crate::hosts::resume::Journal {
    crate::hosts::resume::Journal::new(&state.state_dir)
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
    start_cmd_locked(state, &saved.def, &saved.cmd)
}

pub(super) fn start(
    state: &Shared,
    def: &TargetDef,
    cmd: &RemoteCmd,
    mut plan: SshPlan,
    guard: Option<Box<dyn Send>>,
) -> ApiResult<Running> {
    crate::target_roots::check_base(def.work_root.as_deref().unwrap_or(crate::target_roots::DEFAULT_BASE))?;
    let key = cmd
        .resume_key
        .as_deref()
        .ok_or_else(|| ApiError::invalid("durable execution needs a resume identity"))?;
    let j = journal(state);
    let id = format!("ssh/{key}");
    let saved = match j.load::<Checkpoint>(&id)? {
        Some(saved) => saved,
        None => {
            let saved = Checkpoint {
                def: def.clone(),
                cmd: cmd.clone(),
                token: ulid::Ulid::new().to_string(),
                created: chrono::Utc::now().timestamp(),
            };
            j.save(&id, &saved)?;
            saved
        }
    };
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
    let script = script(&path, &saved.cmd, saved.created + (MAX_RUN + RETAIN) as i64);
    *plan
        .args
        .last_mut()
        .ok_or_else(|| ApiError::internal("empty SSH plan"))? = script;
    let mut process = tokio::process::Command::new(&plan.program);
    process.args(&plan.args);
    let mut raw = pump_process(process, plan.stdin_prefix, None, guard);
    let (tx, events) = mpsc::channel(EVENT_QUEUE);
    let (ctl, mut controls) = mpsc::unbounded_channel();
    let state = state.clone();
    let def = saved.def;
    tokio::spawn(async move {
        // Keep the local SSH client's control owner alive. Capturing just
        // raw.events would drop raw.ctl and pump_process would kill SSH.
        let _transport_control = raw.ctl;
        let mut pending = Vec::new();
        let mut ended = false;
        loop {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    if state.executor.hosts.is_stopped() { break; }
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
                            let event = match line[0] {
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

fn script(path: &str, cmd: &RemoteCmd, expires: i64) -> String {
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
    let janitor = format!(
        r#"
p={path}
echo $$ > "$p/janitor"
n=0
while test "$n" -lt {timeout}; do
  test ! -f "$p/exit" || break
  sleep 60
  n=$((n+60))
done
sleep {RETAIN}
: > "$p/expired"
rm -f "$p/o" "$p/e" "$p/out" "$p/err" "$p/pid" "$p/exit" "$p/exit.part" "$p/cancel"
left=$(({expires}-$(date +%s)+120))
test "$left" -le 0 || sleep "$left"
rm -f "$p/expired" "$p/janitor"
rmdir "$p"
"#,
        path = sh_quote(path),
        timeout = timeout + 60
    );
    // Each logger retains at most 64 MiB and then drains the rest. Output
    // limits do not impose a file-size limit on the user's other files.
    let supervisor = format!(
        r#"
umask 077
p={path}
mkdir "$p" 2>/dev/null || exit 0
echo $$ > "$p/pid"
setsid nohup sh -c {janitor} </dev/null >/dev/null 2>&1 &
mkfifo "$p/o" "$p/e" || exit 1
(head -c 67108864 > "$p/out"; cat > /dev/null) < "$p/o" &
o=$!
(head -c 67108864 > "$p/err"; cat > /dev/null) < "$p/e" &
e=$!
timeout -k 5 {timeout} sh -c {command} > "$p/o" 2> "$p/e" &
c=$!
trap 'kill -TERM "$c" 2>/dev/null' TERM
(while test ! -f "$p/cancel"; do sleep 0.1; done; kill -TERM "$c" 2>/dev/null) &
w=$!
wait "$c"; code=$?
kill "$w" 2>/dev/null
wait "$o" "$e" 2>/dev/null
printf '%s\n' "$code" > "$p/exit.part"
mv "$p/exit.part" "$p/exit"
"#,
        path = sh_quote(path),
        command = sh_quote(&command),
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
mkdir -p {parent} || exit 1
setsid nohup sh -c {supervisor} </dev/null >/dev/null 2>&1 &
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
