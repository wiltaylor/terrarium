//! The harness auto-update (PLAN §6, "Auto-update and pins").
//!
//! A daemon task checks npm for each harness on a schedule
//! (`TERRARIUM_HARNESS_AUTO_UPDATE`: `daily`, the default; `off`; or a
//! duration such as `6h`). The first check is [`FIRST_DELAY`] after the
//! start, or one interval after the last check when that is later, so a
//! restart does not check again at once. Each check has a random delay of
//! up to a tenth of the interval (at most [`MAX_JITTER`]).
//!
//! The policy of each harness (`crate::store::harness_updates`) decides:
//!
//! - `auto` (the default): when npm has a newer release (a major too), the
//!   check installs it into its version dir and switches to it, as
//!   `POST /api/harnesses/{name}/update` does. Running agents keep their
//!   version; each moves at its next start or wake. With
//!   `TERRARIUM_HARNESS_UPDATE_MAIL=on`, the operator gets one mail for
//!   each installed update; with `off` (the default), no mail. A failed
//!   install keeps the active version and mails once for each failed
//!   version, with both values.
//! - `pinned`: no install. `POST /api/harnesses/{name}/pin` sets it (by
//!   default to the previous version, a rollback), and `unpin` removes it.
//!
//! A check skips a harness while a manual update runs (the update lock of
//! `crate::harness_tools::HarnessTools`). The Copilot prompt extension has
//! no policy: a Copilot update reinstalls it into the Copilot home.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::HarnessId;
use tokio::task::JoinHandle;

use crate::api::Shared;
use crate::events::EventDraft;
use crate::harness_versions::{UpdateKind, update_kind};
use crate::store::harness_updates::{self as store, CheckResult, UpdatePolicy, UpdateState};
use crate::store::mail::{self, Address, Outgoing};

/// The env var of the schedule.
pub const ENV_AUTO_UPDATE: &str = "TERRARIUM_HARNESS_AUTO_UPDATE";
/// The env var of the mail for an installed update.
pub const ENV_UPDATE_MAIL: &str = "TERRARIUM_HARNESS_UPDATE_MAIL";
/// The time from the daemon start to the first check.
pub const FIRST_DELAY: Duration = Duration::from_secs(5 * 60);
/// The shortest interval: npm documents are large.
pub const MIN_INTERVAL: Duration = Duration::from_secs(3600);
/// The longest random delay of a check.
pub const MAX_JITTER: Duration = Duration::from_secs(30 * 60);
/// The interval of `daily`.
pub const DAILY: Duration = Duration::from_secs(86_400);
/// The dashboard page of the harness versions, for the mails.
pub const PAGE: &str = "/settings/subscriptions";

/// Does an installed update send a mail to the operator
/// (`TERRARIUM_HARNESS_UPDATE_MAIL`)? A failed install mails with both
/// values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum UpdateMail {
    /// No mail. The audit row, the `harness` event, and the dashboard
    /// table still show the update.
    #[default]
    Off,
    /// One mail for each installed update.
    On,
}

impl std::fmt::Display for UpdateMail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::On => "on",
        })
    }
}

/// When the background check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// No background check. `POST /api/harnesses/check` still runs one.
    Off,
    /// Every interval.
    Every(Duration),
}

impl Default for Schedule {
    fn default() -> Self {
        Self::Every(DAILY)
    }
}

impl std::str::FromStr for Schedule {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "daily" => Ok(Self::Every(DAILY)),
            "off" | "0" | "false" | "no" => Ok(Self::Off),
            other => {
                let d = crate::settings::parse_duration(other)
                    .map_err(|e| format!("{e}; use `daily`, `off`, or a duration such as 6h"))?;
                if d < MIN_INTERVAL {
                    return Err(format!("`{other}` is shorter than 1h"));
                }
                Ok(Self::Every(d))
            }
        }
    }
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Every(d) if *d == DAILY => f.write_str("daily"),
            Self::Every(d) => {
                let s = d.as_secs();
                if s % 86_400 == 0 {
                    write!(f, "{}d", s / 86_400)
                } else if s % 3600 == 0 {
                    write!(f, "{}h", s / 3600)
                } else if s % 60 == 0 {
                    write!(f, "{}m", s / 60)
                } else {
                    write!(f, "{s}s")
                }
            }
        }
    }
}

/// The longest random delay for `schedule`: a tenth of the interval, at
/// most [`MAX_JITTER`].
pub fn max_jitter(schedule: Schedule) -> Duration {
    match schedule {
        Schedule::Off => Duration::ZERO,
        Schedule::Every(i) => (i / 10).min(MAX_JITTER),
    }
}

/// A random delay up to [`max_jitter`].
pub fn random_jitter(schedule: Schedule) -> Duration {
    let max = max_jitter(schedule).as_secs();
    if max == 0 {
        return Duration::ZERO;
    }
    let r = u64::try_from(ulid::Ulid::new().random() % u128::from(max + 1)).unwrap_or(0);
    Duration::from_secs(r)
}

/// When the next check runs: [`FIRST_DELAY`] after `started`, or one
/// interval after `last` when that is later; then `jitter` more. `None`
/// when the schedule is off.
pub fn next_check(
    schedule: Schedule,
    started: DateTime<Utc>,
    last: Option<DateTime<Utc>>,
    jitter: Duration,
) -> Option<DateTime<Utc>> {
    let Schedule::Every(interval) = schedule else {
        return None;
    };
    let span = |d: Duration| chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX);
    let first = started + span(FIRST_DELAY);
    let due = match last {
        Some(l) => (l + span(interval)).max(first),
        None => first,
    };
    Some(due + span(jitter))
}

/// What a check does with one harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The policy is `pinned` to this version.
    Pinned(String),
    /// No active version in the tools dir: the start-time install owns it.
    NotInstalled,
    /// npm did not answer; why.
    NoLatest(String),
    /// The active version is the latest (or newer).
    Current(String),
    /// Install `to` and switch from `from`.
    Install { from: String, to: String },
}

/// The decision for one harness: its state, its active version, and the
/// latest release on npm.
pub fn decide(st: &UpdateState, active: Option<&str>, latest: &Result<String, String>) -> Decision {
    if st.policy == UpdatePolicy::Pinned {
        return Decision::Pinned(
            st.pinned_version
                .clone()
                .or_else(|| active.map(str::to_owned))
                .unwrap_or_default(),
        );
    }
    let Some(active) = active else {
        return Decision::NotInstalled;
    };
    match latest {
        Err(e) => Decision::NoLatest(e.clone()),
        Ok(l) if update_kind(active, l) == UpdateKind::None => Decision::Current(active.to_owned()),
        Ok(l) => Decision::Install {
            from: active.to_owned(),
            to: l.clone(),
        },
    }
}

/// The release notes of a harness, when the project publishes them in a
/// known place.
pub fn changelog_url(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::Claude => {
            Some("https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md")
        }
        HarnessId::Codex => Some("https://github.com/openai/codex/releases"),
        HarnessId::Copilot => Some("https://github.com/github/copilot-cli/releases"),
        HarnessId::Pi => None,
    }
}

/// The npm page of the versions of `package`.
pub fn npm_page(package: &str) -> String {
    format!("https://www.npmjs.com/package/{package}?activeTab=versions")
}

/// The one-line rollback: a pin to `version`.
pub fn rollback_command(harness: &str, version: &str) -> String {
    format!("terrarium admin harness pin {harness} {version}")
}

/// What an installed update mail says.
#[derive(Debug, Clone)]
pub struct UpdateNotice<'a> {
    pub harness: HarnessId,
    pub package: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    /// The live agents that keep `from` until their next start.
    pub agents_on_old: usize,
    /// The dashboard page.
    pub link: &'a str,
}

/// The subject and the body of the mail for an installed update.
pub fn update_mail(n: &UpdateNotice<'_>) -> (String, String) {
    let h = n.harness;
    let subject = format!("Harness update: {h} {} -> {}", n.from, n.to);
    let mut body = format!(
        "The daemon updated {h} ({}) from {} to {}.\n",
        n.package, n.from, n.to
    );
    if n.agents_on_old > 0 {
        body.push_str(&format!(
            "{} running agent(s) keep {}. Each agent moves to {} at its next start or wake.\n",
            n.agents_on_old, n.from, n.to
        ));
    } else {
        body.push_str(&format!(
            "Each agent starts on {} at its next start or wake.\n",
            n.to
        ));
    }
    body.push('\n');
    match changelog_url(h) {
        Some(u) => body.push_str(&format!("Changelog: {u}\n")),
        None => body.push_str(&format!("Versions: {}\n", npm_page(n.package))),
    }
    body.push_str(&format!(
        "\nTo roll back to {} and stop the auto-update of {h}:\n  {}\n\nOpen: {}\n",
        n.from,
        rollback_command(h.as_str(), n.from),
        n.link
    ));
    (subject, body)
}

/// The subject and the body of the mail for a failed install.
pub fn failure_mail(
    harness: HarnessId,
    package: &str,
    from: &str,
    to: &str,
    error: &str,
    link: &str,
) -> (String, String) {
    let h = harness;
    (
        format!("Harness update failed: {h} {to}"),
        format!(
            "The daemon could not install {h} ({package}) {to}:\n  {error}\n\n{h} stays at \
             {from}. The agents are not affected. The daemon tries again at each check, but it \
             does not send this mail again for {to}.\n\nTo try again now:\n  terrarium admin \
             harness update {h} --version {to}\nTo stop the auto-update of {h}:\n  {}\n\nOpen: \
             {link}\n",
            rollback_command(h.as_str(), from)
        ),
    )
}

/// The `auto_update` object of a row of `GET /api/harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAutoUpdate {
    pub policy: UpdatePolicy,
    /// The version of a `pinned` policy.
    pub pinned_version: Option<String>,
    /// The target of a rollback ([`previous_version`]).
    pub previous: Option<String>,
    /// The latest release on npm at the last check.
    pub latest: Option<String>,
    /// When the last check ran.
    pub checked_at: Option<String>,
    /// The result of the last check.
    pub result: Option<CheckResult>,
    /// One line about the last check.
    pub detail: Option<String>,
    /// The release notes, when known.
    pub changelog: Option<String>,
}

/// The target of a rollback: the version before the last switch, when
/// its version dir is on disk; else the newest version dir under the
/// active version. `on_disk` is oldest first.
pub fn previous_version(
    st: &UpdateState,
    active: Option<&str>,
    on_disk: &[String],
) -> Option<String> {
    if let Some(p) = st
        .previous
        .as_deref()
        .filter(|p| Some(*p) != active && on_disk.iter().any(|v| v == p))
    {
        return Some(p.to_owned());
    }
    let active = Version::parse(active?).ok()?;
    on_disk
        .iter()
        .rfind(|v| Version::parse(v).is_ok_and(|x| x < active))
        .cloned()
}

/// The [`HarnessAutoUpdate`] of one harness.
pub fn row_info(
    harness: HarnessId,
    st: &UpdateState,
    active: Option<&str>,
    on_disk: &[String],
) -> HarnessAutoUpdate {
    HarnessAutoUpdate {
        policy: st.policy,
        pinned_version: st.pinned_version.clone(),
        previous: previous_version(st, active, on_disk),
        latest: st.latest.clone(),
        checked_at: st.checked_at.clone(),
        result: st.check_result,
        detail: st.check_detail.clone(),
        changelog: changelog_url(harness).map(str::to_owned),
    }
}

/// The result of one harness in a check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckItem {
    pub harness: String,
    pub result: CheckResult,
    /// One line about the result.
    pub detail: String,
    /// The active version before the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The active version after an update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The latest release on npm; `null` when npm did not answer.
    pub latest: Option<String>,
    /// True when the check sent a mail to the operator.
    #[serde(default)]
    pub mailed: bool,
}

/// The answer of `POST /api/harnesses/check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    pub checked_at: DateTime<Utc>,
    pub results: Vec<CheckItem>,
}

/// The auto-update of a running daemon: the schedule, the next check, and
/// the lock that lets one check run at a time.
#[derive(Debug)]
pub struct Runtime {
    schedule: Mutex<Schedule>,
    /// `TERRARIUM_HARNESS_UPDATE_MAIL`.
    update_mail: Mutex<UpdateMail>,
    next_at: Mutex<Option<DateTime<Utc>>>,
    /// Held while a check runs.
    pub running: tokio::sync::Mutex<()>,
}

impl Default for Runtime {
    /// No background task: the schedule is `off` until [`spawn`] sets it.
    fn default() -> Self {
        Self {
            schedule: Mutex::new(Schedule::Off),
            update_mail: Mutex::new(UpdateMail::Off),
            next_at: Mutex::new(None),
            running: tokio::sync::Mutex::new(()),
        }
    }
}

impl Runtime {
    pub fn schedule(&self) -> Schedule {
        *self
            .schedule
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Does an installed update send a mail (`TERRARIUM_HARNESS_UPDATE_MAIL`)?
    pub fn update_mail(&self) -> UpdateMail {
        *self
            .update_mail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set `TERRARIUM_HARNESS_UPDATE_MAIL` (the daemon start does it).
    pub fn set_update_mail(&self, mail: UpdateMail) {
        *self
            .update_mail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mail;
    }

    /// The time of the next background check; `None` when it is off.
    pub fn next_at(&self) -> Option<DateTime<Utc>> {
        *self
            .next_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set(&self, schedule: Schedule, next: Option<DateTime<Utc>>) {
        *self
            .schedule
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = schedule;
        *self
            .next_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }
}

/// The `auto_update` object of `GET /api/harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleInfo {
    /// `TERRARIUM_HARNESS_AUTO_UPDATE`: `daily`, `off`, or a duration.
    pub schedule: String,
    /// The next background check; `null` when it is off.
    pub next_check_at: Option<DateTime<Utc>>,
}

/// The [`ScheduleInfo`] of `state`.
pub fn schedule_info(state: &Shared) -> ScheduleInfo {
    let rt = &state.harness_auto_update;
    ScheduleInfo {
        schedule: rt.schedule().to_string(),
        next_check_at: rt.next_at(),
    }
}

/// Write the check of one harness, with its audit row, event, and mail.
#[allow(clippy::too_many_arguments)]
async fn record(
    state: &Shared,
    item: &mut CheckItem,
    at: &str,
    pin: &crate::harness_tools::Pin,
    mail_out: Option<(String, String)>,
    switched: bool,
    failed_version: Option<Option<String>>,
    audit_ok: Option<bool>,
) -> crate::error::ApiResult<()> {
    let it = item.clone();
    let at = at.to_owned();
    let package = pin.package.clone();
    let mailed = state
        .db
        .write("system", move |t| {
            if switched && let Some(to) = &it.to {
                store::record_switch(t, &it.harness, it.from.as_deref(), to)?;
            }
            if let Some(f) = &failed_version {
                store::set_failed(t, &it.harness, f.as_deref())?;
            }
            store::record_check(
                t,
                &it.harness,
                &at,
                it.result,
                &it.detail,
                it.latest.as_deref(),
            )?;
            if let Some(ok) = audit_ok {
                t.audit(
                    "harness.auto_update",
                    &it.harness,
                    None,
                    json!({"package": package, "from": it.from, "to": it.to,
                           "latest": it.latest, "ok": ok, "detail": it.detail}),
                )?;
            }
            if switched && let Some(to) = &it.to {
                t.emit(EventDraft::new(
                    "harness",
                    format!(
                        "{} {} -> {to} (auto-update)",
                        it.harness,
                        it.from.as_deref().unwrap_or("(not installed)")
                    ),
                    json!({"change": "auto_updated", "harness": it.harness, "package": package,
                           "from": it.from, "to": to}),
                ))?;
            }
            let mut mailed = false;
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
                mailed = true;
            }
            Ok(mailed)
        })
        .await?;
    item.mailed = mailed;
    Ok(())
}

/// Run one check of every harness at `now`. Each harness gets a row in
/// the answer; a failure of one does not stop the others. Without a tools
/// dir the answer is empty.
pub async fn run_check(state: &Shared, now: DateTime<Utc>) -> CheckReport {
    let mut results = Vec::new();
    let Some(cfg) = state.harness_tools.config() else {
        return CheckReport {
            checked_at: now,
            results,
        };
    };
    let at = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let link = state.web.dashboard_link(PAGE);
    for pin in cfg.pins.iter().filter(|p| !p.extension) {
        let name = pin.name.clone();
        let key = name.clone();
        let st = match state.db.read(move |c| store::get(c, &key)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(harness = %name, "auto-update: cannot read the policy: {e}");
                continue;
            }
        };
        let active = cfg.active(pin).map(|a| a.version);
        // A pinned harness still asks npm, so the dashboard shows the
        // latest release.
        let latest = state
            .harness_latest
            .lookup(&pin.package, true)
            .await
            .map(|l| l.version);
        let mut item = CheckItem {
            harness: name.clone(),
            result: CheckResult::Skipped,
            detail: String::new(),
            from: active.clone(),
            to: None,
            latest: latest.as_ref().ok().cloned(),
            mailed: false,
        };
        let mut mail_out = None;
        let mut switched = false;
        let mut failed_version = None;
        let mut audit_ok = None;
        match decide(&st, active.as_deref(), &latest) {
            Decision::Pinned(v) => {
                item.result = CheckResult::Pinned;
                item.detail = format!("pinned to {v}; no auto-update");
            }
            Decision::NotInstalled => {
                item.detail = "not installed in the tools dir".into();
            }
            Decision::NoLatest(e) => {
                item.detail = e;
            }
            Decision::Current(v) => {
                item.result = CheckResult::Current;
                item.detail = format!("{v} is the latest release");
            }
            Decision::Install { from, to } => match state.harness_tools.busy.try_lock() {
                Err(_) => {
                    item.detail = "a manual harness update runs; the next check tries again".into();
                }
                Ok(_busy) => {
                    tracing::info!(harness = %name, from, to, "auto-update: installing");
                    match cfg.update(&name, &to, false).await {
                        Ok(u) => {
                            switched = true;
                            audit_ok = Some(true);
                            item.result = CheckResult::Updated;
                            item.to = Some(u.to.clone());
                            // The version cache sees the new version.
                            state.supervisor.harness_versions(true).await;
                            item.detail = format!("{from} -> {}", u.to);
                            let agents_on_old = state
                                .supervisor
                                .harness_runs()
                                .iter()
                                .filter(|r| r.harness == pin.harness)
                                .filter(|r| r.version.as_deref() != Some(u.to.as_str()))
                                .count();
                            if state.harness_auto_update.update_mail() == UpdateMail::On {
                                mail_out = Some(update_mail(&UpdateNotice {
                                    harness: pin.harness,
                                    package: &pin.package,
                                    from: &from,
                                    to: &u.to,
                                    agents_on_old,
                                    link: &link,
                                }));
                            }
                            // GC while the update lock is held.
                            let removed =
                                crate::api::system::collect_garbage(state, cfg.clone()).await;
                            if !removed.is_empty() {
                                tracing::info!(removed = ?removed, "removed unused harness versions");
                            }
                        }
                        Err(f) => {
                            audit_ok = Some(false);
                            item.result = CheckResult::Failed;
                            item.detail = f.message.clone();
                            tracing::warn!(harness = %name, to, "auto-update failed: {}", f.message);
                            if st.failed_version.as_deref() != Some(to.as_str()) {
                                mail_out = Some(failure_mail(
                                    pin.harness,
                                    &pin.package,
                                    &from,
                                    &to,
                                    &f.message,
                                    &link,
                                ));
                                failed_version = Some(Some(to.clone()));
                            }
                        }
                    }
                }
            },
        }
        if let Err(e) = record(
            state,
            &mut item,
            &at,
            pin,
            mail_out,
            switched,
            failed_version,
            audit_ok,
        )
        .await
        {
            tracing::warn!(harness = %name, "auto-update: cannot record the check: {e}");
        }
        results.push(item);
    }
    CheckReport {
        checked_at: now,
        results,
    }
}

/// Start the background check with `schedule`. With `off`, the task only
/// records the schedule and ends.
pub fn spawn(state: Shared, schedule: Schedule) -> JoinHandle<()> {
    tokio::spawn(async move {
        let rt = state.harness_auto_update.clone();
        if schedule == Schedule::Off {
            rt.set(schedule, None);
            tracing::info!("harness auto-update is off ({ENV_AUTO_UPDATE})");
            return;
        }
        let started = state.started_at;
        loop {
            let last = state
                .db
                .read(store::last_check)
                .await
                .ok()
                .flatten()
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|t| t.with_timezone(&Utc));
            let Some(at) = next_check(schedule, started, last, random_jitter(schedule)) else {
                return;
            };
            rt.set(schedule, Some(at));
            tracing::info!(next = %at, schedule = %schedule, "harness auto-update scheduled");
            let wait = (at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
            tokio::time::sleep(wait).await;
            let Ok(_running) = rt.running.try_lock() else {
                // A check from the API runs now: it counts as this one.
                continue;
            };
            let report = run_check(&state, Utc::now()).await;
            for r in &report.results {
                tracing::info!(harness = %r.harness, result = r.result.as_str(), detail = %r.detail, "harness auto-update check");
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_schedule_parses_and_prints() {
        assert_eq!("daily".parse::<Schedule>(), Ok(Schedule::Every(DAILY)));
        assert_eq!("".parse::<Schedule>(), Ok(Schedule::default()));
        assert_eq!("OFF".parse::<Schedule>(), Ok(Schedule::Off));
        assert_eq!(
            "6h".parse::<Schedule>(),
            Ok(Schedule::Every(Duration::from_secs(6 * 3600)))
        );
        assert!("30m".parse::<Schedule>().unwrap_err().contains("shorter"));
        assert!("soon".parse::<Schedule>().unwrap_err().contains("daily"));
        assert_eq!(Schedule::default().to_string(), "daily");
        assert_eq!(Schedule::Off.to_string(), "off");
        assert_eq!("6h".parse::<Schedule>().unwrap().to_string(), "6h");
        assert_eq!("2d".parse::<Schedule>().unwrap().to_string(), "2d");
    }

    #[test]
    fn the_first_check_waits_for_the_start_delay_and_the_last_check() {
        let start = t("2026-10-31T10:00:00Z");
        let daily = Schedule::default();
        // No check before: a few minutes after the start.
        assert_eq!(
            next_check(daily, start, None, Duration::ZERO),
            Some(t("2026-10-31T10:05:00Z"))
        );
        // A check long ago: also at the start delay.
        assert_eq!(
            next_check(
                daily,
                start,
                Some(t("2026-10-20T00:00:00Z")),
                Duration::ZERO
            ),
            Some(t("2026-10-31T10:05:00Z"))
        );
        // A recent check: one interval after it, plus the jitter.
        assert_eq!(
            next_check(
                daily,
                start,
                Some(t("2026-10-31T08:00:00Z")),
                Duration::from_secs(60)
            ),
            Some(t("2026-11-01T08:01:00Z"))
        );
        assert_eq!(next_check(Schedule::Off, start, None, Duration::ZERO), None);
    }

    #[test]
    fn the_jitter_is_a_tenth_of_the_interval_at_most_30_minutes() {
        assert_eq!(max_jitter(Schedule::default()), MAX_JITTER);
        assert_eq!(
            max_jitter(Schedule::Every(Duration::from_secs(3600))),
            Duration::from_secs(360)
        );
        assert_eq!(max_jitter(Schedule::Off), Duration::ZERO);
        for _ in 0..50 {
            assert!(random_jitter(Schedule::default()) <= MAX_JITTER);
        }
    }

    fn st(policy: UpdatePolicy, pinned: Option<&str>) -> UpdateState {
        UpdateState {
            policy,
            pinned_version: pinned.map(str::to_owned),
            ..UpdateState::new("codex")
        }
    }

    #[test]
    fn decide_installs_newer_and_skips_a_pin() {
        let auto = st(UpdatePolicy::Auto, None);
        let ok = |v: &str| Ok(v.to_owned());
        assert_eq!(
            decide(&auto, Some("0.164.0"), &ok("0.165.0")),
            Decision::Install {
                from: "0.164.0".into(),
                to: "0.165.0".into()
            },
            "a major too"
        );
        assert_eq!(
            decide(&auto, Some("0.164.0"), &ok("0.164.0")),
            Decision::Current("0.164.0".into())
        );
        assert_eq!(
            decide(&auto, Some("0.165.0"), &ok("0.164.0")),
            Decision::Current("0.165.0".into()),
            "never a downgrade"
        );
        assert_eq!(decide(&auto, None, &ok("0.165.0")), Decision::NotInstalled);
        assert_eq!(
            decide(&auto, Some("0.164.0"), &Err("npm down".into())),
            Decision::NoLatest("npm down".into())
        );
        let pinned = st(UpdatePolicy::Pinned, Some("0.164.0"));
        assert_eq!(
            decide(&pinned, Some("0.164.0"), &ok("0.165.0")),
            Decision::Pinned("0.164.0".into())
        );
    }

    #[test]
    fn previous_is_the_last_switch_else_the_newest_older_dir() {
        let disk: Vec<String> = ["0.163.0", "0.164.0", "0.165.0"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut s = st(UpdatePolicy::Auto, None);
        assert_eq!(
            previous_version(&s, Some("0.165.0"), &disk).as_deref(),
            Some("0.164.0")
        );
        // After a rollback, the previous version is the newer one.
        s.previous = Some("0.165.0".into());
        assert_eq!(
            previous_version(&s, Some("0.164.0"), &disk).as_deref(),
            Some("0.165.0")
        );
        // A previous version that is gone from the disk does not count.
        s.previous = Some("0.160.0".into());
        assert_eq!(
            previous_version(&s, Some("0.164.0"), &disk).as_deref(),
            Some("0.163.0")
        );
        assert_eq!(previous_version(&s, Some("0.163.0"), &disk), None);
        assert_eq!(previous_version(&s, None, &disk), None);
    }

    #[test]
    fn the_update_mail_names_the_versions_and_the_way_back() {
        let pin = terrarium_core::harnesses::pin(HarnessId::Codex);
        let n = UpdateNotice {
            harness: HarnessId::Codex,
            package: pin.package,
            from: pin.version,
            to: "99.0.0",
            agents_on_old: 2,
            link: "http://x/settings/subscriptions",
        };
        let (subject, body) = update_mail(&n);
        assert_eq!(
            subject,
            format!("Harness update: codex {} -> 99.0.0", pin.version)
        );
        // Any version is good: no range and no warning.
        assert!(!body.contains("WARNING"), "{body}");
        assert!(!body.contains("range"), "{body}");
        assert!(body.contains("2 running agent(s) keep"), "{body}");
        assert!(
            body.contains("Changelog: https://github.com/openai/codex/releases"),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "\n  terrarium admin harness pin codex {}\n",
                pin.version
            )),
            "{body}"
        );
        assert!(body.ends_with("Open: http://x/settings/subscriptions\n"));

        // pi has no known changelog.
        let pi = terrarium_core::harnesses::pin(HarnessId::Pi);
        let (_, body) = update_mail(&UpdateNotice {
            harness: HarnessId::Pi,
            package: pi.package,
            from: "0.0.1",
            to: pi.version,
            agents_on_old: 0,
            link: "l",
        });
        assert!(
            body.contains("Versions: https://www.npmjs.com/package/"),
            "{body}"
        );
    }

    #[test]
    fn the_update_mail_setting_parses_and_is_off_by_default() {
        use clap::ValueEnum;
        assert_eq!(UpdateMail::default(), UpdateMail::Off);
        assert_eq!(UpdateMail::from_str("on", true), Ok(UpdateMail::On));
        assert_eq!(UpdateMail::from_str("off", true), Ok(UpdateMail::Off));
        assert!(UpdateMail::from_str("loud", true).is_err());
        assert_eq!(UpdateMail::On.to_string(), "on");
        assert_eq!(Runtime::default().update_mail(), UpdateMail::Off);
    }

    #[test]
    fn the_failure_mail_names_the_version_and_the_way_out() {
        let (s, b) = failure_mail(
            HarnessId::Claude,
            "@anthropic-ai/claude-code",
            "2.1.310",
            "2.1.311",
            "npm install -g x failed (exit 1)",
            "l",
        );
        assert_eq!(s, "Harness update failed: claude 2.1.311");
        assert!(b.contains("claude stays at 2.1.310"), "{b}");
        assert!(
            b.contains("terrarium admin harness pin claude 2.1.310"),
            "{b}"
        );
        assert!(
            b.contains("does not send this mail again for 2.1.311"),
            "{b}"
        );
    }
}
