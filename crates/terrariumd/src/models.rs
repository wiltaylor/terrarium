//! The models a subscription can use: `GET /api/subscriptions/{name}/models`.
//!
//! The provider knows its model list better than a table in Terrarium. The
//! sources, in order:
//!
//! 1. `provider`: the provider's OpenAI-compatible `GET <base>/models`, with
//!    the stored API key as a bearer token. Only for an API-key
//!    subscription whose provider has such an endpoint ([`openai_base`]).
//!    The key stays in the daemon: it is not returned, not logged, and not
//!    in an error text.
//! 2. `plan`: the model list of a harness login, read with the login's own
//!    credential from the shared harness home ([`plan`]): Claude Code
//!    (`api.anthropic.com/v1/models` with the OAuth token, or with a stored
//!    Anthropic key), Codex (`chatgpt.com/backend-api/codex/models` with the
//!    ChatGPT login), and Copilot (`<endpoints.api>/models` with the GitHub
//!    token). Verified live 2026-10-26 (`docs/compat.md`, "Model lists
//!    (plan logins)"). The token stays in the daemon, as for `provider`.
//! 3. `pi`: `pi --offline --list-models <pi provider>`. pi ships its own
//!    catalogue, which a harness update brings up to date. pi lists only the
//!    providers that have a credential, so the command runs in an empty pi
//!    dir with a placeholder in the key env var (never the key: `--offline`
//!    sends no request). A plan login (OAuth) uses the shared pi home.
//! 4. `fallback`: the default models of the adapters
//!    ([`default_models`]).
//!
//! A list is cached for [`CACHE_TTL`] per subscription; `?refresh=true`
//! reads it again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use terrarium_core::HarnessId;
use tokio::process::Command;

use crate::login_session::{self as login, ApiKey};
use crate::probes::{HttpGet, HttpRequest};
use crate::store::subscriptions::Subscription;

pub mod plan;
pub mod selfhosted;

/// How long a model list stays fresh.
pub const CACHE_TTL: Duration = Duration::from_secs(6 * 3600);

/// How long `pi --list-models` may take.
const PI_TIMEOUT: Duration = Duration::from_secs(30);

/// The value of the key env var for `pi --list-models`. pi needs the var
/// to be set to list the provider; `--offline` sends no request.
const PLACEHOLDER_KEY: &str = "terrarium-list-models";

/// One model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The context window in tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
}

impl ModelInfo {
    fn id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            context: None,
        }
    }
}

/// Where a list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    Provider,
    Plan,
    Pi,
    Fallback,
    /// The `models` of a self-hosted subscription
    /// (`terrarium_core::selfhosted`). The daemon reads no server for this
    /// list; `POST /api/subscriptions/fetch-models` does, on request.
    Subscription,
}

/// The answer of `GET /api/subscriptions/{name}/models`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelList {
    pub models: Vec<ModelInfo>,
    pub source: ModelSource,
    pub fetched_at: String,
    /// Why an earlier source gave no list. No secret: the texts name URLs
    /// and commands, never the key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The cache: one list per subscription, keyed also by the subscription's
/// `updated_at`, so a new key or plan reads the list again.
#[derive(Default)]
pub struct ModelCache {
    lists: Mutex<HashMap<String, (String, Instant, ModelList)>>,
    /// The lists that "Fetch models" read from self-hosted servers.
    pub fetched: selfhosted::FetchCache,
}

impl ModelCache {
    fn get(&self, sub: &Subscription) -> Option<ModelList> {
        let lists = self.lists.lock().ok()?;
        let (version, at, list) = lists.get(&sub.name)?;
        (version == &sub.updated_at && at.elapsed() < CACHE_TTL).then(|| list.clone())
    }

    fn put(&self, sub: &Subscription, list: &ModelList) {
        if let Ok(mut lists) = self.lists.lock() {
            lists.insert(
                sub.name.clone(),
                (sub.updated_at.clone(), Instant::now(), list.clone()),
            );
        }
    }
}

/// The default model per harness for a subscription. See
/// `default_models` of `GET /api/subscriptions`.
pub fn default_models(sub: &Subscription) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    // A self-hosted server: its default model (else its first) for each
    // harness that it serves.
    if sub.is_selfhosted() {
        if let Some(m) = sub.selfhosted.as_ref().and_then(|s| s.model_for(None)) {
            for h in &sub.harnesses {
                out.insert(h.as_str().to_owned(), m.clone());
            }
        }
        return out;
    }
    if sub.harnesses.contains(&HarnessId::Pi) {
        let def = crate::runner_supervisor::sub_def(sub);
        // No default where pi refuses the subscription (a Claude plan).
        if let Some(m) = (terrarium_adapter_pi::refusal(&def).is_none())
            .then(|| terrarium_adapter_pi::provider_for(&def))
            .flatten()
            .and_then(|p| terrarium_adapter_pi::flags::default_model(&p))
        {
            out.insert(HarnessId::Pi.as_str().to_owned(), m.to_owned());
        }
    }
    if sub.harnesses.contains(&HarnessId::Claude) && terrarium_core::zai::is_zai(&sub.provider) {
        out.insert(
            HarnessId::Claude.as_str().to_owned(),
            terrarium_core::zai::model_for(None),
        );
    }
    out
}

/// What [`list`] needs from the daemon.
pub struct Sources<'a> {
    pub http: &'a dyn HttpGet,
    pub state_dir: &'a Path,
    /// The pi program.
    pub pi: String,
    /// The shared pi home, for a plan login.
    pub pi_home: PathBuf,
    /// The shared harness homes, for the `plan` source. A missing one is
    /// `/auth/<harness>`.
    pub homes: HashMap<HarnessId, PathBuf>,
    /// The installed Codex version, for `client_version` of the Codex list.
    /// `None`: the pin of `harnesses.toml`.
    pub codex_version: Option<String>,
}

/// The model list of a subscription, from the cache unless `refresh`.
pub async fn list(
    cache: &ModelCache,
    src: &Sources<'_>,
    sub: &Subscription,
    refresh: bool,
) -> ModelList {
    if !refresh && let Some(l) = cache.get(sub) {
        return l;
    }
    let l = fetch(src, sub).await;
    cache.put(sub, &l);
    l
}

/// Read the list from the sources, in order. Never fails: the last source
/// is the table.
pub async fn fetch(src: &Sources<'_>, sub: &Subscription) -> ModelList {
    let mut notes = Vec::new();
    // A self-hosted server: the stored list only. No request goes to the
    // server here (`selfhosted::fetch` does, for the operator).
    if sub.is_selfhosted() {
        let models = sub
            .selfhosted
            .as_ref()
            .map(|s| {
                let mut ids = s.models.clone();
                if let Some(d) = &s.default_model
                    && !ids.contains(d)
                {
                    ids.insert(0, d.clone());
                }
                ids.into_iter()
                    .map(|id| ModelInfo {
                        id,
                        name: None,
                        context: s.context_window,
                    })
                    .collect()
            })
            .unwrap_or_default();
        return done(models, ModelSource::Subscription, notes);
    }
    let key = login::read_key(src.state_dir, &sub.name);
    if let Some(key) = &key
        && let Some(base) = openai_base(sub, key)
    {
        match from_provider(src.http, &base, &key.key).await {
            Ok(models) if !models.is_empty() => return done(models, ModelSource::Provider, notes),
            Ok(_) => notes.push(format!("{base}/models listed no model")),
            Err(e) => notes.push(e),
        }
    }
    match plan::fetch(src, sub, key.as_ref()).await {
        Some(Ok(models)) if !models.is_empty() => return done(models, ModelSource::Plan, notes),
        Some(Ok(_)) => notes.push("the plan login listed no model".into()),
        Some(Err(e)) => notes.push(e),
        None => {}
    }
    let def = crate::runner_supervisor::sub_def(sub);
    if let Some(provider) = terrarium_adapter_pi::provider_for(&def) {
        let env_var = terrarium_adapter_pi::required_env(&def);
        match from_pi(src, &provider, env_var).await {
            Ok(models) if !models.is_empty() => return done(models, ModelSource::Pi, notes),
            Ok(_) => notes.push(format!("pi lists no model for provider `{provider}`")),
            Err(e) => notes.push(e),
        }
    }
    let mut ids: Vec<String> = default_models(sub).into_values().collect();
    ids.dedup();
    // Moonshot: the plan's documented model ids, the default first.
    if terrarium_core::moonshot::is_moonshot(&sub.provider) {
        let plan = terrarium_core::moonshot::plan_or_default(sub.plan.as_deref());
        for m in plan.fallback_models() {
            if !ids.iter().any(|i| i == m) {
                ids.push((*m).to_owned());
            }
        }
    }
    done(
        ids.into_iter().map(ModelInfo::id).collect(),
        ModelSource::Fallback,
        notes,
    )
}

fn done(models: Vec<ModelInfo>, source: ModelSource, notes: Vec<String>) -> ModelList {
    ModelList {
        models,
        source,
        fetched_at: crate::db::now(),
        notes,
    }
}

/// The OpenAI-compatible base URL (without `/models`) for an API-key
/// subscription, or `None` when the provider has none that Terrarium knows.
///
/// - `zai`: the plan's endpoint (`terrarium_core::zai::Plan::openai_base_url`).
///   Checked live on the coding plan (`docs/compat.md`, "Z.ai models").
/// - `moonshot`: the plan's endpoint
///   (`terrarium_core::moonshot::Plan::openai_base_url`). The Open Platform
///   documents `GET /v1/models`; Kimi Code does not, so a failure there
///   falls through to pi's catalog and the table. Not verified live.
/// - `openai`: `https://api.openai.com/v1` (a key, not a ChatGPT plan).
/// - `openrouter`: `https://openrouter.ai/api/v1`.
pub fn openai_base(sub: &Subscription, key: &ApiKey) -> Option<String> {
    if key.key.is_empty() {
        return None;
    }
    let provider = sub.provider.trim().to_ascii_lowercase();
    let base = if terrarium_core::zai::is_zai(&provider) {
        terrarium_core::zai::plan_or_default(sub.plan.as_deref())
            .openai_base_url()
            .to_owned()
    } else if terrarium_core::moonshot::is_moonshot(&provider) {
        terrarium_core::moonshot::plan_or_default(sub.plan.as_deref())
            .openai_base_url()
            .to_owned()
    } else {
        match provider.as_str() {
            "openai" => "https://api.openai.com/v1".to_owned(),
            "openrouter" => "https://openrouter.ai/api/v1".to_owned(),
            _ => return None,
        }
    };
    Some(base.trim_end_matches('/').to_owned())
}

/// The OpenAI list shape: `{"object": "list", "data": [{"id": ...}]}`.
#[derive(Deserialize)]
struct OpenAiList {
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
}

/// Parse an OpenAI-compatible `/models` body. The ids keep the provider's
/// order.
pub fn parse_openai_models(body: &str) -> Result<Vec<ModelInfo>, String> {
    let l: OpenAiList =
        serde_json::from_str(body).map_err(|e| format!("not an OpenAI model list: {e}"))?;
    Ok(l.data
        .into_iter()
        .filter(|m| !m.id.trim().is_empty())
        .map(|m| ModelInfo {
            id: m.id,
            name: m.name,
            context: m.context_length,
        })
        .collect())
}

async fn from_provider(
    http: &dyn HttpGet,
    base: &str,
    key: &str,
) -> Result<Vec<ModelInfo>, String> {
    let url = format!("{base}/models");
    let resp = http
        .get(HttpRequest {
            url: url.clone(),
            headers: vec![("Authorization".into(), format!("Bearer {key}"))],
        })
        .await
        // The error texts of the HTTP client name the URL, never a header;
        // strip the key all the same.
        .map_err(|e| format!("GET {url}: {}", e.replace(key, "<redacted>")))?;
    if resp.status != 200 {
        return Err(format!("GET {url}: status {}", resp.status));
    }
    parse_openai_models(&resp.body).map_err(|e| format!("GET {url}: {e}"))
}

/// `1M` -> 1_000_000, `204.8K` -> 204_800, `8192` -> 8192.
fn parse_tokens(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1_000.0),
        'M' | 'm' => (&s[..s.len() - 1], 1_000_000.0),
        _ => (s, 1.0),
    };
    let n: f64 = num.parse().ok()?;
    // Rounded: the table shows one decimal.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((n * mul).round() as u64)
}

/// Parse the table of `pi --list-models` (pi 1.4): a header line
/// `provider  model  context  max-out  thinking  images`, then one row per
/// model. Only the rows of `provider`.
pub fn parse_pi_list(out: &str, provider: &str) -> Vec<ModelInfo> {
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let cols: Vec<&str> = header.split_whitespace().collect();
    if cols.first() != Some(&"provider") {
        return Vec::new();
    }
    let model_col = cols.iter().position(|c| *c == "model").unwrap_or(1);
    let context_col = cols.iter().position(|c| *c == "context");
    lines
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.first() == Some(&provider)).then_some(())?;
            Some(ModelInfo {
                id: (*f.get(model_col)?).to_owned(),
                name: None,
                context: context_col
                    .and_then(|i| f.get(i))
                    .and_then(|c| parse_tokens(c)),
            })
        })
        .collect()
}

/// A fresh directory for one pi run, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let p =
            std::env::temp_dir().join(format!("terrarium-models-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn from_pi(
    src: &Sources<'_>,
    provider: &str,
    env_var: Option<&str>,
) -> Result<Vec<ModelInfo>, String> {
    let scratch = ScratchDir::new().map_err(|e| format!("cannot make a pi dir: {e}"))?;
    let mut cmd = Command::new(&src.pi);
    cmd.args(["--offline", "--list-models", provider])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &scratch.0)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let dir_env = HarnessId::Pi.home_env();
    match env_var {
        // An API key: an empty pi dir, with the Terrarium providers
        // (`zai-api`) in its `models.json`, and a placeholder key.
        Some(var) => {
            let models = terrarium_adapter_pi::config::agent_models(None);
            std::fs::write(scratch.0.join("models.json"), models.to_string())
                .map_err(|e| format!("cannot write the pi models.json: {e}"))?;
            cmd.env(dir_env, &scratch.0).env(var, PLACEHOLDER_KEY);
        }
        // A custom provider of the shared `models.json` (pi uses API
        // keys only; no plan login).
        None => {
            cmd.env(dir_env, &src.pi_home);
        }
    }
    let line = format!("`{} --offline --list-models {provider}`", src.pi);
    let out = tokio::time::timeout(PI_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("{line} took longer than {} s", PI_TIMEOUT.as_secs()))?
        .map_err(|e| format!("cannot run {line}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{line} exited with code {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    Ok(parse_pi_list(
        &String::from_utf8_lossy(&out.stdout),
        provider,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const PI_ZAI: &str = "provider  model              context  max-out  thinking  images\n\
zai       glm-4.7            204.8K   131.1K   yes       no    \n\
zai       glm-5.3            1M       131.1K   yes       no    \n\
zai       glm-5.3-flash      1M       131.1K   yes       yes   \n\
zai-api   glm-5.3            1M       131.1K   yes       no    \n";

    #[test]
    fn pi_table_rows_of_one_provider() {
        let m = parse_pi_list(PI_ZAI, "zai");
        let ids: Vec<&str> = m.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["glm-4.7", "glm-5.3", "glm-5.3-flash"]);
        assert_eq!(m[0].context, Some(204_800));
        assert_eq!(m[1].context, Some(1_000_000));
        assert_eq!(parse_pi_list(PI_ZAI, "zai-api").len(), 1);
        assert!(parse_pi_list("No models available. Use /login", "zai").is_empty());
    }

    #[test]
    fn openai_list_keeps_the_ids() {
        let body = r#"{"object":"list","data":[{"id":"glm-4.5","object":"model","created":1,"owned_by":"z-ai"},{"id":"glm-5.3-flash","object":"model"}]}"#;
        let m = parse_openai_models(body).unwrap();
        assert_eq!(
            m,
            [ModelInfo::id("glm-4.5"), ModelInfo::id("glm-5.3-flash")]
        );
        assert!(parse_openai_models("<html>").is_err());
    }

    #[test]
    fn base_urls_by_provider_and_plan() {
        let sub = |provider: &str, plan: Option<&str>| Subscription {
            name: "s".into(),
            provider: provider.into(),
            plan: plan.map(str::to_owned),
            harnesses: vec![HarnessId::Pi],
            probe: None,
            alert_at: 80,
            source: "api".into(),
            env_var: None,
            login_status: "api_key".into(),
            verified_at: None,
            probe_status: "none".into(),
            probe_error: None,
            last_probe_at: None,
            last_success_at: None,
            selfhosted: None,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let key = |base: Option<&str>| ApiKey {
            provider: "x".into(),
            key: "k".into(),
            plan: None,
            base_url: base.map(str::to_owned),
            models: vec![],
            key_id: None,
            cloud: None,
        };
        assert_eq!(
            openai_base(&sub("zai", Some("coding")), &key(None)).as_deref(),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert_eq!(
            openai_base(&sub("zai", Some("api")), &key(None)).as_deref(),
            Some("https://api.z.ai/api/paas/v4")
        );
        for (plan, want) in [
            (None, "https://api.kimi.ai/coding/v1"),
            (Some("coding-cn"), "https://api.kimi.com/coding/v1"),
            (Some("api"), "https://api.moonshot.ai/v1"),
            (Some("api-cn"), "https://api.moonshot.cn/v1"),
        ] {
            assert_eq!(
                openai_base(&sub("moonshot", plan), &key(None)).as_deref(),
                Some(want),
                "{plan:?}"
            );
        }
        // A self-hosted server is never read here (only by "Fetch
        // models", for the operator).
        assert_eq!(
            openai_base(&sub("selfhosted", None), &key(Some("http://l/v1/"))),
            None
        );
        assert_eq!(openai_base(&sub("xai", None), &key(None)), None);
    }

    mod sources {
        use super::*;
        use crate::probes::HttpResponse;
        use crate::probes::tests::FakeHttp;
        use crate::supervisor::BoxFuture;

        /// Fails each request with its header values in the error text, as
        /// a careless HTTP client could.
        struct EchoHttp;

        impl HttpGet for EchoHttp {
            fn get(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
                let all: Vec<String> = req.headers.into_iter().map(|(_, v)| v).collect();
                Box::pin(async move { Err(format!("refused: {}", all.join(" "))) })
            }
        }

        const CLAUDE: &str = include_str!("../tests/fixtures/models/anthropic_models.json");
        const CODEX: &str = include_str!("../tests/fixtures/models/codex_models.json");
        const SECRET: &str = "tok-secret-123";

        fn sub(provider: &str, h: HarnessId) -> Subscription {
            Subscription {
                name: "s".into(),
                provider: provider.into(),
                plan: None,
                harnesses: vec![h],
                probe: None,
                alert_at: 80,
                source: "api".into(),
                env_var: None,
                login_status: "logged_in".into(),
                verified_at: None,
                probe_status: "none".into(),
                probe_error: None,
                last_probe_at: None,
                last_success_at: None,
                selfhosted: None,
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            }
        }

        struct Env {
            state: tempfile::TempDir,
            home: tempfile::TempDir,
        }

        impl Env {
            fn new() -> Self {
                let e = Self {
                    state: tempfile::tempdir().unwrap(),
                    home: tempfile::tempdir().unwrap(),
                };
                std::fs::write(
                    e.home.path().join(".credentials.json"),
                    format!(r#"{{"claudeAiOauth":{{"accessToken":"{SECRET}","expiresAt":4102444800000}}}}"#),
                )
                .unwrap();
                std::fs::write(
                    e.home.path().join("auth.json"),
                    format!(r#"{{"tokens":{{"access_token":"{SECRET}","account_id":"acct-9"}}}}"#),
                )
                .unwrap();
                e
            }

            fn src<'a>(&'a self, http: &'a dyn HttpGet, codex: Option<&str>) -> Sources<'a> {
                Sources {
                    http,
                    state_dir: self.state.path(),
                    pi: "/nonexistent/pi".into(),
                    pi_home: self.home.path().into(),
                    homes: [HarnessId::Claude, HarnessId::Codex, HarnessId::Copilot]
                        .into_iter()
                        .map(|h| (h, self.home.path().to_owned()))
                        .collect(),
                    codex_version: codex.map(str::to_owned),
                }
            }
        }

        #[tokio::test]
        async fn a_claude_login_lists_from_the_plan() {
            let env = Env::new();
            let http = FakeHttp::with(
                &format!("{}?limit=1000", plan::ANTHROPIC_MODELS),
                200,
                CLAUDE,
            );
            let l = fetch(&env.src(&http, None), &sub("anthropic", HarnessId::Claude)).await;
            assert_eq!(l.source, ModelSource::Plan);
            assert_eq!(l.models.len(), 14);
            assert!(l.notes.is_empty(), "{:?}", l.notes);
            let seen = http.seen.lock().unwrap();
            assert!(
                seen[0]
                    .headers
                    .iter()
                    .any(|(k, v)| k == "Authorization" && v == &format!("Bearer {SECRET}"))
            );
        }

        #[tokio::test]
        async fn a_refused_plan_falls_back_with_a_note() {
            let env = Env::new();
            let http = FakeHttp::with(&format!("{}?limit=1000", plan::ANTHROPIC_MODELS), 401, "{}");
            let l = fetch(&env.src(&http, None), &sub("anthropic", HarnessId::Claude)).await;
            assert_eq!(l.source, ModelSource::Fallback);
            assert!(l.notes[0].contains("status 401"), "{:?}", l.notes);
            assert!(l.notes.iter().all(|n| !n.contains(SECRET)));
        }

        #[tokio::test]
        async fn no_token_in_the_notes() {
            let env = Env::new();
            for (provider, h) in [
                ("anthropic", HarnessId::Claude),
                ("openai", HarnessId::Codex),
            ] {
                let l = fetch(&env.src(&EchoHttp, None), &sub(provider, h)).await;
                assert_eq!(l.source, ModelSource::Fallback);
                assert!(l.notes[0].contains("<redacted>"), "{:?}", l.notes);
                assert!(
                    l.notes
                        .iter()
                        .all(|n| !n.contains(SECRET) && !n.contains("acct-9")),
                    "{:?}",
                    l.notes
                );
            }
        }

        #[tokio::test]
        async fn codex_asks_with_the_installed_version_else_the_pin() {
            let env = Env::new();
            let url = |v: &str| format!("{}?client_version={v}", plan::CODEX_MODELS);
            let http = FakeHttp::with(&url("0.165.2"), 200, CODEX);
            let l = fetch(
                &env.src(&http, Some("0.165.2")),
                &sub("openai", HarnessId::Codex),
            )
            .await;
            assert_eq!(l.source, ModelSource::Plan);
            assert_eq!(l.models[0].id, "gpt-6.1-sol");
            assert!(
                http.seen.lock().unwrap()[0]
                    .headers
                    .iter()
                    .any(|(k, v)| k == "ChatGPT-Account-Id" && v == "acct-9")
            );
            let pin = terrarium_core::harnesses::pin(HarnessId::Codex).version;
            let http = FakeHttp::with(&url(pin), 200, CODEX);
            let l = fetch(&env.src(&http, None), &sub("openai", HarnessId::Codex)).await;
            assert_eq!(l.source, ModelSource::Plan);
        }

        #[tokio::test]
        async fn a_key_goes_to_the_provider_first() {
            let env = Env::new();
            let key = ApiKey {
                provider: "openai".into(),
                key: SECRET.into(),
                plan: None,
                base_url: None,
                models: vec![],
                key_id: None,
                cloud: None,
            };
            login::store_key(env.state.path(), "s", &key).unwrap();
            let http = FakeHttp::with(
                "https://api.openai.com/v1/models",
                200,
                r#"{"data":[{"id":"gpt-6-luna"}]}"#,
            );
            let l = fetch(&env.src(&http, None), &sub("openai", HarnessId::Codex)).await;
            assert_eq!(l.source, ModelSource::Provider);
            // A failed provider with a key does not try the ChatGPT login.
            let http = FakeHttp::default();
            let l = fetch(&env.src(&http, None), &sub("openai", HarnessId::Codex)).await;
            assert_eq!(l.source, ModelSource::Fallback);
            assert!(
                http.seen
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|r| !r.url.starts_with(plan::CODEX_MODELS))
            );
            // An Anthropic key lists Anthropic's models with x-api-key.
            let http = FakeHttp::with(
                &format!("{}?limit=1000", plan::ANTHROPIC_MODELS),
                200,
                CLAUDE,
            );
            let l = fetch(&env.src(&http, None), &sub("anthropic", HarnessId::Claude)).await;
            assert_eq!(l.source, ModelSource::Plan);
            let seen = http.seen.lock().unwrap();
            assert!(
                seen[0]
                    .headers
                    .iter()
                    .any(|(k, v)| k == "x-api-key" && v == SECRET)
            );
            assert!(!seen[0].headers.iter().any(|(k, _)| k == "Authorization"));
        }
    }

    #[test]
    fn token_counts() {
        assert_eq!(parse_tokens("1M"), Some(1_000_000));
        assert_eq!(parse_tokens("204.8K"), Some(204_800));
        assert_eq!(parse_tokens("8192"), Some(8192));
        assert_eq!(parse_tokens("-"), None);
    }
}
