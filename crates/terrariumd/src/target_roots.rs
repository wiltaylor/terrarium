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
        let mut locks = LOCKS.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, v| v.strong_count() != 0);
        let mutex = locks.get(agent).and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        locks.insert(agent.to_owned(), Arc::downgrade(&mutex));
        mutex
    };
    mutex.lock_owned().await
}

pub fn check_base(base: &str) -> ApiResult<()> {
    if !base.starts_with('/') || base.split('/').skip(1)
        .any(|c| matches!(c, "" | "." | "..") || c.chars().any(char::is_control)) {
        return Err(ApiError::invalid("work_root must be an absolute non-root path without empty, dot or parent components"));
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

fn inside(root: &str, path: &str) -> bool {
    path.starts_with(&format!("{root}/"))
        && path.split('/').skip(1).all(|c| !matches!(c, "" | "." | ".."))
}

pub fn build_env(root: &str, cmd: &mut RemoteCmd) -> ApiResult<()> {
    for (key, suffix) in [("CARGO_TARGET_DIR", "target"), ("RESULTS_DIR", "results")] {
        let value = cmd.env.entry(key.into()).or_insert(format!("{root}/{suffix}"));
        if !inside(root, value) {
            return Err(ApiError::invalid(format!("{key} must be strictly under {root}")));
        }
    }
    Ok(())
}

// All traversal uses directory FDs and O_NOFOLLOW, including rmtree's
// symlink-resistant implementation. Never resolve a link and delete its target.
const SCRIPT: &str = r#"
import os, shutil, sys
mode, base, project, name, agent, dest = sys.argv[1:]
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
            if mode == 'delete': sys.exit(0)
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
        if mode == 'delete': sys.exit(0)
        raise
    fds.append(fd)
    flags = os.O_RDONLY | os.O_NOFOLLOW
    if created:
        marker = os.open('.terrarium-owner', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
        with os.fdopen(marker, 'w') as out: out.write(agent)
    marker = os.open('.terrarium-owner', flags, dir_fd=fd)
    with os.fdopen(marker) as inp:
        if inp.read(128) != agent: raise RuntimeError('foreign target root')
    if mode == 'prepare' and dest:
        for part in dest.split('/'):
            if part in ('', '.', '..'): raise RuntimeError('unsafe checkout path')
            try: os.mkdir(part, dir_fd=fd)
            except FileExistsError: pass
            fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            fds.append(fd)
    if mode == 'delete':
        if not shutil.rmtree.avoids_symlink_attacks: raise RuntimeError('unsafe rmtree implementation')
        size = 0
        for _, dirs, files, walkfd in os.fwalk('.', dir_fd=fd, follow_symlinks=False):
            for item in dirs + files:
                size += os.stat(item, dir_fd=walkfd, follow_symlinks=False).st_blocks * 512
        shutil.rmtree(name, dir_fd=parent)
        print(size)
finally:
    for fd in reversed(fds): os.close(fd)
"#;

fn command(mode: &str, base: &str, project: &str, name: &str, agent: &str, dest: &str) -> ApiResult<RemoteCmd> {
    root(base, project, name)?;
    Ok(RemoteCmd {
        argv: ["python3", "-c", SCRIPT, mode, base, project, name, agent, dest]
            .into_iter().map(str::to_owned).collect(),
        timeout_secs: Some(120),
        ..RemoteCmd::default()
    })
}

/// Record before creating anything, so a crash cannot orphan a new root.
/// Only bound card agents get this contract. Other callers retain their paths.
pub async fn prepare(
    state: &Shared, actor: &Actor, def: &TargetDef, dest: Option<&str>,
) -> ApiResult<Option<String>> {
    let Some(a) = actor.agent() else { return Ok(None) };
    let id = a.id.clone();
    let card = state.db.read(move |c| {
        use rusqlite::OptionalExtension;
        let status = c.query_row("SELECT bound_kind, finished_at FROM spawns WHERE id=?1", [&id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))).optional()?;
        match status {
            Some((Some(kind), finished)) if kind == "card" => {
                if finished.is_some() { return Err(ApiError::conflict("card agent already finished")); }
                Ok(true)
            }
            _ => Ok(false),
        }
    }).await?;
    if !card || def.kind == terrarium_core::config::TargetKind::Vmlab { return Ok(None); }
    let path = root(base(def), &a.project, &a.name)?;
    if dest.is_some_and(|d| !inside(&path, d)) {
        warn_outside(state, actor, def, &path).await?;
        return Ok(None);
    }
    let (id, project, name, target) = (a.id.clone(), a.project.clone(), a.name.clone(), def.name.clone());
    let definition = serde_json::to_string(def).map_err(|e| ApiError::internal(e.to_string()))?;
    state.db.write("system", move |t| {
        t.execute("INSERT INTO agent_target_roots (agent, project, name, target, definition)
                   VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(agent, target) DO NOTHING",
                  params![id, project, name, target, definition])?;
        let old: String = t.query_row("SELECT definition FROM agent_target_roots WHERE agent=?1 AND target=?2",
                                     params![id, target], |r| r.get(0))?;
        if old != definition { return Err(ApiError::conflict("owned target definition changed")); }
        Ok(())
    }).await?;
    let relative = if dest.is_some() { dest.map(|d| &d[path.len() + 1..]).unwrap_or_default() } else { "" };
    let out = start_cmd(state, def, &command("prepare", base(def), &a.project, &a.name, &a.id, relative)?)?
        .collect(4096).await;
    if !out.end().success() {
        return Err(ApiError::invalid(format!("cannot prepare owned target root: {}", out.stderr_tail())));
    }
    Ok(Some(path))
}

pub async fn prepare_exec(state: &Shared, actor: &Actor, def: &TargetDef, cmd: &mut RemoteCmd) -> ApiResult<()> {
    let Some(a) = actor.agent() else { return Ok(()) };
    let r = root(base(def), &a.project, &a.name)?;
    let paths = [cmd.cwd.as_deref(), cmd.env.get("CARGO_TARGET_DIR").map(String::as_str),
                 cmd.env.get("RESULTS_DIR").map(String::as_str)];
    let outside = paths.into_iter().flatten().find(|p| !inside(&r, p));
    if let Some(r) = prepare(state, actor, def, outside).await? {
        build_env(&r, cmd)?;
    }
    Ok(())
}

/// Retry persisted roots after close or restart. Never kill a run to free a root.
pub async fn sweep(state: &Shared) -> ApiResult<()> {
    let rows = state.db.read(|c| {
        let mut q = c.prepare("SELECT r.agent, r.project, r.name, r.target, r.definition
            FROM agent_target_roots r WHERE NOT EXISTS (SELECT 1 FROM agents a WHERE a.id=r.agent)")?;
        Ok(q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
            r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?
            .collect::<Result<Vec<_>, _>>()?)
    }).await?;
    for (id, project, name, target, definition) in rows {
        let _guard = lock(&id).await;
        if state.executor.live_of(&id) != 0 { continue; }
        let key = target.clone();
        let row = state.db.read(move |c| targets::get(c, &key)).await?;
        if serde_json::to_string(&row.def).ok().as_deref() != Some(&definition) {
            tracing::warn!(%id, %target, "target cleanup refused: definition changed");
            continue;
        }
        let out = start_cmd(state, &row.def, &command("delete", base(&row.def), &project, &name, &id, "")?)?
            .collect(4096).await;
        if !out.end().success() {
            tracing::warn!(%id, %target, error = %out.stderr_tail(), "owned target cleanup failed; will retry");
            continue;
        }
        let path = root(base(&row.def), &project, &name)?;
        let size = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap_or(0);
        state.db.write("system", move |t| {
            t.audit("agent.target_cleanup", &id, Some(&project),
                serde_json::json!({"agent": id, "target": target, "path": path, "size": size}))?;
            t.execute("DELETE FROM agent_target_roots WHERE agent=?1 AND target=?2", params![id, target])?;
            Ok(())
        }).await?;
    }
    Ok(())
}

async fn warn_outside(state: &Shared, actor: &Actor, def: &TargetDef, root: &str) -> ApiResult<()> {
    let Some(a) = actor.agent() else { return Ok(()) };
    let (id, target) = (a.id.clone(), def.name.clone());
    let first = state.db.write("system", move |t| {
        Ok(t.execute("INSERT OR IGNORE INTO agent_target_root_warnings (agent, target) VALUES (?1, ?2)",
                     params![id, target])? != 0)
    }).await?;
    if first {
        tracing::warn!(agent = %a.id, target = %def.name, %root,
            "work outside the card-agent root is not auto-cleaned; move checkouts, build output and results under the root; keep shared caches outside it");
    }
    Ok(())
}

pub fn spawn(state: Shared) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = sweep(&state).await { tracing::warn!("target root sweep failed: {e}"); }
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
    fn paths_are_strictly_owned() {
        let r = root("/src/terrarium", "p", "a").unwrap();
        for bad in ["", "/", "/src", "relative", &r, "/src/terrarium/p/ab/x",
                    "/src/terrarium/p/a/../b", "/src/terrarium/p/a/./x", "/src/terrarium/p/a//x"] {
            assert!(!inside(&r, bad), "{bad}");
        }
        assert!(inside(&r, &format!("{r}/repo/target")));
        for bad in ["", "..", "/", "a/b"] { assert!(root("/src/terrarium", bad, "a").is_err()); }
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
        let script = SCRIPT.replace("os.open('/',", &format!("os.open({:?},", base.to_str().unwrap()));
        Command::new("python3").args(["-c", &script, mode, "/src/terrarium", "p", "a", "agent-id", dest])
            .output().unwrap().status.success()
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
        for component in ["src", "src/terrarium", "src/terrarium/p", "src/terrarium/p/a"] {
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
