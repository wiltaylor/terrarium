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

#[derive(Default, Serialize, Deserialize)]
struct Retention {
    #[serde(default)]
    initialized: bool,
    #[serde(default)]
    epoch: Option<String>,
    #[serde(default)]
    first_pass: Option<Vec<String>>,
    #[serde(default)]
    retention_running: Option<RetentionRunning>,
    pending: Option<String>,
    protected: std::collections::BTreeMap<String, String>,
    released: std::collections::BTreeSet<String>,
}

/// Persisted before exec start. PIDs are observations, never guessed identities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionRunning {
    pub container: String,
    pub exec_id: String,
    pub token: String,
    pub pid: Option<u64>,
    pub worker_pid: Option<u64>,
    pub worker_start: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum HistoryEntry {
    Run(Box<Run>),
    Initialization { retention_initialized: Epoch },
}

#[derive(Clone, Serialize, Deserialize)]
struct Epoch {
    id: String,
    at: String,
}

fn history_runs(entries: &[HistoryEntry]) -> Vec<Run> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Run(run) => Some((**run).clone()),
            HistoryEntry::Initialization { .. } => None,
        })
        .collect()
}

fn validate_epoch(r: &Retention, entries: &[HistoryEntry]) -> std::io::Result<()> {
    if !r.initialized || !entries.iter().any(|entry| matches!(entry, HistoryEntry::Initialization { retention_initialized } if Some(&retention_initialized.id) == r.epoch.as_ref())) {
        return Err(std::io::Error::other("retention initialized epoch missing: use repair"));
    }
    Ok(())
}

fn strict_json<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<T> {
    let bytes = std::fs::read(path)
        .map_err(|e| std::io::Error::other(format!("{} needs repair: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::other(format!("{} needs repair: {e}", path.display())))
}

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
    pub fn retention_begin(&self, run: &Run) -> std::io::Result<()> {
        let mut r: Retention = strict_json(&self.dir.join("retention.json"))?;
        let entries: Vec<HistoryEntry> = strict_json(&self.dir.join(HISTORY))?;
        validate_epoch(&r, &entries)?;
        let _: Option<Previous> = strict_json(&self.dir.join(PREVIOUS))?;
        if r.retention_running.is_some() {
            return Err(std::io::Error::other(
                "retention helper requires reconciliation before updates",
            ));
        }
        if r.pending.is_some() {
            return Err(std::io::Error::other(
                "unfinished update requires operator recovery release",
            ));
        }
        r.pending = Some(run.id.clone());
        write_json(&self.dir.join("retention.json"), &r)
    }

    pub fn retention_finish(&self, run: &Run) -> std::io::Result<()> {
        let mut r: Retention = strict_json(&self.dir.join("retention.json"))?;
        if matches!(
            run.outcome,
            crate::protocol::Outcome::RecoveryFailed | crate::protocol::Outcome::RollbackFailed
        ) && let Some(backup) = &run.backup
        {
            r.protected.insert(run.id.clone(), backup.clone());
        }
        r.pending = None;
        write_json(&self.dir.join("retention.json"), &r)
    }

    /// Strict reads: corrupt updater state never means permission to delete.
    pub fn retention_protected(&self, release: Option<&str>) -> std::io::Result<Vec<String>> {
        let mut r: Retention = strict_json(&self.dir.join("retention.json"))?;
        if r.retention_running.is_some() {
            return Err(std::io::Error::other(
                "retention helper requires reconciliation",
            ));
        }
        if !r.initialized {
            return Err(std::io::Error::other(
                "retention not initialized: operator must run admin backup retention init",
            ));
        }
        let entries: Vec<HistoryEntry> = strict_json(&self.dir.join(HISTORY))?;
        validate_epoch(&r, &entries)?;
        let history = history_runs(&entries);
        let previous: Option<Previous> = strict_json(&self.dir.join(PREVIOUS))?;
        for run in history {
            if !r.released.contains(&run.id)
                && matches!(
                    run.outcome,
                    crate::protocol::Outcome::RecoveryFailed
                        | crate::protocol::Outcome::RollbackFailed
                )
                && let Some(backup) = run.backup
            {
                r.protected.insert(run.id, backup);
            }
        }
        if let Some(id) = release {
            if !r.protected.contains_key(id) && r.pending.as_deref() != Some(id) {
                return Err(std::io::Error::other("not a protected recovery run"));
            }
            // Audit durably BEFORE making the release effective.
            use std::os::unix::fs::OpenOptionsExt;
            let mut audit = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(self.dir.join("retention-audit.jsonl"))?;
            writeln!(
                audit,
                "{}",
                serde_json::json!({"action":"backup.recovery.release", "actor":"operator", "run":id, "at":chrono::Utc::now()})
            )?;
            audit.sync_all()?;
            r.protected.remove(id);
            r.released.insert(id.to_owned());
            if r.pending.as_deref() == Some(id) {
                r.pending = None;
            }
        }
        write_json(&self.dir.join("retention.json"), &r)?;
        if r.pending.is_some() {
            return Err(std::io::Error::other(
                "unfinished update blocks retention until operator recovery release",
            ));
        }
        let mut protected: Vec<_> = r.protected.into_values().collect();
        if let Some(p) = previous.and_then(|p| p.backup) {
            protected.push(p);
        }
        Ok(protected)
    }

    /// Only an explicit operator confirmation establishes the initial baseline.
    /// This is never called by normal retention or update admission.
    pub fn retention_init(&self, confirm: bool, no_rollback: bool) -> std::io::Result<Vec<String>> {
        self.retention_initialize(confirm, no_rollback, None)
    }

    fn retention_initialize(
        &self,
        confirm: bool,
        no_rollback: bool,
        first_pass: Option<Vec<String>>,
    ) -> std::io::Result<Vec<String>> {
        let path = self.dir.join("retention.json");
        let mut entries: Vec<HistoryEntry> =
            if no_rollback && !self.dir.join(HISTORY).try_exists()? {
                Vec::new()
            } else {
                strict_json(&self.dir.join(HISTORY))?
            };
        let prior_epoch = entries
            .iter()
            .any(|entry| matches!(entry, HistoryEntry::Initialization { .. }));
        if prior_epoch && strict_json::<Retention>(&path).is_err() {
            return Err(std::io::Error::other("retention state lost: use repair"));
        }
        let mut r: Retention = if path.try_exists()? {
            strict_json(&path)?
        } else {
            Retention::default()
        };
        if prior_epoch || r.initialized {
            validate_epoch(&r, &entries)?;
        }
        if r.retention_running.is_some() {
            return Err(std::io::Error::other(
                "retention helper requires reconciliation before initialization",
            ));
        }
        if r.pending.is_some() {
            return Err(std::io::Error::other(
                "unfinished update blocks retention initialization",
            ));
        }
        let history = history_runs(&entries);
        let previous: Option<Previous> = if no_rollback && !self.dir.join(PREVIOUS).try_exists()? {
            None
        } else {
            strict_json(&self.dir.join(PREVIOUS))?
        };
        if no_rollback && previous.is_some() {
            return Err(std::io::Error::other(
                "rollback reference exists: cannot assert --no-rollback-reference",
            ));
        }
        for run in &history {
            if !r.released.contains(&run.id)
                && matches!(
                    run.outcome,
                    crate::protocol::Outcome::RecoveryFailed
                        | crate::protocol::Outcome::RollbackFailed
                )
                && let Some(backup) = &run.backup
            {
                r.protected.insert(run.id.clone(), backup.clone());
            }
        }
        let mut protected: Vec<_> = r.protected.values().cloned().collect();
        if let Some(p) = previous.as_ref().and_then(|p| p.backup.clone()) {
            protected.push(p);
        }
        if confirm {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::create_dir_all(&self.dir)?;
            let mut audit = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(self.dir.join("retention-audit.jsonl"))?;
            writeln!(
                audit,
                "{}",
                serde_json::json!({"action":"backup.retention.init", "actor":"operator", "no_rollback_reference":no_rollback, "protected":protected, "first_pass":first_pass, "at":chrono::Utc::now()})
            )?;
            audit.sync_all()?;
            // Explicitly record even an empty baseline. Later loss fails closed.
            let epoch = ulid::Ulid::new().to_string();
            entries.push(HistoryEntry::Initialization {
                retention_initialized: Epoch {
                    id: epoch.clone(),
                    at: chrono::Utc::now().to_rfc3339(),
                },
            });
            write_json(&self.dir.join(HISTORY), &entries)?;
            write_json(&self.dir.join(PREVIOUS), &previous)?;
            r.initialized = true;
            r.epoch = Some(epoch);
            r.first_pass = first_pass;
            write_json(&path, &r)?;
        }
        Ok(protected)
    }

    pub fn retention_running(&self) -> std::io::Result<Option<RetentionRunning>> {
        let r: Retention = strict_json(&self.dir.join("retention.json"))?;
        Ok(r.retention_running)
    }

    pub fn retention_confirm(&self, no_rollback: bool, names: Vec<String>) -> std::io::Result<()> {
        self.retention_initialize(true, no_rollback, Some(names))
            .map(|_| ())
    }

    /// Caller holds admission lock and has inspected every container ExecID.
    pub fn retention_repair(
        &self,
        confirm: bool,
        inventory: &std::collections::BTreeMap<String, String>,
        names: Vec<String>,
    ) -> std::io::Result<Vec<String>> {
        let mut entries: Vec<HistoryEntry> = strict_json(&self.dir.join(HISTORY))?;
        let previous: Option<Previous> = strict_json(&self.dir.join(PREVIOUS))?;
        let path = self.dir.join("retention.json");
        // Unreadable lost state can only be replaced through this audited path.
        let mut r: Retention = strict_json(&path).unwrap_or_default();
        if r.pending.is_some() {
            return Err(std::io::Error::other(
                "unfinished update blocks retention repair",
            ));
        }
        r.protected.extend(inventory.clone());
        for run in history_runs(&entries) {
            if !r.released.contains(&run.id)
                && matches!(
                    run.outcome,
                    crate::protocol::Outcome::RecoveryFailed
                        | crate::protocol::Outcome::RollbackFailed
                )
                && let Some(backup) = run.backup
            {
                r.protected.insert(run.id, backup);
            }
        }
        let mut protected: Vec<_> = r.protected.values().cloned().collect();
        protected.extend(previous.and_then(|p| p.backup));
        if confirm {
            use std::os::unix::fs::OpenOptionsExt;
            let epoch = ulid::Ulid::new().to_string();
            let mut audit = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(self.dir.join("retention-audit.jsonl"))?;
            writeln!(
                audit,
                "{}",
                serde_json::json!({"action":"backup.retention.repair","actor":"operator","epoch":epoch,"protected":r.protected,"first_pass":names,"at":chrono::Utc::now()})
            )?;
            audit.sync_all()?;
            entries.push(HistoryEntry::Initialization {
                retention_initialized: Epoch {
                    id: epoch.clone(),
                    at: chrono::Utc::now().to_rfc3339(),
                },
            });
            write_json(&self.dir.join(HISTORY), &entries)?;
            r.epoch = Some(epoch);
            r.initialized = true;
            r.retention_running = None;
            r.first_pass = Some(names);
            write_json(&path, &r)?;
        }
        Ok(protected)
    }

    pub fn retention_authorization(
        &self,
        running: &RetentionRunning,
    ) -> std::io::Result<serde_json::Value> {
        let r: Retention = strict_json(&self.dir.join("retention.json"))?;
        let entries: Vec<HistoryEntry> = strict_json(&self.dir.join(HISTORY))?;
        validate_epoch(&r, &entries)?;
        let history = history_runs(&entries);
        let previous: Option<Previous> = strict_json(&self.dir.join(PREVIOUS))?;
        if !r.initialized
            || r.pending.is_some()
            || !r.retention_running.as_ref().is_some_and(|current| {
                current.token == running.token
                    && current.exec_id == running.exec_id
                    && current.container == running.container
            })
        {
            return Err(std::io::Error::other(
                "retention authorization safety state changed",
            ));
        }
        let mut protected: Vec<_> = r.protected.values().cloned().collect();
        protected.extend(previous.and_then(|p| p.backup));
        protected.extend(
            history
                .into_iter()
                .filter(|run| {
                    !r.released.contains(&run.id)
                        && matches!(
                            run.outcome,
                            crate::protocol::Outcome::RecoveryFailed
                                | crate::protocol::Outcome::RollbackFailed
                        )
                })
                .filter_map(|run| run.backup),
        );
        Ok(
            serde_json::json!({"token":running.token,"protected":protected,"first_pass":r.first_pass,"expires_at":chrono::Utc::now().timestamp_millis() as f64 / 1000.0 + 10.0}),
        )
    }

    pub fn retention_pass_finished(&self) -> std::io::Result<()> {
        let path = self.dir.join("retention.json");
        let mut r: Retention = strict_json(&path)?;
        if r.retention_running.is_some() {
            return Err(std::io::Error::other("retention still running"));
        }
        r.first_pass = None;
        write_json(&path, &r)
    }

    pub fn set_retention_running(&self, running: Option<RetentionRunning>) -> std::io::Result<()> {
        let path = self.dir.join("retention.json");
        let mut r: Retention = strict_json(&path)?;
        r.retention_running = running;
        write_json(&path, &r)
    }
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
        let entries: Vec<HistoryEntry> = read_json(&self.dir.join(HISTORY)).unwrap_or_default();
        history_runs(&entries)
    }

    /// Put `run` first, and keep the newest runs.
    pub fn push(&self, run: &Run) -> std::io::Result<()> {
        let mut entries: Vec<HistoryEntry> = if self.dir.join(HISTORY).try_exists()? {
            strict_json(&self.dir.join(HISTORY))?
        } else {
            Vec::new()
        };
        entries.retain(|entry| matches!(entry, HistoryEntry::Initialization { .. }));
        let mut h = self.history();
        h.retain(|r| r.id != run.id);
        h.insert(0, run.clone());
        h.truncate(self.keep);
        entries.extend(h.into_iter().map(|run| HistoryEntry::Run(Box::new(run))));
        write_json(&self.dir.join(HISTORY), &entries)
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
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()
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
    fn lost_retention_after_rotated_recovery_hold_requires_conservative_repair() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path(), 1);
        s.retention_init(true, true).unwrap();
        let mut failed = run("failed");
        failed.outcome = Outcome::RecoveryFailed;
        failed.backup = Some("/state/backups/old.zip".into());
        s.retention_finish(&failed).unwrap();
        s.push(&failed).unwrap();
        s.push(&run("new")).unwrap();
        assert_eq!(s.history().len(), 1);
        std::fs::remove_file(dir.path().join("retention.json")).unwrap();
        assert!(
            s.retention_init(true, false)
                .unwrap_err()
                .to_string()
                .contains("retention state lost: use repair")
        );
        let inventory = [("snapshot:old.zip".into(), "/state/backups/old.zip".into())]
            .into_iter()
            .collect();
        s.retention_repair(true, &inventory, Vec::new()).unwrap();
        assert_eq!(
            s.retention_protected(None).unwrap(),
            ["/state/backups/old.zip"]
        );
        s.retention_protected(Some("snapshot:old.zip")).unwrap();
        assert!(s.retention_protected(None).unwrap().is_empty());
    }

    #[test]
    fn lease_renewal_rereads_each_safety_file_and_initialized_marker() {
        for file in ["retention.json", HISTORY, PREVIOUS, "initialized"] {
            for corrupt in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let s = Store::new(dir.path(), 1);
                s.retention_init(true, true).unwrap();
                let running = RetentionRunning {
                    container: "daemon".into(),
                    exec_id: "exact".into(),
                    token: "A".repeat(26),
                    pid: None,
                    worker_pid: None,
                    worker_start: None,
                };
                s.set_retention_running(Some(running.clone())).unwrap();
                assert!(s.retention_authorization(&running).is_ok());
                if file == "initialized" {
                    let path = dir.path().join("retention.json");
                    let mut value: serde_json::Value = strict_json(&path).unwrap();
                    value.as_object_mut().unwrap().remove("initialized");
                    write_json(&path, &value).unwrap();
                } else if corrupt {
                    std::fs::write(dir.path().join(file), "{").unwrap();
                } else {
                    std::fs::remove_file(dir.path().join(file)).unwrap();
                }
                assert!(
                    s.retention_authorization(&running).is_err(),
                    "{file}, corrupt={corrupt}"
                );
            }
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn paused_real_helpers_stop_after_safety_state_loss_and_lease_expiry() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        // Each real helper is authorized, then paused on the creator's flock.
        // Simulate updater loss too: it cannot deliver revocation, so expiry
        // itself must prevent every unlink when the helpers resume.
        let script = r#"
import datetime, fcntl, importlib.util, json, os, pathlib, subprocess, sys, zipfile
helper, directory, authorization = sys.argv[1:]
os.environ['TERRARIUM_BACKUP_DIR'] = directory
os.environ['TERRARIUM_STATE_DIR'] = str(pathlib.Path(directory).parent)
spec = importlib.util.spec_from_file_location('hot', helper)
hot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hot)
directory = pathlib.Path(directory)
directory.mkdir()
for age in range(20, 40):
    created = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0) - datetime.timedelta(days=age)
    name = f'terrarium-backup-{created:%Y%m%dT%H%M%SZ}-full.zip'
    with zipfile.ZipFile(directory / name, 'w') as archive:
        archive.writestr('backup.json', json.dumps(dict(format='terrarium-backup', version=1, kind='full', created_at=created.isoformat())))
authorization = json.loads(authorization)
hot.retention_control('prepare', authorization)
with (directory / '.retention.lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    child = subprocess.Popen([sys.executable, helper, 'retain-backups', authorization['token']], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        print('authorized', flush=True)
        sys.stdin.readline()
        fcntl.flock(lock, fcntl.LOCK_UN)
        # Scheduling delay is not the safety assertion. The actual assertions
        # below still require an expired lease, a stopped receipt and no unlink.
        stdout, stderr = child.communicate(timeout=60)
        if child.returncode:
            raise RuntimeError(stderr.decode())
        receipt = hot.retention_read(directory, '.retention-' + authorization['token'] + '.receipt.json')
        assert receipt['stopped'] and receipt['removed'] == [], receipt
        assert len(list(directory.glob('*.zip'))) == 20
    finally:
        if child.poll() is None:
            child.kill()
        child.communicate()
"#;
        let mut children = Vec::new();
        let mut latest_expiry = 0.0_f64;
        for file in ["retention.json", HISTORY, PREVIOUS, "initialized"] {
            for corrupt in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let s = Store::new(dir.path().join("updater"), 1);
                s.retention_init(true, true).unwrap();
                let running = RetentionRunning {
                    container: "daemon".into(),
                    exec_id: "exact".into(),
                    token: ulid::Ulid::new().to_string(),
                    pid: None,
                    worker_pid: None,
                    worker_start: None,
                };
                s.set_retention_running(Some(running.clone())).unwrap();
                let authorization = s.retention_authorization(&running).unwrap();
                latest_expiry = latest_expiry.max(authorization["expires_at"].as_f64().unwrap());
                let mut child = Command::new("python3")
                    .arg("-u")
                    .arg("-c")
                    .arg(script)
                    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/hot-swap.py"))
                    .arg(dir.path().join("backups"))
                    .arg(authorization.to_string())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut ready = String::new();
                BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut ready)
                    .unwrap();
                assert_eq!(ready.trim(), "authorized");
                let path = s.dir().join(if file == "initialized" {
                    "retention.json"
                } else {
                    file
                });
                if file == "initialized" {
                    let mut value: serde_json::Value = strict_json(&path).unwrap();
                    if corrupt {
                        value["initialized"] = false.into();
                    } else {
                        value.as_object_mut().unwrap().remove("initialized");
                    }
                    write_json(&path, &value).unwrap();
                } else if corrupt {
                    std::fs::write(path, "{").unwrap();
                } else {
                    std::fs::remove_file(path).unwrap();
                }
                assert!(
                    s.retention_authorization(&running).is_err(),
                    "{file}, corrupt={corrupt}"
                );
                children.push((child, dir));
            }
        }
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        std::thread::sleep(std::time::Duration::from_secs_f64(
            (latest_expiry - now + 0.1).max(0.0),
        ));
        for (mut child, _dir) in children {
            child.stdin.take().unwrap().write_all(b"resume\n").unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
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
