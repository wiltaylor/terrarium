//! Z.ai (Zhipu GLM) as a subscription provider (PLAN §16).
//!
//! The operator stores a Z.ai API key (`POST /api/subscriptions/{name}/
//! api-key`, provider `zai`). The subscription's `plan` picks the
//! endpoints:
//!
//! | Plan | Claude Code (Anthropic-compatible) | pi (OpenAI-compatible) |
//! |---|---|---|
//! | `coding` (GLM Coding Plan) | [`ANTHROPIC_BASE_URL`] | [`CODING_BASE_URL`], pi's built-in `zai` provider |
//! | `api` (pay as you go) | [`ANTHROPIC_BASE_URL`] | [`API_BASE_URL`], the custom pi provider [`PI_API_PROVIDER`] |
//!
//! Every value here is from the Z.ai docs, not verified live: no Z.ai key
//! was available (`docs/compat.md`, "Z.ai"). The key goes in
//! `Authorization: Bearer <key>` for the OpenAI-compatible endpoints and
//! in `ANTHROPIC_AUTH_TOKEN` for Claude Code.

/// The provider word in a subscription.
pub const PROVIDER: &str = "zai";
/// The GLM Coding Plan (a monthly plan with 5-hour and weekly windows).
pub const PLAN_CODING: &str = "coding";
/// Pay as you go on the general API.
pub const PLAN_API: &str = "api";
/// Every plan word, in the order the wizard shows them.
pub const PLANS: [&str; 2] = [PLAN_CODING, PLAN_API];

/// The env var that carries the key. pi's built-in `zai` provider reads it.
pub const KEY_ENV: &str = "ZAI_API_KEY";

/// The Anthropic-compatible base URL for Claude Code
/// (docs.z.ai/devpack/tool/claude).
pub const ANTHROPIC_BASE_URL: &str = "https://api.z.ai/api/anthropic";
/// The OpenAI-compatible base URL of the GLM Coding Plan
/// (docs.z.ai/devpack/tool/others). Requests to another URL do not spend
/// the plan's quota.
pub const CODING_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";
/// The OpenAI-compatible base URL of the general (pay as you go) API
/// (docs.z.ai/guides/overview/quick-start).
pub const API_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
/// Claude Code's request timeout, in ms, from the Z.ai Claude Code page.
pub const API_TIMEOUT_MS: &str = "3000000";

/// The flagship model.
pub const MODEL_FLAGSHIP: &str = "glm-5.3";
/// The cheap, fast model. Both plans include it.
pub const MODEL_FAST: &str = "glm-5.3-flash";

/// The pi provider of the `api` plan, a custom provider in the per-agent
/// `models.json`. The `coding` plan uses pi's built-in `zai`.
pub const PI_API_PROVIDER: &str = "zai-api";

/// A Z.ai plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Coding,
    Api,
}

impl Plan {
    /// The plan word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coding => PLAN_CODING,
            Self::Api => PLAN_API,
        }
    }

    /// The OpenAI-compatible base URL of the plan.
    pub fn openai_base_url(self) -> &'static str {
        match self {
            Self::Coding => CODING_BASE_URL,
            Self::Api => API_BASE_URL,
        }
    }

    /// The pi provider id of the plan.
    pub fn pi_provider(self) -> &'static str {
        match self {
            Self::Coding => PROVIDER,
            Self::Api => PI_API_PROVIDER,
        }
    }
}

/// True when `provider` is Z.ai.
pub fn is_zai(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(PROVIDER)
}

/// Parse the plan of a Z.ai subscription. No plan is `coding`. An error
/// names the plan words.
pub fn parse_plan(plan: Option<&str>) -> Result<Plan, String> {
    match plan.map(str::trim).filter(|p| !p.is_empty()) {
        None => Ok(Plan::Coding),
        Some(p) if p.eq_ignore_ascii_case(PLAN_CODING) => Ok(Plan::Coding),
        Some(p) if p.eq_ignore_ascii_case(PLAN_API) => Ok(Plan::Api),
        Some(p) => Err(format!(
            "unknown zai plan `{p}`; use `{PLAN_CODING}` (GLM Coding Plan) or `{PLAN_API}` (pay as you go)"
        )),
    }
}

/// The plan of a Z.ai subscription, with `coding` for a plan that does
/// not parse (the API refuses such a plan when it is written).
pub fn plan_or_default(plan: Option<&str>) -> Plan {
    parse_plan(plan).unwrap_or(Plan::Coding)
}

/// The env that Claude Code needs for Z.ai: the base URL, the key as a
/// bearer token, the timeout, and the model of each Claude tier. No
/// `ANTHROPIC_API_KEY`: with it, Claude Code asks whether to use the key.
/// The value of `ANTHROPIC_AUTH_TOKEN` is a secret.
pub fn claude_env(key: &str) -> Vec<(String, String)> {
    [
        ("ANTHROPIC_BASE_URL", ANTHROPIC_BASE_URL),
        ("ANTHROPIC_AUTH_TOKEN", key),
        ("API_TIMEOUT_MS", API_TIMEOUT_MS),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", MODEL_FLAGSHIP),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", MODEL_FLAGSHIP),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", MODEL_FAST),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// The GLM model id for the `model` of an agent on Z.ai.
///
/// - No model: [`MODEL_FLAGSHIP`].
/// - A Claude alias or id (`haiku`, `claude-haiku-…`): the GLM model of
///   that tier ([`MODEL_FAST`] for Haiku, else [`MODEL_FLAGSHIP`]).
/// - `zai/<id>`: `<id>`.
/// - Anything else (`glm-5.3`, `glm-4.6`): as it is.
pub fn model_for(model: Option<&str>) -> String {
    let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) else {
        return MODEL_FLAGSHIP.to_owned();
    };
    let m = m.strip_prefix("zai/").unwrap_or(m);
    let lower = m.to_ascii_lowercase();
    if lower.starts_with("claude") || matches!(lower.as_str(), "haiku" | "sonnet" | "opus") {
        return if lower.contains("haiku") {
            MODEL_FAST
        } else {
            MODEL_FLAGSHIP
        }
        .to_owned();
    }
    m.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_parse_and_default_to_coding() {
        assert_eq!(parse_plan(None), Ok(Plan::Coding));
        assert_eq!(parse_plan(Some(" ")), Ok(Plan::Coding));
        assert_eq!(parse_plan(Some("Coding")), Ok(Plan::Coding));
        assert_eq!(parse_plan(Some("api")), Ok(Plan::Api));
        assert!(parse_plan(Some("Max 20x")).unwrap_err().contains("coding"));
        assert_eq!(plan_or_default(Some("bad")), Plan::Coding);
        assert_eq!(Plan::Coding.openai_base_url(), CODING_BASE_URL);
        assert_eq!(Plan::Api.openai_base_url(), API_BASE_URL);
        assert_eq!(Plan::Api.pi_provider(), "zai-api");
        assert!(is_zai(" ZAI "));
        assert!(!is_zai("openai"));
    }

    #[test]
    fn claude_env_uses_a_bearer_token_not_an_api_key() {
        let env = claude_env("k-1");
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("ANTHROPIC_BASE_URL"), Some(ANTHROPIC_BASE_URL));
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some("k-1"));
        assert_eq!(get("ANTHROPIC_API_KEY"), None);
        assert_eq!(get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some(MODEL_FAST));
        assert_eq!(get("ANTHROPIC_DEFAULT_SONNET_MODEL"), Some(MODEL_FLAGSHIP));
    }

    #[test]
    fn agent_models_map_to_glm_ids() {
        assert_eq!(model_for(None), MODEL_FLAGSHIP);
        assert_eq!(model_for(Some("haiku")), MODEL_FAST);
        assert_eq!(model_for(Some("claude-haiku-5-5")), MODEL_FAST);
        assert_eq!(model_for(Some("sonnet")), MODEL_FLAGSHIP);
        assert_eq!(model_for(Some("claude-opus-5")), MODEL_FLAGSHIP);
        assert_eq!(model_for(Some("glm-4.6")), "glm-4.6");
        assert_eq!(model_for(Some("zai/glm-5.3-flash")), MODEL_FAST);
    }
}
