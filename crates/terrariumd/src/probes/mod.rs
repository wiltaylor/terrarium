//! Provider quota probes (PLAN §16.2).
//!
//! A [`QuotaProbe`] reads one provider's usage windows for a subscription.
//! It reads the credential of the subscription's plan account (PLAN §16.3;
//! before the migration, the shared harness home): the same login the
//! harness uses, so there is no second login. It sends one read-only GET, and
//! parses the answer into [`QuotaWindow`]s. The endpoints are undocumented,
//! so each probe is best-effort: a failure sets `probe_status = failed` and
//! keeps the last success time (PLAN §21). `docs/compat.md`, "Usage
//! probes", records each endpoint and its answer.
//!
//! A probe never refreshes a token. A refresh would rotate the refresh
//! token under the harness that owns the login. An expired token is a
//! failed probe until the harness refreshes it.
//!
//! The scheduler ([`spawn_scheduler`]) runs the probe of each subscription
//! on an interval (default 5 minutes), stores the samples, and calls the
//! quota policy. A successful probe removes each stored window that its
//! answer does not have (`subs::retire_windows`): the window and its
//! quota notice rows go, for every probe. A failed probe removes nothing.
//! Each hour the scheduler also deletes the samples older than
//! `TERRARIUM_QUOTA_SAMPLE_KEEP` (default 48 hours) in small batches
//! ([`prune_samples`]); the newest sample of each window stays.

pub mod anthropic;
pub mod github;
pub mod kimi;
pub mod litellm;
pub mod moonshot;
pub mod openai;
pub mod openrouter;
pub mod zai;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::HarnessId;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::api::Shared;
use crate::db::Db;
use crate::error::ApiResult;
use crate::events::EventDraft;
use crate::store::subscriptions::{self as subs, PROBE_FAILED, PROBE_NONE, PROBE_OK};
use crate::supervisor::BoxFuture;

/// One usage window of a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// `5h`, `7d`, `monthly`, ... Stable per probe.
    pub window: String,
    /// Percent of the window used, 0 to 100 (it can pass 100 with overage).
    pub used_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// The provider's own numbers for this window.
    #[serde(default)]
    pub raw: Value,
}

/// Why a probe failed. The message goes into `probe_error`; it never holds
/// a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    #[error("no credential: {0}")]
    NoCredential(String),
    #[error(
        "the credential expired at {0}; the harness of an agent on it refreshes it at its next \
         request (the daemon never refreshes a token)"
    )]
    Expired(String),
    #[error("HTTP {status} from {url}")]
    Status { status: u16, url: String },
    /// A non-2xx answer, with what the operator must change.
    #[error("HTTP {status} from {url}: {hint}")]
    StatusHint {
        status: u16,
        url: String,
        hint: String,
    },
    #[error("request failed: {0}")]
    Http(String),
    #[error("unexpected answer: {0}")]
    Parse(String),
    #[error("unknown probe `{0}`")]
    Unknown(String),
}

/// A bearer credential. `Debug` does not print the token.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    token: String,
    /// The account header some providers need (OpenAI's ChatGPT account).
    pub account_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"<redacted>")
            .field("account_id", &self.account_id.as_ref().map(|_| "<set>"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credentials {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            account_id: None,
            expires_at: None,
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// An expired credential is an error before any request.
    pub fn check_fresh(&self, now: DateTime<Utc>) -> Result<(), ProbeError> {
        match self.expires_at {
            Some(t) if t <= now => Err(ProbeError::Expired(
                t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )),
            _ => Ok(()),
        }
    }
}

/// One GET request.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("headers", &names)
            .finish()
    }
}

/// An HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Sends GET requests. The daemon uses [`ReqwestHttp`]; tests give a fake
/// that answers from fixtures.
pub trait HttpGet: Send + Sync + 'static {
    fn get(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>>;
}

/// HTTPS with rustls and the bundled web roots, 20 s timeout.
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    pub fn new() -> Self {
        Self::with_timeout(Duration::from_secs(20))
    }

    /// The same client with another request timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("terrarium/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpGet for ReqwestHttp {
    fn get(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            let mut b = self.client.get(&req.url);
            for (k, v) in &req.headers {
                b = b.header(k, v);
            }
            // The error text of reqwest names the URL, never the headers.
            let resp = b.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            Ok(HttpResponse { status, body })
        })
    }
}

/// One provider's usage endpoint.
pub trait QuotaProbe: Send + Sync {
    /// The name in `subscriptions/*.toml` (`probe = "..."`).
    fn name(&self) -> &'static str;
    /// The harness whose shared home holds the credential.
    fn harness(&self) -> HarnessId;
    /// Read the credential from the shared home. This blocks (file reads).
    fn credentials(&self, home: &Path) -> Result<Credentials, ProbeError>;
    /// True when a stored API key (`POST /api/subscriptions/{name}/api-key`)
    /// can stand in for the home credential.
    fn accepts_api_key(&self) -> bool {
        false
    }
    fn request(&self, creds: &Credentials) -> HttpRequest;
    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError>;

    /// Send the request and parse the answer.
    fn run<'a>(
        &'a self,
        http: &'a dyn HttpGet,
        creds: &'a Credentials,
    ) -> BoxFuture<'a, Result<Vec<QuotaWindow>, ProbeError>> {
        Box::pin(async move {
            creds.check_fresh(Utc::now())?;
            let req = self.request(creds);
            let url = req.url.clone();
            let resp = http.get(req).await.map_err(ProbeError::Http)?;
            if !(200..300).contains(&resp.status) {
                return Err(ProbeError::Status {
                    status: resp.status,
                    url,
                });
            }
            self.parse(&resp.body)
        })
    }
}

/// Every probe, by name.
pub fn all() -> Vec<Arc<dyn QuotaProbe>> {
    vec![
        Arc::new(anthropic::AnthropicOauthUsage),
        Arc::new(openai::OpenAiCodexUsage),
        Arc::new(github::CopilotQuota),
        Arc::new(zai::ZaiUsage),
        Arc::new(openrouter::OpenRouterCredits),
        Arc::new(litellm::registered()),
        Arc::new(moonshot::GLOBAL),
        Arc::new(moonshot::CHINA),
        Arc::new(kimi::KimiCodeUsage),
    ]
}

/// The probe called `name`. `codex-rate-limits` is an alias of
/// `openai-codex-usage`.
pub fn by_name(name: &str) -> Option<Arc<dyn QuotaProbe>> {
    let name = match name {
        "codex-rate-limits" | "openai-codex-rate-limits" => openai::NAME,
        "copilot-premium-requests" => github::NAME,
        n => n,
    };
    all().into_iter().find(|p| p.name() == name)
}

/// The probe that fits a provider, for definitions that the login wizard
/// makes.
pub fn for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some(anthropic::NAME),
        "openai" | "openai-codex" => Some(openai::NAME),
        "github" | "github-copilot" | "copilot" => Some(github::NAME),
        "litellm" => Some(litellm::NAME),
        _ => None,
    }
}

/// Parse a JSON body, with a parse error.
pub(crate) fn json_body(body: &str) -> Result<Value, ProbeError> {
    serde_json::from_str(body).map_err(|e| ProbeError::Parse(format!("not JSON: {e}")))
}

/// Read a JSON file from a harness home.
pub(crate) fn read_json(path: &Path) -> Result<Value, ProbeError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ProbeError::NoCredential(format!("cannot read {}: {}", path.display(), e.kind()))
    })?;
    serde_json::from_str(&text)
        .map_err(|e| ProbeError::NoCredential(format!("{} is not JSON: {e}", path.display())))
}

/// `<n>h` below two days, else `<n>d`, from a window length in seconds.
pub(crate) fn window_name(seconds: u64) -> String {
    if seconds == 0 {
        return "window".into();
    }
    let hours = seconds.div_ceil(3600);
    if hours < 48 {
        format!("{hours}h")
    } else {
        format!("{}d", hours.div_ceil(24))
    }
}

/// What one probe run did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeOutcome {
    pub subscription: String,
    /// `ok`, `failed`, or `none`.
    pub probe_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

/// The credential of `probe` for `subscription`: a stored API key when the
/// probe takes one, else the plan account of the subscription (PLAN
/// §16.3), else the shared harness home `home` (a login from before the
/// migration). This blocks (file reads). The model lists
/// (`crate::models`) use it too. It only reads: an expired token stays
/// expired until an agent's harness refreshes it.
pub fn read_credentials(
    probe: &dyn QuotaProbe,
    state_dir: &Path,
    subscription: &str,
    home: &Path,
) -> Result<Credentials, ProbeError> {
    if probe.accepts_api_key()
        && let Some(key) = crate::login_session::stored_key(state_dir, subscription)
    {
        return Ok(Credentials::new(key));
    }
    let harness = probe.harness();
    if crate::accounts::has_credential(state_dir, subscription, harness) {
        let store = crate::accounts::store_dir(state_dir, subscription);
        return probe.credentials(&crate::accounts::probe_home(&store, harness));
    }
    probe.credentials(home)
}

/// The credential for a subscription's probe, off the async threads.
async fn credentials_for(
    state: &Shared,
    probe: &Arc<dyn QuotaProbe>,
    subscription: &str,
) -> Result<Credentials, ProbeError> {
    let home = state.usage.config.home(probe.harness());
    let (p, dir, name) = (
        probe.clone(),
        state.state_dir.clone(),
        subscription.to_owned(),
    );
    tokio::task::spawn_blocking(move || read_credentials(p.as_ref(), &dir, &name, &home))
        .await
        .map_err(|e| ProbeError::NoCredential(format!("credential task failed: {e}")))?
}

/// Run the probe of one subscription now, store the samples and the probe
/// status, and apply the quota policy.
pub async fn run_subscription(state: &Shared, name: &str) -> ApiResult<ProbeOutcome> {
    let key = name.to_owned();
    let sub = state.db.read(move |c| subs::get(c, &key)).await?;
    let Some(probe_name) = sub.probe.clone() else {
        return Ok(ProbeOutcome {
            subscription: sub.name,
            probe_status: PROBE_NONE.into(),
            error: None,
            windows: Vec::new(),
        });
    };
    let result = if probe_name == litellm::NAME {
        async {
            if sub.provider != "litellm" {
                return Err(litellm::unavailable());
            }
            let settings = sub.selfhosted.as_ref().ok_or_else(litellm::unavailable)?;
            let probe =
                Arc::new(litellm::LiteLlmKeySpend::new(&settings.base_url)?) as Arc<dyn QuotaProbe>;
            let creds = credentials_for(state, &probe, &sub.name)
                .await
                .map_err(|_| litellm::unavailable())?;
            let scoped = litellm::LiteLlmKeySpend::new(&settings.base_url)?;
            litellm::fetch(&scoped, &creds).await
        }
        .await
    } else {
        match by_name(&probe_name) {
            None => Err(ProbeError::Unknown(probe_name.clone())),
            Some(probe) => match credentials_for(state, &probe, &sub.name).await {
                Err(e) => Err(e),
                Ok(creds) => probe.run(state.usage.config.http.as_ref(), &creds).await,
            },
        }
    };
    let name = sub.name.clone();
    let outcome = match result {
        Ok(windows) => {
            let w = windows.clone();
            let n = name.clone();
            state
                .db
                .write("system", move |t| {
                    for q in &w {
                        let resets = q
                            .resets_at
                            .map(|r| r.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                        subs::insert_sample(
                            t,
                            &n,
                            &q.window,
                            q.used_pct,
                            resets.as_deref(),
                            &q.raw,
                        )?;
                    }
                    // A window that the answer no longer has goes
                    // (`subs::retire_windows`).
                    let keep: Vec<&str> = w.iter().map(|q| q.window.as_str()).collect();
                    let removed = subs::retire_windows(t, &n, &keep)?;
                    subs::set_probe_result(t, &n, Ok(()))?;
                    subs::clear_expired(t, &n)?;
                    let mut summary = w
                        .iter()
                        .map(|q| format!("{} {:.0}%", q.window, q.used_pct))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut data = json!({"subscription": n, "windows": w});
                    if !removed.is_empty() {
                        summary.push_str(&format!("; removed {}", removed.join(", ")));
                        data["removed_windows"] = json!(removed);
                    }
                    t.emit(EventDraft::new(
                        "quota.sample",
                        format!("{n}: {summary}"),
                        data,
                    ))?;
                    Ok(())
                })
                .await?;
            debug!(subscription = %name, windows = windows.len(), "probe ok");
            ProbeOutcome {
                subscription: name.clone(),
                probe_status: PROBE_OK.into(),
                error: None,
                windows,
            }
        }
        Err(e) => {
            let msg = e.to_string();
            warn!(subscription = %name, probe = %probe_name, "probe failed: {msg}");
            // The provider refused the credential: the login expired.
            let refused = matches!(
                e,
                ProbeError::Status { status: 401, .. } | ProbeError::StatusHint { status: 401, .. }
            );
            let n = name.clone();
            let m = msg.clone();
            state
                .db
                .write("system", move |t| {
                    subs::set_probe_result(t, &n, Err(&m))?;
                    if refused {
                        subs::mark_expired(t, &n, &format!("the usage probe got {m}"))?;
                    }
                    Ok(())
                })
                .await?;
            ProbeOutcome {
                subscription: name.clone(),
                probe_status: PROBE_FAILED.into(),
                error: Some(msg),
                windows: Vec::new(),
            }
        }
    };
    crate::quota_policy::evaluate_subscription(state, &name).await?;
    Ok(outcome)
}

/// True when the subscription's probe is due.
fn due(sub: &subs::Subscription, interval: Duration, now: DateTime<Utc>) -> bool {
    if sub.probe.is_none() {
        return false;
    }
    let Some(last) = sub
        .last_probe_at
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
    else {
        return true;
    };
    let age = now.signed_duration_since(last.with_timezone(&Utc));
    age.to_std().is_ok_and(|a| a >= interval)
}

/// How often the probe scheduler prunes old quota samples (TER-395).
pub const SAMPLE_PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// The pause between two prune batches, so that the waiting readers and
/// writers get the database lock (TER-395).
pub const SAMPLE_PRUNE_PAUSE: Duration = Duration::from_millis(20);

/// Delete the quota samples older than `keep` (back from `now`), in
/// transactions of at most [`subs::SAMPLE_PRUNE_BATCH`] rows, so no write
/// holds the single database connection long (TER-394). The newest sample
/// of each window stays at any age: [`subs::latest_windows`] reads it.
/// Returns the rows deleted.
pub async fn prune_samples(db: &Db, keep: Duration, now: DateTime<Utc>) -> ApiResult<usize> {
    let Some(before) = chrono::Duration::from_std(keep)
        .ok()
        .and_then(|k| now.checked_sub_signed(k))
    else {
        return Ok(0);
    };
    let before = crate::time_param::stored(before);
    let names = db
        .read(|c| {
            Ok(subs::list(c)?
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>())
        })
        .await?;
    let mut deleted = 0;
    for name in names {
        let n = name.clone();
        let windows = db.read(move |c| subs::sample_windows(c, &n)).await?;
        for (window, newest) in windows {
            loop {
                let (s, w, b) = (name.clone(), window.clone(), before.clone());
                let rows = db
                    .write("system", move |t| {
                        subs::prune_window_samples(t, &s, &w, newest, &b, subs::SAMPLE_PRUNE_BATCH)
                    })
                    .await?;
                deleted += rows;
                if rows < subs::SAMPLE_PRUNE_BATCH {
                    break;
                }
                // Let the waiting readers and writers take the lock.
                tokio::time::sleep(SAMPLE_PRUNE_PAUSE).await;
            }
        }
    }
    Ok(deleted)
}

/// Run each subscription's probe on the interval of the usage config.
pub fn spawn_scheduler(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let cfg = &state.usage.config;
        let (interval, tick) = (cfg.probe_interval, cfg.probe_tick);
        let keep = cfg.quota_sample_keep;
        info!(
            interval_s = interval.as_secs(),
            sample_keep_h = keep.as_secs() / 3600,
            "quota probe scheduler started"
        );
        // The first prune runs at the first tick, so a backlog of old
        // samples goes soon after a start (TER-395).
        let mut next_prune = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(tick).await;
            if tokio::time::Instant::now() >= next_prune {
                next_prune = tokio::time::Instant::now() + SAMPLE_PRUNE_EVERY;
                match prune_samples(&state.db, keep, Utc::now()).await {
                    Ok(0) => {}
                    Ok(n) => info!(deleted = n, "pruned old quota samples"),
                    Err(e) => warn!("the quota sample prune failed: {e}"),
                }
            }
            let list = match state.db.read(subs::list).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("probe scheduler cannot list subscriptions: {e}");
                    continue;
                }
            };
            let now = Utc::now();
            for sub in list.iter().filter(|s| due(s, interval, now)) {
                if let Err(e) = run_subscription(&state, &sub.name).await {
                    warn!(subscription = %sub.name, "probe run failed: {e}");
                }
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Answers each URL from a map, and records the header names it saw.
    #[derive(Default)]
    pub struct FakeHttp {
        pub answers: Mutex<HashMap<String, HttpResponse>>,
        pub seen: Mutex<Vec<HttpRequest>>,
    }

    impl FakeHttp {
        pub fn with(url: &str, status: u16, body: &str) -> Self {
            let f = Self::default();
            f.answers.lock().unwrap().insert(
                url.into(),
                HttpResponse {
                    status,
                    body: body.into(),
                },
            );
            f
        }
    }

    impl HttpGet for FakeHttp {
        fn get(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
            let answer = self.answers.lock().unwrap().get(&req.url).cloned();
            self.seen.lock().unwrap().push(req);
            Box::pin(async move { answer.ok_or_else(|| "connection refused".to_owned()) })
        }
    }

    /// A probe reads the account of its subscription (PLAN §16.3), not the
    /// shared home, and only reads it.
    #[test]
    fn the_probe_reads_the_account() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let shared = tmp.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            shared.join(".credentials.json"),
            r#"{"claudeAiOauth": {"accessToken": "shared"}}"#,
        )
        .unwrap();
        let p = anthropic::AnthropicOauthUsage;
        let c = read_credentials(&p, &state, "max-b", &shared).unwrap();
        assert_eq!(c.token(), "shared", "no account yet: the shared login");
        let login = r#"{"claudeAiOauth": {"accessToken": "acct-b", "expiresAt": 1}}"#;
        crate::accounts::write_credential(&state, "max-b", HarnessId::Claude, login.as_bytes())
            .unwrap();
        let path = crate::accounts::credential_path(
            &crate::accounts::store_dir(&state, "max-b"),
            HarnessId::Claude,
        );
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let c = read_credentials(&p, &state, "max-b", &shared).unwrap();
        assert_eq!(c.token(), "acct-b");
        // Expired: the probe stops before any request, and the file stays.
        assert!(matches!(
            c.check_fresh(Utc::now()),
            Err(ProbeError::Expired(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), login);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        // Copilot: the token file of the account.
        crate::accounts::write_credential(&state, "gh", HarnessId::Copilot, b"gho_t\n").unwrap();
        let c = read_credentials(&github::CopilotQuota, &state, "gh", &shared).unwrap();
        assert_eq!(c.token(), "gho_t");
    }

    #[test]
    fn window_names() {
        assert_eq!(window_name(18000), "5h");
        assert_eq!(window_name(604800), "7d");
        assert_eq!(window_name(86400), "24h");
        assert_eq!(window_name(2_592_000), "30d");
        assert_eq!(window_name(0), "window");
    }

    #[test]
    fn credentials_hide_the_token() {
        let mut c = Credentials::new("sk-secret");
        c.account_id = Some("acct".into());
        let s = format!("{c:?}");
        assert!(!s.contains("sk-secret") && !s.contains("acct"), "{s}");
        let r = HttpRequest {
            url: "https://x".into(),
            headers: vec![("Authorization".into(), "Bearer sk-secret".into())],
        };
        assert!(!format!("{r:?}").contains("sk-secret"));
        c.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        assert!(matches!(
            c.check_fresh(Utc::now()),
            Err(ProbeError::Expired(_))
        ));
    }

    #[test]
    fn probe_names_and_aliases() {
        assert_eq!(
            by_name("anthropic-oauth-usage").unwrap().name(),
            anthropic::NAME
        );
        assert_eq!(by_name("codex-rate-limits").unwrap().name(), openai::NAME);
        assert_eq!(
            by_name("copilot-premium-requests").unwrap().name(),
            github::NAME
        );
        assert!(by_name("gemini").is_none());
        assert_eq!(for_provider("openai"), Some(openai::NAME));
        assert_eq!(for_provider("google"), None);
        assert_eq!(for_provider("litellm"), Some(litellm::NAME));
        assert_eq!(by_name(litellm::NAME).unwrap().name(), litellm::NAME);
        assert_eq!(by_name("zai-usage").unwrap().name(), zai::NAME);
        assert_eq!(by_name("moonshot-balance").unwrap().name(), moonshot::NAME);
        assert_eq!(
            by_name("moonshot-cn-balance").unwrap().name(),
            moonshot::NAME_CN
        );
        assert_eq!(by_name("kimi-code-usage").unwrap().name(), kimi::NAME);
        assert_eq!(
            by_name("openrouter-credits").unwrap().name(),
            openrouter::NAME
        );
    }

    #[tokio::test]
    async fn run_maps_http_failures() {
        let p = anthropic::AnthropicOauthUsage;
        let creds = Credentials::new("t");
        let http = FakeHttp::with(anthropic::URL, 401, "{}");
        assert_eq!(
            p.run(&http, &creds).await,
            Err(ProbeError::Status {
                status: 401,
                url: anthropic::URL.into()
            })
        );
        let http = FakeHttp::default();
        assert!(matches!(
            p.run(&http, &creds).await,
            Err(ProbeError::Http(_))
        ));
    }

    #[test]
    fn due_rules() {
        let mut s: subs::Subscription = serde_json::from_value(json!({
            "name": "s", "provider": "anthropic", "harnesses": [], "probe": "anthropic-oauth-usage",
            "alert_at": 80, "source": "api", "login_status": "unknown", "probe_status": "none",
            "created_at": "", "updated_at": ""
        }))
        .unwrap();
        let now = Utc::now();
        assert!(due(&s, Duration::from_secs(300), now));
        s.last_probe_at = Some(now.to_rfc3339());
        assert!(!due(&s, Duration::from_secs(300), now));
        assert!(due(
            &s,
            Duration::from_secs(300),
            now + chrono::Duration::minutes(6)
        ));
        s.probe = None;
        assert!(!due(
            &s,
            Duration::from_secs(300),
            now + chrono::Duration::minutes(6)
        ));
    }

    /// TER-395: the prune deletes the samples past the keep time in
    /// batches, keeps the recent ones, and keeps the newest sample of each
    /// window at any age, so `latest_windows` answers as before.
    #[tokio::test]
    async fn the_sample_prune_keeps_recent_and_newest_rows() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = Db::new(conn, crate::events::EventHub::default());
        let def = |name: &str| -> terrarium_core::config::SubscriptionDef {
            toml::from_str(&format!(
                "name = \"{name}\"\nprovider = \"anthropic\"\nplan = \"Max 20x\"\nharnesses = [\"claude\"]\nprobe = \"anthropic-oauth-usage\"\n"
            ))
            .unwrap()
        };
        let (d1, d2) = (def("s1"), def("s2"));
        db.write("system", move |t| {
            subs::upsert_def(t, &d1, crate::store::projects::SOURCE_API)?;
            subs::upsert_def(t, &d2, crate::store::projects::SOURCE_API)?;
            // s1 5h: 1200 samples 3 days old (more than two batches), then
            // one 47 hours old and one an hour old.
            // s1 7d and s2 monthly: only old samples.
            t.execute_batch(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1200)
                 INSERT INTO quota_samples (subscription, window, used_pct, at)
                 SELECT 's1', '5h', x % 100, '2026-11-01T00:00:00.000Z' FROM n;
                 INSERT INTO quota_samples (subscription, window, used_pct, at) VALUES
                   ('s1', '5h', 40, '2026-11-02T01:00:00.000Z'),
                   ('s1', '5h', 41, '2026-11-03T23:00:00.000Z'),
                   ('s1', '7d', 10, '2026-10-20T00:00:00.000Z'),
                   ('s1', '7d', 11, '2026-10-21T00:00:00.000Z'),
                   ('s2', 'monthly', 70, '2026-10-01T00:00:00.000Z'),
                   ('s2', 'monthly', 71, '2026-10-02T00:00:00.000Z');",
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let latest = |db: &Db, name: &'static str| {
            let db = db.clone();
            async move {
                db.read(move |c| subs::latest_windows(c, name))
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|w| (w.window, w.used_pct, w.at))
                    .collect::<Vec<_>>()
            }
        };
        let before = (latest(&db, "s1").await, latest(&db, "s2").await);
        let now = DateTime::parse_from_rfc3339("2026-11-04T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let keep = Duration::from_secs(48 * 3600);
        let deleted = prune_samples(&db, keep, now).await.unwrap();
        assert_eq!(deleted, 1200 + 1 + 1, "the old 5h, 7d, and monthly rows");
        assert_eq!(
            (latest(&db, "s1").await, latest(&db, "s2").await),
            before,
            "the newest row of each window survives"
        );
        let left = db
            .read(|c| {
                let mut st = c.prepare(
                    "SELECT subscription, window, used_pct FROM quota_samples ORDER BY id",
                )?;
                let rows = st.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, f64>(2)?,
                    ))
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap();
        let left: Vec<(&str, &str, f64)> = left
            .iter()
            .map(|(s, w, u)| (s.as_str(), w.as_str(), *u))
            .collect();
        assert_eq!(
            left,
            vec![
                ("s1", "5h", 40.0),
                ("s1", "5h", 41.0),
                ("s1", "7d", 11.0),
                ("s2", "monthly", 71.0),
            ]
        );
        assert_eq!(
            prune_samples(&db, keep, now).await.unwrap(),
            0,
            "nothing more to do"
        );
    }
}
