//! The scheduled backup (`TERRARIUM_BACKUP_SCHEDULE`, docs/api.md §32.4).
//!
//! Off by default. With an interval (`daily`, `12h`, ...), a daemon task
//! makes a backup one interval after the newest backup in the backup dir
//! (or [`FIRST_DELAY`] after the start, when that is later). The task
//! makes a full backup when the dir has none, or when the newest full
//! backup is older than `TERRARIUM_BACKUP_FULL_EVERY` (default `7d`);
//! else a diff of the newest full backup. Then it keeps the newest
//! `TERRARIUM_BACKUP_KEEP` full backups (default `4`) and their diffs, and
//! deletes the rest.
//!
//! `TERRARIUM_BACKUP_WORK=on` adds the workspaces.
//! `TERRARIUM_BACKUP_PASSPHRASE_FILE` seals each backup with the first
//! line of that file.
//!
//! The project schedules (`crate::scheduler`) start agent turns in one
//! project; a backup is of the whole daemon, so it has its own task, as
//! the harness auto-update has.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use tokio::task::JoinHandle;

use super::Kind;
use crate::api::Shared;
use crate::api::backups::{CreateBody, create_online};

pub const ENV_SCHEDULE: &str = "TERRARIUM_BACKUP_SCHEDULE";
pub const ENV_FULL_EVERY: &str = "TERRARIUM_BACKUP_FULL_EVERY";
pub const ENV_KEEP: &str = "TERRARIUM_BACKUP_KEEP";
pub const ENV_WORK: &str = "TERRARIUM_BACKUP_WORK";
pub const ENV_PASSPHRASE_FILE: &str = "TERRARIUM_BACKUP_PASSPHRASE_FILE";
/// The time from the start to the first backup, at the earliest.
pub const FIRST_DELAY: Duration = Duration::from_secs(10 * 60);
/// The shortest interval.
pub const MIN_INTERVAL: Duration = Duration::from_secs(3600);
/// The audit actor of a scheduled backup.
pub const ACTOR: &str = "system:backup";

/// The settings of the scheduled backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `None`: off.
    pub interval: Option<Duration>,
    pub full_every: Duration,
    pub keep: usize,
    pub work: bool,
    pub passphrase_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: None,
            full_every: Duration::from_secs(7 * 86_400),
            keep: 4,
            work: false,
            passphrase_file: None,
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

fn newest(dir: &std::path::Path, full_only: bool) -> Option<DateTime<Utc>> {
    super::list(dir)
        .ok()?
        .into_iter()
        .filter_map(|l| l.header)
        .filter(|h| !full_only || h.kind == Kind::Full)
        .map(|h| h.created_at)
        .max()
}

fn read_passphrase(path: &std::path::Path) -> Result<String, String> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let line = s.lines().next().unwrap_or_default().to_owned();
    if line.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(line)
}

/// Make one scheduled backup, then prune.
async fn run_once(state: &Shared, cfg: &Config) {
    let dir = crate::api::backups::dir(state);
    let kind = {
        let dir = dir.clone();
        let every = cfg.full_every;
        tokio::task::spawn_blocking(move || next_kind(Utc::now(), newest(&dir, true), every))
            .await
            .unwrap_or(Kind::Full)
    };
    let passphrase = match &cfg.passphrase_file {
        None => None,
        Some(p) => match read_passphrase(p) {
            Ok(p) => Some(p),
            Err(e) => {
                fail(state, kind, &e).await;
                return;
            }
        },
    };
    let body = CreateBody {
        kind: Some(kind),
        work: cfg.work,
        passphrase,
        base: None,
    };
    if let Err(e) = create_online(state, body, ACTOR).await {
        fail(state, kind, &e.message).await;
        return;
    }
    let keep = cfg.keep;
    match tokio::task::spawn_blocking(move || super::prune(&dir, keep)).await {
        Ok(Ok(removed)) if !removed.is_empty() => {
            tracing::info!(removed = ?removed, "old backups deleted");
            let r = state
                .db
                .write(ACTOR, move |t| {
                    t.audit("backup.prune", "backups", None, json!({"removed": removed}))
                })
                .await;
            if let Err(e) = r {
                tracing::warn!("cannot write the audit row of backup.prune: {}", e.message);
            }
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("cannot delete the old backups: {e:#}"),
        Err(e) => tracing::warn!("backup prune task failed: {e}"),
    }
}

async fn fail(state: &Shared, kind: Kind, error: &str) {
    tracing::error!(kind = %kind, error, "scheduled backup failed");
    let error = error.to_owned();
    let r = state
        .db
        .write(ACTOR, move |t| {
            t.audit(
                "backup.failed",
                "backups",
                None,
                json!({"kind": kind, "error": error}),
            )
        })
        .await;
    if let Err(e) = r {
        tracing::warn!("cannot write the audit row of backup.failed: {}", e.message);
    }
}

/// Start the task. `None` when the schedule is off.
pub fn spawn(state: Shared, cfg: Config) -> Option<JoinHandle<()>> {
    let Some(interval) = cfg.interval else {
        tracing::info!("scheduled backup is off ({ENV_SCHEDULE})");
        return None;
    };
    Some(tokio::spawn(async move {
        // A failed run counts as a run: the next try is one interval later.
        let mut last_run: Option<DateTime<Utc>> = None;
        loop {
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
            run_once(&state, &cfg).await;
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
