//! What a run left in a shared harness home, and the clean-up.
//!
//! [`Snapshot::take`] lists the home before the run. After the run,
//! [`clean`] compares a new listing with it:
//!
//! - A new path whose text contains a run key (a session ID, the agent
//!   ULID, or the unique name of the run's temp root, which is also in the
//!   workspace path) is the run's. It is removed.
//! - A new small file whose content contains a run key is also removed.
//! - A new mount point that the adapter's `materialize` made in the home
//!   is removed when it still has the bytes that `materialize` wrote.
//! - A new directory that is empty after those removals, and that held a
//!   removed path, is removed.
//! - Everything else that is new is reported, not removed: the operator's
//!   own sessions write to the same home at the same time.
//! - A file that was there before and changed is never removed. It is
//!   reported, with the number of lines that name a run key.
//!
//! Nothing outside the home is touched, and the home itself stays.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A file larger than this is not read for a content match.
const CONTENT_LIMIT: u64 = 2 * 1024 * 1024;
/// A changed file larger than this is not searched for run lines.
const SEARCH_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    dir: bool,
    len: u64,
    modified: Option<SystemTime>,
}

/// The paths under one root, with their size and mtime.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub root: PathBuf,
    entries: BTreeMap<PathBuf, Entry>,
}

impl Snapshot {
    /// Walk `root`. Symlinks are listed, not followed. A root that does not
    /// exist gives an empty snapshot.
    pub fn take(root: &Path) -> Self {
        let mut entries = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let is_dir = meta.is_dir();
                entries.insert(
                    path.clone(),
                    Entry {
                        dir: is_dir,
                        len: meta.len(),
                        modified: meta.modified().ok(),
                    },
                );
                if is_dir {
                    stack.push(path);
                }
            }
        }
        Self {
            root: root.to_path_buf(),
            entries,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, p: &Path) -> bool {
        self.entries.contains_key(p)
    }
}

/// A mount point that `materialize` made in the shared home, and the bytes
/// it wrote (`None` for a directory).
#[derive(Debug, Clone)]
pub struct MountPoint {
    pub path: PathBuf,
    pub bytes: Option<Vec<u8>>,
}

/// The result of [`clean`].
#[derive(Debug, Clone, Default)]
pub struct Leftovers {
    /// Removed: the run's own paths.
    pub removed: Vec<PathBuf>,
    /// New paths that no run key names. Not removed.
    pub unattributed: Vec<PathBuf>,
    /// Files that were there before, changed, and name a run key on this
    /// many lines. Not removed: they are shared files in live use.
    pub changed_with_run_lines: Vec<(PathBuf, usize)>,
    /// Files that were there before and changed, with no run key in them.
    pub changed_other: Vec<PathBuf>,
    /// Paths that could not be removed, with the error.
    pub errors: Vec<String>,
}

fn names_key(text: &str, keys: &[String]) -> bool {
    keys.iter()
        .any(|k| !k.is_empty() && text.contains(k.as_str()))
}

fn content_names_key(path: &Path, len: u64, keys: &[String]) -> bool {
    len <= CONTENT_LIMIT
        && fs::read(path)
            .map(|b| names_key(&String::from_utf8_lossy(&b), keys))
            .unwrap_or(false)
}

/// Compare `root` now with `before`, remove the run's paths, and report the
/// rest. `keys` must be unique to the run (see the module doc).
pub fn clean(before: &Snapshot, keys: &[String], mounts: &[MountPoint]) -> Leftovers {
    let root = before.root.clone();
    let now = Snapshot::take(&root);
    let mut out = Leftovers::default();
    let mut gone: BTreeSet<PathBuf> = BTreeSet::new();
    let mut had_removed_child: BTreeSet<PathBuf> = BTreeSet::new();
    let inside = |p: &Path| p.starts_with(&root) && p != root.as_path();
    let under_gone = |p: &Path, gone: &BTreeSet<PathBuf>| gone.iter().any(|g| p.starts_with(g));

    let new: Vec<(&PathBuf, &Entry)> = now
        .entries
        .iter()
        .filter(|(p, _)| !before.contains(p))
        .collect();

    let mut remove = |p: &Path, dir: bool, out: &mut Leftovers, gone: &mut BTreeSet<PathBuf>| {
        if !inside(p) {
            out.errors
                .push(format!("refused: {} is outside the home", p.display()));
            return;
        }
        let r = if dir {
            fs::remove_dir_all(p)
        } else {
            fs::remove_file(p)
        };
        match r {
            Ok(()) => {
                out.removed.push(p.to_path_buf());
                gone.insert(p.to_path_buf());
                if let Some(parent) = p.parent() {
                    had_removed_child.insert(parent.to_path_buf());
                }
            }
            Err(e) => out.errors.push(format!("{}: {e}", p.display())),
        }
    };

    // 1. New paths that a run key names. Top-down, so a removed directory
    //    covers its children.
    for (p, e) in &new {
        if under_gone(p, &gone) {
            continue;
        }
        let rel = p.strip_prefix(&root).unwrap_or(p);
        if names_key(&rel.to_string_lossy(), keys) {
            remove(p, e.dir, &mut out, &mut gone);
        }
    }
    // 2. New files: a run key in the content, or an unchanged mount point.
    for (p, e) in &new {
        if e.dir || under_gone(p, &gone) {
            continue;
        }
        let mount = mounts.iter().find(|m| m.path == **p);
        let unchanged_mount = mount.is_some_and(|m| {
            m.bytes
                .as_ref()
                .is_some_and(|b| fs::read(p).is_ok_and(|now| &now == b))
        });
        if unchanged_mount || content_names_key(p, e.len, keys) {
            remove(p, false, &mut out, &mut gone);
        } else {
            out.unattributed.push((*p).clone());
        }
    }
    // 3. New directories, deepest first: empty mount points, and
    //    directories that held only the run's files.
    let mut dirs: Vec<&PathBuf> = new
        .iter()
        .filter(|(p, e)| e.dir && !under_gone(p, &gone))
        .map(|(p, _)| *p)
        .collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in dirs {
        let empty = fs::read_dir(d)
            .map(|mut r| r.next().is_none())
            .unwrap_or(false);
        let is_mount = mounts.iter().any(|m| m.path == *d && m.bytes.is_none());
        if empty && (is_mount || had_removed_child.contains(d)) {
            match fs::remove_dir(d) {
                Ok(()) => {
                    out.removed.push(d.clone());
                    if let Some(parent) = d.parent() {
                        had_removed_child.insert(parent.to_path_buf());
                    }
                }
                Err(e) => out.errors.push(format!("{}: {e}", d.display())),
            }
        } else if empty {
            out.unattributed.push(d.clone());
        }
    }
    out.unattributed.retain(|p| !out.removed.contains(p));
    // 4. Files that were there before and changed.
    for (p, e) in &now.entries {
        let Some(old) = before.entries.get(p) else {
            continue;
        };
        if e.dir || (old.len == e.len && old.modified == e.modified) {
            continue;
        }
        let lines = if e.len <= SEARCH_LIMIT {
            fs::read(p)
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .lines()
                        .filter(|l| names_key(l, keys))
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };
        if lines > 0 {
            out.changed_with_run_lines.push((p.clone(), lines));
        } else {
            out.changed_other.push(p.clone());
        }
    }
    out
}

/// A short text for the report: what was removed and what stays.
pub fn summary(l: &Leftovers, root: &Path) -> String {
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let list = |v: &[PathBuf]| {
        let mut names: Vec<String> = v.iter().take(6).map(|p| rel(p)).collect();
        if v.len() > 6 {
            names.push(format!("and {} more", v.len() - 6));
        }
        names.join(", ")
    };
    let mut parts = vec![format!(
        "removed {} path(s): [{}]",
        l.removed.len(),
        list(&l.removed)
    )];
    if !l.changed_with_run_lines.is_empty() {
        let s: Vec<String> = l
            .changed_with_run_lines
            .iter()
            .map(|(p, n)| format!("{} ({n} line(s))", rel(p)))
            .collect();
        parts.push(format!(
            "kept, shared files with run lines: [{}]",
            s.join(", ")
        ));
    }
    if !l.unattributed.is_empty() {
        parts.push(format!(
            "new but not attributed (not removed): {} path(s): [{}]",
            l.unattributed.len(),
            list(&l.unattributed)
        ));
    }
    if !l.changed_other.is_empty() {
        parts.push(format!(
            "changed, no run key: {} file(s): [{}]",
            l.changed_other.len(),
            list(&l.changed_other)
        ));
    }
    if !l.errors.is_empty() {
        parts.push(format!("errors: {}", l.errors.join("; ")));
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_the_runs_paths() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("projects/old")).expect("dir");
        fs::write(home.join("history.jsonl"), "old\n").expect("w");
        fs::write(home.join("keep.json"), "{}").expect("w");
        let before = Snapshot::take(&home);

        let sid = "aee36d42-3246-4e16-be9c-16dd97c85c57".to_owned();
        let keys = vec![sid.clone(), "tcompat-Ab12".to_owned()];
        // The run's files.
        fs::create_dir_all(home.join("projects/-tmp-tcompat-Ab12-ws/memory")).expect("dir");
        fs::write(
            home.join(format!("projects/-tmp-tcompat-Ab12-ws/{sid}.jsonl")),
            "x",
        )
        .expect("w");
        fs::create_dir_all(home.join("session-env").join(&sid)).expect("dir");
        fs::create_dir_all(home.join("sessions")).expect("dir");
        fs::write(
            home.join("sessions/123.json"),
            format!("{{\"sessionId\":\"{sid}\"}}"),
        )
        .expect("w");
        fs::write(home.join("SYSTEM.md"), "").expect("w");
        // Not the run's.
        fs::write(home.join("projects/old/other.jsonl"), "y").expect("w");
        fs::write(
            home.join("history.jsonl"),
            format!("old\n{{\"s\":\"{sid}\"}}\n"),
        )
        .expect("w");

        let mounts = vec![MountPoint {
            path: home.join("SYSTEM.md"),
            bytes: Some(Vec::new()),
        }];
        let l = clean(&before, &keys, &mounts);
        assert!(!home.join("projects/-tmp-tcompat-Ab12-ws").exists());
        assert!(!home.join("session-env").join(&sid).exists());
        assert!(!home.join("sessions/123.json").exists());
        assert!(!home.join("sessions").exists(), "emptied new dir goes too");
        assert!(!home.join("SYSTEM.md").exists());
        assert!(home.join("projects/old/other.jsonl").exists());
        assert!(home.join("keep.json").exists());
        assert!(home.join("history.jsonl").exists());
        assert_eq!(l.unattributed, vec![home.join("projects/old/other.jsonl")]);
        assert_eq!(
            l.changed_with_run_lines,
            vec![(home.join("history.jsonl"), 1)]
        );
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        // session-env/ held only the run's dir, so it goes too.
        assert!(!home.join("session-env").exists());
        assert!(summary(&l, &home).contains("removed"));
    }

    #[test]
    fn changed_mount_point_stays() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().to_path_buf();
        let before = Snapshot::take(&home);
        fs::write(home.join("APPEND_SYSTEM.md"), "operator text").expect("w");
        let mounts = vec![MountPoint {
            path: home.join("APPEND_SYSTEM.md"),
            bytes: Some(Vec::new()),
        }];
        let l = clean(&before, &["nothing-matches".to_owned()], &mounts);
        assert!(home.join("APPEND_SYSTEM.md").exists());
        assert_eq!(l.unattributed.len(), 1);
    }
}
