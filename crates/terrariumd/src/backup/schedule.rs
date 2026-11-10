//! The scheduled backup (`TERRARIUM_BACKUP_SCHEDULE`, docs/api.md §32.4).
//!
//! Off by default. With an interval (`daily`, `12h`, ...), a daemon task
//! makes a backup one interval after the newest backup in the backup dir
//! (or [`FIRST_DELAY`] after the start, when that is later). The task
//! makes a full backup when the dir has none, when the newest full
//! backup is older than `TERRARIUM_BACKUP_FULL_EVERY` (default `7d`), or
//! when the key does not open the newest full backup (a rotated key);
//! else a diff of the newest full backup.
//!
//! A scheduled backup is always sealed: it holds every secret. The
//! passphrase is the first line of `TERRARIUM_BACKUP_PASSPHRASE_FILE`
//! when that is set, else the global secret `TERRARIUM_BACKUP_KEY_SECRET`
//! (default `backup-key`: its `passphrase` field, else `value`). With no
//! key the run fails.
//!
//! Each run verifies the new backup (every file against the manifest). A
//! full backup that verifies prunes the dir: it keeps the newest
//! `TERRARIUM_BACKUP_KEEP` full backups (default `4`) and their diffs.
//! Nothing is deleted before a newer full backup is verified. Each run
//! writes an audit row and a `backup` event. The first failure of a
//! streak and a missed run mail the operator.
//!
//! `TERRARIUM_BACKUP_WORK=on` adds the workspaces.
//!
//! The project schedules (`crate::scheduler`) start agent turns in one
//! project; a backup is of the whole daemon, so it has its own task, as
//! the harness auto-update has.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use super::Kind;
use crate::api::Shared;
use crate::api::backups::{CreateBody, create_queued};
use crate::events::EventDraft;
use crate::store::mail::{self, Address, Outgoing};

pub const ENV_SCHEDULE: &str = "TERRARIUM_BACKUP_SCHEDULE";
pub const ENV_FULL_EVERY: &str = "TERRARIUM_BACKUP_FULL_EVERY";
pub const ENV_KEEP: &str = "TERRARIUM_BACKUP_KEEP";
pub const ENV_WORK: &str = "TERRARIUM_BACKUP_WORK";
pub const ENV_PASSPHRASE_FILE: &str = "TERRARIUM_BACKUP_PASSPHRASE_FILE";
pub const ENV_KEY_SECRET: &str = "TERRARIUM_BACKUP_KEY_SECRET";
/// The global secret with the passphrase, when [`ENV_KEY_SECRET`] is not set.
pub const DEFAULT_KEY_SECRET: &str = "backup-key";
/// The time from the start to the first backup, at the earliest.
pub const FIRST_DELAY: Duration = Duration::from_secs(10 * 60);
/// The shortest interval.
pub const MIN_INTERVAL: Duration = Duration::from_secs(3600);
/// A run that is this late after its due time is missed.
pub const MISSED_GRACE: Duration = Duration::from_secs(3600);
/// The prefix of a scheduled backup that did not verify, in the backup
/// dir. The list skips it.
pub const UNVERIFIED_PREFIX: &str = ".unverified-";
/// The audit actor of a scheduled backup.
pub const ACTOR: &str = "system:backup";
/// The event kind of a scheduled backup.
pub const EVENT: &str = "backup";

/// The settings of the scheduled backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `None`: off.
    pub interval: Option<Duration>,
    pub full_every: Duration,
    pub keep: usize,
    pub work: bool,
    pub passphrase_file: Option<PathBuf>,
    /// The name of the global secret with the passphrase.
    pub key_secret: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: None,
            full_every: Duration::from_secs(7 * 86_400),
            keep: 4,
            work: false,
            passphrase_file: None,
            key_secret: DEFAULT_KEY_SECRET.to_owned(),
        }
    }
}

fn on(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "" | "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Config {
    /// Read the settings from `get` (the env in the daemon).
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut c = Self::default();
        let dur = |name: &str, v: &str| {
            crate::settings::parse_duration(v).map_err(|e| format!("{name}: {e}"))
        };
        if let Some(v) = get(ENV_SCHEDULE) {
            c.interval = match v.trim() {
                "" | "off" | "0" => None,
                "daily" => Some(Duration::from_secs(86_400)),
                "weekly" => Some(Duration::from_secs(7 * 86_400)),
                other => {
                    let d = dur(ENV_SCHEDULE, other)?;
                    if d < MIN_INTERVAL {
                        return Err(format!("{ENV_SCHEDULE}: the shortest interval is 1h"));
                    }
                    Some(d)
                }
            };
        }
        if let Some(v) = get(ENV_FULL_EVERY).filter(|v| !v.trim().is_empty()) {
            c.full_every = dur(ENV_FULL_EVERY, &v)?;
        }
        if let Some(v) = get(ENV_KEEP).filter(|v| !v.trim().is_empty()) {
            c.keep = v
                .trim()
                .parse()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or_else(|| format!("{ENV_KEEP}: `{v}` is not a number of 1 or more"))?;
        }
        if let Some(v) = get(ENV_WORK) {
            c.work = on(&v).ok_or_else(|| format!("{ENV_WORK}: `{v}` is not on or off"))?;
        }
        c.passphrase_file = get(ENV_PASSPHRASE_FILE)
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from);
        if let Some(v) = get(ENV_KEY_SECRET).filter(|v| !v.trim().is_empty()) {
            let v = v.trim();
            if !v
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "_-.".contains(ch))
            {
                return Err(format!("{ENV_KEY_SECRET}: `{v}` is not a secret name"));
            }
            v.clone_into(&mut c.key_secret);
        }
        Ok(c)
    }

    pub fn from_env() -> Result<Self, String> {
        Self::parse(|k| std::env::var(k).ok())
    }
}

/// When the next backup runs: one interval after the newest backup, and
/// not before `started + FIRST_DELAY`.
pub fn next_run(
    interval: Duration,
    started: DateTime<Utc>,
    newest: Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    let interval = chrono::Duration::from_std(interval).unwrap_or(chrono::Duration::days(1));
    let first = started + chrono::Duration::from_std(FIRST_DELAY).unwrap_or_default();
    match newest {
        Some(t) => (t + interval).max(first),
        None => first,
    }
}

/// The due time of a run that is missed at `now`: one interval after the
/// newest backup, more than [`MISSED_GRACE`] ago. `None`: nothing missed.
pub fn missed(
    interval: Duration,
    now: DateTime<Utc>,
    newest: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let interval = chrono::Duration::from_std(interval).ok()?;
    let grace = chrono::Duration::from_std(MISSED_GRACE).ok()?;
    let due = newest? + interval;
    (now - due > grace).then_some(due)
}

/// The kind of the next backup: full when there is no full backup, or
/// when the newest is older than `full_every`.
pub fn next_kind(
    now: DateTime<Utc>,
    newest_full: Option<DateTime<Utc>>,
    full_every: Duration,
) -> Kind {
    let every = chrono::Duration::from_std(full_every).unwrap_or(chrono::Duration::days(7));
    match newest_full {
        Some(t) if now - t < every => Kind::Diff,
        _ => Kind::Full,
    }
}

fn newest(dir: &Path, full_only: bool) -> Option<DateTime<Utc>> {
    super::list(dir)
        .ok()?
        .into_iter()
        .filter_map(|l| l.header)
        .filter(|h| !full_only || h.kind == Kind::Full)
        .map(|h| h.created_at)
        .max()
}

/// The kind of the next scheduled backup in `dir` with `passphrase`: as
/// [`next_kind`], and full when the newest full backup is not sealed or
/// the passphrase does not open it (the key was rotated): a diff and its
/// base always have the same key.
pub fn kind_for(dir: &Path, now: DateTime<Utc>, full_every: Duration, passphrase: &str) -> Kind {
    let Ok(Some(path)) = super::newest_full(dir) else {
        return Kind::Full;
    };
    let Ok(h) = super::read_header(&path) else {
        return Kind::Full;
    };
    if next_kind(now, Some(h.created_at), full_every) == Kind::Full {
        return Kind::Full;
    }
    match &h.encryption {
        Some(e) if super::SealKey::open(passphrase, &h.id, e).is_ok() => Kind::Diff,
        _ => Kind::Full,
    }
}

fn read_passphrase(path: &Path) -> Result<Zeroizing<String>, String> {
    let s = Zeroizing::new(
        std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
    );
    let line = Zeroizing::new(s.lines().next().unwrap_or_default().to_owned());
    if line.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(line)
}

/// The passphrase of the next scheduled backup. No error names a value.
async fn passphrase(state: &Shared, cfg: &Config) -> Result<Zeroizing<String>, String> {
    if let Some(p) = &cfg.passphrase_file {
        return read_passphrase(p);
    }
    let name = cfg.key_secret.clone();
    let dir = state.state_dir.clone();
    let opened = state
        .db
        .read(move |c| {
            let key = crate::secrets::key(&dir)?;
            crate::secrets::open(c, &key, crate::secrets::GLOBAL, &name)
        })
        .await
        .map_err(|e| {
            format!(
                "cannot open the backup key `{}`: {}",
                cfg.key_secret, e.message
            )
        })?;
    let Some(o) = opened else {
        return Err(format!(
            "no backup key: a scheduled backup holds every secret and is always sealed; \
             set the global secret `{}` (`terrarium secret set {}`) or {ENV_PASSPHRASE_FILE}",
            cfg.key_secret, cfg.key_secret
        ));
    };
    let v = o
        .values
        .get("passphrase")
        .or_else(|| o.values.get("value"))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            format!(
                "the backup key `{}` has no `passphrase` or `value` field",
                cfg.key_secret
            )
        })?;
    Ok(Zeroizing::new(v.as_str().to_owned()))
}

/// The result of one scheduled run.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub kind: Kind,
    /// The new backup file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub verified: bool,
    /// The deleted backups.
    pub removed: Vec<String>,
    /// Why the run failed. Never a key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The run mailed the operator.
    pub mailed: bool,
}

/// The scheduled backup between runs: the settings and the alert marks.
#[derive(Debug)]
pub struct Runner {
    pub cfg: Config,
    /// A run failed and the operator has the mail: the next failures of
    /// the streak write events only.
    failing: bool,
    /// The due time of the last missed run that the operator has a mail of.
    missed_due: Option<DateTime<Utc>>,
}

impl Runner {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            failing: false,
            missed_due: None,
        }
    }

    /// Make one scheduled backup, verify it, and prune after a verified
    /// full backup.
    pub async fn run(&mut self, state: &Shared) -> Report {
        let r = run_once(state, &self.cfg).await;
        let mut r = match r {
            Ok(r) => r,
            Err((kind, error)) => Report {
                kind,
                name: None,
                verified: false,
                removed: Vec::new(),
                error: Some(error),
                mailed: false,
            },
        };
        match &r.error {
            Some(error) => {
                let first = !self.failing;
                self.failing = true;
                r.mailed = alert_failed(state, r.kind, error, first).await;
            }
            None => {
                if self.failing {
                    record(
                        state,
                        "backup.recovered",
                        json!({"name": r.name}),
                        EventDraft::new(
                            EVENT,
                            "the scheduled backup works again",
                            json!({"change": "recovered", "name": r.name}),
                        ),
                        None,
                    )
                    .await;
                }
                self.failing = false;
            }
        }
        r
    }

    /// Check for a missed run at `now`: no backup and no try (`last_run`)
    /// since one interval and [`MISSED_GRACE`]. One mail per due time.
    /// True when it mailed.
    pub async fn check_missed(
        &mut self,
        state: &Shared,
        now: DateTime<Utc>,
        last_run: Option<DateTime<Utc>>,
    ) -> bool {
        let Some(interval) = self.cfg.interval else {
            return false;
        };
        let dir = crate::api::backups::dir(state);
        let newest = tokio::task::spawn_blocking(move || newest(&dir, false))
            .await
            .ok()
            .flatten();
        let Some(due) = missed(interval, now, newest.max(last_run)) else {
            return false;
        };
        if self.missed_due == Some(due) {
            return false;
        }
        self.missed_due = Some(due);
        let at = due.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        tracing::warn!(due = %at, "a scheduled backup was missed");
        let subject = "Scheduled backup missed";
        let body = format!(
            "The scheduled backup due at {at} did not run (the daemon was down or busy). \
             The newest backup is from {}. A new backup runs soon; \
             check `terrariumd backup ls` and the `backup` events.",
            newest.map_or_else(String::new, |t| t
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        );
        record(
            state,
            "backup.missed",
            json!({"due": at}),
            EventDraft::new(
                EVENT,
                format!("the scheduled backup due at {at} was missed"),
                json!({"change": "missed", "level": "error", "due": at}),
            ),
            Some((subject, &body)),
        )
        .await
    }
}

/// Write the audit row and the event, and with `mail` mail the operator,
/// in one transaction. True when it mailed.
async fn record(
    state: &Shared,
    action: &'static str,
    detail: serde_json::Value,
    event: EventDraft,
    mail_out: Option<(&str, &str)>,
) -> bool {
    let mail_out = mail_out.map(|(s, b)| (s.to_owned(), b.to_owned()));
    let mailed = mail_out.is_some();
    let r = state
        .db
        .write(ACTOR, move |t| {
            t.audit(action, "backups", None, detail)?;
            t.emit(event)?;
            if let Some((subject, body)) = &mail_out {
                mail::send(
                    t,
                    Outgoing {
                        from: &Address::System,
                        to: &Address::Operator,
                        subject,
                        body,
                        thread: None,
                        in_reply_to: None,
                    },
                )?;
            }
            Ok(())
        })
        .await;
    match r {
        Ok(()) => mailed,
        Err(e) => {
            tracing::warn!("cannot record {action}: {}", e.message);
            false
        }
    }
}

/// One run. `Err((kind, error))` on a failure.
async fn run_once(state: &Shared, cfg: &Config) -> Result<Report, (Kind, String)> {
    let dir = crate::api::backups::dir(state);
    let pass = passphrase(state, cfg).await.map_err(|e| (Kind::Full, e))?;
    let kind = {
        let (dir, every, pass) = (dir.clone(), cfg.full_every, pass.clone());
        tokio::task::spawn_blocking(move || kind_for(&dir, Utc::now(), every, &pass))
            .await
            .unwrap_or(Kind::Full)
    };
    let body = CreateBody {
        kind: Some(kind),
        work: cfg.work,
        passphrase: Some(pass.as_str().to_owned()),
        base: None,
    };
    let created = create_queued(state, body, ACTOR)
        .await
        .map_err(|e| (kind, e.message))?;
    let verified = {
        let (path, pass) = (created.path.clone(), pass.clone());
        tokio::task::spawn_blocking(move || {
            let r = super::verify(&path, None, Some(pass.as_str()));
            if r.is_err() {
                // Out of the list: not a base, not a kept backup.
                if let Some(name) = path.file_name() {
                    let aside = path
                        .with_file_name(format!("{UNVERIFIED_PREFIX}{}", name.to_string_lossy()));
                    let _ = std::fs::rename(&path, aside);
                }
            }
            r
        })
        .await
        .map_err(|e| (kind, format!("the verify task failed: {e}")))?
        .map_err(|e| {
            (
                kind,
                format!(
                    "{} does not verify (moved to {UNVERIFIED_PREFIX}{}): {e:#}",
                    created.name, created.name
                ),
            )
        })?
    };
    record(
        state,
        "backup.verify",
        json!({"name": created.name, "id": verified.id, "kind": kind,
               "checked_files": verified.checked_files}),
        EventDraft::new(
            EVENT,
            format!("scheduled {kind} backup {} made and verified", created.name),
            json!({"change": "verified", "name": created.name, "id": verified.id,
                   "kind": kind, "size": created.size,
                   "checked_files": verified.checked_files}),
        ),
        None,
    )
    .await;
    let mut report = Report {
        kind,
        name: Some(created.name.clone()),
        verified: true,
        removed: Vec::new(),
        error: None,
        mailed: false,
    };
    if kind != Kind::Full {
        return Ok(report);
    }
    let (keep, id) = (cfg.keep, verified.id.clone());
    let removed = tokio::task::spawn_blocking(move || super::prune(&dir, keep, &id))
        .await
        .map_err(|e| (kind, format!("the prune task failed: {e}")))?
        .map_err(|e| (kind, format!("cannot delete the old backups: {e:#}")))?;
    if !removed.is_empty() {
        tracing::info!(removed = ?removed, "old backups deleted");
        record(
            state,
            "backup.prune",
            json!({"removed": removed, "verified": verified.id}),
            EventDraft::new(
                EVENT,
                format!("{} old backups deleted", removed.len()),
                json!({"change": "pruned", "removed": removed, "verified": verified.id}),
            ),
            None,
        )
        .await;
    }
    report.removed = removed;
    Ok(report)
}

/// Record a failed run; `first` (the first of a streak) mails the
/// operator. True when it mailed.
async fn alert_failed(state: &Shared, kind: Kind, error: &str, first: bool) -> bool {
    tracing::error!(kind = %kind, error, "scheduled backup failed");
    let body = format!(
        "The scheduled {kind} backup failed:\n\n{error}\n\n\
         The next try is one interval later. While it fails, no old backup is deleted. \
         The next failures write `backup` events and no mail."
    );
    record(
        state,
        "backup.failed",
        json!({"kind": kind, "error": error}),
        EventDraft::new(
            EVENT,
            format!("scheduled {kind} backup failed"),
            json!({"change": "failed", "level": "error", "kind": kind, "error": error}),
        ),
        first.then_some(("Scheduled backup failed", body.as_str())),
    )
    .await
}

/// Start the task. `None` when the schedule is off.
pub fn spawn(state: Shared, cfg: Config) -> Option<JoinHandle<()>> {
    let Some(interval) = cfg.interval else {
        tracing::info!("scheduled backup is off ({ENV_SCHEDULE})");
        return None;
    };
    Some(tokio::spawn(async move {
        let mut runner = Runner::new(cfg);
        // A failed run counts as a run: the next try is one interval later.
        let mut last_run: Option<DateTime<Utc>> = None;
        loop {
            runner.check_missed(&state, Utc::now(), last_run).await;
            let dir = crate::api::backups::dir(&state);
            let newest = tokio::task::spawn_blocking(move || newest(&dir, false))
                .await
                .ok()
                .flatten();
            let at = next_run(interval, state.started_at, newest.max(last_run));
            tracing::info!(next = %at, "scheduled backup");
            let wait = (at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
            tokio::time::sleep(wait).await;
            last_run = Some(Utc::now());
            runner.run(&state).await;
        }
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::parse(|k| m.get(k).cloned())
    }

    #[test]
    fn parses_the_settings() {
        assert_eq!(parse(&[]).unwrap(), Config::default());
        let c = parse(&[
            (ENV_SCHEDULE, "daily"),
            (ENV_FULL_EVERY, "3d"),
            (ENV_KEEP, "2"),
            (ENV_WORK, "on"),
            (ENV_PASSPHRASE_FILE, "/run/secrets/backup"),
        ])
        .unwrap();
        assert_eq!(c.interval, Some(Duration::from_secs(86_400)));
        assert_eq!(c.full_every, Duration::from_secs(3 * 86_400));
        assert_eq!(c.keep, 2);
        assert!(c.work);
        assert_eq!(
            c.passphrase_file.as_deref(),
            Some(std::path::Path::new("/run/secrets/backup"))
        );
        assert_eq!(parse(&[(ENV_SCHEDULE, "off")]).unwrap().interval, None);
        assert!(parse(&[(ENV_SCHEDULE, "10m")]).is_err());
        assert!(parse(&[(ENV_KEEP, "0")]).is_err());
        assert!(parse(&[(ENV_WORK, "maybe")]).is_err());
        assert_eq!(parse(&[]).unwrap().key_secret, DEFAULT_KEY_SECRET);
        assert_eq!(
            parse(&[(ENV_KEY_SECRET, "ops-backup")]).unwrap().key_secret,
            "ops-backup"
        );
        assert!(parse(&[(ENV_KEY_SECRET, "a b")]).is_err());
    }

    #[test]
    fn finds_a_missed_run() {
        let t0: DateTime<Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        let day = Duration::from_secs(86_400);
        assert_eq!(missed(day, t0, None), None);
        // Due at t0 + 1d; within the grace it is not missed.
        assert_eq!(
            missed(day, t0 + chrono::Duration::minutes(1500), Some(t0)),
            None
        );
        assert_eq!(
            missed(day, t0 + chrono::Duration::hours(26), Some(t0)),
            Some(t0 + chrono::Duration::days(1))
        );
    }

    #[test]
    fn picks_the_time_and_the_kind() {
        let t0: DateTime<Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        let day = Duration::from_secs(86_400);
        // No backup: soon after the start.
        assert_eq!(next_run(day, t0, None), t0 + chrono::Duration::minutes(10));
        // One interval after the newest backup.
        let newest = t0 - chrono::Duration::hours(2);
        assert_eq!(
            next_run(day, t0, Some(newest)),
            newest + chrono::Duration::days(1)
        );
        // A backup long ago: not at once, after the first delay.
        let old = t0 - chrono::Duration::days(30);
        assert_eq!(
            next_run(day, t0, Some(old)),
            t0 + chrono::Duration::minutes(10)
        );
        let week = Duration::from_secs(7 * 86_400);
        assert_eq!(next_kind(t0, None, week), Kind::Full);
        assert_eq!(
            next_kind(t0, Some(t0 - chrono::Duration::days(2)), week),
            Kind::Diff
        );
        assert_eq!(
            next_kind(t0, Some(t0 - chrono::Duration::days(8)), week),
            Kind::Full
        );
    }
}
