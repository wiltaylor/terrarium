//! Moonshot AI (Kimi) as a subscription provider (PLAN §16).
//!
//! The operator stores a Moonshot or Kimi Code API key (`POST
//! /api/subscriptions/{name}/api-key`, provider `moonshot`). A Moonshot
//! subscription is for **pi only**: the daemon refuses any other harness
//! ([`harness_refusal`]). Claude Code does not get the Anthropic-compatible
//! endpoints (an operator decision, 2026-10-28).
//!
//! The subscription's `plan` picks the product and the region. The keys of
//! the two products are different, and a key works only in the region
//! where it was made:
//!
//! | Plan | Product | OpenAI-compatible base URL | pi provider |
//! |---|---|---|---|
//! | `coding` | Kimi Code membership, overseas | [`CODING_BASE_URL`] | the custom [`PI_CODING_PROVIDER`] |
//! | `coding-cn` | Kimi Code membership, China | [`CODING_CN_BASE_URL`] | pi's built-in `kimi-coding` |
//! | `api` | Kimi Open Platform (pay as you go), global | [`API_BASE_URL`] | pi's built-in `moonshotai` |
//! | `api-cn` | Kimi Open Platform (pay as you go), China | [`API_CN_BASE_URL`] | pi's built-in `moonshotai-cn` |
//!
//! Sources (read 2026-10-28; not verified live, no key was available,
//! `docs/compat.md`, "Moonshot"):
//!
//! - Kimi Code overview (base URLs per region, model ids):
//!   <https://www.kimi.com/code/docs/en/>
//! - Kimi Code models: <https://www.kimi.com/code/docs/en/kimi-code/models.html>
//! - Kimi Open Platform API overview (global): <https://platform.kimi.ai/docs/api/overview>
//! - Kimi Open Platform API overview (China): <https://platform.kimi.com/docs/api/overview>
//! - pi 1.4.0's built-in providers `kimi-coding`, `moonshotai` and
//!   `moonshotai-cn` (`pi-ai/dist/providers/`).
//!
//! The key goes in `Authorization: Bearer <key>`.
//!
//! Usage probes (`crates/terrariumd/src/probes/`): the Open Platform plans
//! get `moonshot-balance` or `moonshot-cn-balance`. Both Kimi Code plans get
//! `kimi-code-usage`, which reads `GET https://api.kimi.com/coding/v1/usages`
//! (the host of the official `kimi-cli`). That host answered an overseas key
//! (verified live 2026-10-30, `docs/compat.md`, "Kimi Code usage").

use crate::HarnessId;

/// The provider word in a subscription.
pub const PROVIDER: &str = "moonshot";
/// Kimi Code membership, overseas endpoint.
pub const PLAN_CODING: &str = "coding";
/// Kimi Code membership, China endpoint.
pub const PLAN_CODING_CN: &str = "coding-cn";
/// Kimi Open Platform, pay as you go, global endpoint.
pub const PLAN_API: &str = "api";
/// Kimi Open Platform, pay as you go, China endpoint.
pub const PLAN_API_CN: &str = "api-cn";
/// Every plan word, in the order the wizard shows them.
pub const PLANS: [&str; 4] = [PLAN_CODING, PLAN_CODING_CN, PLAN_API, PLAN_API_CN];

/// The harnesses that can spend a Moonshot subscription: pi only.
pub const HARNESSES: [HarnessId; 1] = [HarnessId::Pi];

/// The env var of the subscription (its `env_var`). pi's built-in
/// `moonshotai` and `moonshotai-cn` providers read it.
pub const KEY_ENV: &str = "MOONSHOT_API_KEY";
/// The env var that pi's built-in `kimi-coding` provider reads. The daemon
/// also sets it, to the same key, for a `coding` or `coding-cn` plan.
pub const KIMI_KEY_ENV: &str = "KIMI_API_KEY";

/// The OpenAI-compatible base URL of Kimi Code, overseas
/// (kimi.com/code/docs/en/).
pub const CODING_BASE_URL: &str = "https://api.kimi.ai/coding/v1";
/// The OpenAI-compatible base URL of Kimi Code, China
/// (kimi.com/code/docs/en/). pi's built-in `kimi-coding` uses it.
pub const CODING_CN_BASE_URL: &str = "https://api.kimi.com/coding/v1";
/// The OpenAI-compatible base URL of the Open Platform, global
/// (platform.kimi.ai/docs/api/overview).
pub const API_BASE_URL: &str = "https://api.moonshot.ai/v1";
/// The OpenAI-compatible base URL of the Open Platform, China
/// (platform.kimi.com/docs/api/overview).
pub const API_CN_BASE_URL: &str = "https://api.moonshot.cn/v1";

/// The custom pi provider of the `coding` plan, in the per-agent
/// `models.json`. pi 1.4.0's built-in `kimi-coding` has the China host
/// only.
pub const PI_CODING_PROVIDER: &str = "kimi-coding-intl";
/// pi's built-in provider of the `coding-cn` plan.
pub const PI_CODING_CN_PROVIDER: &str = "kimi-coding";
/// pi's built-in provider of the `api` plan.
pub const PI_API_PROVIDER: &str = "moonshotai";
/// pi's built-in provider of the `api-cn` plan.
pub const PI_API_CN_PROVIDER: &str = "moonshotai-cn";

/// The default model of a Kimi Code plan: every membership tier with Kimi
/// Code has it, and it is the cheapest model in pi 1.4.0's `kimi-coding`
/// catalog.
pub const MODEL_CODING_DEFAULT: &str = "kimi-for-coding";
/// The default model of an Open Platform plan: Kimi K3 (an operator
/// decision, 2026-10-29). The id is from the Open Platform model list
/// (platform.kimi.ai/docs/models) and pi 1.4.0's `moonshotai` and
/// `moonshotai-cn` catalogs. The K2 series retires by the end of December
/// 2026, and new accounts lose it on 2026-10-31 (the same page).
pub const MODEL_API_DEFAULT: &str = "kimi-k3";

/// The Kimi Code model ids that pi 1.4.0's `kimi-coding` catalog does not
/// have. Terrarium adds them to the per-agent `models.json`, for the
/// custom `kimi-coding-intl` and for the built-in `kimi-coding`
/// (kimi.com/code/docs/en/kimi-code/models.html, read 2026-10-29).
pub const CODING_MODELS_NOT_IN_PI: [&str; 2] = ["k3.1-pro", "k3.1-pro-highspeed"];

/// The Kimi Code model ids (kimi.com/code/docs/en/kimi-code/models.html).
/// The fallback when no list can be read. The tier of the membership
/// decides which ids work.
pub const CODING_MODELS: [&str; 6] = [
    "k3.1-pro",
    "k3.1-pro-highspeed",
    "k3",
    "k3-256k",
    "kimi-for-coding",
    "kimi-for-coding-highspeed",
];
/// The Open Platform chat model ids (pi 1.4.0's `moonshotai` catalog and
/// platform.kimi.ai/docs/pricing/chat). The fallback when no list can be
/// read.
pub const API_MODELS: [&str; 4] = [
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "kimi-k2.6",
];

/// A Moonshot plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Coding,
    CodingCn,
    Api,
    ApiCn,
}

impl Plan {
    /// The plan word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coding => PLAN_CODING,
            Self::CodingCn => PLAN_CODING_CN,
            Self::Api => PLAN_API,
            Self::ApiCn => PLAN_API_CN,
        }
    }

    /// True for a Kimi Code membership plan.
    pub fn is_coding(self) -> bool {
        matches!(self, Self::Coding | Self::CodingCn)
    }

    /// The OpenAI-compatible base URL of the plan.
    pub fn openai_base_url(self) -> &'static str {
        match self {
            Self::Coding => CODING_BASE_URL,
            Self::CodingCn => CODING_CN_BASE_URL,
            Self::Api => API_BASE_URL,
            Self::ApiCn => API_CN_BASE_URL,
        }
    }

    /// The pi provider id of the plan.
    pub fn pi_provider(self) -> &'static str {
        match self {
            Self::Coding => PI_CODING_PROVIDER,
            Self::CodingCn => PI_CODING_CN_PROVIDER,
            Self::Api => PI_API_PROVIDER,
            Self::ApiCn => PI_API_CN_PROVIDER,
        }
    }

    /// The env var that the plan's pi provider reads.
    pub fn pi_key_env(self) -> &'static str {
        if self.is_coding() {
            KIMI_KEY_ENV
        } else {
            KEY_ENV
        }
    }

    /// The model that pi gets when the agent sets none.
    pub fn default_model(self) -> &'static str {
        if self.is_coding() {
            MODEL_CODING_DEFAULT
        } else {
            MODEL_API_DEFAULT
        }
    }

    /// The model ids to show when no list can be read.
    pub fn fallback_models(self) -> &'static [&'static str] {
        if self.is_coding() {
            &CODING_MODELS
        } else {
            &API_MODELS
        }
    }
}

/// True when `provider` is Moonshot.
pub fn is_moonshot(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(PROVIDER)
}

/// Parse the plan of a Moonshot subscription. No plan is `coding`. An
/// error names the plan words.
pub fn parse_plan(plan: Option<&str>) -> Result<Plan, String> {
    let Some(p) = plan.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(Plan::Coding);
    };
    [Plan::Coding, Plan::CodingCn, Plan::Api, Plan::ApiCn]
        .into_iter()
        .find(|k| k.as_str().eq_ignore_ascii_case(p))
        .ok_or_else(|| {
            format!(
                "unknown moonshot plan `{p}`; use `{PLAN_CODING}` (Kimi Code), `{PLAN_CODING_CN}` \
                 (Kimi Code, China), `{PLAN_API}` (Open Platform, pay as you go), or \
                 `{PLAN_API_CN}` (Open Platform, China)"
            )
        })
}

/// The plan of a Moonshot subscription, with `coding` for a plan that
/// does not parse (the API refuses such a plan when it is written).
pub fn plan_or_default(plan: Option<&str>) -> Plan {
    parse_plan(plan).unwrap_or(Plan::Coding)
}

/// Why a Moonshot subscription cannot list `harnesses`, or `None` when
/// it can. Only pi can spend it.
pub fn harness_refusal(harnesses: &[HarnessId]) -> Option<String> {
    let other: Vec<&str> = harnesses
        .iter()
        .filter(|h| !HARNESSES.contains(h))
        .map(|h| h.as_str())
        .collect();
    (!other.is_empty()).then(|| {
        format!(
            "a moonshot subscription is for the pi harness only, not {}; set harnesses to [\"pi\"]",
            other.join(", ")
        )
    })
}

/// The env that an agent on a Moonshot subscription gets for its key,
/// beside the subscription's own `env_var` ([`KEY_ENV`]): [`KIMI_KEY_ENV`]
/// for a Kimi Code plan, because pi's `kimi-coding` reads it. The values
/// are secrets.
pub fn extra_env(plan: Plan, key: &str) -> Vec<(String, String)> {
    if plan.is_coding() {
        vec![(KIMI_KEY_ENV.to_owned(), key.to_owned())]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_parse_and_default_to_coding() {
        assert_eq!(parse_plan(None), Ok(Plan::Coding));
        assert_eq!(parse_plan(Some(" ")), Ok(Plan::Coding));
        assert_eq!(parse_plan(Some("Coding")), Ok(Plan::Coding));
        assert_eq!(parse_plan(Some("coding-CN")), Ok(Plan::CodingCn));
        assert_eq!(parse_plan(Some("api")), Ok(Plan::Api));
        assert_eq!(parse_plan(Some("api-cn")), Ok(Plan::ApiCn));
        assert!(
            parse_plan(Some("Allegretto"))
                .unwrap_err()
                .contains("coding-cn")
        );
        assert_eq!(plan_or_default(Some("bad")), Plan::Coding);
        assert_eq!(
            Plan::Coding.openai_base_url(),
            "https://api.kimi.ai/coding/v1"
        );
        assert_eq!(
            Plan::CodingCn.openai_base_url(),
            "https://api.kimi.com/coding/v1"
        );
        assert_eq!(Plan::Api.openai_base_url(), "https://api.moonshot.ai/v1");
        assert_eq!(Plan::ApiCn.openai_base_url(), "https://api.moonshot.cn/v1");
        assert_eq!(Plan::Coding.pi_provider(), "kimi-coding-intl");
        assert_eq!(Plan::CodingCn.pi_provider(), "kimi-coding");
        assert_eq!(Plan::Api.pi_provider(), "moonshotai");
        assert_eq!(Plan::ApiCn.pi_provider(), "moonshotai-cn");
        assert!(is_moonshot(" Moonshot "));
        assert!(!is_moonshot("zai"));
    }

    #[test]
    fn each_plan_names_its_key_env_and_default_model() {
        assert_eq!(Plan::Coding.pi_key_env(), "KIMI_API_KEY");
        assert_eq!(Plan::CodingCn.pi_key_env(), "KIMI_API_KEY");
        assert_eq!(Plan::Api.pi_key_env(), "MOONSHOT_API_KEY");
        assert_eq!(Plan::ApiCn.pi_key_env(), "MOONSHOT_API_KEY");
        assert_eq!(Plan::Coding.default_model(), "kimi-for-coding");
        assert_eq!(Plan::Api.default_model(), "kimi-k3");
        assert_eq!(Plan::ApiCn.default_model(), "kimi-k3");
        for m in CODING_MODELS_NOT_IN_PI {
            assert!(CODING_MODELS.contains(&m), "{m}");
        }
        for p in [Plan::Coding, Plan::CodingCn, Plan::Api, Plan::ApiCn] {
            assert!(p.fallback_models().contains(&p.default_model()), "{p:?}");
        }
    }

    #[test]
    fn only_pi_can_spend_it() {
        assert_eq!(harness_refusal(&[HarnessId::Pi]), None);
        assert_eq!(harness_refusal(&[]), None);
        for h in [HarnessId::Claude, HarnessId::Codex, HarnessId::Copilot] {
            let e = harness_refusal(&[HarnessId::Pi, h]).unwrap();
            assert!(
                e.contains(h.as_str()) && e.contains("pi harness only"),
                "{e}"
            );
        }
    }

    #[test]
    fn coding_plans_also_set_the_kimi_key_var() {
        assert_eq!(
            extra_env(Plan::Coding, "k"),
            vec![("KIMI_API_KEY".to_owned(), "k".to_owned())]
        );
        assert!(extra_env(Plan::Api, "k").is_empty());
    }
}
