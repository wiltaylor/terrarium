//! Agent workspaces under the work root (PLAN §11, "mode 1").
//!
//! The workspace of an agent is `<work>/<project>/<name>/`
//! (`TERRARIUM_WORK_DIR`, default `/work`). The ULID-keyed agent dir
//! `<state>/agents/<ulid>/` keeps `private/` and `harness/`. The sandbox
//! binds the workspace at `$HOME/work` and again at `/work/<project>/<name>`,
//! and a host agent attaches it into a docker or vmlab target at the same
//! `/work/<project>/<name>`, so a path in a compiler message is the same in
//! the sandbox and on the target.
//!
//! An agent made before the work root has its files in
//! `<state>/agents/<ulid>/workspace`. [`migrate`] moves them once, at the
//! agent's next start (and before a checkout into it).

use std::path::{Component, Path, PathBuf};

use tracing::{info, warn};

/// The default of `TERRARIUM_WORK_DIR`.
pub const DEFAULT_WORK_DIR: &str = "/work";

/// Where a workspace appears in the sandbox and on a target.
pub use terrarium_host::protocol::WORKSPACE_MOUNT_ROOT;

/// `<work>/<project>/<name>`.
pub fn dir(work_dir: &Path, project: &str, name: &str) -> PathBuf {
    work_dir.join(project).join(name)
}

/// The workspace relative to the work root, `<project>/<name>`, when
/// `workspace` is two plain levels under `work_dir`.
pub fn rel(work_dir: &Path, workspace: &Path) -> Option<String> {
    let r = workspace.strip_prefix(work_dir).ok()?;
    let parts: Vec<&str> = r
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (parts.len() == 2).then(|| parts.join("/"))
}

/// The path of a workspace inside the sandbox and on a target:
/// `/work/<project>/<name>`.
pub fn inside(rel: &str) -> PathBuf {
    Path::new(WORKSPACE_MOUNT_ROOT).join(rel)
}

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Migration {
    /// Nothing to move.
    None,
    /// The files moved from the old path.
    Moved,
    /// The new path has files already; the old path stays as it is.
    Skipped(String),
}

fn is_empty_dir(p: &Path) -> bool {
    std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_none())
}

/// Copy a tree without following links: a link is copied as a link, a
/// file with its mode, a FIFO or a socket is skipped.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(from)?;
    std::fs::create_dir_all(to)?;
    std::fs::set_permissions(
        to,
        std::fs::Permissions::from_mode(meta.permissions().mode()),
    )?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let src = e.path();
        let dst = to.join(e.file_name());
        let t = e.file_type()?;
        if t.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if t.is_dir() {
            copy_tree(&src, &dst)?;
        } else if t.is_file() {
            std::fs::copy(&src, &dst)?;
        } else {
            warn!(path = %src.display(), "workspace move: skipped a special file");
        }
    }
    Ok(())
}

/// Move the workspace from `legacy` (`<state>/agents/<ulid>/workspace`) to
/// `new` once. Idempotent: no old dir, or the same path, is
/// [`Migration::None`]. A new path that has files already is
/// [`Migration::Skipped`], and the old files stay. A link at `legacy` is
/// not followed. Across file systems (the state and the work root are two
/// bind mounts in the container), the files are copied, then the old dir
/// is removed.
pub fn migrate(legacy: &Path, new: &Path) -> Migration {
    if legacy == new {
        return Migration::None;
    }
    let Ok(meta) = std::fs::symlink_metadata(legacy) else {
        return Migration::None;
    };
    if !meta.is_dir() {
        return Migration::None;
    }
    if is_empty_dir(legacy) {
        let _ = std::fs::remove_dir(legacy);
        return Migration::None;
    }
    if std::fs::symlink_metadata(new).is_ok() && !is_empty_dir(new) {
        let why = format!(
            "{} has files already; the old workspace {} stays",
            new.display(),
            legacy.display()
        );
        warn!("workspace move skipped: {why}");
        return Migration::Skipped(why);
    }
    if let Some(parent) = new.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        let why = format!("cannot create {}: {e}", parent.display());
        warn!("workspace move skipped: {why}");
        return Migration::Skipped(why);
    }
    let _ = std::fs::remove_dir(new);
    match std::fs::rename(legacy, new) {
        Ok(()) => {}
        Err(_) => {
            if let Err(e) = copy_tree(legacy, new) {
                let why = format!("cannot copy {} to {}: {e}", legacy.display(), new.display());
                warn!("workspace move failed: {why}");
                return Migration::Skipped(why);
            }
            if let Err(e) = std::fs::remove_dir_all(legacy) {
                warn!(old = %legacy.display(), "workspace copied, but the old dir stays: {e}");
            }
        }
    }
    info!(from = %legacy.display(), to = %new.display(), "moved the agent workspace to the work root");
    Migration::Moved
}

/// Make sure the workspace of `agent` is in place: with `move_old`, move
/// it from the old place once ([`migrate`]); then make the dir. Returns
/// the workspace, or `None` when the dir cannot be made.
///
/// Pass `move_old` only for an agent that does not run: the sandbox of a
/// running agent binds its old dir, and a move across file systems is a
/// copy and a delete.
pub fn ensure(
    state_dir: &Path,
    work_dir: &Path,
    agent: &terrarium_core::Agent,
    move_old: bool,
) -> Option<PathBuf> {
    let ws = agent.workspace.clone();
    if move_old && ws.starts_with(work_dir) {
        let legacy = terrarium_runner::workspace_dir(state_dir, agent.id);
        migrate(&legacy, &ws);
    }
    match std::fs::create_dir_all(&ws) {
        Ok(()) => Some(ws),
        Err(e) => {
            warn!(workspace = %ws.display(), "cannot create the workspace: {e}");
            None
        }
    }
}

/// Legacy/generic consumers must record durable use before creation or
/// migration. Owner-sandbox startup needs a separate incarnation-bound path.
pub async fn ensure_generic(
    state: &crate::api::Shared,
    agent: &terrarium_core::Agent,
    move_old: bool,
) -> crate::error::ApiResult<Option<PathBuf>> {
    let agent = agent.clone();
    let state_dir = state.state_dir.clone();
    let work_dir = state.work_dir.clone();
    state
        .db
        .write_full("workspace:generic", move |t| {
            crate::store::live_workspaces::admit_generic_agent(t, &agent.id.to_string())?;
            Ok(ensure(&state_dir, &work_dir, &agent, move_old))
        })
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let w = Path::new("/work");
        assert_eq!(dir(w, "terra", "w1"), PathBuf::from("/work/terra/w1"));
        assert_eq!(
            rel(w, Path::new("/work/terra/w1")).as_deref(),
            Some("terra/w1")
        );
        assert_eq!(rel(w, Path::new("/state/agents/X/workspace")), None);
        assert_eq!(rel(w, Path::new("/work/terra")), None);
        assert_eq!(inside("terra/w1"), PathBuf::from("/work/terra/w1"));
    }

    #[test]
    fn migrate_moves_once_and_skips_a_full_target() {
        let t = tempfile::tempdir().unwrap();
        let legacy = t.path().join("state/agents/A/workspace");
        let new = t.path().join("work/p/a");
        std::fs::create_dir_all(legacy.join("src")).unwrap();
        std::fs::write(legacy.join("src/main.rs"), "fn main() {}").unwrap();
        std::os::unix::fs::symlink("src/main.rs", legacy.join("link")).unwrap();
        assert_eq!(migrate(&legacy, &new), Migration::Moved);
        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read_to_string(new.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
        assert!(
            std::fs::symlink_metadata(new.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(migrate(&legacy, &new), Migration::None, "idempotent");
        // A target with files: skip, and keep the old files.
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("old.txt"), "old").unwrap();
        assert!(matches!(migrate(&legacy, &new), Migration::Skipped(_)));
        assert!(legacy.join("old.txt").is_file());
        // An empty old dir goes away.
        std::fs::remove_file(legacy.join("old.txt")).unwrap();
        assert_eq!(migrate(&legacy, &new), Migration::None);
        assert!(!legacy.exists());
    }

    #[test]
    fn copy_tree_keeps_links_and_modes() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        std::fs::create_dir_all(a.join("d")).unwrap();
        std::fs::write(a.join("d/x.sh"), "#!/bin/sh").unwrap();
        std::fs::set_permissions(a.join("d/x.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", a.join("l")).unwrap();
        copy_tree(&a, &t.path().join("b")).unwrap();
        let m = std::fs::metadata(t.path().join("b/d/x.sh")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(t.path().join("b/l")).unwrap(),
            PathBuf::from("/etc/passwd")
        );
    }
}
