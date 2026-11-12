//! The self-update of Terrarium (docs/api.md §34, PLAN §23.8).
//!
//! The daemon never holds `docker.sock`. It asks the updater
//! (`terrarium-updater`, a service of its own in compose.yaml) over the
//! updater socket, and the updater moves the terrarium container to a
//! release image, with a backup and a way back. This module is the daemon
//! side:
//!
//! - **Releases.** The GitHub releases of `TERRARIUM_UPDATE_REPO`
//!   (default `wiltaylor/terrarium`), with a cache of [`CACHE_TTL`]. The
//!   newest release of the channel (`TERRARIUM_UPDATE_CHANNEL`: `stable`,
//!   the default, or `pre`) is an update when its version is newer than
//!   the crate version of the daemon.
//! - **Policy** (`TERRARIUM_SELF_UPDATE`): `off` (no background check, no
//!   mail; the API still updates on request), `notify` (the default: a
//!   check every [`CHECK_INTERVAL`] and one Inbox mail for each new
//!   version), or `auto` (`notify`, and an update inside a global
//!   calendar event of kind `maintenance`, at most once for each
//!   occurrence).
//! - **Drain.** The updater asks for it before it stops the container: no
//!   agent wakes, and no new turn goes to a running agent (the stream gate
//!   is closed, `crate::stream_limit`); the running turns end. No job of
//!   the job queue starts, and the running jobs go on (`crate::jobs`). The drain
//!   lives in memory: the new daemon starts without it, and
//!   `POST /api/system/update/resume` ends it.
//! - **Drain of the jobs** (TER-614 R4). Before an update, the updater or
//!   the operator drains the job slots only (`POST
//!   /api/system/update/drain` with `{"jobs_only": true}`): no job of the
//!   job queue starts, the running jobs finish, and the agents go on. The
//!   queued jobs wait. It lives in memory as the drain does, and the
//!   resume ends it too.
//! - **Results.** The background task reads the history of the updater
//!   and mails each result once (`self_update_state`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::AgentStatus;
use terrarium_updater::protocol::{Outcome, Request, Response, Run, RunKind, Status};
use tokio::task::JoinHandle;

use crate::api::Shared;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult, Code};
use crate::store::mail::{self, Address, Outgoing};

pub const ENV_POLICY: &str = "TERRARIUM_SELF_UPDATE";
pub const ENV_CHANNEL: &str = "TERRARIUM_UPDATE_CHANNEL";
pub const ENV_REPO: &str = "TERRARIUM_UPDATE_REPO";
pub const ENV_RELEASES_URL: &str = "TERRARIUM_UPDATE_RELEASES_URL";
pub const DEFAULT_REPO: &str = "wiltaylor/terrarium";
/// How long a releases answer is good.
pub const CACHE_TTL: Duration = Duration::from_secs(3600);
/// The background check of the releases.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// The tick of the background task: the results, the window.
pub const TICK: Duration = Duration::from_secs(60);
/// The first tick after the start.
pub const FIRST_TICK: Duration = Duration::from_secs(30);
/// The dashboard page.
pub const PAGE: &str = "/settings/software";
/// The audit actor and the run actor of an auto-update.
pub const AUTO_ACTOR: &str = "system:auto-update";

/// `TERRARIUM_SELF_UPDATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    Off,
    #[default]
    Notify,
    Auto,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Notify => "notify",
            Self::Auto => "auto",
        }
    }
}

impl std::str::FromStr for Policy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "notify" => Ok(Self::Notify),
            "off" | "0" | "false" | "no" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            o => Err(format!("{ENV_POLICY}: `{o}`: use off, notify, or auto")),
        }
    }
}

/// `TERRARIUM_UPDATE_CHANNEL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    #[default]
    Stable,
    Pre,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Pre => "pre",
        }
    }

    /// The image tag of the newest release of the channel.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Stable => "latest",
            Self::Pre => "pre",
        }
    }
}

impl std::str::FromStr for Channel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "stable" => Ok(Self::Stable),
            "pre" | "prerelease" | "pre-release" => Ok(Self::Pre),
            o => Err(format!("{ENV_CHANNEL}: `{o}`: use stable or pre")),
        }
    }
}

/// The settings of the self-update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub policy: Policy,
    pub channel: Channel,
    /// The updater socket.
    pub socket: PathBuf,
    /// The GitHub releases API URL.
    pub releases_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            policy: Policy::Notify,
            channel: Channel::Stable,
            socket: PathBuf::from(terrarium_updater::protocol::DEFAULT_SOCKET),
            releases_url: releases_url(DEFAULT_REPO),
        }
    }
}

/// The releases API URL of `owner/repo`.
pub fn releases_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases?per_page=30")
}

impl Config {
    /// The settings from the env. A bad value stops the start.
    pub fn from_env() -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let repo = Some(var(ENV_REPO))
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REPO.into());
        Ok(Self {
            policy: var(ENV_POLICY).parse()?,
            channel: var(ENV_CHANNEL).parse()?,
            socket: Some(var(terrarium_updater::protocol::ENV_SOCKET))
                .filter(|s| !s.trim().is_empty())
                .map_or_else(
                    || PathBuf::from(terrarium_updater::protocol::DEFAULT_SOCKET),
                    PathBuf::from,
                ),
            releases_url: Some(var(ENV_RELEASES_URL))
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| releases_url(repo.trim())),
        })
    }
}

/// One GitHub release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// The version, without the `v` of the tag.
    pub version: String,
    pub tag: String,
    pub name: String,
    pub prerelease: bool,
    pub published_at: Option<String>,
    /// The release notes (Markdown).
    pub notes: String,
    pub url: String,
}

/// The releases of a GitHub answer that have a semver tag; no drafts.
pub fn parse_releases(v: &serde_json::Value) -> Vec<Release> {
    let Some(items) = v.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|r| r["draft"].as_bool() != Some(true))
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            let version = tag.strip_prefix('v').unwrap_or(tag);
            Version::parse(version).ok()?;
            Some(Release {
                version: version.to_owned(),
                tag: tag.to_owned(),
                name: r["name"].as_str().unwrap_or(tag).to_owned(),
                prerelease: r["prerelease"].as_bool().unwrap_or(false),
                published_at: r["published_at"].as_str().map(str::to_owned),
                notes: r["body"].as_str().unwrap_or_default().to_owned(),
                url: r["html_url"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// The newest release of `channel`: `stable` takes no pre-release; `pre`
/// takes both.
pub fn newest(releases: &[Release], channel: Channel) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| channel == Channel::Pre || !r.prerelease)
        .filter_map(|r| Version::parse(&r.version).ok().map(|v| (v, r)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, r)| r)
}

/// Is `candidate` newer than `current` (the crate version of the daemon)?
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Ok(c), Ok(n)) => c > n,
        _ => false,
    }
}

/// The releases cache.
#[derive(Debug, Clone, Default)]
struct Cache {
    fetched: Option<std::time::Instant>,
    checked_at: Option<DateTime<Utc>>,
    releases: Vec<Release>,
    error: Option<String>,
}

/// The drain of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Drain {
    pub since: DateTime<Utc>,
    pub by: String,
}

/// The self-update of a running daemon (`AppState::self_update`).
#[derive(Debug, Default)]
pub struct Runtime {
    cfg: Mutex<Config>,
    cache: Mutex<Cache>,
    drain: Mutex<Option<Drain>>,
    /// The drain of the job slots only (TER-614 R4).
    jobs_drain: Mutex<Option<Drain>>,
    /// Held while the releases are fetched.
    fetching: tokio::sync::Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Runtime {
    pub fn config(&self) -> Config {
        lock(&self.cfg).clone()
    }

    pub fn set_config(&self, cfg: Config) {
        *lock(&self.cfg) = cfg;
    }

    /// True while the daemon drains.
    pub fn draining(&self) -> bool {
        lock(&self.drain).is_some()
    }

    pub fn drain_state(&self) -> Option<Drain> {
        lock(&self.drain).clone()
    }

    /// True while no job of the job queue may start: the daemon drains,
    /// or its job slots drain (TER-614 R4).
    pub fn jobs_draining(&self) -> bool {
        self.draining() || lock(&self.jobs_drain).is_some()
    }

    pub fn jobs_drain_state(&self) -> Option<Drain> {
        lock(&self.jobs_drain).clone()
    }

    /// Set the releases (the tests; a fetch).
    pub fn set_releases(&self, releases: Vec<Release>, at: DateTime<Utc>) {
        let mut c = lock(&self.cache);
        c.fetched = Some(std::time::Instant::now());
        c.checked_at = Some(at);
        c.releases = releases;
        c.error = None;
    }

    fn fresh(&self, ttl: Duration) -> bool {
        lock(&self.cache).fetched.is_some_and(|f| f.elapsed() < ttl)
    }
}

/// Fetch the releases when the cache is older than `ttl` (`ZERO`: now).
pub async fn refresh(state: &Shared, ttl: Duration) {
    let rt = &state.self_update;
    if rt.fresh(ttl) {
        return;
    }
    let _f = rt.fetching.lock().await;
    if ttl > Duration::ZERO && rt.fresh(ttl) {
        return;
    }
    let url = rt.config().releases_url;
    let got = fetch(&url).await;
    let mut c = lock(&rt.cache);
    c.fetched = Some(std::time::Instant::now());
    c.checked_at = Some(Utc::now());
    match got {
        Ok(r) => {
            c.releases = r;
            c.error = None;
        }
        Err(e) => {
            tracing::warn!("cannot read the releases: {e}");
            c.error = Some(e);
        }
    }
}

async fn fetch(url: &str) -> Result<Vec<Release>, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("terrariumd/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(parse_releases(&v))
}

/// Ask the updater.
pub async fn ask(state: &Shared, req: &Request) -> Result<Response, String> {
    let socket = state.self_update.config().socket;
    terrarium_updater::client::call(&socket, req)
        .await
        .map_err(|e| e.to_string())
}

/// The updater as `GET /api/system/update` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct UpdaterInfo {
    pub reachable: bool,
    pub socket: String,
    pub error: Option<String>,
    pub status: Option<Status>,
}

pub async fn updater_info(state: &Shared) -> UpdaterInfo {
    let socket = state.self_update.config().socket.display().to_string();
    match ask(state, &Request::Status).await {
        Ok(Response::Status(s)) => UpdaterInfo {
            reachable: true,
            socket,
            error: None,
            status: Some(*s),
        },
        Ok(Response::Error { message, .. }) => UpdaterInfo {
            reachable: true,
            socket,
            error: Some(message),
            status: None,
        },
        Ok(other) => UpdaterInfo {
            reachable: true,
            socket,
            error: Some(format!("unexpected answer: {other:?}")),
            status: None,
        },
        Err(e) => UpdaterInfo {
            reachable: false,
            socket,
            error: Some(e),
            status: None,
        },
    }
}

/// A global maintenance window that runs now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Window {
    /// `<event id>@<occurrence start>`: one auto-update for each.
    pub key: String,
    pub event: String,
    pub title: String,
    pub start: String,
    pub end: String,
}

/// The global `maintenance` occurrence that runs at `now`, if any.
pub fn window_at(events: &[crate::store::calendar::Event], now: DateTime<Utc>) -> Option<Window> {
    let global: Vec<_> = events
        .iter()
        .filter(|e| e.global && e.kind == "maintenance")
        .cloned()
        .collect();
    let (occ, _) =
        crate::store::calendar::expand(&global, now, now + chrono::Duration::milliseconds(1));
    occ.into_iter()
        .find(|o| {
            let s = crate::store::calendar::parse_utc(&o.start);
            let e = crate::store::calendar::parse_utc(&o.end);
            matches!((s, e), (Some(s), Some(e)) if s <= now && now < e)
        })
        .map(|o| Window {
            key: format!("{}@{}", o.event_id, o.start),
            event: o.event_id,
            title: o.title,
            start: o.start,
            end: o.end,
        })
}

/// The global maintenance window now.
pub async fn current_window(state: &Shared, now: DateTime<Utc>) -> Option<Window> {
    let events = state
        .db
        .read(|c| crate::store::calendar::list(c, &crate::store::calendar::Filter::Global))
        .await
        .ok()?;
    window_at(&events, now)
}

// ------------------------------------------------------------ the state

fn get_key(c: &Connection, key: &str) -> ApiResult<Option<String>> {
    Ok(c.query_row(
        "SELECT value FROM self_update_state WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()?)
}

fn set_key(t: &Tx<'_>, key: &str, value: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO self_update_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// The key of the new-release mail of `version`.
fn notified_key(version: &str) -> String {
    format!("notified:{version}")
}

/// The key of the result mail of the run `id`.
fn run_key(id: &str) -> String {
    format!("run:{id}")
}

/// The key of the last auto-update window.
pub const AUTO_WINDOW: &str = "auto_window";
/// Set after the first read of the updater history: older runs get no
/// mail.
pub const RESULTS_BASELINE: &str = "results_baseline";

// ------------------------------------------------------------ the mails

pub fn release_mail(r: &Release, current: &str, policy: Policy, link: &str) -> (String, String) {
    let subject = format!("Terrarium {} is available", r.version);
    let mut body = format!(
        "A new release of Terrarium is out: {} ({}). This daemon runs {current}.\n\n",
        r.version,
        if r.prerelease {
            "a pre-release"
        } else {
            "stable"
        }
    );
    match policy {
        Policy::Auto => body.push_str(
            "The policy is `auto`: the update runs in the next global maintenance window \
             (a calendar event of kind `maintenance`).\n",
        ),
        _ => body.push_str(&format!(
            "To update now:\n  terrarium admin update apply --version {}\nor select Update \
             in Settings > Software.\n",
            r.version
        )),
    }
    if !r.url.is_empty() {
        body.push_str(&format!("\nRelease notes: {}\n", r.url));
    }
    body.push_str(&format!("\nOpen: {link}\n"));
    (subject, body)
}

pub fn result_mail(run: &Run, link: &str) -> (String, String) {
    let what = match run.kind {
        RunKind::Update => "update",
        RunKind::Rollback => "rollback",
    };
    let to = run
        .to
        .as_ref()
        .map(|i| i.version.clone().unwrap_or_else(|| i.reference.clone()))
        .unwrap_or_else(|| run.target.clone());
    let from = run
        .from
        .as_ref()
        .map(|i| i.version.clone().unwrap_or_else(|| i.reference.clone()))
        .unwrap_or_else(|| "?".into());
    let subject = match run.outcome {
        Outcome::Succeeded => format!("Terrarium {what} to {to} succeeded"),
        Outcome::Current => format!("Terrarium {what}: already on {to}"),
        Outcome::Failed => format!("Terrarium {what} to {to} failed"),
        Outcome::RolledBack => format!("Terrarium {what} to {to} failed and was rolled back"),
        Outcome::RollbackFailed | Outcome::RecoveryFailed => {
            format!("ACTION NEEDED: Terrarium {what} to {to} failed")
        }
        Outcome::Running => format!("Terrarium {what} to {to} runs"),
    };
    let mut body = format!(
        "The {what} from {from} to {to}, asked by {}.\nOutcome: {}\n",
        run.actor,
        run.outcome.as_str()
    );
    if let Some(e) = &run.error {
        body.push_str(&format!("Error: {e}\n"));
    }
    if let Some(b) = &run.backup {
        body.push_str(&format!("Backup made before the switch: {b}\n"));
    }
    match run.outcome {
        Outcome::Succeeded if run.kind == RunKind::Update => body.push_str(
            "\nTo go back (this restores the backup; the data since the update is lost):\n  \
             terrarium admin update rollback\n",
        ),
        Outcome::RolledBack => body.push_str(
            "\nThe updater put the old image and the backup back. The daemon runs the old \
             version.\n",
        ),
        Outcome::RollbackFailed => body.push_str(
            "\nThe way back failed too. Look at the updater log on the host \
             (`docker compose logs terrarium-updater`) and restore the backup by hand \
             (docs/api.md §32.3).\n",
        ),
        Outcome::RecoveryFailed => body.push_str(
            "\nRecovery completion is uncertain. Keep the daemon container stopped. Verify the database and restore the recorded paused snapshot or ZIP by hand before restarting (docs/api.md §32.3).\n",
        ),
        _ => {}
    }
    body.push_str("\nSteps:\n");
    for s in &run.steps {
        body.push_str(&format!(
            "  {:<8} {:<8} {}\n",
            s.name,
            format!("{:?}", s.state).to_ascii_lowercase(),
            s.detail
        ));
    }
    body.push_str(&format!("\nOpen: {link}\n"));
    (subject, body)
}

fn send_operator(t: &mut Tx<'_>, subject: &str, body: &str) -> ApiResult<()> {
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
    Ok(())
}

// ------------------------------------------------------------ the report

/// The body of `GET /api/system/update`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub current: crate::api::VersionInfo,
    pub policy: Policy,
    pub channel: Channel,
    /// The newest release of the channel.
    pub latest: Option<Release>,
    /// `latest` is newer than the daemon.
    pub update_available: bool,
    pub checked_at: Option<DateTime<Utc>>,
    pub check_error: Option<String>,
    pub updater: UpdaterInfo,
    /// The last run of the updater.
    pub last_result: Option<Run>,
    /// The global maintenance window now.
    pub maintenance: Option<Window>,
    pub drain: DrainInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrainInfo {
    pub on: bool,
    pub since: Option<DateTime<Utc>>,
    pub by: Option<String>,
    /// The agents that run a turn now (`project/name`).
    pub busy: Vec<String>,
    /// The jobs of the job queue whose run has not ended yet (TER-123):
    /// no job starts during the drain, and these go on.
    pub jobs: Vec<String>,
    /// The drain of the job slots only (TER-614 R4): no job starts, the
    /// agents go on. `None` when it is off.
    pub jobs_drain: Option<Drain>,
}

/// The agents that run a turn or start now.
pub async fn busy_agents(state: &Shared) -> Vec<String> {
    let mut out = Vec::new();
    for r in state.supervisor.harness_runs() {
        if matches!(
            state.supervisor.status(r.agent).await,
            Some(AgentStatus::Working | AgentStatus::Starting)
        ) {
            out.push(r.name.clone());
        }
    }
    out.sort();
    out
}

pub async fn drain_info(state: &Shared) -> DrainInfo {
    let d = state.self_update.drain_state();
    DrainInfo {
        on: d.is_some(),
        since: d.as_ref().map(|d| d.since),
        by: d.map(|d| d.by),
        busy: busy_agents(state).await,
        jobs: crate::jobs::busy_jobs(state).await,
        jobs_drain: state.self_update.jobs_drain_state(),
    }
}

pub async fn report(state: &Shared) -> Report {
    let rt = &state.self_update;
    let cfg = rt.config();
    let (latest, checked_at, check_error) = {
        let c = lock(&rt.cache);
        (
            newest(&c.releases, cfg.channel).cloned(),
            c.checked_at,
            c.error.clone(),
        )
    };
    let updater = updater_info(state).await;
    let last_result = updater
        .status
        .as_ref()
        .and_then(|s| s.running.clone().or_else(|| s.history.first().cloned()));
    Report {
        current: crate::api::system::version_for(state)
            .await
            .unwrap_or_else(|_| crate::api::VersionInfo::current()),
        policy: cfg.policy,
        channel: cfg.channel,
        update_available: latest
            .as_ref()
            .is_some_and(|r| is_newer(&r.version, terrarium_version::VERSION)),
        latest,
        checked_at,
        check_error,
        updater,
        last_result,
        maintenance: current_window(state, Utc::now()).await,
        drain: drain_info(state).await,
    }
}

// ------------------------------------------------------------ the actions

/// The target of an update with no version: the newest release of the
/// channel when the cache knows it, else the channel tag.
pub fn default_target(state: &Shared) -> String {
    let rt = &state.self_update;
    let ch = rt.config().channel;
    let c = lock(&rt.cache);
    newest(&c.releases, ch).map_or_else(|| ch.tag().to_owned(), |r| r.version.clone())
}

fn updater_error(e: String) -> ApiError {
    ApiError::new(
        Code::Unavailable,
        format!(
            "the updater does not answer ({e}). Start the `terrarium-updater` service \
             (compose profile `updater`, docs/getting-started.md)"
        ),
    )
}

/// Ask the updater for an update or a rollback, and write the audit row
/// (`system.update`, `system.rollback`). Returns the run ID.
pub async fn request(state: &Shared, req: Request, actor: &str) -> ApiResult<String> {
    let (action, target) = match &req {
        Request::Update { target, .. } => ("system.update", target.clone()),
        Request::Rollback { .. } => ("system.rollback", "previous".to_owned()),
        Request::Status
        | Request::Retention { .. }
        | Request::RetentionInit { .. }
        | Request::RetentionRepair { .. } => {
            return Err(ApiError::bad_request("not an update action"));
        }
    };
    let answer = ask(state, &req).await.map_err(updater_error)?;
    let (run, ok, error) = match answer {
        Response::Accepted { run } => (Some(run), true, None),
        Response::Error { message, .. } => (None, false, Some(message)),
        Response::Status(_) | Response::Retained { .. } => {
            (None, false, Some("unexpected answer".to_owned()))
        }
    };
    let detail = json!({"target": target, "run": run, "ok": ok, "error": error,
                        "from": terrarium_version::FULL});
    let (a, t) = (action.to_owned(), target.clone());
    let r = state
        .db
        .write(actor.to_owned(), move |tx| tx.audit(&a, &t, None, detail))
        .await;
    if let Err(e) = r {
        tracing::warn!("cannot write the audit row of {action}: {e}");
    }
    match (run, error) {
        (Some(run), _) => Ok(run),
        (None, e) => Err(ApiError::conflict(e.unwrap_or_default())),
    }
}

/// Start the drain.
pub fn start_drain(state: &Shared, by: &str) {
    {
        let mut d = lock(&state.self_update.drain);
        if d.is_none() {
            *d = Some(Drain {
                since: Utc::now(),
                by: by.to_owned(),
            });
        }
    }
    state.usage.streams.set_closed(true);
    tracing::info!(by, "the daemon drains: no new turns");
}

/// Start the drain of the job slots (TER-614 R4): no job of the job
/// queue starts, and the running ones finish. Agents go on.
pub fn start_jobs_drain(state: &Shared, by: &str) {
    let mut d = lock(&state.self_update.jobs_drain);
    if d.is_none() {
        *d = Some(Drain {
            since: Utc::now(),
            by: by.to_owned(),
        });
    }
    tracing::info!(by, "the job slots drain: no new jobs");
}

/// End the drain, and the drain of the job slots: the work that waited
/// goes out.
pub async fn end_drain(state: &Shared) {
    lock(&state.self_update.jobs_drain).take();
    let was = lock(&state.self_update.drain).take();
    let parked = state.usage.streams.set_closed(false);
    if was.is_some() {
        tracing::info!("the drain ends");
    }
    for id in parked {
        state.supervisor.stream_granted(id);
    }
    crate::lifecycle::wake_with_work(state, None, "drain_end").await;
    // The jobs that waited in the queue start (TER-123).
    crate::jobs::kick(state);
}

// ------------------------------------------------------------ the task

/// What one tick did (the tests).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tick {
    pub release_mailed: Option<String>,
    pub results_mailed: Vec<String>,
    pub auto_update: Option<String>,
}

/// One tick of the background task at `now`. `fetch`: read the releases
/// when the cache is older than [`CHECK_INTERVAL`].
pub async fn tick(state: &Shared, now: DateTime<Utc>, fetch: bool) -> Tick {
    let mut out = Tick::default();
    let cfg = state.self_update.config();
    if cfg.policy == Policy::Off {
        return out;
    }
    if fetch {
        refresh(state, CHECK_INTERVAL).await;
    }
    let link = state.web.dashboard_link(PAGE);
    // A new release: one mail for each version.
    let latest = {
        let c = lock(&state.self_update.cache);
        newest(&c.releases, cfg.channel).cloned()
    };
    let available = latest
        .clone()
        .filter(|r| is_newer(&r.version, terrarium_version::VERSION));
    if let Some(r) = &available {
        let (subject, body) = release_mail(r, terrarium_version::FULL, cfg.policy, &link);
        let key = notified_key(&r.version);
        let sent = state
            .db
            .write("system", move |t| {
                if get_key(t, &key)?.is_some() {
                    return Ok(false);
                }
                set_key(t, &key, &now.to_rfc3339())?;
                send_operator(t, &subject, &body)?;
                Ok(true)
            })
            .await;
        match sent {
            Ok(true) => out.release_mailed = Some(r.version.clone()),
            Ok(false) => {}
            Err(e) => tracing::warn!("cannot mail the new release: {e}"),
        }
    }
    // The results of the updater: one mail for each run.
    let status = match ask(state, &Request::Status).await {
        Ok(Response::Status(s)) => Some(*s),
        _ => None,
    };
    if let Some(s) = &status {
        let ended: Vec<Run> = s
            .history
            .iter()
            .filter(|r| r.ended_at.is_some())
            .cloned()
            .collect();
        let link2 = link.clone();
        let mailed = state
            .db
            .write("system", move |t| {
                let baseline = get_key(t, RESULTS_BASELINE)?.is_none();
                let mut sent = Vec::new();
                for r in &ended {
                    let key = run_key(&r.id);
                    if get_key(t, &key)?.is_some() {
                        continue;
                    }
                    set_key(t, &key, r.outcome.as_str())?;
                    if !baseline {
                        let (subject, body) = result_mail(r, &link2);
                        send_operator(t, &subject, &body)?;
                        sent.push(r.id.clone());
                    }
                }
                if baseline {
                    set_key(t, RESULTS_BASELINE, &now.to_rfc3339())?;
                }
                Ok(sent)
            })
            .await;
        match mailed {
            Ok(m) => out.results_mailed = m,
            Err(e) => tracing::warn!("cannot mail the update results: {e}"),
        }
    }
    // The auto-update: inside a global maintenance window, once.
    if cfg.policy == Policy::Auto
        && let Some(r) = available
        && status.as_ref().is_some_and(|s| s.running.is_none())
        && let Some(w) = current_window(state, now).await
    {
        let key = w.key.clone();
        let fresh = state
            .db
            .write(AUTO_ACTOR, move |t| {
                if get_key(t, AUTO_WINDOW)?.as_deref() == Some(key.as_str()) {
                    return Ok(false);
                }
                set_key(t, AUTO_WINDOW, &key)?;
                Ok(true)
            })
            .await
            .unwrap_or(false);
        if fresh {
            tracing::info!(version = %r.version, window = %w.key, "auto-update in the maintenance window");
            let req = Request::Update {
                target: r.version.clone(),
                actor: AUTO_ACTOR.into(),
            };
            match request(state, req, AUTO_ACTOR).await {
                Ok(run) => out.auto_update = Some(run),
                Err(e) => tracing::warn!("the auto-update did not start: {}", e.message),
            }
        }
    }
    out
}

/// Start the background task with `cfg`.
pub fn spawn(state: Shared, cfg: Config) -> JoinHandle<()> {
    state.self_update.set_config(cfg.clone());
    tokio::spawn(async move {
        tracing::info!(
            policy = cfg.policy.as_str(),
            channel = cfg.channel.as_str(),
            socket = %cfg.socket.display(),
            "self-update"
        );
        if cfg.policy == Policy::Off {
            return;
        }
        tokio::time::sleep(FIRST_TICK).await;
        loop {
            let t = tick(&state, Utc::now(), true).await;
            if t != Tick::default() {
                tracing::info!(?t, "self-update tick");
            }
            tokio::time::sleep(TICK).await;
        }
    })
}

#[cfg(test)]
#[path = "self_update_tests.rs"]
mod tests;
