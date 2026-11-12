//! Owned build roots for card agents. Legacy and shared paths are never swept.
use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{RemoteCmd, start_cmd};
use crate::store::targets;
use futures_util::TryStreamExt;
use rusqlite::params;
use terrarium_core::config::TargetDef;

const PYTHON_PROBE: &str = "command -v python3 >/dev/null || printf missing-python3";

fn commands_only_host(kind: terrarium_core::config::TargetKind, os: &str, no_workspaces: bool) -> bool {
    kind == terrarium_core::config::TargetKind::Host && os == "windows" && no_workspaces
}

fn is_commands_only_host(state: &Shared, def: &TargetDef) -> bool {
    def.host.as_deref().and_then(|host| state.executor.hosts.view(host).info)
        .is_some_and(|info| commands_only_host(def.kind, &info.os, info.capabilities.no_workspaces))
}

fn measurement(stdout: &[u8]) -> Option<u64> {
    std::str::from_utf8(stdout).ok()?.trim().parse().ok()
}

/// Serialize starts with cleanup, including checkouts (not exec sessions).
pub async fn lock(agent: &str) -> tokio::sync::OwnedMutexGuard<()> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    type Locks = HashMap<String, Weak<tokio::sync::Mutex<()>>>;
    static LOCKS: OnceLock<Mutex<Locks>> = OnceLock::new();
    let mutex = {
        let mut locks = LOCKS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, v| v.strong_count() != 0);
        let mutex = locks
            .get(agent)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        locks.insert(agent.to_owned(), Arc::downgrade(&mutex));
        mutex
    };
    mutex.lock_owned().await
}

pub async fn lock_card(
    state: &Shared,
    actor: &Actor,
    target: &str,
) -> ApiResult<Option<tokio::sync::OwnedMutexGuard<()>>> {
    let Some(a) = actor.agent() else {
        return Ok(None);
    };
    let id = a.id.clone();
    let card = state
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM spawns WHERE id=?1 AND bound_kind='card')",
                [&id],
                |r| r.get::<_, bool>(0),
            )?)
        })
        .await?;
    if card {
        Ok(Some(lock(&format!("{}:{target}", a.id)).await))
    } else {
        Ok(None)
    }
}

pub fn check_base(base: &str) -> ApiResult<()> {
    if !base.starts_with('/')
        || base
            .split('/')
            .skip(1)
            .any(|c| matches!(c, "" | "." | "..") || c.chars().any(char::is_control))
    {
        return Err(ApiError::invalid(
            "work_root must be an absolute non-root path without empty, dot or parent components",
        ));
    }
    Ok(())
}

fn root(base: &str, project: &str, name: &str) -> ApiResult<String> {
    check_base(base)?;
    crate::store::check_name("project", project)?;
    crate::store::check_name("agent", name)?;
    Ok(format!("{base}/{project}/{name}"))
}

/// The base of a target without `work_root`.
pub const DEFAULT_BASE: &str = "/src/terrarium";
/// The exit code of the prepare script when the agent user may not create
/// the base (EACCES, EPERM, EROFS).
const BASE_UNAVAILABLE: i32 = 3;
/// The fallback base under the agent user's home, when the target has no
/// `work_root` and the default base is unavailable.
const FALLBACK_DIR: &str = ".terrarium-work";
const HOME_PROBE: &str = "import os, sys; sys.stdout.write(os.path.expanduser('~'))";

fn base(def: &TargetDef) -> &str {
    def.work_root.as_deref().unwrap_or(DEFAULT_BASE)
}

/// The fallback base for a home directory, checked like a `work_root`.
fn fallback_base(home: &str) -> ApiResult<String> {
    if !home.starts_with('/') {
        return Err(ApiError::invalid("the home must be an absolute path"));
    }
    let fallback = format!("{}/{FALLBACK_DIR}", home.trim_end_matches('/'));
    check_base(&fallback)?;
    Ok(fallback)
}

/// The base that an agent's root on a target already uses: the stored
/// fallback, else the base of the definition.
async fn stored_base(state: &Shared, agent: &str, target: &str) -> ApiResult<Option<String>> {
    let (agent, target) = (agent.to_owned(), target.to_owned());
    state
        .db
        .read(move |c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT base FROM agent_target_roots WHERE agent=?1 AND target=?2",
                params![agent, target],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
        })
        .await
}

fn exit_code(out: &crate::executor::Collected) -> Option<i32> {
    match out.end() {
        crate::executor::End::Exit { code, .. } => code,
        _ => None,
    }
}

/// The error for a failed prepare. It names `work_root` and the command
/// that sets it when the base itself is the problem.
fn prepare_error(def: &TargetDef, base: &str, unavailable: bool, tail: &str) -> ApiError {
    let edit = format!(
        "set a writable work_root with `terrarium admin target edit {} --work-root <absolute path>`",
        def.name
    );
    ApiError::invalid(match (unavailable, &def.work_root) {
        (false, _) => format!("cannot prepare owned target root under {base}: {tail}"),
        (true, Some(_)) => format!(
            "cannot prepare owned target root: work_root {base} of target {} is not writable by the agent user: {tail}; fix its permissions or {edit}",
            def.name
        ),
        (true, None) => format!(
            "cannot prepare owned target root: target {} has no work_root, the default {DEFAULT_BASE} is not writable by the agent user, and the fallback {base} failed: {tail}; {edit}",
            def.name
        ),
    })
}

/// Warn one time per target and daemon run about the home fallback.
fn warn_fallback(def: &TargetDef, fallback: &str) {
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let first = WARNED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(def.name.clone());
    if first {
        tracing::warn!(target = %def.name, %fallback,
            "target has no work_root and {DEFAULT_BASE} is not writable; card-agent roots fall back to the agent user's home; set one with `terrarium admin target edit {} --work-root <absolute path>`", def.name);
    }
}

/// Only machine/path identity matters. Credentials and limits may be rotated
/// without stranding roots or preventing live agents from starting work.
pub(crate) fn same_location(old: &str, def: &TargetDef) -> bool {
    let Ok(old) = serde_json::from_str::<TargetDef>(old) else {
        return false;
    };
    old.kind == def.kind
        && old.host == def.host
        && old.address == def.address
        && old.port == def.port
        && old.container == def.container
        && old.username == def.username
        && base(&old) == base(def)
}

fn inside(root: &str, path: &str) -> bool {
    path.starts_with(&format!("{root}/"))
        && path
            .split('/')
            .skip(1)
            .all(|c| !matches!(c, "" | "." | ".."))
}

pub fn build_env(root: &str, cmd: &mut RemoteCmd) -> ApiResult<()> {
    for (key, suffix) in [("CARGO_TARGET_DIR", "target"), ("RESULTS_DIR", "results")] {
        let value = cmd
            .env
            .entry(key.into())
            .or_insert(format!("{root}/{suffix}"));
        if !inside(root, value) {
            return Err(ApiError::invalid(format!(
                "{key} must be strictly under {root}"
            )));
        }
    }
    Ok(())
}

// All traversal and deletion uses checked directory FDs. Supported on Linux
// with Python 3.9+, without the Python 3.11-only rmtree(dir_fd=...) API.
// Accepted boundary: a process with the same UID can rename an opened subtree
// outside the root during traversal. Directory FDs prevent symlink redirection,
// not same-UID renames. That UID already has permission to delete these files.
const SCRIPT: &str = r#"
import errno, os, stat, sys
mode, base, project, name, agent, dest = sys.argv[1:]
def mount_id(fd):
    with open('/proc/self/fdinfo/' + str(fd)) as info:
        for line in info:
            if line.startswith('mnt_id:'): return line.split()[1]
    raise RuntimeError('cannot determine mount identity; cleanup requires Linux procfs')
def walk(fd, remove, device, mount, keep_owner=False):
    size = 0
    for item in os.listdir(fd):
        meta = os.stat(item, dir_fd=fd, follow_symlinks=False)
        if meta.st_dev != device: raise RuntimeError('refusing mounted data in owned root')
        if stat.S_ISDIR(meta.st_mode):
            child = os.open(item, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            try:
                if mount_id(child) != mount: raise RuntimeError('refusing bind mount in owned root')
                size += walk(child, remove, device, mount)
                if remove:
                    current = os.stat(item, dir_fd=fd, follow_symlinks=False)
                    if (current.st_dev, current.st_ino) != (meta.st_dev, meta.st_ino):
                        raise RuntimeError('directory changed during cleanup')
                    os.rmdir(item, dir_fd=fd)
            finally: os.close(child)
        elif remove and not (keep_owner and item == '.terrarium-owner'):
            os.unlink(item, dir_fd=fd)
        size += meta.st_blocks * 512
    return size
fds = []
try:
    # Ancestors need only search permission (an Android /data is 0771), so
    # they open with O_PATH. The agent root below them opens for reading.
    fd = os.open('/', os.O_PATH | os.O_DIRECTORY)
    fds.append(fd)
    for part in base.split('/')[1:] + [project]:
        try:
            if mode == 'prepare':
                try: os.mkdir(part, dir_fd=fd)
                except FileExistsError: pass
            try:
                fd = os.open(part, os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                fds.append(fd)
                # O_PATH with O_NOFOLLOW may return the symlink itself: refuse it.
                if not stat.S_ISDIR(os.fstat(fd).st_mode): raise RuntimeError('refusing non-directory ancestor')
            except FileNotFoundError:
                if mode != 'prepare':
                    print(0)
                    sys.exit(0)
                raise
        except OSError as e:
            # Only a base the agent user may not create is unavailable. A
            # symlink or a foreign owner still fails closed (exit 1).
            if mode == 'prepare' and e.errno in (errno.EACCES, errno.EPERM, errno.EROFS):
                sys.stderr.write('base unavailable: ' + str(e) + '\n')
                sys.exit(3)
            raise
    parent = fd
    created = False
    if mode == 'prepare':
        try:
            os.mkdir(name, dir_fd=parent)
            created = True
        except FileExistsError: pass
    try: fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    except FileNotFoundError:
        if mode != 'prepare':
            print(0)
            sys.exit(0)
        raise
    fds.append(fd)
    flags = os.O_RDONLY | os.O_NOFOLLOW
    if created:
        # Group-readable: on a native host the next prepare or the sweep can
        # run as another pool UID of the shared group (TER-635).
        marker = os.open('.terrarium-owner', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o640, dir_fd=fd)
        with os.fdopen(marker, 'w') as out: out.write(agent)
    marker = os.open('.terrarium-owner', flags, dir_fd=fd)
    with os.fdopen(marker) as inp:
        if inp.read(128) != agent:
            raise RuntimeError('root belongs to an earlier or different agent; wait for its live runs and cleanup, or inspect its cleanup refusal audit')
    if mode == 'prepare' and dest:
        for part in dest.split('/'):
            if part in ('', '.', '..'): raise RuntimeError('unsafe checkout path')
            try: os.mkdir(part, dir_fd=fd)
            except FileExistsError: pass
            fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            fds.append(fd)
    if mode in ('measure', 'delete', 'clear'):
        meta = os.fstat(fd)
        if mount_id(fd) != mount_id(parent): raise RuntimeError('refusing mounted agent root')
        # First check the complete tree. In particular, refuse mounts before
        # removing the owner marker so a refused cleanup stays retryable.
        size = walk(fd, False, meta.st_dev, mount_id(fd))
        if mode in ('delete', 'clear'):
            walk(fd, True, meta.st_dev, mount_id(fd), keep_owner=True)
            current = os.stat(name, dir_fd=parent, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != (meta.st_dev, meta.st_ino):
                raise RuntimeError('agent root changed during cleanup')
        if mode == 'delete':
            os.unlink('.terrarium-owner', dir_fd=fd)
            try: os.rmdir(name, dir_fd=parent)
            except OSError:
                marker = os.open('.terrarium-owner', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o640, dir_fd=fd)
                with os.fdopen(marker, 'w') as out: out.write(agent)
                raise
        print(size)
finally:
    for fd in reversed(fds): os.close(fd)
"#;

/// The daemon's own fixed root scripts (the python probe, the home
/// fallback, prepare, measure, delete, also in the sweep of an ended agent)
/// run with the operator's scope, as the kill and clear scripts of
/// `run_kill` do: they are daemon-written, their only arguments are names
/// that [`root`] checked, and no agent code runs in them. On a host with
/// native GPU enforcement the operator scope (the whole workspace root,
/// no tmpfs) keeps their mount check and their mkdir working; an agent's
/// own execs keep the holder `<project>/<agent>` and its own directory.
///
/// `cwd` comes from [`script_cwd`].
fn root_script(argv: Vec<String>, timeout_secs: u64, cwd: Option<&str>) -> RemoteCmd {
    RemoteCmd {
        argv,
        cwd: cwd.map(str::to_owned),
        timeout_secs: Some(timeout_secs),
        operator: true,
        ..RemoteCmd::default()
    }
}

/// The working directory of the root scripts on `def` with the owned-root
/// base `base` (TER-615). The native helper refuses a working directory
/// outside its `workspace_root`, and a command with no cwd gets the host
/// agent's own `work_dir`, which is outside it (`/work` on helios). So on
/// a host with native GPU enforcement the scripts run in the base: the
/// `work_root` is under the helper's `workspace_root` there (TER-485), and
/// the helper accepts the workspace root itself. The scripts use only
/// absolute paths, so the directory changes nothing else. Elsewhere they
/// keep no cwd: the base can be missing before the first prepare creates
/// it.
fn script_cwd(state: &Shared, def: &TargetDef, base: &str) -> Option<String> {
    script_cwd_for(def, host_caps(state, def).as_ref(), base)
}

fn host_caps(
    state: &Shared,
    def: &TargetDef,
) -> Option<terrarium_core::protocol::HostCapabilities> {
    def.host
        .as_deref()
        .and_then(|h| state.executor.hosts.capabilities(h))
}

/// The default working directory of a card agent's command on `def`
/// (TER-634). On a host with native GPU enforcement a command with no cwd
/// would get the host agent's `work_dir` (`/work` on helios), which the
/// helper refuses: it runs in the agent's owned root `root` instead, which
/// prepare has made. An explicit cwd is kept, so one outside the
/// workspace root is still refused by the helper. Elsewhere nothing
/// changes.
fn default_exec_cwd(
    def: &TargetDef,
    caps: Option<&terrarium_core::protocol::HostCapabilities>,
    root: &str,
    cmd: &mut RemoteCmd,
) {
    if cmd.cwd.is_none() && crate::executor::native_enforced(def, caps) {
        cmd.cwd = Some(root.to_owned());
    }
}

fn script_cwd_for(
    def: &TargetDef,
    caps: Option<&terrarium_core::protocol::HostCapabilities>,
    base: &str,
) -> Option<String> {
    crate::executor::native_enforced(def, caps).then(|| base.to_owned())
}

fn command(
    mode: &str,
    base: &str,
    project: &str,
    name: &str,
    agent: &str,
    dest: &str,
    cwd: Option<&str>,
) -> ApiResult<RemoteCmd> {
    root(base, project, name)?;
    let mut cmd = root_script(
        [
            "python3",
            "-c",
            SCRIPT,
            if mode == "delete" && cwd.is_some() {
                "clear"
            } else {
                mode
            },
            base,
            project,
            name,
            agent,
            dest,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        if mode == "prepare" { 120 } else { 900 },
        cwd,
    );
    // A native helper creates the protected parent and root before this
    // unprivileged operator-scope script checks the marker. Other targets
    // keep the existing Python preparation path.
    if matches!(mode, "prepare" | "delete") && cwd.is_some() {
        cmd.prepare_root = Some(terrarium_host::native::OwnedRoot {
            cleanup: mode == "delete",
            base: base.to_owned(),
            project: project.to_owned(),
            name: name.to_owned(),
            agent: agent.to_owned(),
        });
    }
    Ok(cmd)
}

/// Record before creating anything, so a crash cannot orphan a new root.
/// Only bound card agents get this contract. Other callers retain their paths.
pub async fn prepare(
    state: &Shared,
    actor: &Actor,
    def: &TargetDef,
    dest: Option<&str>,
) -> ApiResult<Option<String>> {
    // Windows host sessions are commands only. Do not create Linux build
    // roots, run sh/python probes, or arrange automatic workspace cleanup.
    if is_commands_only_host(state, def) { return Ok(None); }
    let Some(a) = actor.agent() else {
        return Ok(None);
    };
    let id = a.id.clone();
    let card = state
        .db
        .read(move |c| {
            use rusqlite::OptionalExtension;
            let status = c
                .query_row(
                    "SELECT bound_kind, finished_at FROM spawns WHERE id=?1",
                    [&id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()?;
            match status {
                Some((Some(kind), finished)) if kind == "card" => {
                    if finished.is_some() {
                        return Err(ApiError::conflict("card agent already finished"));
                    }
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await?;
    if !card || def.kind == terrarium_core::config::TargetKind::Vmlab {
        return Ok(None);
    }
    let stored = stored_base(state, &a.id, &def.name).await?;
    let mut current = stored.unwrap_or_else(|| base(def).to_owned());
    // Probe separately: a missing interpreter is a legacy target, whereas a
    // failed ownership check must still fail closed.
    let probe = start_cmd(
        state,
        def,
        &root_script(
            vec!["sh".into(), "-c".into(), PYTHON_PROBE.into()],
            30,
            script_cwd(state, def, &current).as_deref(),
        ),
    )?
    .collect(4096)
    .await;
    if probe.end().success() && probe.stdout == b"missing-python3" {
        tracing::warn!(agent = %a.id, target = %def.name,
            "python3 unavailable; owned root preparation and automatic cleanup disabled for this operation");
        return Ok(None);
    }
    if !probe.end().success() {
        return Err(ApiError::invalid(probe_failure(&probe)));
    }
    let mut path = root(&current, &a.project, &a.name)?;
    if dest.is_some_and(|d| !inside(&path, d)) {
        warn_outside(state, actor, def, &path).await?;
        return Ok(None);
    }
    let (id, project, name, target) = (
        a.id.clone(),
        a.project.clone(),
        a.name.clone(),
        def.name.clone(),
    );
    let definition = serde_json::to_string(def).map_err(|e| ApiError::internal(e.to_string()))?;
    let location = def.clone();
    state
        .db
        .write("system", move |t| {
            t.execute(
                "INSERT INTO agent_target_roots (agent, project, name, target, definition)
                   VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(agent, target) DO NOTHING",
                params![id, project, name, target, definition],
            )?;
            let old: String = t.query_row(
                "SELECT definition FROM agent_target_roots WHERE agent=?1 AND target=?2",
                params![id, target],
                |r| r.get(0),
            )?;
            if !same_location(&old, &location) {
                return Err(ApiError::conflict(
                    "owned target machine or work_root changed",
                ));
            }
            Ok(())
        })
        .await?;
    let relative = dest.map(|d| &d[path.len() + 1..]).unwrap_or_default();
    let mut out = start_cmd(
        state,
        def,
        &command(
            "prepare",
            &current,
            &a.project,
            &a.name,
            &a.id,
            relative,
            script_cwd(state, def, &current).as_deref(),
        )?,
    )?
    .collect(4096)
    .await;
    if exit_code(&out) == Some(BASE_UNAVAILABLE)
        && def.work_root.is_none()
        && current == DEFAULT_BASE
    {
        // Exit 3 means the agent user cannot reach the default base, so no
        // root of this agent exists there: switching the row orphans nothing.
        current = home_fallback(state, def, &current).await?;
        path = root(&current, &a.project, &a.name)?;
        let (id, target, fallback) = (a.id.clone(), def.name.clone(), current.clone());
        state
            .db
            .write("system", move |t| {
                t.execute(
                    "UPDATE agent_target_roots SET base=?3 WHERE agent=?1 AND target=?2",
                    params![id, target, fallback],
                )?;
                Ok(())
            })
            .await?;
        warn_fallback(def, &current);
        if dest.is_some() {
            return Err(ApiError::invalid(format!(
                "{DEFAULT_BASE} is not writable on target {0}; the owned root is now {path}: use a path under it, or set a writable work_root with `terrarium admin target edit {0} --work-root <absolute path>`",
                def.name
            )));
        }
        out = start_cmd(
            state,
            def,
            &command(
                "prepare",
                &current,
                &a.project,
                &a.name,
                &a.id,
                "",
                script_cwd(state, def, &current).as_deref(),
            )?,
        )?
        .collect(4096)
        .await;
    }
    if !out.end().success() {
        // Each failure at the home fallback (now or stored from an earlier
        // operation) names work_root, whatever the exit code.
        let unavailable = exit_code(&out) == Some(BASE_UNAVAILABLE) || current != base(def);
        return Err(prepare_error(
            def,
            &current,
            unavailable,
            &out.stderr_tail(),
        ));
    }
    Ok(Some(path))
}

/// The error of a failed python3 probe, with what the target said: its
/// error output, or else how the run ended (TER-566: a refused native
/// exec showed only the first sentence).
fn probe_failure(probe: &crate::executor::Collected) -> String {
    let mut why = probe.stderr_tail();
    if why.is_empty() {
        why = match probe.end() {
            crate::executor::End::Error { message, .. } => message,
            crate::executor::End::Exit {
                code,
                signal,
                reason,
            } => match (code, signal) {
                (Some(c), _) => format!("exit code {c}"),
                (None, Some(s)) => format!("signal {s}"),
                _ => reason,
            },
            crate::executor::End::Cp { .. } => String::new(),
        };
    }
    if why.is_empty() {
        "cannot check target python3 availability".to_owned()
    } else {
        format!("cannot check target python3 availability: {why}")
    }
}

/// The home fallback base of the agent user on the target. `base` is the
/// base that failed, for [`script_cwd`].
async fn home_fallback(state: &Shared, def: &TargetDef, base: &str) -> ApiResult<String> {
    let out = start_cmd(
        state,
        def,
        &root_script(
            vec!["python3".into(), "-c".into(), HOME_PROBE.into()],
            30,
            script_cwd(state, def, base).as_deref(),
        ),
    )?
    .collect(4096)
    .await;
    let home = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.end().success() {
        let tail = format!(
            "cannot read the home of the agent user: {}",
            out.stderr_tail()
        );
        return Err(prepare_error(
            def,
            &format!("~/{FALLBACK_DIR}"),
            true,
            &tail,
        ));
    }
    fallback_base(&home).map_err(|_| {
        prepare_error(
            def,
            &format!("~/{FALLBACK_DIR}"),
            true,
            &format!("the home of the agent user `{home}` is not a usable absolute path"),
        )
    })
}

pub async fn prepare_exec(
    state: &Shared,
    actor: &Actor,
    def: &TargetDef,
    cmd: &mut RemoteCmd,
) -> ApiResult<()> {
    if is_commands_only_host(state, def) { return Ok(()); }
    let Some(a) = actor.agent() else {
        return Ok(());
    };
    let kind = crate::executor::path_kind(state, def);
    if kind == terrarium_host::protocol::PathKind::Windows {
        // Windows creation and ACL enforcement belong to the host. Do not
        // send the Linux prepare script to it. Still enforce lexical scope.
        let root = kind
            .workspace(&a.project, &a.name)
            .ok_or_else(|| ApiError::invalid("invalid Windows workspace name"))?;
        if cmd
            .cwd
            .as_deref()
            .is_some_and(|p| !kind.inside(&root, p, true))
        {
            return Err(ApiError::invalid(format!("cwd must be inside {root}")));
        }
        cmd.cwd.get_or_insert(root.clone());
        for (key, suffix) in [("CARGO_TARGET_DIR", "target"), ("RESULTS_DIR", "results")] {
            let value = cmd
                .env
                .entry(key.into())
                .or_insert(format!(r"{root}\{suffix}"));
            if !kind.inside(&root, value, false) {
                return Err(ApiError::invalid(format!(
                    "{key} must be strictly under {root}"
                )));
            }
        }
        return Ok(());
    }
    let stored = stored_base(state, &a.id, &def.name).await?;
    let r = root(stored.as_deref().unwrap_or(base(def)), &a.project, &a.name)?;
    let paths = [
        // The root itself is a valid cwd. `inside` is deliberately strict
        // for build/results directories, but must not skip preparation here.
        cmd.cwd.as_deref().filter(|p| *p != r),
        cmd.env.get("CARGO_TARGET_DIR").map(String::as_str),
        cmd.env.get("RESULTS_DIR").map(String::as_str),
    ];
    let outside = paths.into_iter().flatten().find(|p| !inside(&r, p));
    if let Some(r) = prepare(state, actor, def, outside).await? {
        build_env(&r, cmd)?;
        default_exec_cwd(def, host_caps(state, def).as_ref(), &r, cmd);
    }
    Ok(())
}

/// Retry persisted roots after close or restart. Never kill a run to free a root.
pub async fn sweep(state: &Shared) -> ApiResult<()> {
    let rows = state
        .db
        .read(|c| {
            let mut q = c.prepare(
                "SELECT r.agent, r.project, r.name, r.target, r.definition, r.base
            FROM agent_target_roots r WHERE NOT EXISTS (SELECT 1 FROM agents a WHERE a.id=r.agent)",
            )?;
            Ok(q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let mut by_target = std::collections::HashMap::<_, Vec<_>>::new();
    for row in rows {
        by_target.entry(row.3.clone()).or_default().push(row);
    }
    // Serial within a target, at most three targets at once. Each measure and
    // delete has its own deadline so one large tree cannot stall other targets.
    futures_util::stream::iter(by_target.into_values().map(Ok::<_, ApiError>))
        .try_for_each_concurrent(3, |rows| async move {
    for (id, project, name, target, definition, stored) in rows {
        let _guard = lock(&format!("{id}:{target}")).await;
        if state.executor.live_of(&id) != 0 {
            continue;
        }
        let key = target.clone();
        let mut row = match state.db.read(move |c| targets::get(c, &key)).await {
            Ok(row) => row,
            Err(e) => {
                refusal(state, &id, &project, &target, &e.to_string()).await?;
                tracing::warn!(%id, %target, "target cleanup deferred: {e}");
                continue;
            }
        };
        // Roots are recorded against the VM's resolved SSH definition. Resolve
        // the current VM too, so cleanup checks the same machine identity.
        if row.def.kind == terrarium_core::config::TargetKind::Exedev {
            match crate::exedev::root_vm_absent(state, &row.def, &definition).await {
                Ok(true) => {
                    state.db.write("system", move |t| {
                        let changed = t.execute(
                            "DELETE FROM agent_target_roots WHERE agent=?1 AND target=?2",
                            params![id, target],
                        )?;
                        if changed != 0 {
                            t.audit("target.root_dropped", &id, Some(&project),
                                serde_json::json!({"agent": id, "target": target, "reason": "vm_absent"}))?;
                        }
                        Ok(())
                    }).await?;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    refusal(state, &id, &project, &target, &e.to_string()).await?;
                    continue;
                }
            }
            match crate::exedev::exec_def(state, &row.def) {
                Ok(def) => row.def = def,
                Err(e) => {
                    refusal(state, &id, &project, &target, &e.to_string()).await?;
                    continue;
                }
            }
        }
        if !same_location(&definition, &row.def) {
            refusal(
                state,
                &id,
                &project,
                &target,
                "target machine or work_root changed",
            )
            .await?;
            tracing::warn!(%id, %target, "target cleanup refused: definition changed");
            continue;
        }
        let owned = stored.unwrap_or_else(|| base(&row.def).to_owned());
        let measured = match start_cmd(
            state,
            &row.def,
            &command(
                "measure",
                &owned,
                &project,
                &name,
                &id,
                "",
                script_cwd(state, &row.def, &owned).as_deref(),
            )?,
        ) {
            Ok(run) => run.collect(4096).await,
            Err(e) => {
                refusal(state, &id, &project, &target, &e.to_string()).await?;
                continue;
            }
        };
        if !measured.end().success() {
            refusal(state, &id, &project, &target, &measured.stderr_tail()).await?;
            continue;
        }
        let size = match measurement(&measured.stdout) {
            Some(size) => size,
            None => {
                refusal(state, &id, &project, &target, "target measurement returned no size").await?;
                tracing::warn!(%id, %target, "target cleanup deferred: measurement returned no size");
                continue;
            }
        };
        let path = root(&owned, &project, &name)?;
        let (audit_id, audit_project, audit_target, audit_path) =
            (id.clone(), project.clone(), target.clone(), path.clone());
        state.db.write("system", move |t| {
            t.audit("agent.target_cleanup_started", &audit_id, Some(&audit_project),
                serde_json::json!({"agent": audit_id, "target": audit_target, "path": audit_path, "size": size}))
        }).await?;
        let run = match start_cmd(
            state,
            &row.def,
            &command(
                "delete",
                &owned,
                &project,
                &name,
                &id,
                "",
                script_cwd(state, &row.def, &owned).as_deref(),
            )?,
        ) {
            Ok(run) => run,
            Err(e) => {
                refusal(state, &id, &project, &target, &e.to_string()).await?;
                tracing::warn!(%id, %target, "target cleanup deferred: {e}");
                continue;
            }
        };
        let out = run.collect(4096).await;
        if !out.end().success() {
            refusal(state, &id, &project, &target, &out.stderr_tail()).await?;
            tracing::warn!(%id, %target, error = %out.stderr_tail(), "owned target cleanup failed; will retry");
            continue;
        }
        state
            .db
            .write("system", move |t| {
                t.audit(
                    "agent.target_cleanup",
                    &id,
                    Some(&project),
                    serde_json::json!({"agent": id, "target": target, "path": path, "size": size}),
                )?;
                t.execute(
                    "DELETE FROM agent_target_roots WHERE agent=?1 AND target=?2",
                    params![id, target],
                )?;
                Ok(())
            })
            .await?;
    }
    Ok(())
    }).await
}

/// Audit a new refusal once, retaining its state for operator inspection and
/// retry. The started audit keeps the original size even after a partial delete.
async fn refusal(
    state: &Shared,
    id: &str,
    project: &str,
    target: &str,
    error: &str,
) -> ApiResult<()> {
    let (id, project, target, error) = (
        id.to_owned(),
        project.to_owned(),
        target.to_owned(),
        error.to_owned(),
    );
    state.db.write("system", move |t| {
        let changed = t.execute("UPDATE agent_target_roots SET last_error=?3 WHERE agent=?1 AND target=?2 AND (last_error IS NULL OR last_error<>?3)",
            params![id, target, error])?;
        if changed != 0 {
            t.audit("agent.target_cleanup_refused", &id, Some(&project),
                serde_json::json!({"agent": id, "target": target, "error": error}))?;
        }
        Ok(())
    }).await
}

async fn warn_outside(state: &Shared, actor: &Actor, def: &TargetDef, root: &str) -> ApiResult<()> {
    let Some(a) = actor.agent() else {
        return Ok(());
    };
    let (id, target) = (a.id.clone(), def.name.clone());
    let first = state
        .db
        .write("system", move |t| {
            Ok(t.execute(
                "INSERT OR IGNORE INTO agent_target_root_warnings (agent, target) VALUES (?1, ?2)",
                params![id, target],
            )? != 0)
        })
        .await?;
    if first {
        tracing::warn!(agent = %a.id, target = %def.name, %root,
            "work outside the card-agent root is not auto-cleaned; move checkouts, build output and results under the root; keep shared caches outside it");
    }
    Ok(())
}

pub fn spawn(state: Shared) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = sweep(&state).await {
                tracing::warn!("target root sweep failed: {e}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn only_native_windows_commands_skip_owned_linux_roots() {
        use terrarium_core::config::TargetKind;
        assert!(commands_only_host(TargetKind::Host, "windows", true));
        assert!(!commands_only_host(TargetKind::Host, "linux", true));
        assert!(!commands_only_host(TargetKind::Host, "windows", false));
        assert!(!commands_only_host(TargetKind::Ssh, "windows", true));
        assert!(!commands_only_host(TargetKind::Docker, "windows", true));
    }

    /// TER-566: the error of a failed python3 probe says why.
    #[test]
    fn a_failed_python_probe_carries_its_error_output() {
        use crate::executor::{Collected, End};
        let refused = Collected {
            stderr: b"terrarium-host native: refused (Invalid): bad environment variable \"X\"\n"
                .to_vec(),
            end: Some(End::Exit {
                code: Some(1),
                signal: None,
                reason: "exited".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            probe_failure(&refused),
            "cannot check target python3 availability: terrarium-host native: refused (Invalid): bad environment variable \"X\""
        );
        let silent = Collected {
            end: Some(End::Exit {
                code: Some(2),
                signal: None,
                reason: "exited".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            probe_failure(&silent),
            "cannot check target python3 availability: exit code 2"
        );
        let lost = Collected::default();
        assert_eq!(
            probe_failure(&lost),
            "cannot check target python3 availability: the run ended without an end"
        );
    }

    /// The daemon's fixed root scripts run with the operator scope on a
    /// host with native GPU enforcement (they pass, and the helper gives
    /// them the whole workspace root, so their mount check and mkdir work);
    /// they name only the root of the agent they act for. Any other
    /// command with no holder is still refused there.
    #[test]
    fn root_scripts_run_with_the_operator_scope() {
        let mut h: TargetDef = toml::from_str("kind = \"host\"\nhost = \"h\"\n").unwrap();
        h.name = "box".into();
        let caps = terrarium_core::protocol::HostCapabilities {
            native_gpu: Some(terrarium_core::protocol::NativeGpuCapability {
                enforced: true,
                version: 1,
                reason: None,
            }),
            ..Default::default()
        };
        let ws = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws.path()).unwrap();
        let mut scripts: Vec<RemoteCmd> = ["prepare", "measure", "delete"]
            .into_iter()
            .map(|mode| command(mode, "/srv/roots", "terra", "x", "agent-x", "", None).unwrap())
            .collect();
        for cmd in &scripts {
            assert!(cmd.argv.iter().any(|a| a == "x"));
            assert!(!cmd.argv.iter().any(|a| a == "y" || a.contains("/y")));
        }
        scripts.push(root_script(
            vec!["sh".into(), "-c".into(), PYTHON_PROBE.into()],
            30,
            None,
        ));
        scripts.push(root_script(
            vec!["python3".into(), "-c".into(), HOME_PROBE.into()],
            30,
            None,
        ));
        for cmd in &scripts {
            assert!(cmd.operator && cmd.holder.is_none(), "{cmd:?}");
            let l = crate::executor::native_gpu_limits(
                &h,
                Some(&caps),
                cmd.holder.as_deref(),
                cmd.operator,
                None,
            )
            .unwrap()
            .unwrap();
            assert_eq!(l.holder, "operator");
            assert_eq!(
                terrarium_host::native::holder_workspace(&ws, &l.holder).unwrap(),
                Some(ws.clone())
            );
        }
        // A command that is no root script, with no holder: refused.
        let bare = RemoteCmd {
            argv: vec!["true".into()],
            ..RemoteCmd::default()
        };
        let e = crate::executor::native_gpu_limits(
            &h,
            Some(&caps),
            bare.holder.as_deref(),
            bare.operator,
            None,
        )
        .unwrap_err();
        assert!(e.to_string().contains("has no holder"), "{e}");
    }

    /// TER-615: on a host with native GPU enforcement every root script
    /// (the probes, prepare, measure, delete) runs in the owned-root base,
    /// so the helper takes it even though the host agent's own `work_dir`
    /// (`/work` on helios) is outside its `workspace_root`. The path is the
    /// real one: the daemon frame, the host agent's exec plan, the
    /// `start_request` that fills a missing cwd with `work_dir`, and the
    /// helper's `plan_start`. Without the cwd the same command is refused
    /// as on helios. Elsewhere the scripts keep no cwd.
    #[test]
    fn root_scripts_on_a_native_host_run_in_the_base_under_the_workspace_root() {
        use terrarium_core::protocol::{HostCapabilities, NativeGpuCapability};
        use terrarium_host::native::{Ctx, HostDevices, Identity, plan_start, start_request};
        let ws = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws.path()).unwrap();
        let base = ws.to_str().unwrap().to_owned();
        let mut h: TargetDef = toml::from_str("kind = \"host\"\nhost = \"h\"\n").unwrap();
        h.name = "box".into();
        h.work_root = Some(base.clone());
        let caps = HostCapabilities {
            native_gpu: Some(NativeGpuCapability {
                enforced: true,
                version: 1,
                reason: None,
            }),
            ..Default::default()
        };
        let cwd = script_cwd_for(&h, Some(&caps), &base);
        assert_eq!(cwd.as_deref(), Some(base.as_str()));
        // No native enforcement, or not a host target: no cwd, as before.
        assert_eq!(script_cwd_for(&h, None, &base), None);
        assert_eq!(
            script_cwd_for(&h, Some(&HostCapabilities::default()), &base),
            None
        );
        let mut d = h.clone();
        d.kind = terrarium_core::config::TargetKind::Docker;
        assert_eq!(script_cwd_for(&d, Some(&caps), &base), None);

        let ctx = Ctx {
            workspace_root: ws.clone(),
            state_dir: "/run/terrarium-host-helper/sessions".into(),
            dev_dir: "/dev".into(),
            support_nodes: vec![],
        };
        let workload = Identity {
            name: String::new(),
            uid: 2001,
            gid: 2001,
        };
        // The host agent's work_dir, outside the workspace root (`/work`
        // on helios; a directory that exists on every test machine here).
        let work = tempfile::tempdir().unwrap();
        let host_work_dir = std::fs::canonicalize(work.path()).unwrap();
        let host_work_dir = host_work_dir.as_path();
        assert!(!host_work_dir.starts_with(&ws));
        let helper = |cmd: &RemoteCmd| {
            let l = crate::executor::native_gpu_limits(
                &h,
                Some(&caps),
                cmd.holder.as_deref(),
                cmd.operator,
                None,
            )
            .unwrap();
            let terrarium_host::protocol::DaemonFrame::Exec(req) =
                crate::executor::host_frame(&h, None, cmd, l).unwrap()(1)
            else {
                panic!("not an exec frame");
            };
            let plan = terrarium_host::verb::plan_exec(&req, &Default::default()).unwrap();
            let start = start_request(&plan.exec, 1, req.limits.as_ref(), host_work_dir);
            assert_eq!(start.holder, "operator");
            plan_start(&ctx, &start, &[], &HostDevices, 1, &workload).map(|_| start.cwd)
        };
        let mut scripts: Vec<RemoteCmd> = ["prepare", "measure", "delete"]
            .into_iter()
            .map(|mode| command(mode, &base, "terra", "x", "agent-x", "", cwd.as_deref()).unwrap())
            .collect();
        scripts.push(root_script(
            vec!["sh".into(), "-c".into(), PYTHON_PROBE.into()],
            30,
            cwd.as_deref(),
        ));
        scripts.push(root_script(
            vec!["python3".into(), "-c".into(), HOME_PROBE.into()],
            30,
            cwd.as_deref(),
        ));
        for cmd in &scripts {
            assert_eq!(helper(cmd).unwrap(), base, "{cmd:?}");
            // The same script with no cwd: the helper gets the host
            // agent's work_dir and refuses it, as on helios.
            let bare = RemoteCmd {
                cwd: None,
                ..cmd.clone()
            };
            let e = helper(&bare).unwrap_err();
            assert!(
                e.message.contains(&format!(
                    "{} is not under the workspace root",
                    host_work_dir.display()
                )),
                "{e:?}"
            );
        }
        // The helper takes the workspace root itself, but nothing outside
        // it: `/` and the host agent's work_dir stay refused.
        for outside in ["/", host_work_dir.to_str().unwrap()] {
            let cmd = RemoteCmd {
                cwd: Some(outside.into()),
                ..scripts[0].clone()
            };
            let e = helper(&cmd).unwrap_err();
            assert!(e.message.contains("not under the workspace root"), "{e:?}");
        }
    }

    /// TER-634: a card agent's exec with no cwd on a native host target
    /// runs in its owned root `<work_root>/<project>/<agent>` (here
    /// `work_root` = `workspace_root`, as on helios). Through the real path
    /// (daemon frame, host exec plan, `start_request` with the host
    /// agent's `work_dir`, the helper's `plan_start`): no cwd gives the
    /// root, a cwd inside it is kept and passes, a cwd outside the
    /// workspace root is kept and refused with a message that names the
    /// workspace root. Elsewhere a missing cwd stays missing.
    #[test]
    fn a_card_agent_exec_on_a_native_host_defaults_to_its_owned_root() {
        use terrarium_core::protocol::{HostCapabilities, NativeGpuCapability};
        use terrarium_host::native::{Ctx, HostDevices, Identity, plan_start, start_request};
        let ws = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws.path()).unwrap();
        let base = ws.to_str().unwrap().to_owned();
        let mut h: TargetDef = toml::from_str("kind = \"host\"\nhost = \"h\"\n").unwrap();
        h.name = "box".into();
        h.work_root = Some(base.clone());
        let caps = HostCapabilities {
            native_gpu: Some(NativeGpuCapability {
                enforced: true,
                version: 1,
                reason: None,
            }),
            ..Default::default()
        };
        // The root that prepare makes, with a directory in it.
        let owned = root(&base, "terra", "x").unwrap();
        let sub = format!("{owned}/repo");
        std::fs::create_dir_all(&sub).unwrap();
        let work = tempfile::tempdir().unwrap();
        let host_work_dir = std::fs::canonicalize(work.path()).unwrap();
        assert!(!host_work_dir.starts_with(&ws));
        let ctx = Ctx {
            workspace_root: ws.clone(),
            state_dir: "/run/terrarium-host-helper/sessions".into(),
            dev_dir: "/dev".into(),
            support_nodes: vec![],
        };
        let workload = Identity {
            name: String::new(),
            uid: 2001,
            gid: 2001,
        };
        let card_cmd = |cwd: Option<&str>| {
            let mut cmd = RemoteCmd {
                argv: vec!["sh".into(), "-c".into(), "id; pwd".into()],
                cwd: cwd.map(str::to_owned),
                holder: Some("terra/x".into()),
                ..RemoteCmd::default()
            };
            default_exec_cwd(&h, Some(&caps), &owned, &mut cmd);
            cmd
        };
        let helper = |cmd: &RemoteCmd| {
            let l = crate::executor::native_gpu_limits(
                &h,
                Some(&caps),
                cmd.holder.as_deref(),
                cmd.operator,
                None,
            )
            .unwrap();
            let terrarium_host::protocol::DaemonFrame::Exec(req) =
                crate::executor::host_frame(&h, None, cmd, l).unwrap()(1)
            else {
                panic!("not an exec frame");
            };
            let plan = terrarium_host::verb::plan_exec(&req, &Default::default()).unwrap();
            let start = start_request(&plan.exec, 1, req.limits.as_ref(), &host_work_dir);
            assert_eq!(start.holder, "terra/x");
            plan_start(&ctx, &start, &[], &HostDevices, 1, &workload).map(|_| start.cwd)
        };
        // No cwd: the owned root.
        let none = card_cmd(None);
        assert_eq!(none.cwd.as_deref(), Some(owned.as_str()));
        assert_eq!(helper(&none).unwrap(), owned);
        // A cwd inside the root: kept, and the helper takes it.
        let inside = card_cmd(Some(&sub));
        assert_eq!(inside.cwd.as_deref(), Some(sub.as_str()));
        assert_eq!(helper(&inside).unwrap(), sub);
        // A cwd outside the workspace root: kept, and refused with the
        // allowed base named.
        let out = host_work_dir.to_str().unwrap();
        let outside = card_cmd(Some(out));
        assert_eq!(outside.cwd.as_deref(), Some(out));
        let e = helper(&outside).unwrap_err();
        assert!(
            e.message
                .contains(&format!("is not under the workspace root {base}")),
            "{e:?}"
        );
        // Without native enforcement, or on another kind: no default.
        let mut cmd = RemoteCmd::default();
        default_exec_cwd(&h, None, &owned, &mut cmd);
        assert_eq!(cmd.cwd, None);
        default_exec_cwd(&h, Some(&HostCapabilities::default()), &owned, &mut cmd);
        assert_eq!(cmd.cwd, None);
        let mut d = h.clone();
        d.kind = terrarium_core::config::TargetKind::Docker;
        default_exec_cwd(&d, Some(&caps), &owned, &mut cmd);
        assert_eq!(cmd.cwd, None);
    }

    /// The owned root as the agent's own directory under the workspace root
    /// (`work_root` = `workspace_root`): prepare creates `<ws>/<p>/<a>`
    /// with the owner marker, and measure and delete pass the mount check,
    /// since the root and its parent share a mount in the operator scope.
    #[test]
    fn the_owned_root_can_be_the_agents_directory_under_the_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        // The workspace root with the project directory of the host agent.
        std::fs::create_dir_all(tmp.path().join("ws/p")).unwrap();
        assert_eq!(run_code_at(tmp.path(), "/ws", "prepare", "repo"), Some(0));
        let owned = tmp.path().join("ws/p/a");
        assert_eq!(
            std::fs::read_to_string(owned.join(".terrarium-owner")).unwrap(),
            "agent-id"
        );
        assert!(owned.join("repo").is_dir());
        assert_eq!(run_code_at(tmp.path(), "/ws", "measure", ""), Some(0));
        assert_eq!(run_code_at(tmp.path(), "/ws", "delete", ""), Some(0));
        assert!(!owned.exists());
        assert!(tmp.path().join("ws/p").is_dir());
    }

    #[test]
    fn missing_python_probe_preserves_legacy_execution() {
        let empty = tempfile::tempdir().unwrap();
        let out = Command::new("/bin/sh")
            .args(["-c", PYTHON_PROBE])
            .env("PATH", empty.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"missing-python3");
        let out = Command::new("/bin/sh")
            .args(["-c", PYTHON_PROBE])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn measurements_reject_noise_and_commands_have_finite_bounds() {
        assert_eq!(measurement(b"123\n"), Some(123));
        assert_eq!(measurement(b"0"), Some(0));
        for bad in [
            b"".as_slice(),
            b"banner\n123",
            b"-1",
            b"18446744073709551616",
            b"\xff",
        ] {
            assert_eq!(measurement(bad), None);
        }
        for mode in ["prepare", "measure", "delete"] {
            let cmd = command(mode, "/src/terrarium", "p", "a", "id", "", None).unwrap();
            assert_eq!(
                cmd.timeout_secs,
                Some(if mode == "prepare" { 120 } else { 900 })
            );
        }
    }

    #[test]
    fn location_ignores_unrelated_edits() {
        let mut def: TargetDef = toml::from_str("kind = 'ssh'\naddress = 'user@box'\n").unwrap();
        let saved = serde_json::to_string(&def).unwrap();
        def.keep = true;
        def.snapshot_before_task = true;
        def.credential = Some("rotated-key".into());
        def.display_name = Some(":2".into());
        def.work_root = Some("/src/terrarium".into());
        assert!(same_location(&saved, &def));
        def.address = Some("user@other".into());
        assert!(!same_location(&saved, &def));
        def.address = Some("user@box".into());
        def.work_root = Some("/var/build".into());
        assert!(!same_location(&saved, &def));
    }

    #[test]
    fn swapped_root_is_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(run(tmp.path(), "prepare", "repo"));
        let owned = tmp.path().join("src/terrarium/p/a");
        let injection = "os.rename(name, name + '.old', src_dir_fd=parent, dst_dir_fd=parent)\n            os.mkdir(name, dir_fd=parent)\n            replacement = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)\n            keep = os.open('keep', os.O_WRONLY | os.O_CREAT, 0o600, dir_fd=replacement)\n            os.close(keep)\n            os.close(replacement)\n            walk(fd, True, meta.st_dev, mount_id(fd), keep_owner=True)";
        let script = SCRIPT.replace(
            "walk(fd, True, meta.st_dev, mount_id(fd), keep_owner=True)",
            injection,
        );
        let script = script.replace(
            "os.open('/',",
            &format!("os.open({:?},", tmp.path().to_str().unwrap()),
        );
        let out = Command::new("python3")
            .args([
                "-c",
                &script,
                "delete",
                "/src/terrarium",
                "p",
                "a",
                "agent-id",
                "",
            ])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(owned.join("keep").exists());
        assert!(
            owned
                .with_file_name("a.old")
                .join(".terrarium-owner")
                .exists()
        );
    }

    #[test]
    fn mounted_subtree_is_refused_before_any_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(run(tmp.path(), "prepare", "repo"));
        let owned = tmp.path().join("src/terrarium/p/a");
        std::fs::create_dir(owned.join("mounted")).unwrap();
        std::fs::write(owned.join("mounted/keep"), "mounted data").unwrap();
        // Model a bind mount with the same device but a different mount ID.
        let script = SCRIPT.replace("if mount_id(child) != mount:",
            "if os.readlink('/proc/self/fd/' + str(child)).endswith('/mounted') or mount_id(child) != mount:");
        let script = script.replace(
            "os.open('/',",
            &format!("os.open({:?},", tmp.path().to_str().unwrap()),
        );
        let out = Command::new("python3")
            .args([
                "-c",
                &script,
                "delete",
                "/src/terrarium",
                "p",
                "a",
                "agent-id",
                "",
            ])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(owned.join("mounted/keep").exists());
        assert!(owned.join(".terrarium-owner").exists());
        assert!(owned.join("repo").exists());
    }

    #[test]
    fn paths_are_strictly_owned() {
        let r = root("/src/terrarium", "p", "a").unwrap();
        for bad in [
            "",
            "/",
            "/src",
            "relative",
            &r,
            "/src/terrarium/p/ab/x",
            "/src/terrarium/p/a/../b",
            "/src/terrarium/p/a/./x",
            "/src/terrarium/p/a//x",
        ] {
            assert!(!inside(&r, bad), "{bad}");
        }
        assert!(inside(&r, &format!("{r}/repo/target")));
        for bad in ["", "..", "/", "a/b"] {
            assert!(root("/src/terrarium", bad, "a").is_err());
        }
        for bad in ["", "/", "relative", "/src/../shared", "/src//x", "/src/."] {
            assert!(check_base(bad).is_err());
        }
        assert_eq!(root("/var/build", "p", "a").unwrap(), "/var/build/p/a");
        let mut cmd = RemoteCmd::default();
        build_env(&r, &mut cmd).unwrap();
        assert_eq!(cmd.env["CARGO_TARGET_DIR"], format!("{r}/target"));
        cmd.env.insert("RESULTS_DIR".into(), "/tmp/shared".into());
        assert!(build_env(&r, &mut cmd).is_err());
    }

    fn run(base: &std::path::Path, mode: &str, dest: &str) -> bool {
        run_code(base, mode, dest) == Some(0)
    }

    fn run_code(base: &std::path::Path, mode: &str, dest: &str) -> Option<i32> {
        run_code_at(base, "/src/terrarium", mode, dest)
    }

    /// [`run_code`] with another owned-root base (`work_root`) in the tree.
    fn run_code_at(base: &std::path::Path, work_root: &str, mode: &str, dest: &str) -> Option<i32> {
        // Exercise the exact target program in an isolated tree, not /src.
        let script = SCRIPT.replace(
            "os.open('/',",
            &format!("os.open({:?},", base.to_str().unwrap()),
        );
        Command::new("python3")
            .args(["-c", &script, mode, work_root, "p", "a", "agent-id", dest])
            .output()
            .unwrap()
            .status
            .code()
    }

    #[test]
    fn unwritable_base_exits_unavailable_and_nothing_is_made() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores the mode: then there is nothing to prove here.
        if std::fs::create_dir(src.join("probe")).is_ok() {
            return;
        }
        assert_eq!(run_code(tmp.path(), "prepare", ""), Some(BASE_UNAVAILABLE));
        assert!(!src.join("terrarium").exists());
        // Measure and delete of a root that cannot exist report nothing.
        assert!(run(tmp.path(), "delete", ""));
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(run(tmp.path(), "prepare", "repo"));
    }

    /// TER-498 review: ancestors need search permission only. Root ignores
    /// modes, so run this one as a non-root user to prove it.
    #[test]
    fn search_only_ancestor_is_enough() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("terrarium")).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o111)).unwrap();
        let readable = std::fs::read_dir(&src).is_ok();
        let code = run_code(tmp.path(), "prepare", "repo");
        let owned = tmp.path().join("src/terrarium/p/a");
        let made = owned.join("repo").is_dir();
        let deleted = run(tmp.path(), "delete", "") && !owned.exists();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            eprintln!("search_only_ancestor_is_enough: running as root, modes are not enforced");
        }
        assert_eq!(code, Some(0));
        assert!(made);
        assert!(deleted);
    }

    #[test]
    fn fallback_base_is_under_home_and_checked() {
        assert_eq!(
            fallback_base("/home/agent").unwrap(),
            "/home/agent/.terrarium-work"
        );
        assert_eq!(
            fallback_base("/home/agent/").unwrap(),
            "/home/agent/.terrarium-work"
        );
        assert_eq!(fallback_base("/").unwrap(), "/.terrarium-work");
        for bad in ["", "relative", "/home/../root", "~"] {
            assert!(fallback_base(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn prepare_errors_name_work_root() {
        let mut def: TargetDef = toml::from_str("kind = 'ssh'\naddress = 'user@box'\n").unwrap();
        def.name = "box".into();
        let msg = |e: ApiError| e.to_string();
        let none = msg(prepare_error(
            &def,
            "/home/u/.terrarium-work",
            true,
            "denied",
        ));
        assert!(none.contains("has no work_root"), "{none}");
        assert!(none.contains(DEFAULT_BASE), "{none}");
        assert!(none.contains("/home/u/.terrarium-work"), "{none}");
        assert!(
            none.contains("terrarium admin target edit box --work-root"),
            "{none}"
        );
        def.work_root = Some("/var/build".into());
        let set = msg(prepare_error(&def, "/var/build", true, "denied"));
        assert!(set.contains("work_root /var/build"), "{set}");
        assert!(set.contains("--work-root"), "{set}");
        let other = msg(prepare_error(&def, "/var/build", false, "owner"));
        assert!(
            other.contains("cannot prepare owned target root under /var/build: owner"),
            "{other}"
        );
    }

    /// TER-635: on a native host the units run with UMask=0007 under the
    /// setgid workspace root, and the sessions of one holder can run as
    /// different pool UIDs of the group terrarium-ws. Prepare must leave
    /// the project dir and the holder dir setgid with group rwx, and the
    /// owner marker group-readable, so that any pool UID of the holder
    /// can write the root and a prepare or sweep as another pool UID can
    /// check the owner.
    #[test]
    fn prepare_under_a_setgid_root_with_umask_0007_leaves_group_access() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("src/terrarium");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o2775)).unwrap();
        let script = SCRIPT.replace(
            "os.open('/',",
            &format!("os.open({:?},", tmp.path().to_str().unwrap()),
        );
        let out = Command::new("sh")
            .args(["-c", "umask 0007 && exec python3 -c \"$0\" \"$@\""])
            .args([
                &script,
                "prepare",
                "/src/terrarium",
                "p",
                "a",
                "agent-id",
                "repo/x",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mode =
            |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        for dir in ["p", "p/a", "p/a/repo", "p/a/repo/x"] {
            assert_eq!(mode(&work.join(dir)), 0o2770, "{dir}");
        }
        assert_eq!(mode(&work.join("p/a/.terrarium-owner")), 0o640);
    }

    #[test]
    fn teardown_removes_only_owned_tree_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(run(tmp.path(), "prepare", "repo"));
        let owned = tmp.path().join("src/terrarium/p/a");
        std::fs::create_dir_all(owned.join("target/debug")).unwrap();
        std::fs::write(owned.join("target/debug/output"), "build").unwrap();
        let shared = tmp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("keep"), "data").unwrap();
        std::os::unix::fs::symlink(&shared, owned.join("link")).unwrap();
        assert!(run(tmp.path(), "delete", ""));
        assert!(!owned.exists());
        assert!(shared.join("keep").exists());
        assert!(run(tmp.path(), "delete", ""));
    }

    #[test]
    fn refuses_symlink_ancestors_roots_and_destinations() {
        for component in [
            "src",
            "src/terrarium",
            "src/terrarium/p",
            "src/terrarium/p/a",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let link = tmp.path().join(component);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            // A symlink fails closed: never the unavailable code, so no fallback.
            let code = run_code(tmp.path(), "prepare", "repo");
            assert!(
                code != Some(0) && code != Some(BASE_UNAVAILABLE),
                "{code:?}"
            );
            assert!(!run(tmp.path(), "delete", ""));
            assert!(link.is_symlink());
        }
        let tmp = tempfile::tempdir().unwrap();
        assert!(run(tmp.path(), "prepare", "repo"));
        let owned = tmp.path().join("src/terrarium/p/a");
        std::os::unix::fs::symlink("/tmp", owned.join("escape")).unwrap();
        assert!(!run(tmp.path(), "prepare", "escape/repo"));
        std::fs::write(owned.join(".terrarium-owner"), "another-agent").unwrap();
        assert!(!run(tmp.path(), "delete", ""));
        assert!(owned.exists());
    }

    #[test]
    fn refuses_preexisting_unowned_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/terrarium/p/a")).unwrap();
        let code = run_code(tmp.path(), "prepare", "repo");
        assert!(
            code != Some(0) && code != Some(BASE_UNAVAILABLE),
            "{code:?}"
        );
        assert!(!run(tmp.path(), "delete", ""));
    }
}
