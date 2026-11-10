//! The `plan` source: the model list of a harness login, read with the
//! login's own credential. Verified live on 2026-10-26 (docs/compat.md,
//! "Model lists (plan logins)").
//!
//! - Claude Code: `GET https://api.anthropic.com/v1/models?limit=1000` with
//!   the OAuth token (`Authorization: Bearer`, `anthropic-beta:
//!   oauth-2025-04-20`), or with a stored Anthropic key (`x-api-key`). The
//!   API order stays. `has_more` pages on with `after_id`.
//! - Codex: `GET https://chatgpt.com/backend-api/codex/models?client_version=<v>`
//!   with the ChatGPT token and `ChatGPT-Account-Id`. Only the rows with
//!   `visibility == "list"`, sorted by `priority`.
//! - Copilot: `GET https://api.github.com/copilot_internal/user` gives
//!   `endpoints.api`, then `GET <api>/models` with `Copilot-Integration-Id`.
//!   Only the rows with `model_picker_enabled`, `capabilities.type ==
//!   "chat"`, and a `policy.state` that is not `disabled`. The API order
//!   stays. The ids keep their dots (`claude-sonnet-4.6`): Copilot's
//!   `--model` takes them so.
//!
//! The credential readers are the ones of the usage probes
//! ([`crate::probes::read_credentials`]). A token never leaves the daemon:
//! each error text goes through [`redact`].

use std::path::PathBuf;

use serde::Deserialize;
use terrarium_core::HarnessId;

use super::{ModelInfo, Sources};
use crate::login_session::ApiKey;
use crate::probes::{
    Credentials, HttpGet, HttpRequest, QuotaProbe, anthropic::AnthropicOauthUsage,
    github::CopilotQuota, openai::OpenAiCodexUsage,
};
use crate::store::subscriptions::Subscription;

pub const ANTHROPIC_MODELS: &str = "https://api.anthropic.com/v1/models";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
pub const CODEX_MODELS: &str = "https://chatgpt.com/backend-api/codex/models";
pub const COPILOT_USER: &str = crate::probes::github::URL;
/// The Copilot API when `endpoints.api` names none.
pub const COPILOT_API: &str = "https://api.githubcopilot.com";
/// The integration id of the Copilot CLI.
pub const COPILOT_INTEGRATION: &str = "copilot-developer-cli";

/// The most Anthropic pages to read (1000 models each).
const MAX_PAGES: usize = 10;

/// The credential of a plan list.
#[derive(Debug)]
pub enum PlanAuth {
    /// The Claude Code OAuth login.
    ClaudeOauth(Credentials),
    /// A stored Anthropic API key.
    AnthropicKey(Credentials),
    /// The Codex ChatGPT login: the token and the account id.
    Codex(Credentials),
    /// A GitHub token for Copilot.
    Copilot(Credentials),
}

impl PlanAuth {
    fn creds(&self) -> &Credentials {
        match self {
            Self::ClaudeOauth(c) | Self::AnthropicKey(c) | Self::Codex(c) | Self::Copilot(c) => c,
        }
    }

    /// The secrets to strip from an error text.
    fn secrets(&self) -> Vec<&str> {
        let c = self.creds();
        let mut s = vec![c.token()];
        if let Some(a) = &c.account_id {
            s.push(a);
        }
        s.retain(|x| !x.is_empty());
        s
    }
}

/// The plan kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Claude,
    Codex,
    Copilot,
}

/// The plan list that fits `sub`, or `None` when the plan source does not
/// apply.
///
/// - `anthropic` with Claude Code, or with a stored Anthropic key: Claude.
/// - `openai` / `openai-codex` with Codex and no stored key (a key uses the
///   `provider` source): Codex.
/// - `github` / `github-copilot` / `copilot` with Copilot: Copilot.
pub fn kind(sub: &Subscription, key: Option<&ApiKey>) -> Option<PlanKind> {
    let has = |h: HarnessId| sub.harnesses.contains(&h);
    let has_key = key.is_some_and(|k| !k.key.is_empty());
    match sub.provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" if has(HarnessId::Claude) || has_key => Some(PlanKind::Claude),
        "openai" | "openai-codex" if has(HarnessId::Codex) && !has_key => Some(PlanKind::Codex),
        "github" | "github-copilot" | "copilot" if has(HarnessId::Copilot) => {
            Some(PlanKind::Copilot)
        }
        _ => None,
    }
}

/// Read the credential of `kind`. This blocks (file reads).
pub fn read_auth(
    kind: PlanKind,
    src_state_dir: &std::path::Path,
    sub: &str,
    key: Option<&ApiKey>,
    home: &std::path::Path,
) -> Result<PlanAuth, String> {
    let err = |e: crate::probes::ProbeError| e.to_string();
    match kind {
        PlanKind::Claude => match key.filter(|k| !k.key.is_empty()) {
            Some(k) => Ok(PlanAuth::AnthropicKey(Credentials::new(k.key.clone()))),
            None => crate::probes::read_credentials(&AnthropicOauthUsage, src_state_dir, sub, home)
                .map(PlanAuth::ClaudeOauth)
                .map_err(err),
        },
        PlanKind::Codex => {
            crate::probes::read_credentials(&OpenAiCodexUsage, src_state_dir, sub, home)
                .map(PlanAuth::Codex)
                .map_err(err)
        }
        PlanKind::Copilot => {
            crate::probes::read_credentials(&CopilotQuota, src_state_dir, sub, home)
                .map(PlanAuth::Copilot)
                .map_err(err)
        }
    }
}

/// The harness whose home holds the login of `kind`.
pub fn harness(kind: PlanKind) -> HarnessId {
    match kind {
        PlanKind::Claude => AnthropicOauthUsage.harness(),
        PlanKind::Codex => OpenAiCodexUsage.harness(),
        PlanKind::Copilot => CopilotQuota.harness(),
    }
}

/// Replace each secret in `text` with `<redacted>`.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|s| !s.is_empty())
        .fold(text.to_owned(), |t, s| t.replace(s, "<redacted>"))
}

/// The model list of the plan login of `sub`. `Ok(None)`: the plan source
/// does not apply. An error text names the URL, never a secret.
pub async fn fetch(
    src: &Sources<'_>,
    sub: &Subscription,
    key: Option<&ApiKey>,
) -> Option<Result<Vec<ModelInfo>, String>> {
    let kind = kind(sub, key)?;
    let home: PathBuf = src
        .homes
        .get(&harness(kind))
        .cloned()
        .unwrap_or_else(|| PathBuf::from(harness(kind).auth_dir()));
    let (dir, name, key) = (src.state_dir.to_owned(), sub.name.clone(), key.cloned());
    let auth = match tokio::task::spawn_blocking(move || {
        read_auth(kind, &dir, &name, key.as_ref(), &home)
    })
    .await
    {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return Some(Err(format!("plan login: {e}"))),
        Err(e) => return Some(Err(format!("plan login: the credential task failed: {e}"))),
    };
    if let Err(e) = auth.creds().check_fresh(chrono::Utc::now()) {
        return Some(Err(format!("plan login: {e}")));
    }
    let version = src.codex_version.clone().unwrap_or_else(|| {
        terrarium_core::harnesses::pin(HarnessId::Codex)
            .version
            .to_owned()
    });
    let secrets = auth.secrets();
    let out = with_auth(src.http, &auth, &version).await;
    Some(out.map_err(|e| redact(&e, &secrets)))
}

/// Read the list with a credential in hand.
pub async fn with_auth(
    http: &dyn HttpGet,
    auth: &PlanAuth,
    codex_version: &str,
) -> Result<Vec<ModelInfo>, String> {
    match auth {
        PlanAuth::ClaudeOauth(_) | PlanAuth::AnthropicKey(_) => anthropic(http, auth).await,
        PlanAuth::Codex(c) => {
            let req = codex_request(c, codex_version);
            parse_codex_models(&get(http, req).await?)
        }
        PlanAuth::Copilot(c) => {
            let api = copilot_api(http, c).await;
            parse_copilot_models(&get(http, copilot_models_request(&api, c)).await?)
        }
    }
}

/// Send `req`; the body of a 200, else an error that names the URL.
async fn get(http: &dyn HttpGet, req: HttpRequest) -> Result<String, String> {
    let url = req.url.clone();
    let resp = http.get(req).await.map_err(|e| format!("GET {url}: {e}"))?;
    if resp.status != 200 {
        return Err(format!("GET {url}: status {}", resp.status));
    }
    Ok(resp.body)
}

/// `GET /v1/models` of Anthropic, one page. `after`: the `after_id`.
pub fn anthropic_request(auth: &PlanAuth, after: Option<&str>) -> HttpRequest {
    let mut url = format!("{ANTHROPIC_MODELS}?limit=1000");
    if let Some(a) = after {
        url.push_str("&after_id=");
        url.push_str(a);
    }
    let mut headers = vec![
        ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
        ("Accept".into(), "application/json".into()),
    ];
    match auth {
        PlanAuth::AnthropicKey(c) => headers.push(("x-api-key".into(), c.token().to_owned())),
        other => {
            headers.push((
                "Authorization".into(),
                format!("Bearer {}", other.creds().token()),
            ));
            headers.push(("anthropic-beta".into(), ANTHROPIC_OAUTH_BETA.into()));
        }
    }
    HttpRequest { url, headers }
}

async fn anthropic(http: &dyn HttpGet, auth: &PlanAuth) -> Result<Vec<ModelInfo>, String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let body = get(http, anthropic_request(auth, after.as_deref())).await?;
        let page = parse_anthropic_models(&body)?;
        out.extend(page.models);
        match page.next {
            Some(n) if page.has_more => after = Some(n),
            _ => break,
        }
    }
    Ok(out)
}

/// One page of Anthropic's `/v1/models`.
#[derive(Debug, PartialEq)]
pub struct AnthropicPage {
    pub models: Vec<ModelInfo>,
    pub has_more: bool,
    /// `last_id`: the `after_id` of the next page.
    pub next: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicList {
    data: Vec<AnthropicModel>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
}

/// Parse a page of Anthropic's `/v1/models`. The API order stays.
pub fn parse_anthropic_models(body: &str) -> Result<AnthropicPage, String> {
    let l: AnthropicList =
        serde_json::from_str(body).map_err(|e| format!("not an Anthropic model list: {e}"))?;
    Ok(AnthropicPage {
        models: l
            .data
            .into_iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| ModelInfo {
                id: m.id,
                name: m.display_name,
                context: m.max_input_tokens,
            })
            .collect(),
        has_more: l.has_more,
        next: l.last_id,
    })
}

/// `GET /backend-api/codex/models` with the ChatGPT login.
pub fn codex_request(c: &Credentials, client_version: &str) -> HttpRequest {
    let mut headers = vec![
        ("Authorization".into(), format!("Bearer {}", c.token())),
        ("Accept".into(), "application/json".into()),
    ];
    if let Some(a) = &c.account_id {
        headers.push(("ChatGPT-Account-Id".into(), a.clone()));
    }
    HttpRequest {
        url: format!("{CODEX_MODELS}?client_version={client_version}"),
        headers,
    }
}

#[derive(Deserialize)]
struct CodexList {
    models: Vec<CodexModel>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    priority: Option<i64>,
}

/// Parse the Codex list: the rows with `visibility == "list"`, by
/// `priority` (a row with none goes last; equal priorities keep the API
/// order).
pub fn parse_codex_models(body: &str) -> Result<Vec<ModelInfo>, String> {
    let l: CodexList =
        serde_json::from_str(body).map_err(|e| format!("not a Codex model list: {e}"))?;
    let mut rows: Vec<CodexModel> = l
        .models
        .into_iter()
        .filter(|m| m.visibility.as_deref() == Some("list") && !m.slug.trim().is_empty())
        .collect();
    rows.sort_by_key(|m| m.priority.unwrap_or(i64::MAX));
    Ok(rows
        .into_iter()
        .map(|m| ModelInfo {
            id: m.slug,
            name: m.display_name,
            context: m.context_window,
        })
        .collect())
}

/// `GET /copilot_internal/user`, for `endpoints.api`.
pub fn copilot_user_request(c: &Credentials) -> HttpRequest {
    HttpRequest {
        url: COPILOT_USER.into(),
        headers: vec![
            ("Authorization".into(), format!("token {}", c.token())),
            ("Accept".into(), "application/json".into()),
        ],
    }
}

/// `endpoints.api` of a `copilot_internal/user` body, when it is an
/// `https://` URL.
pub fn parse_copilot_api(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let api = v.get("endpoints")?.get("api")?.as_str()?.trim();
    api.starts_with("https://")
        .then(|| api.trim_end_matches('/').to_owned())
}

/// The Copilot API of the token's account. [`COPILOT_API`] when the user
/// endpoint does not name one.
async fn copilot_api(http: &dyn HttpGet, c: &Credentials) -> String {
    match get(http, copilot_user_request(c)).await {
        Ok(body) => parse_copilot_api(&body).unwrap_or_else(|| COPILOT_API.to_owned()),
        Err(_) => COPILOT_API.to_owned(),
    }
}

/// `GET <api>/models` of Copilot.
pub fn copilot_models_request(api: &str, c: &Credentials) -> HttpRequest {
    HttpRequest {
        url: format!("{api}/models"),
        headers: vec![
            ("Authorization".into(), format!("Bearer {}", c.token())),
            ("Copilot-Integration-Id".into(), COPILOT_INTEGRATION.into()),
            ("Accept".into(), "application/json".into()),
        ],
    }
}

#[derive(Deserialize)]
struct CopilotList {
    data: Vec<CopilotModel>,
}

#[derive(Deserialize)]
struct CopilotModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model_picker_enabled: bool,
    #[serde(default)]
    policy: Option<CopilotPolicy>,
    #[serde(default)]
    capabilities: Option<CopilotCaps>,
}

#[derive(Deserialize)]
struct CopilotPolicy {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
struct CopilotCaps {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    limits: Option<CopilotLimits>,
}

#[derive(Deserialize)]
struct CopilotLimits {
    #[serde(default)]
    max_context_window_tokens: Option<u64>,
}

/// Parse Copilot's `/models`: the picker's chat models that the policy does
/// not disable. The API order stays; the ids stay as they are.
pub fn parse_copilot_models(body: &str) -> Result<Vec<ModelInfo>, String> {
    let l: CopilotList =
        serde_json::from_str(body).map_err(|e| format!("not a Copilot model list: {e}"))?;
    Ok(l.data
        .into_iter()
        .filter(|m| {
            m.model_picker_enabled
                && m.capabilities.as_ref().and_then(|c| c.kind.as_deref()) == Some("chat")
                && m.policy.as_ref().and_then(|p| p.state.as_deref()) != Some("disabled")
                && !m.id.trim().is_empty()
        })
        .map(|m| ModelInfo {
            context: m
                .capabilities
                .as_ref()
                .and_then(|c| c.limits.as_ref())
                .and_then(|l| l.max_context_window_tokens),
            id: m.id,
            name: m.name,
        })
        .collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::probes::HttpResponse;
    use crate::probes::tests::FakeHttp;

    const CLAUDE: &str = include_str!("../../tests/fixtures/models/anthropic_models.json");
    const CODEX: &str = include_str!("../../tests/fixtures/models/codex_models.json");
    const COPILOT: &str = include_str!("../../tests/fixtures/models/copilot_models.json");
    const COPILOT_USER_BODY: &str =
        include_str!("../../tests/fixtures/probes/github_copilot_user.json");

    fn ids(m: &[ModelInfo]) -> Vec<&str> {
        m.iter().map(|x| x.id.as_str()).collect()
    }

    fn header<'a>(r: &'a HttpRequest, k: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn anthropic_list_keeps_the_api_order() {
        let p = parse_anthropic_models(CLAUDE).unwrap();
        assert_eq!(p.models.len(), 14);
        assert_eq!(
            ids(&p.models)[..4],
            [
                "claude-haiku-5-5",
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "claude-fable-5-1"
            ]
        );
        assert_eq!(p.models[0].name.as_deref(), Some("Claude Haiku 5.5"));
        assert_eq!(p.models[0].context, Some(1_000_000));
        assert_eq!(p.models[13].context, Some(200_000));
        assert!(!p.has_more);
        assert!(parse_anthropic_models("<html>").is_err());
    }

    #[test]
    fn codex_list_keeps_listed_rows_by_priority() {
        let m = parse_codex_models(CODEX).unwrap();
        assert_eq!(
            ids(&m),
            [
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna"
            ]
        );
        assert_eq!(m[0].name.as_deref(), Some("GPT-6.1-Sol"));
        assert_eq!(m[0].context, Some(500_000));
        // Out of order in the body: the sort puts the lower priority first.
        let body = r#"{"models":[{"slug":"b","visibility":"list","priority":5},
            {"slug":"a","visibility":"list","priority":1},{"slug":"c","visibility":"list"},
            {"slug":"h","visibility":"hide","priority":0}]}"#;
        assert_eq!(ids(&parse_codex_models(body).unwrap()), ["a", "b", "c"]);
        assert!(parse_codex_models(r#"{"data":[]}"#).is_err());
    }

    #[test]
    fn copilot_list_keeps_picker_chat_models_not_disabled() {
        let m = parse_copilot_models(COPILOT).unwrap();
        assert_eq!(
            ids(&m),
            [
                "claude-sonnet-4.6",
                "gpt-5.3-codex",
                "mai-code-1.1-flash",
                "claude-haiku-4.5"
            ]
        );
        assert_eq!(m[0].name.as_deref(), Some("Claude Sonnet 4.6"));
        assert_eq!(m[0].context, Some(264_000));
        assert_eq!(m[3].context, Some(144_000));
    }

    #[test]
    fn copilot_api_from_the_user_endpoint() {
        assert_eq!(
            parse_copilot_api(
                r#"{"endpoints":{"api":"https://api.individual.githubcopilot.com/"}}"#
            )
            .as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        assert_eq!(
            parse_copilot_api(r#"{"endpoints":{"api":"http://evil"}}"#),
            None
        );
        assert_eq!(parse_copilot_api(r#"{"login":"x"}"#), None);
        assert_eq!(parse_copilot_api("<html>"), None);
    }

    #[test]
    fn requests_carry_the_right_headers() {
        let oauth = PlanAuth::ClaudeOauth(Credentials::new("tok"));
        let r = anthropic_request(&oauth, None);
        assert_eq!(r.url, "https://api.anthropic.com/v1/models?limit=1000");
        assert_eq!(header(&r, "Authorization"), Some("Bearer tok"));
        assert_eq!(header(&r, "anthropic-beta"), Some("oauth-2025-04-20"));
        assert_eq!(header(&r, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(header(&r, "x-api-key"), None);
        let key = PlanAuth::AnthropicKey(Credentials::new("sk-ant"));
        let r = anthropic_request(&key, Some("m-9"));
        assert!(r.url.ends_with("&after_id=m-9"));
        assert_eq!(header(&r, "x-api-key"), Some("sk-ant"));
        assert_eq!(header(&r, "Authorization"), None);
        assert_eq!(header(&r, "anthropic-beta"), None);

        let mut c = Credentials::new("tok");
        c.account_id = Some("acct".into());
        let r = codex_request(&c, "0.164.0");
        assert_eq!(
            r.url,
            "https://chatgpt.com/backend-api/codex/models?client_version=0.164.0"
        );
        assert_eq!(header(&r, "Authorization"), Some("Bearer tok"));
        assert_eq!(header(&r, "ChatGPT-Account-Id"), Some("acct"));

        let c = Credentials::new("gho");
        assert_eq!(
            header(&copilot_user_request(&c), "Authorization"),
            Some("token gho")
        );
        let r = copilot_models_request("https://api.x", &c);
        assert_eq!(r.url, "https://api.x/models");
        assert_eq!(header(&r, "Authorization"), Some("Bearer gho"));
        assert_eq!(
            header(&r, "Copilot-Integration-Id"),
            Some("copilot-developer-cli")
        );
    }

    #[tokio::test]
    async fn anthropic_follows_has_more() {
        let http = FakeHttp::default();
        let page = |ids: &[&str], more: bool| {
            let data: Vec<String> = ids
                .iter()
                .map(|i| format!(r#"{{"id":"{i}","type":"model"}}"#))
                .collect();
            format!(
                r#"{{"data":[{}],"has_more":{more},"last_id":"{}"}}"#,
                data.join(","),
                ids.last().unwrap()
            )
        };
        {
            let mut a = http.answers.lock().unwrap();
            a.insert(
                format!("{ANTHROPIC_MODELS}?limit=1000"),
                HttpResponse {
                    status: 200,
                    body: page(&["m1", "m2"], true),
                },
            );
            a.insert(
                format!("{ANTHROPIC_MODELS}?limit=1000&after_id=m2"),
                HttpResponse {
                    status: 200,
                    body: page(&["m3"], false),
                },
            );
        }
        let m = with_auth(&http, &PlanAuth::ClaudeOauth(Credentials::new("t")), "0")
            .await
            .unwrap();
        assert_eq!(ids(&m), ["m1", "m2", "m3"]);
    }

    #[tokio::test]
    async fn copilot_discovers_the_api() {
        let http = FakeHttp::with(
            COPILOT_USER,
            200,
            r#"{"endpoints":{"api":"https://api.individual.githubcopilot.com"}}"#,
        );
        http.answers.lock().unwrap().insert(
            "https://api.individual.githubcopilot.com/models".into(),
            HttpResponse {
                status: 200,
                body: COPILOT.into(),
            },
        );
        let m = with_auth(&http, &PlanAuth::Copilot(Credentials::new("gho")), "0")
            .await
            .unwrap();
        assert_eq!(m.len(), 4);

        // No `endpoints.api` (the probe fixture has none): the default API.
        let http = FakeHttp::with(COPILOT_USER, 200, COPILOT_USER_BODY);
        http.answers.lock().unwrap().insert(
            format!("{COPILOT_API}/models"),
            HttpResponse {
                status: 200,
                body: COPILOT.into(),
            },
        );
        let m = with_auth(&http, &PlanAuth::Copilot(Credentials::new("gho")), "0")
            .await
            .unwrap();
        assert_eq!(m.len(), 4);
        let seen = http.seen.lock().unwrap();
        assert_eq!(seen[1].url, format!("{COPILOT_API}/models"));
    }

    #[test]
    fn the_kind_follows_provider_harness_and_key() {
        let sub = |provider: &str, h: HarnessId| Subscription {
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
        };
        let key = ApiKey {
            provider: "x".into(),
            key: "k".into(),
            plan: None,
            base_url: None,
            models: vec![],
            key_id: None,
            cloud: None,
        };
        assert_eq!(
            kind(&sub("anthropic", HarnessId::Claude), None),
            Some(PlanKind::Claude)
        );
        assert_eq!(
            kind(&sub("anthropic", HarnessId::Pi), Some(&key)),
            Some(PlanKind::Claude)
        );
        assert_eq!(kind(&sub("anthropic", HarnessId::Pi), None), None);
        assert_eq!(
            kind(&sub("openai", HarnessId::Codex), None),
            Some(PlanKind::Codex)
        );
        assert_eq!(kind(&sub("openai", HarnessId::Codex), Some(&key)), None);
        assert_eq!(
            kind(&sub("github", HarnessId::Copilot), None),
            Some(PlanKind::Copilot)
        );
        assert_eq!(kind(&sub("zai", HarnessId::Claude), None), None);
        assert_eq!(kind(&sub("moonshot", HarnessId::Pi), None), None);
    }

    #[test]
    fn redact_strips_each_secret() {
        assert_eq!(
            redact("GET u?t=tok acct: boom", &["tok", "acct", ""]),
            "GET u?t=<redacted> <redacted>: boom"
        );
    }
}
