//! Agent workspaces on this host (docs/host-protocol.md, "Workspaces").
//!
//! The daemon names a workspace as `<project>/<agent>`. The host agent
//! finds it under its own `work_dir`, the host path of the daemon's work
//! root. Only a directory that exists, and whose real path is under the
//! real `work_dir`, passes: a symbolic link cannot lead out.

use std::path::{Path, PathBuf};

use crate::protocol::WorkspaceMount;
use crate::verb::BuildError;

/// Does this host have the agent workspaces: is `work_dir` a directory
/// here? A remote host agent with no copy of the daemon's work root has
/// none, and says so in its capabilities (`no_workspaces`, TER-41).
pub fn available(work_dir: &Path) -> bool {
    std::fs::canonicalize(work_dir).is_ok_and(|p| p.is_dir())
}

/// The real path of the workspace `m` under `work_dir`.
pub fn resolve(work_dir: &Path, m: &WorkspaceMount) -> Result<PathBuf, BuildError> {
    m.check().map_err(BuildError::bad)?;
    let root = std::fs::canonicalize(work_dir).map_err(|e| {
        BuildError::path(format!(
            "work_dir {} is not usable ({e}); set work_dir in the host agent config to the \
             host path of the daemon's TERRARIUM_WORK_DIR",
            work_dir.display()
        ))
    })?;
    let p = work_dir.join(&m.path);
    let real = std::fs::canonicalize(&p).map_err(|_| {
        BuildError::path(format!(
            "the workspace {} is not in work_dir {} on this host; work_dir must be the host \
             path of the daemon's TERRARIUM_WORK_DIR, and the agent must have started once",
            m.path,
            work_dir.display()
        ))
    })?;
    if !real.starts_with(&root) || real == root {
        return Err(BuildError::path(format!(
            "the workspace {} leads out of work_dir",
            m.path
        )));
    }
    if !real.is_dir() {
        return Err(BuildError::path(format!(
            "the workspace {} is not a directory",
            m.path
        )));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(p: &str) -> WorkspaceMount {
        WorkspaceMount { path: p.into() }
    }

    /// TER-41: a host has the workspaces only when work_dir is a directory.
    #[test]
    fn workspaces_are_available_only_with_a_work_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available(dir.path()));
        assert!(!available(&dir.path().join("missing")));
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(!available(&file));
    }

    #[cfg(unix)]
    #[test]
    fn resolves_under_work_dir_only() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join("p/a")).unwrap();
        std::fs::create_dir_all(dir.path().join("outside")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside"), work.join("p/link")).unwrap();
        let real = resolve(&work, &ws("p/a")).unwrap();
        assert!(real.ends_with("work/p/a"));
        assert!(
            resolve(&work, &ws("p/link")).is_err(),
            "a link out of work_dir"
        );
        assert!(resolve(&work, &ws("p/missing")).is_err());
        assert!(resolve(&work, &ws("../x")).is_err());
        assert!(resolve(&work, &ws("/etc/x")).is_err());
        assert!(resolve(&work, &ws("p")).is_err(), "one segment");
        assert!(resolve(&work, &ws("p/./a")).is_err());
        assert_eq!(ws("p/a").target(), "/work/p/a");
    }
}
