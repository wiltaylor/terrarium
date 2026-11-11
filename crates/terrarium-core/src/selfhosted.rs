//! A self-hosted model server as a subscription provider (PLAN §16).
//!
//! The operator runs a model server (vLLM, Ollama, llama.cpp, SGLang, LM
//! Studio, a proxy) and adds it as a subscription with the provider
//! `selfhosted`. The server speaks one of two API styles ([`Api`]):
//!
//! | `api` | Base URL | Harnesses | How the harness gets the endpoint |
//! |---|---|---|---|
//! | `openai` | the OpenAI root, with `/v1` (`http://gpu:8000/v1`) | pi, Codex | pi: the custom provider [`PI_PROVIDER`] (`openai-completions`) in the per-agent `models.json`; Codex: the `-c` model provider [`CODEX_PROVIDER`] (`wire_api = "responses"`) |
//! | `anthropic` | the Anthropic root, without `/v1` (`http://gpu:8000`) | Claude Code, pi | Claude Code: `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`, as for Z.ai; pi: the custom provider [`PI_PROVIDER`] (`anthropic-messages`) |
//!
//! Copilot CLI cannot use a self-hosted server. Codex 0.166 takes only the
//! Responses API (`wire_api = "chat"` is "no longer supported"), so a
//! Codex agent needs a server with `/v1/responses` (vLLM and Ollama have
//! it; not every server does).
//!
//! The API key is optional: many local servers need none. A stored key
//! goes into [`KEY_ENV`] through `terrarium exec-env`, as every stored key
//! does. With no key, each harness gets the placeholder [`NO_KEY`], so no
//! harness sends another credential (the shared Claude login) to the
//! server.
//!
//! The limits ([`SelfHosted::context_window`],
//! [`SelfHosted::max_output_tokens`]) go to each harness where it has a
//! setting. [`SelfHosted::max_concurrent`] is the number of agents on the
//! subscription that may run a turn at the same time; the daemon enforces
//! it (`terrariumd::stream_limit`).
//!
//! `openai-compatible` (the provider word before this module) is an alias:
//! the daemon stores it as `selfhosted` with `api = "openai"`.

use serde::{Deserialize, Serialize};

use crate::HarnessId;

/// The provider word.
pub const PROVIDER: &str = "selfhosted";
/// The old provider word, taken as an alias of `selfhosted` with
/// `api = "openai"`.
pub const ALIAS_OPENAI_COMPATIBLE: &str = "openai-compatible";

/// The env var that carries the stored key (the subscription's
/// `env_var`). pi and Codex read it.
pub const KEY_ENV: &str = "TERRARIUM_SELFHOSTED_API_KEY";
/// The value of the key vars when no key is stored. Local servers that
/// need no key ignore it.
pub const NO_KEY: &str = "terrarium-no-key";

/// The custom pi provider in the per-agent `models.json`.
pub const PI_PROVIDER: &str = "terrarium-selfhosted";
/// The Codex model provider id (`-c model_providers.<id>`).
pub const CODEX_PROVIDER: &str = "terrarium_selfhosted";

/// Claude Code's request timeout, in ms: a local model can be slow (the
/// Z.ai value, `crate::zai::API_TIMEOUT_MS`).
pub const API_TIMEOUT_MS: &str = "3000000";

/// The smallest context window that the daemon takes.
pub const MIN_CONTEXT_WINDOW: u64 = 1024;
/// The largest number of concurrent streams that the daemon takes.
pub const MAX_CONCURRENT: u32 = 1024;
/// The longest URL that the daemon takes.
pub const MAX_URL_LEN: usize = 2048;

/// The API style of the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    /// OpenAI Chat Completions (pi) and Responses (Codex).
    #[default]
    Openai,
    /// Anthropic Messages.
    Anthropic,
}

impl Api {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    /// The harnesses that can use a server of this style.
    pub fn harnesses(self) -> &'static [HarnessId] {
        match self {
            Self::Openai => &[HarnessId::Pi, HarnessId::Codex],
            Self::Anthropic => &[HarnessId::Claude, HarnessId::Pi],
        }
    }

    /// The `api` of pi's `models.json`.
    pub fn pi_api(self) -> &'static str {
        match self {
            Self::Openai => "openai-completions",
            Self::Anthropic => "anthropic-messages",
        }
    }
}

impl std::str::FromStr for Api {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(Self::Openai),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(format!(
                "unknown api style `{other}`; use `openai` or `anthropic`"
            )),
        }
    }
}

/// The server of a `selfhosted` subscription (`[selfhosted]` in
/// `subscriptions/<name>.toml`, `selfhosted` in the API). No secret: the
/// key is in the daemon's key store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SelfHosted {
    /// Optional monthly USD fallback budget for LiteLLM, counted from session costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<u32>,
    #[serde(default)]
    pub api: Api,
    /// `http` or `https`. A LAN host or `localhost` is fine.
    pub base_url: String,
    /// The model ids that the server serves (typed, or from a fetch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// The model of an agent that sets none. `None`: the first of
    /// `models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// The context window of the model, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The most output tokens of one answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// How many agents on the subscription may run a turn at the same
    /// time. `None`: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// The status page of the server, when the operator has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_url: Option<String>,
}

/// True when `provider` is `selfhosted` or its alias `openai-compatible`.
pub fn is_selfhosted(provider: &str) -> bool {
    let p = provider.trim();
    p.eq_ignore_ascii_case(PROVIDER)
        || p.eq_ignore_ascii_case(ALIAS_OPENAI_COMPATIBLE)
        || p.eq_ignore_ascii_case("litellm")
}

/// True when `provider` is the old word `openai-compatible`.
pub fn is_alias(provider: &str) -> bool {
    provider
        .trim()
        .eq_ignore_ascii_case(ALIAS_OPENAI_COMPATIBLE)
}

/// Check an `http(s)` URL: a scheme, a host, no user or password in it,
/// no white space or control characters, and at most [`MAX_URL_LEN`]
/// characters. `allow_query`: a `?` or `#` part is fine. Returns the URL
/// without white space round it and without a trailing `/`.
pub fn check_http_url(what: &str, url: &str, allow_query: bool) -> Result<String, String> {
    let u = url.trim().trim_end_matches('/');
    if u.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if u.len() > MAX_URL_LEN {
        return Err(format!("{what} is longer than {MAX_URL_LEN} characters"));
    }
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{what} has white space or a control character"));
    }
    let lower = u.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("http://") {
        &u[u.len() - r.len()..]
    } else if let Some(r) = lower.strip_prefix("https://") {
        &u[u.len() - r.len()..]
    } else {
        return Err(format!("{what} must start with http:// or https://"));
    };
    if !allow_query && (rest.contains('?') || rest.contains('#')) {
        return Err(format!(
            "{what} must not have a query (`?`) or a fragment (`#`)"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(format!(
            "{what} must not hold a user or a password; store the key with the API key instead"
        ));
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let Some((h, after)) = v6.split_once(']') else {
            return Err(format!("{what} has a bad IPv6 host"));
        };
        (h, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(format!("{what} has no host"));
    }
    if let Some(p) = port
        && (p.is_empty() || p.parse::<u16>().is_err())
    {
        return Err(format!("{what} has a bad port `{p}`"));
    }
    Ok(u.to_owned())
}

impl SelfHosted {
    /// Check the fields and give them in their stored form: a clean base
    /// URL, models with no empty or repeated id, and limits in range.
    pub fn checked(mut self) -> Result<Self, String> {
        self.base_url = check_http_url("base_url", &self.base_url, false)?;
        let mut models: Vec<String> = Vec::new();
        for m in self
            .models
            .iter()
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
        {
            if m.chars().any(char::is_control) {
                return Err(format!("model id `{m}` has a control character"));
            }
            if !models.iter().any(|o| o == m) {
                models.push(m.to_owned());
            }
        }
        self.models = models;
        self.default_model = self
            .default_model
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty());
        if let Some(c) = self.context_window
            && c < MIN_CONTEXT_WINDOW
        {
            return Err(format!(
                "context_window is {c}; it is a token count of at least {MIN_CONTEXT_WINDOW}"
            ));
        }
        if self.max_output_tokens == Some(0) {
            return Err("max_output_tokens must be at least 1".into());
        }
        if let (Some(o), Some(c)) = (self.max_output_tokens, self.context_window)
            && o > c
        {
            return Err(format!(
                "max_output_tokens ({o}) is larger than context_window ({c})"
            ));
        }
        if let Some(n) = self.max_concurrent
            && (n == 0 || n > MAX_CONCURRENT)
        {
            return Err(format!(
                "max_concurrent is {n}; use 1 to {MAX_CONCURRENT}, or leave it out for no limit"
            ));
        }
        self.status_url = match self.status_url.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(u) => Some(check_http_url("status_url", u, true)?),
        };
        Ok(self)
    }

    /// The URL of the model list: `{base}/models` (OpenAI style) or
    /// `{base}/v1/models` (Anthropic style).
    pub fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.api {
            Api::Openai => format!("{base}/models"),
            Api::Anthropic => format!("{base}/v1/models"),
        }
    }

    /// The model of an agent: its `model` (a `selfhosted/` prefix goes),
    /// else [`SelfHosted::default_model`], else the first of `models`.
    pub fn model_for(&self, agent_model: Option<&str>) -> Option<String> {
        if let Some(m) = agent_model.map(str::trim).filter(|m| !m.is_empty()) {
            let m = m.strip_prefix("selfhosted/").unwrap_or(m);
            return Some(m.to_owned());
        }
        self.default_model
            .clone()
            .or_else(|| self.models.first().cloned())
    }

    /// Why `harness` cannot use this server, or `None` when it can.
    pub fn harness_refusal(&self, harness: HarnessId) -> Option<String> {
        let ok = self.api.harnesses();
        (!ok.contains(&harness)).then(|| refusal_text(self.api, harness))
    }

    /// The env of Claude Code for an Anthropic-style server. No secret:
    /// `ANTHROPIC_AUTH_TOKEN` is [`NO_KEY`] here; a stored key replaces it
    /// through `terrarium exec-env`. Each Claude tier and the sub-agents
    /// get `model`. The limits: `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (Claude
    /// Code compacts before the window is full) and
    /// `CLAUDE_CODE_MAX_OUTPUT_TOKENS`.
    pub fn claude_env(&self, model: Option<&str>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = vec![
            ("ANTHROPIC_BASE_URL".into(), self.base_url.clone()),
            ("ANTHROPIC_AUTH_TOKEN".into(), NO_KEY.into()),
            ("API_TIMEOUT_MS".into(), API_TIMEOUT_MS.into()),
        ];
        if let Some(m) = model {
            for k in [
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "ANTHROPIC_DEFAULT_SONNET_MODEL",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                "CLAUDE_CODE_SUBAGENT_MODEL",
            ] {
                out.push((k.into(), m.to_owned()));
            }
        }
        if let Some(c) = self.context_window {
            out.push(("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), c.to_string()));
        }
        if let Some(o) = self.max_output_tokens {
            out.push(("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), o.to_string()));
        }
        out
    }

    /// The `-c` values of Codex for an OpenAI-style server: the model
    /// provider [`CODEX_PROVIDER`] (Responses API, the key from
    /// [`KEY_ENV`]) and `model_context_window`. Codex 0.166 has no
    /// setting for the output tokens.
    pub fn codex_overrides(&self) -> Vec<String> {
        let q = |s: &str| {
            serde_json::to_string(s)
                .unwrap_or_else(|_| "\"\"".into())
                .replace('\u{7f}', "\\u007F")
        };
        let mut out = vec![
            format!("model_provider={}", q(CODEX_PROVIDER)),
            format!(
                "model_providers.{CODEX_PROVIDER}={{name={},base_url={},env_key={},wire_api=\"responses\"}}",
                q("Self-hosted (Terrarium)"),
                q(&self.base_url),
                q(KEY_ENV)
            ),
        ];
        if let Some(c) = self.context_window {
            out.push(format!("model_context_window={c}"));
        }
        out
    }

    /// The custom pi provider [`PI_PROVIDER`] for the per-agent
    /// `models.json`: the base URL, the API of the style, the key from
    /// `$`[`KEY_ENV`], and one entry per model (`models`, the default,
    /// and `extra`), each with `contextWindow` and `maxTokens` when set.
    pub fn pi_provider(&self, extra: &[&str]) -> serde_json::Value {
        let mut ids: Vec<String> = self.models.clone();
        for m in self
            .default_model
            .iter()
            .map(String::as_str)
            .chain(extra.iter().copied())
        {
            let m = m.trim();
            if !m.is_empty() && !ids.iter().any(|i| i == m) {
                ids.push(m.to_owned());
            }
        }
        let models: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                let mut m = serde_json::json!({"id": id, "name": id, "input": ["text"]});
                if let Some(c) = self.context_window {
                    m["contextWindow"] = c.into();
                }
                if let Some(o) = self.max_output_tokens {
                    m["maxTokens"] = o.into();
                }
                m
            })
            .collect();
        serde_json::json!({
            "name": "Self-hosted (Terrarium)",
            "baseUrl": self.base_url,
            "api": self.api.pi_api(),
            "apiKey": format!("${KEY_ENV}"),
            "models": models,
        })
    }
}

/// Why `harness` cannot use a server of style `api`.
pub fn refusal_text(api: Api, harness: HarnessId) -> String {
    let ok: Vec<&str> = api.harnesses().iter().map(|h| h.as_str()).collect();
    format!(
        "a self-hosted subscription with the {} API is for the {} harnesses, not {harness}",
        api.as_str(),
        ok.join(" and ")
    )
}

/// Why a self-hosted subscription of style `api` cannot list
/// `harnesses`, or `None` when it can.
pub fn harnesses_refusal(api: Api, harnesses: &[HarnessId]) -> Option<String> {
    harnesses
        .iter()
        .find(|h| !api.harnesses().contains(h))
        .map(|h| refusal_text(api, *h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(api: Api, url: &str) -> SelfHosted {
        SelfHosted {
            api,
            base_url: url.into(),
            ..SelfHosted::default()
        }
    }

    #[test]
    fn urls_are_checked() {
        for good in [
            "http://localhost:11434/v1",
            "https://gpu.lan/v1/",
            "http://192.168.1.20:8000",
            "http://[::1]:8080/v1",
            "HTTP://Gpu.Lan:80",
        ] {
            assert!(check_http_url("base_url", good, false).is_ok(), "{good}");
        }
        assert_eq!(
            check_http_url("base_url", " https://gpu.lan/v1/ ", false).unwrap(),
            "https://gpu.lan/v1"
        );
        for (bad, why) in [
            ("", "empty"),
            ("ftp://gpu/v1", "http://"),
            ("file:///etc/passwd", "http://"),
            ("http://", "http://"),
            ("http:///v1", "no host"),
            ("http://user:pw@gpu/v1", "password"),
            ("http://gpu/v1?x=1", "query"),
            ("http://gpu:99999/v1", "port"),
            ("http://gpu:/v1", "port"),
            ("http://g pu/v1", "white space"),
        ] {
            let e = check_http_url("base_url", bad, false).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(check_http_url("status_url", "https://s.lan/?page=1", true).is_ok());
    }

    #[test]
    fn fields_are_checked_and_cleaned() {
        let mut s = sh(Api::Openai, "http://gpu:8000/v1/");
        s.models = vec![" m1 ".into(), "".into(), "m1".into(), "m2".into()];
        s.default_model = Some(" ".into());
        s.context_window = Some(131_072);
        s.max_output_tokens = Some(8192);
        s.max_concurrent = Some(2);
        s.status_url = Some("".into());
        let c = s.clone().checked().unwrap();
        assert_eq!(c.base_url, "http://gpu:8000/v1");
        assert_eq!(c.models, ["m1", "m2"]);
        assert_eq!(c.default_model, None);
        assert_eq!(c.status_url, None);
        for (f, why) in [
            (
                SelfHosted {
                    context_window: Some(10),
                    ..s.clone()
                },
                "at least 1024",
            ),
            (
                SelfHosted {
                    max_output_tokens: Some(200_000),
                    ..s.clone()
                },
                "larger than context_window",
            ),
            (
                SelfHosted {
                    max_concurrent: Some(0),
                    ..s.clone()
                },
                "1 to 1024",
            ),
            (
                SelfHosted {
                    status_url: Some("javascript:alert(1)".into()),
                    ..s.clone()
                },
                "status_url must start",
            ),
        ] {
            let e = f.checked().unwrap_err();
            assert!(e.contains(why), "{e}");
        }
    }

    #[test]
    fn styles_pick_the_harnesses_and_the_models_url() {
        let o = sh(Api::Openai, "http://gpu/v1");
        let a = sh(Api::Anthropic, "http://gpu");
        assert_eq!(o.models_url(), "http://gpu/v1/models");
        assert_eq!(a.models_url(), "http://gpu/v1/models");
        assert_eq!(o.harness_refusal(HarnessId::Pi), None);
        assert_eq!(o.harness_refusal(HarnessId::Codex), None);
        assert!(
            o.harness_refusal(HarnessId::Claude)
                .unwrap()
                .contains("pi and codex")
        );
        assert_eq!(a.harness_refusal(HarnessId::Claude), None);
        assert_eq!(a.harness_refusal(HarnessId::Pi), None);
        assert!(o.harness_refusal(HarnessId::Copilot).is_some());
        assert!(a.harness_refusal(HarnessId::Copilot).is_some());
        assert!(
            harnesses_refusal(Api::Anthropic, &[HarnessId::Claude, HarnessId::Codex]).is_some()
        );
        assert_eq!(harnesses_refusal(Api::Openai, &[HarnessId::Pi]), None);
        assert_eq!("Anthropic".parse::<Api>(), Ok(Api::Anthropic));
        assert!("chat".parse::<Api>().is_err());
        assert!(is_selfhosted("openai-compatible") && is_selfhosted(" SelfHosted "));
        assert!(!is_selfhosted("openai"));
    }

    #[test]
    fn the_model_comes_from_the_agent_then_the_default_then_the_list() {
        let mut s = sh(Api::Openai, "http://gpu/v1");
        assert_eq!(s.model_for(None), None);
        s.models = vec!["a".into(), "b".into()];
        assert_eq!(s.model_for(None).as_deref(), Some("a"));
        s.default_model = Some("b".into());
        assert_eq!(s.model_for(Some(" ")).as_deref(), Some("b"));
        assert_eq!(s.model_for(Some("selfhosted/c")).as_deref(), Some("c"));
    }

    /// Golden: the Claude Code env of an Anthropic-style server.
    #[test]
    fn claude_env_golden() {
        let mut s = sh(Api::Anthropic, "http://gpu:8000");
        s.context_window = Some(65_536);
        s.max_output_tokens = Some(8192);
        let env = s.claude_env(Some("qwen3-coder"));
        let want: Vec<(String, String)> = [
            ("ANTHROPIC_BASE_URL", "http://gpu:8000"),
            ("ANTHROPIC_AUTH_TOKEN", NO_KEY),
            ("API_TIMEOUT_MS", "3000000"),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", "qwen3-coder"),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", "qwen3-coder"),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "qwen3-coder"),
            ("CLAUDE_CODE_SUBAGENT_MODEL", "qwen3-coder"),
            ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "65536"),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "8192"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(env, want);
        assert!(!env.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
    }

    /// Golden: the Codex `-c` values, which parse as TOML.
    #[test]
    fn codex_overrides_golden() {
        let mut s = sh(Api::Openai, "http://gpu:8000/v1");
        s.context_window = Some(131_072);
        let o = s.codex_overrides();
        assert_eq!(
            o,
            [
                "model_provider=\"terrarium_selfhosted\"",
                "model_providers.terrarium_selfhosted={name=\"Self-hosted (Terrarium)\",base_url=\"http://gpu:8000/v1\",env_key=\"TERRARIUM_SELFHOSTED_API_KEY\",wire_api=\"responses\"}",
                "model_context_window=131072",
            ]
        );
        for line in &o {
            let t: toml::Table = toml::from_str(line).unwrap();
            assert!(!t.is_empty(), "{line}");
        }
    }

    /// Golden: the pi provider of each style.
    #[test]
    fn pi_provider_golden() {
        let mut s = sh(Api::Anthropic, "http://gpu:8000");
        s.models = vec!["m1".into()];
        s.default_model = Some("m2".into());
        s.context_window = Some(32_768);
        s.max_output_tokens = Some(4096);
        assert_eq!(
            s.pi_provider(&["m3", "m1"]),
            serde_json::json!({
                "name": "Self-hosted (Terrarium)",
                "baseUrl": "http://gpu:8000",
                "api": "anthropic-messages",
                "apiKey": "$TERRARIUM_SELFHOSTED_API_KEY",
                "models": [
                    {"id": "m1", "name": "m1", "input": ["text"], "contextWindow": 32768, "maxTokens": 4096},
                    {"id": "m2", "name": "m2", "input": ["text"], "contextWindow": 32768, "maxTokens": 4096},
                    {"id": "m3", "name": "m3", "input": ["text"], "contextWindow": 32768, "maxTokens": 4096},
                ],
            })
        );
        let o = sh(Api::Openai, "http://gpu/v1").pi_provider(&["x"]);
        assert_eq!(o["api"], "openai-completions");
        assert_eq!(
            o["models"],
            serde_json::json!([{"id": "x", "name": "x", "input": ["text"]}])
        );
    }
}
