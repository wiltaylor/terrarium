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
//! when that is set (owned by the daemon user, no access for the group
//! or others), else the global secret `TERRARIUM_BACKUP_KEY_SECRET`
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
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use super::Kind;
use crate::api::Shared;
use crate::api::backups::{CreateBody, create_held, lock_queued};
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
/// The marks of the schedule ([`Marks`]) in the backup dir. The list
/// skips it.
pub const MARKS_FILE: &str = ".schedule.json";
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

/// The kind of the next scheduled backup in `dir` with `passphrase`, and
/// the base file name of a diff: as [`next_kind`], and full when the
/// newest full backup is not sealed or the passphrase does not open it
/// (the key was rotated): a diff and its base always have the same key.
/// The caller holds the backup lock until the backup is made, so the
/// base is still the newest full backup then.
pub fn plan(
    dir: &Path,
    now: DateTime<Utc>,
    full_every: Duration,
    passphrase: &str,
) -> (Kind, Option<String>) {
    let Ok(Some(path)) = super::newest_full(dir) else {
        return (Kind::Full, None);
    };
    let Ok(h) = super::read_header(&path) else {
        return (Kind::Full, None);
    };
    if next_kind(now, Some(h.created_at), full_every) == Kind::Full {
        return (Kind::Full, None);
    }
    let base = path.file_name().map(|n| n.to_string_lossy().into_owned());
    match &h.encryption {
        Some(e) if base.is_some() && super::SealKey::open(passphrase, &h.id, e).is_ok() => {
            (Kind::Diff, base)
        }
        _ => (Kind::Full, None),
    }
}

/// Refuse a passphrase file that another user owns, or that the group or
/// others can read or change: it holds the key of every backup.
fn check_key_file(path: &Path, mode: u32, owner: u32, me: u32) -> Result<(), String> {
    if owner != me {
        return Err(format!(
            "{} is owned by uid {owner}, not by the daemon user (uid {me}): \
             chown it to the daemon user and chmod 600 it",
            path.display()
        ));
    }
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} has mode {:04o}: the group or others can read or change the backup key; \
             chmod 600 it",
            path.display(),
            mode & 0o7777
        ));
    }
    Ok(())
}

fn read_passphrase(path: &Path) -> Result<Zeroizing<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let cannot = |e: std::io::Error| format!("cannot read {}: {e}", path.display());
    let mut f = std::fs::File::open(path).map_err(cannot)?;
    // The mode of the opened file, not of the path: no swap in between.
    let meta = f.metadata().map_err(cannot)?;
    check_key_file(
        path,
        meta.mode(),
        meta.uid(),
        nix::unistd::geteuid().as_raw(),
    )?;
    let cap = usize::try_from(meta.len()).unwrap_or(0).saturating_add(1);
    let mut s = Zeroizing::new(String::with_capacity(cap));
    f.read_to_string(&mut s).map_err(cannot)?;
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

/// The marks of the schedule that outlive a restart, in [`MARKS_FILE`]
/// in the backup dir. The file goes when the schedule is off.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marks {
    /// The schedule is on since then: no run before it is missed.
    #[serde(default)]
    pub on_since: Option<DateTime<Utc>>,
    /// The last try of a run, also a failed one.
    #[serde(default)]
    pub last_run: Option<DateTime<Utc>>,
    /// The due time of the last missed run that the operator has a mail of.
    #[serde(default)]
    pub missed_due: Option<DateTime<Utc>>,
}

impl Marks {
    /// The marks in `dir`; none when the file is absent or bad.
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(MARKS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write the marks to `dir` (a temp file, then a rename).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{MARKS_FILE}.tmp"));
        std::fs::write(
            &tmp,
            serde_json::to_vec(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, dir.join(MARKS_FILE))
    }
}

/// Change the marks in `dir` in a blocking task. A failed write is
/// logged: the cost is a mail too many after a restart.
async fn update_marks(dir: PathBuf, f: impl FnOnce(&mut Marks) + Send + 'static) -> Marks {
    tokio::task::spawn_blocking(move || {
        let mut m = Marks::load(&dir);
        f(&mut m);
        if let Err(e) = m.save(&dir) {
            tracing::warn!("cannot write {}: {e}", dir.join(MARKS_FILE).display());
        }
        m
    })
    .await
    .unwrap_or_default()
}

/// The check of a new backup: [`super::verify`].
pub type VerifyFn = fn(&Path, Option<&Path>, Option<&str>) -> anyhow::Result<super::Verified>;

/// The scheduled backup between runs: the settings and the alert mark.
#[derive(Debug)]
pub struct Runner {
    pub cfg: Config,
    /// The check of each new backup. Tests put a failing one here.
    pub verify: VerifyFn,
    /// A run failed and the operator has the mail: the next failures of
    /// the streak write events only.
    failing: bool,
}

impl Runner {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            verify: super::verify,
            failing: false,
        }
    }

    /// Make one scheduled backup, verify it, and prune after a verified
    /// full backup.
    pub async fn run(&mut self, state: &Shared) -> Report {
        let now = Utc::now();
        update_marks(crate::api::backups::dir(state), move |m| {
            m.on_since.get_or_insert(now);
            m.last_run = Some(now);
        })
        .await;
        let r = run_once(state, &self.cfg, self.verify).await;
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

    /// Check for a missed run at `now`: no backup and no try (`last_run`,
    /// or the last try in the [`Marks`]) since one interval and
    /// [`MISSED_GRACE`]. The first check of a schedule that was off marks
    /// it on and finds nothing: an old manual backup is not a missed run.
    /// One mail per due time, also over a restart. True when it mailed.
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
        let (newest, marks) = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || (newest(&dir, false), Marks::load(&dir)))
                .await
                .unwrap_or_default()
        };
        let Some(on_since) = marks.on_since else {
            update_marks(dir, move |m| {
                m.on_since.get_or_insert(now);
            })
            .await;
            return false;
        };
        let since = newest.max(last_run).max(marks.last_run).max(Some(on_since));
        let Some(due) = missed(interval, now, since) else {
            return false;
        };
        if marks.missed_due.is_some_and(|m| m >= due) {
            return false;
        }
        update_marks(dir, move |m| m.missed_due = Some(due)).await;
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

/// Take a backup that did not verify out of the list, so it is no base
/// and no kept backup: rename it to `.unverified-<name>` (with `-<n>` at
/// the end when that is taken: a later backup can have the same name),
/// else delete it. Answers what was done; an error when the file stays
/// in the list. The caller holds the backup lock.
pub fn set_aside(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut aside = format!("{UNVERIFIED_PREFIX}{name}");
    let mut n = 1;
    while n < 1000 && std::fs::symlink_metadata(path.with_file_name(&aside)).is_ok() {
        n += 1;
        aside = format!("{UNVERIFIED_PREFIX}{name}-{n}");
    }
    set_aside_to(path, &aside)
}

fn set_aside_to(path: &Path, aside: &str) -> Result<String, String> {
    match std::fs::rename(path, path.with_file_name(aside)) {
        Ok(()) => Ok(format!("moved to {aside}")),
        Err(re) => match std::fs::remove_file(path) {
            Ok(()) => Ok(format!("deleted: cannot move it to {aside}: {re}")),
            Err(de) => Err(format!(
                "cannot move it to {aside} ({re}) or delete it ({de}): \
                 it is still in the backup dir; delete it by hand"
            )),
        },
    }
}

/// One run. `Err((kind, error))` on a failure.
async fn run_once(
    state: &Shared,
    cfg: &Config,
    verify: VerifyFn,
) -> Result<Report, (Kind, String)> {
    let dir = crate::api::backups::dir(state);
    let pass = passphrase(state, cfg).await.map_err(|e| (Kind::Full, e))?;
    // Held from the choice of the kind and the base to the prune: a
    // manual backup in between would change the newest full backup, and
    // the prune would refuse (a false alert).
    let held = lock_queued(state).await;
    let (kind, base) = {
        let (dir, every, pass) = (dir.clone(), cfg.full_every, pass.clone());
        tokio::task::spawn_blocking(move || plan(&dir, Utc::now(), every, &pass))
            .await
            .unwrap_or((Kind::Full, None))
    };
    let body = CreateBody {
        kind: Some(kind),
        work: cfg.work,
        passphrase: Some(pass.as_str().to_owned()),
        base,
    };
    let created = create_held(state, &held, body, ACTOR)
        .await
        .map_err(|e| (kind, e.message))?;
    let verified = {
        let (path, pass, name) = (created.path.clone(), pass.clone(), created.name.clone());
        tokio::task::spawn_blocking(move || {
            verify(&path, None, Some(pass.as_str())).map_err(|e| {
                let aside = set_aside(&path).unwrap_or_else(|e| e);
                format!("{name} does not verify ({aside}): {e:#}")
            })
        })
        .await
        .map_err(|e| (kind, format!("the verify task failed: {e}")))?
        .map_err(|e| (kind, e))?
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
    drop(held);
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
        // A later turn-on starts fresh: an old backup is not missed then.
        let marks = crate::api::backups::dir(&state).join(MARKS_FILE);
        if let Err(e) = std::fs::remove_file(&marks)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("cannot delete {}: {e}", marks.display());
        }
        return None;
    };
    Some(tokio::spawn(async move {
        let mut runner = Runner::new(cfg);
        loop {
            // The last try is in the marks: a failed run counts as a
            // run, and the next try is one interval later.
            runner.check_missed(&state, Utc::now(), None).await;
            let dir = crate::api::backups::dir(&state);
            let (newest, marks) =
                tokio::task::spawn_blocking(move || (newest(&dir, false), Marks::load(&dir)))
                    .await
                    .unwrap_or_default();
            let at = next_run(interval, state.started_at, newest.max(marks.last_run));
            tracing::info!(next = %at, "scheduled backup");
            let wait = (at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
            tokio::time::sleep(wait).await;
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
    fn refuses_a_key_file_of_another_user_or_open_to_others() {
        let p = Path::new("/k");
        check_key_file(p, 0o100_600, 7, 7).unwrap();
        check_key_file(p, 0o100_400, 7, 7).unwrap();
        let e = check_key_file(p, 0o100_640, 7, 7).unwrap_err();
        assert!(e.contains("0640") && e.contains("chmod 600"), "{e}");
        assert!(check_key_file(p, 0o100_604, 7, 7).is_err());
        assert!(check_key_file(p, 0o100_620, 7, 7).is_err());
        let e = check_key_file(p, 0o100_600, 0, 7).unwrap_err();
        assert!(e.contains("uid 0"), "{e}");
    }

    #[test]
    fn reads_the_first_line_of_a_private_key_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("pass");
        std::fs::write(&f, "pass phrase one\nrest\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_passphrase(&f).unwrap().as_str(), "pass phrase one");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = read_passphrase(&f).unwrap_err();
        assert!(e.contains("0644") && !e.contains("pass phrase"), "{e}");
    }

    #[test]
    fn sets_aside_a_bad_backup_or_deletes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.zip");
        std::fs::write(&f, "bad").unwrap();
        let r = set_aside(&f).unwrap();
        assert!(r.starts_with("moved"), "{r}");
        assert!(!f.exists());
        assert!(
            tmp.path()
                .join(format!("{UNVERIFIED_PREFIX}a.zip"))
                .exists()
        );
        // A later bad backup with the same name keeps the first one.
        std::fs::write(&f, "bad 2").unwrap();
        let r = set_aside(&f).unwrap();
        assert_eq!(r, format!("moved to {UNVERIFIED_PREFIX}a.zip-2"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(format!("{UNVERIFIED_PREFIX}a.zip"))).unwrap(),
            "bad"
        );
        // The rename fails (a dir with content has the name): delete it.
        let g = tmp.path().join("b.zip");
        std::fs::write(&g, "bad").unwrap();
        let aside = tmp.path().join(format!("{UNVERIFIED_PREFIX}b.zip"));
        std::fs::create_dir(&aside).unwrap();
        std::fs::write(aside.join("x"), "x").unwrap();
        let r = set_aside_to(&g, &format!("{UNVERIFIED_PREFIX}b.zip")).unwrap();
        assert!(r.starts_with("deleted"), "{r}");
        assert!(!g.exists());
    }

    #[test]
    fn keeps_the_marks_in_the_backup_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backups");
        assert_eq!(Marks::load(&dir), Marks::default());
        let t: DateTime<Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        let m = Marks {
            on_since: Some(t),
            last_run: Some(t),
            missed_due: Some(t),
        };
        m.save(&dir).unwrap();
        assert_eq!(Marks::load(&dir), m);
        // Not a backup in the list.
        assert!(super::super::list(&dir).unwrap().is_empty());
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
