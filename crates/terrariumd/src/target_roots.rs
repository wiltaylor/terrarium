//! Owned build roots for card agents. Legacy and shared paths are never swept.
use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{RemoteCmd, start_cmd};
use crate::store::targets;
use rusqlite::params;
use terrarium_core::config::TargetDef;

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

fn base(def: &TargetDef) -> &str {
    def.work_root.as_deref().unwrap_or("/src/terrarium")
}

/// Only machine/path identity matters. Credentials and limits may be rotated
/// without stranding roots or preventing live agents from starting work.
fn same_location(old: &str, def: &TargetDef) -> bool {
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
const SCRIPT: &str = r#"
import os, stat, sys
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
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    fds.append(fd)
    for part in base.split('/')[1:] + [project]:
        if mode == 'prepare':
            try: os.mkdir(part, dir_fd=fd)
            except FileExistsError: pass
        try: fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
        except FileNotFoundError:
            if mode != 'prepare':
                print(0)
                sys.exit(0)
            raise
        fds.append(fd)
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
        marker = os.open('.terrarium-owner', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
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
    if mode in ('measure', 'delete'):
        meta = os.fstat(fd)
        if mount_id(fd) != mount_id(parent): raise RuntimeError('refusing mounted agent root')
        # First check the complete tree. In particular, refuse mounts before
        # removing the owner marker so a refused cleanup stays retryable.
        size = walk(fd, False, meta.st_dev, mount_id(fd))
        if mode == 'delete':
            walk(fd, True, meta.st_dev, mount_id(fd), keep_owner=True)
            current = os.stat(name, dir_fd=parent, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != (meta.st_dev, meta.st_ino):
                raise RuntimeError('agent root changed during cleanup')
            os.unlink('.terrarium-owner', dir_fd=fd)
            try: os.rmdir(name, dir_fd=parent)
            except OSError:
                marker = os.open('.terrarium-owner', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
                with os.fdopen(marker, 'w') as out: out.write(agent)
                raise
        print(size)
finally:
    for fd in reversed(fds): os.close(fd)
"#;

fn command(
    mode: &str,
    base: &str,
    project: &str,
    name: &str,
    agent: &str,
    dest: &str,
) -> ApiResult<RemoteCmd> {
    root(base, project, name)?;
    Ok(RemoteCmd {
        argv: [
            "python3", "-c", SCRIPT, mode, base, project, name, agent, dest,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        timeout_secs: Some(if mode == "delete" { 900 } else { 120 }),
        ..RemoteCmd::default()
    })
}

/// Record before creating anything, so a crash cannot orphan a new root.
/// Only bound card agents get this contract. Other callers retain their paths.
pub async fn prepare(
    state: &Shared,
    actor: &Actor,
    def: &TargetDef,
    dest: Option<&str>,
) -> ApiResult<Option<String>> {
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
    let path = root(base(def), &a.project, &a.name)?;
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
    let out = start_cmd(
        state,
        def,
        &command("prepare", base(def), &a.project, &a.name, &a.id, relative)?,
    )?
    .collect(4096)
    .await;
    if !out.end().success() {
        return Err(ApiError::invalid(format!(
            "cannot prepare owned target root: {}",
            out.stderr_tail()
        )));
    }
    Ok(Some(path))
}

pub async fn prepare_exec(
    state: &Shared,
    actor: &Actor,
    def: &TargetDef,
    cmd: &mut RemoteCmd,
) -> ApiResult<()> {
    let Some(a) = actor.agent() else {
        return Ok(());
    };
    let r = root(base(def), &a.project, &a.name)?;
    let paths = [
        cmd.cwd.as_deref(),
        cmd.env.get("CARGO_TARGET_DIR").map(String::as_str),
        cmd.env.get("RESULTS_DIR").map(String::as_str),
    ];
    let outside = paths.into_iter().flatten().find(|p| !inside(&r, p));
    if let Some(r) = prepare(state, actor, def, outside).await? {
        build_env(&r, cmd)?;
    }
    Ok(())
}

/// Retry persisted roots after close or restart. Never kill a run to free a root.
pub async fn sweep(state: &Shared) -> ApiResult<()> {
    let rows = state
        .db
        .read(|c| {
            let mut q = c.prepare(
                "SELECT r.agent, r.project, r.name, r.target, r.definition
            FROM agent_target_roots r WHERE NOT EXISTS (SELECT 1 FROM agents a WHERE a.id=r.agent)",
            )?;
            Ok(q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    for (id, project, name, target, definition) in rows {
        let _guard = lock(&format!("{id}:{target}")).await;
        if state.executor.live_of(&id) != 0 {
            continue;
        }
        let key = target.clone();
        let row = match state.db.read(move |c| targets::get(c, &key)).await {
            Ok(row) => row,
            Err(e) => {
                refusal(state, &id, &project, &target, &e.to_string()).await?;
                tracing::warn!(%id, %target, "target cleanup deferred: {e}");
                continue;
            }
        };
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
        let measured = match start_cmd(
            state,
            &row.def,
            &command("measure", base(&row.def), &project, &name, &id, "")?,
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
        let size = String::from_utf8_lossy(&measured.stdout)
            .trim()
            .parse::<u64>()
            .map_err(|_| ApiError::internal("target measurement returned no size"))?;
        let path = root(base(&row.def), &project, &name)?;
        let (audit_id, audit_project, audit_target, audit_path) =
            (id.clone(), project.clone(), target.clone(), path.clone());
        state.db.write("system", move |t| {
            t.audit("agent.target_cleanup_started", &audit_id, Some(&audit_project),
                serde_json::json!({"agent": audit_id, "target": audit_target, "path": audit_path, "size": size}))
        }).await?;
        let run = match start_cmd(
            state,
            &row.def,
            &command("delete", base(&row.def), &project, &name, &id, "")?,
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
        let script = SCRIPT.replace("walk(fd, True, meta.st_dev, mount_id(fd), keep_owner=True)", injection);
        let script = script.replace("os.open('/',", &format!("os.open({:?},", tmp.path().to_str().unwrap()));
        let out = Command::new("python3").args(["-c", &script, "delete", "/src/terrarium", "p", "a", "agent-id", ""])
            .output().unwrap();
        assert!(!out.status.success());
        assert!(owned.join("keep").exists());
        assert!(owned.with_file_name("a.old").join(".terrarium-owner").exists());
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
        // Exercise the exact target program in an isolated tree, not /src.
        let script = SCRIPT.replace(
            "os.open('/',",
            &format!("os.open({:?},", base.to_str().unwrap()),
        );
        Command::new("python3")
            .args([
                "-c",
                &script,
                mode,
                "/src/terrarium",
                "p",
                "a",
                "agent-id",
                dest,
            ])
            .output()
            .unwrap()
            .status
            .success()
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
            assert!(!run(tmp.path(), "prepare", "repo"));
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
        assert!(!run(tmp.path(), "prepare", "repo"));
        assert!(!run(tmp.path(), "delete", ""));
    }
}
