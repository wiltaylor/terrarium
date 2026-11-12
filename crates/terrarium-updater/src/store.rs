//! The files of the updater in its state dir (`TERRARIUM_UPDATER_STATE`,
//! default `/updater`; compose binds `<data>/updater`):
//!
//! - `history.json`: the last runs, newest first ([`DEFAULT_KEEP`]).
//! - `previous.json`: the image before the last switch and the backup that
//!   the switch made: the target of a rollback.
//! - `image`: the reference of the image that the terrarium container
//!   runs after the last good switch. The deploy wrapper
//!   (`terrarium-compose`) reads it, so a later `up` keeps that image.
//!
//! Each write goes to a temp file first and then renames it into place.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::{Image, Run};

/// How many runs `history.json` keeps.
pub const DEFAULT_KEEP: usize = 20;
pub const HISTORY: &str = "history.json";
pub const PREVIOUS: &str = "previous.json";
/// The image file that the deploy wrapper reads.
pub const IMAGE: &str = "image";
pub const FRONT_RUN: &str = "current-run.json";

/// The target of a rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Previous {
    pub image: Image,
    /// Original engine spec, including environment. Never include in logs.
    #[serde(default)]
    pub spec: Option<serde_json::Value>,
    /// The path of the backup (in the terrarium container) that the
    /// switch away from `image` made.
    pub backup: Option<String>,
}

/// The state dir.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    keep: usize,
}

impl Store {
    /// Isolated, read-only maintenance projection. Never export command output,
    /// container settings, backup paths, actor names, or image specs.
    pub fn export_front(&self, run: &Run) -> std::io::Result<()> {
        let steps: Vec<_> = run
            .steps
            .iter()
            .map(|step| {
                serde_json::json!({
                    "name": step.name, "state": step.state, "at": step.at,
                    "detail": if step.state == crate::protocol::StepState::Failed {
                        "Step failed (details available in updater status)"
                    } else { "" },
                })
            })
            .collect();
        let error = run.error.as_ref().map(|_| {
            if run.outcome == crate::protocol::Outcome::RecoveryFailed {
                "Recovery failed. Do not restart. Manual database verification and restore required."
            } else {
                "Update failed. See authenticated updater status for details."
            }
        });
        write_json(
            &self.dir.join("front").join(FRONT_RUN),
            &serde_json::json!({
                "started_at": run.started_at, "ended_at": run.ended_at,
                "outcome": run.outcome, "steps": steps, "error": error,
                "from": run.from.as_ref().and_then(|image| image.version.as_ref()),
                "to": run.to.as_ref().and_then(|image| image.version.as_ref()),
            }),
        )
    }

    pub fn new(dir: impl Into<PathBuf>, keep: usize) -> Self {
        Self {
            dir: dir.into(),
            keep: keep.max(1),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The last runs, newest first. A missing or bad file is empty.
    pub fn history(&self) -> Vec<Run> {
        read_json(&self.dir.join(HISTORY)).unwrap_or_default()
    }

    /// Put `run` first, and keep the newest runs.
    pub fn push(&self, run: &Run) -> std::io::Result<()> {
        let mut h = self.history();
        h.retain(|r| r.id != run.id);
        h.insert(0, run.clone());
        h.truncate(self.keep);
        write_json(&self.dir.join(HISTORY), &h)
    }

    pub fn previous(&self) -> Option<Previous> {
        read_json(&self.dir.join(PREVIOUS))
    }

    pub fn set_previous(&self, p: &Previous) -> std::io::Result<()> {
        write_json(&self.dir.join(PREVIOUS), p)
    }

    /// The image of the last good switch.
    pub fn image(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join(IMAGE))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    pub fn set_image(&self, reference: &str) -> std::io::Result<()> {
        write_atomic(&self.dir.join(IMAGE), format!("{reference}\n").as_bytes())
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(path = %path.display(), "cannot read the file: {e}");
            None
        }
    }
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    let mode = if name == IMAGE || name == FRONT_RUN {
        0o644
    } else {
        0o600
    };
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut f = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::protocol::{Outcome, RunKind};

    fn run(id: &str) -> Run {
        Run {
            id: id.into(),
            kind: RunKind::Update,
            actor: "operator".into(),
            target: "latest".into(),
            from: None,
            to: None,
            backup: None,
            outcome: Outcome::Succeeded,
            error: None,
            steps: vec![],
            started_at: "2026-11-04T00:00:00Z".into(),
            ended_at: None,
        }
    }

    #[test]
    fn the_history_keeps_the_newest_runs_first() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path(), 2);
        assert!(s.history().is_empty());
        s.push(&run("a")).unwrap();
        s.push(&run("b")).unwrap();
        s.push(&run("c")).unwrap();
        let ids: Vec<String> = s.history().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["c", "b"]);
        // The same run again replaces its row.
        s.push(&run("b")).unwrap();
        let ids: Vec<String> = s.history().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn front_export_excludes_private_state_for_every_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), 2);
        store
            .set_previous(&Previous {
                image: Image::default(),
                backup: Some("private-backup".into()),
                spec: Some(serde_json::json!({"PRIVATE_KEY": "private-value"})),
            })
            .unwrap();
        for outcome in [
            Outcome::Running,
            Outcome::Succeeded,
            Outcome::Current,
            Outcome::Failed,
            Outcome::RolledBack,
            Outcome::RollbackFailed,
            Outcome::RecoveryFailed,
        ] {
            let mut run = run("private-id");
            run.outcome = outcome;
            run.error = Some("private-command-output".into());
            run.backup = Some("private-backup".into());
            run.from = Some(Image {
                version: Some("1.0.0".into()),
                reference: "private-image".into(),
                ..Image::default()
            });
            store.export_front(&run).unwrap();
            let path = dir.path().join("front").join(FRONT_RUN);
            let bytes = std::fs::read(&path).unwrap();
            let text = String::from_utf8(bytes.clone()).unwrap();
            assert!(!text.contains("private-"));
            assert!(!text.contains("PRIVATE_KEY"));
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["outcome"], outcome.as_str());
            assert_eq!(value["from"], "1.0.0");
            if outcome == Outcome::RecoveryFailed {
                assert!(value["error"].as_str().unwrap().contains("Do not restart"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o644
                );
            }
        }
    }

    #[test]
    fn the_image_and_the_previous_switch_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().join("sub"), 5);
        assert_eq!(s.image(), None);
        assert_eq!(s.previous(), None);
        s.set_image("ghcr.io/x/y@sha256:ab").unwrap();
        assert_eq!(s.image().as_deref(), Some("ghcr.io/x/y@sha256:ab"));
        let p = Previous {
            spec: Some(serde_json::json!({"container": {"Config": {"Env": ["SETTING=old"]}}})),
            image: Image {
                reference: "terrarium:dev".into(),
                id: "sha256:1".into(),
                ..Image::default()
            },
            backup: Some("/state/backups/b.zip".into()),
        };
        s.set_previous(&p).unwrap();
        assert_eq!(s.previous(), Some(p));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(s.dir().join(PREVIOUS))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(s.dir().join(IMAGE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
        // A bad file reads as empty.
        std::fs::write(dir.path().join("sub").join(HISTORY), "{").unwrap();
        assert!(s.history().is_empty());
    }
}
