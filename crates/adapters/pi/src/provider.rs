//! Subscriptions to pi providers (PLAN §16).
//!
//! An agent names its subscription. The subscription names a provider in
//! Terrarium's words (`SubscriptionDef::provider`: the words of the login
//! wizard and of `terrariumd`'s `env_var_for`). pi has its own provider
//! ids. [`provider_for`] maps one to the other, and [`required_env`] names
//! the env var that carries an API key.
//!
//! **pi uses API keys only.** The daemon keeps the key in its credential
//! store and injects it into the env of the agents on that subscription
//! only. A plan login (Claude Pro/Max, ChatGPT, Copilot, any OAuth login)
//! is never a pi subscription ([`is_plan`], [`PLAN_REFUSAL`]): Claude Code
//! spends a Claude plan, and Codex spends a ChatGPT plan. The login wizard
//! marks an API-key subscription with `plan = "API key"` ([`is_api_key`]).
//!
//! `anthropic` and `openai` have two meanings: a plan login (refused) or
//! an API key (pi `anthropic` with `ANTHROPIC_API_KEY`, pi `openai` with
//! `OPENAI_API_KEY`). The plan field decides.
//!
//! The API-key providers are not tested; the env var names are from
//! `pi --help`.

use terrarium_core::config::SubscriptionDef;

/// The plan value that the login wizard gives an API-key subscription.
pub const API_KEY_PLAN: &str = "API key";

/// Why pi refuses a plan subscription.
pub const PLAN_REFUSAL: &str =
    "pi uses API keys only; use Claude Code for a Claude plan and Codex for a ChatGPT plan";

/// True when the subscription is an API key, not a plan login.
pub fn is_api_key(sub: &SubscriptionDef) -> bool {
    sub.plan
        .as_deref()
        .is_some_and(|p| p.trim().eq_ignore_ascii_case(API_KEY_PLAN))
}

/// One row of the provider table.
struct Row {
    /// Terrarium's words for the provider.
    names: &'static [&'static str],
    /// The provider sells a plan login (OAuth). pi does not spend it.
    plan: bool,
    /// pi's provider id and env var for an API key.
    api_key: Option<(&'static str, &'static str)>,
}

const TABLE: &[Row] = &[
    Row {
        names: &["anthropic", "claude"],
        plan: true,
        api_key: Some(("anthropic", "ANTHROPIC_API_KEY")),
    },
    Row {
        names: &["openai"],
        plan: true,
        api_key: Some(("openai", "OPENAI_API_KEY")),
    },
    Row {
        names: &["openai-codex", "codex", "chatgpt"],
        plan: true,
        api_key: None,
    },
    Row {
        names: &["github", "github-copilot", "copilot"],
        plan: true,
        api_key: None,
    },
    Row {
        names: &["google", "gemini"],
        plan: false,
        api_key: Some(("google", "GEMINI_API_KEY")),
    },
    Row {
        names: &["google-gemini-cli"],
        plan: true,
        api_key: None,
    },
    Row {
        names: &["openrouter"],
        plan: false,
        api_key: Some(("openrouter", "OPENROUTER_API_KEY")),
    },
    Row {
        names: &["xai"],
        plan: false,
        api_key: Some(("xai", "XAI_API_KEY")),
    },
    Row {
        names: &["groq"],
        plan: false,
        api_key: Some(("groq", "GROQ_API_KEY")),
    },
    Row {
        names: &["deepseek"],
        plan: false,
        api_key: Some(("deepseek", "DEEPSEEK_API_KEY")),
    },
    Row {
        names: &["mistral"],
        plan: false,
        api_key: Some(("mistral", "MISTRAL_API_KEY")),
    },
    // Z.ai: pi's built-in `zai` provider is the GLM Coding Plan endpoint.
    // The `api` plan (pay as you go) is the custom provider `zai-api` in
    // the per-agent `models.json` ([`crate::config::zai_api_provider`]);
    // [`provider_for`] picks it. A Z.ai plan is an API key, never a login.
    Row {
        names: &["zai"],
        plan: false,
        api_key: Some(("zai", terrarium_core::zai::KEY_ENV)),
    },
    // Moonshot AI (Kimi), pi only: the plan picks the pi provider and its
    // env var (`terrarium_core::moonshot::Plan`). [`provider_for`] and
    // [`required_env`] read the plan; this row only marks an API key.
    Row {
        names: &["moonshot"],
        plan: false,
        api_key: Some(("moonshotai", terrarium_core::moonshot::KEY_ENV)),
    },
    // A self-hosted server (`terrarium_core::selfhosted`, and its alias
    // `openai-compatible`): the custom provider `terrarium-selfhosted` in
    // the per-agent `models.json` ([`crate::config::agent_models`]).
    // [`provider_for`] and [`required_env`] pick it; this row only marks
    // an API key (or no key).
    Row {
        names: &["selfhosted", "openai-compatible"],
        plan: false,
        api_key: Some((
            terrarium_core::selfhosted::PI_PROVIDER,
            terrarium_core::selfhosted::KEY_ENV,
        )),
    },
];

fn row(provider: &str) -> Option<&'static Row> {
    let p = provider.trim();
    TABLE
        .iter()
        .find(|r| r.names.iter().any(|n| n.eq_ignore_ascii_case(p)))
}

/// True when the subscription is a plan login (Claude Pro/Max, ChatGPT,
/// Copilot, any OAuth login), which pi does not spend: a provider that
/// sells a plan, and no `plan = "API key"` (or a provider with no API key
/// at all). A Z.ai plan, a key-only provider, and a custom provider are
/// not plans.
pub fn is_plan(sub: &SubscriptionDef) -> bool {
    if terrarium_core::zai::is_zai(&sub.provider) {
        return false;
    }
    row(&sub.provider).is_some_and(|r| r.plan && (r.api_key.is_none() || !is_api_key(sub)))
}

/// The pi provider id (`--provider`) for a subscription.
///
/// A provider that is not in the table goes through as it is: it can be a
/// custom provider in the shared `models.json` (the spike host has three).
/// `None` when the provider is empty, is a plan login ([`is_plan`]), or
/// cannot be used through pi.
pub fn provider_for(sub: &SubscriptionDef) -> Option<String> {
    let name = sub.provider.trim();
    if name.is_empty() || is_plan(sub) {
        return None;
    }
    if terrarium_core::zai::is_zai(name) {
        let plan = terrarium_core::zai::plan_or_default(sub.plan.as_deref());
        return Some(plan.pi_provider().to_owned());
    }
    if terrarium_core::moonshot::is_moonshot(name) {
        let plan = terrarium_core::moonshot::plan_or_default(sub.plan.as_deref());
        return Some(plan.pi_provider().to_owned());
    }
    let Some(r) = row(name) else {
        return Some(name.to_owned());
    };
    r.api_key.map(|(id, _)| id.to_owned())
}

/// The env var that must carry the subscription's API key, or `None` for
/// a plan login (refused) and a custom provider.
///
/// The daemon injects the value from its credential store
/// (`<state>/credentials/subscriptions/<name>.json`) into
/// `LaunchSpec::env`, for the agents on this subscription only. The names
/// agree with `terrariumd`'s `login_session::env_var_for`.
pub fn required_env(sub: &SubscriptionDef) -> Option<&'static str> {
    if is_plan(sub) {
        return None;
    }
    if terrarium_core::moonshot::is_moonshot(&sub.provider) {
        let plan = terrarium_core::moonshot::plan_or_default(sub.plan.as_deref());
        return Some(plan.pi_key_env());
    }
    row(&sub.provider)?.api_key.map(|(_, var)| var)
}

/// Why pi must not spend this subscription, or `None` when it can.
///
/// pi uses API keys only: a plan login is [`PLAN_REFUSAL`]. A Claude
/// Pro/Max login through pi is also billed as extra usage, not from the
/// plan (`docs/compat.md`, "pi: facts that contradict the plan", 1).
pub fn refusal(sub: &SubscriptionDef) -> Option<&'static str> {
    if is_plan(sub) {
        return Some(PLAN_REFUSAL);
    }
    if provider_for(sub).is_none() {
        return Some("pi has no provider for this subscription");
    }
    None
}

/// Why pi must not start this agent on this subscription, or `None` when
/// it can. [`refusal`], and one more case: a provider with no default model
/// ([`crate::flags::default_model`], for example a custom provider in the
/// shared `models.json`) and an agent with no `model`. pi 1.4 needs
/// `--model` with `--provider`, and a start with neither flag can spend
/// another subscription.
pub fn start_refusal(sub: &SubscriptionDef, model: Option<&str>) -> Option<String> {
    if let Some(r) = refusal(sub) {
        return Some(r.to_owned());
    }
    let has_model = model.is_some_and(|m| !m.trim().is_empty());
    if terrarium_core::selfhosted::is_selfhosted(&sub.provider) {
        let Some(sh) = &sub.selfhosted else {
            return Some(
                "the self-hosted subscription has no server (`selfhosted.base_url`)".into(),
            );
        };
        if sh.model_for(model).is_none() {
            return Some(
                "the self-hosted subscription names no model: set the agent's `model`, or the \
                 subscription's `default_model` or `models`"
                    .into(),
            );
        }
        return None;
    }
    let provider = provider_for(sub)?;
    if !has_model && crate::flags::default_model(&provider).is_none() {
        return Some(format!(
            "pi has no default model for provider `{provider}`: set the agent's `model`"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(provider: &str, plan: Option<&str>) -> SubscriptionDef {
        SubscriptionDef {
            name: "s".into(),
            provider: provider.into(),
            plan: plan.map(str::to_owned),
            harnesses: vec![],
            probe: None,
            alert_at: 80,
            selfhosted: None,
            cloud: None,
        }
    }

    /// pi uses API keys only: every plan login is refused, with one text.
    #[test]
    fn plan_logins_are_refused() {
        for (p, plan) in [
            ("anthropic", Some("Max 20x")),
            ("claude", None),
            ("openai", Some("ChatGPT Pro")),
            ("openai", None),
            ("openai-codex", None),
            ("openai-codex", Some("API key")),
            ("github", Some("Pro")),
            ("copilot", None),
            ("google-gemini-cli", None),
        ] {
            let s = sub(p, plan);
            assert!(is_plan(&s), "{p} {plan:?}");
            assert_eq!(refusal(&s), Some(PLAN_REFUSAL), "{p}");
            assert_eq!(start_refusal(&s, Some("m")).as_deref(), Some(PLAN_REFUSAL));
            assert_eq!(provider_for(&s), None, "{p}");
            assert_eq!(required_env(&s), None, "{p}");
        }
        assert!(PLAN_REFUSAL.starts_with("pi uses API keys only"));
    }

    #[test]
    fn api_keys_map_to_key_providers_and_env() {
        for (p, want, var) in [
            ("anthropic", "anthropic", "ANTHROPIC_API_KEY"),
            ("openai", "openai", "OPENAI_API_KEY"),
            ("google", "google", "GEMINI_API_KEY"),
            ("gemini", "google", "GEMINI_API_KEY"),
            ("openrouter", "openrouter", "OPENROUTER_API_KEY"),
            ("xai", "xai", "XAI_API_KEY"),
            ("groq", "groq", "GROQ_API_KEY"),
        ] {
            let s = sub(p, Some("API key"));
            assert!(is_api_key(&s));
            assert_eq!(provider_for(&s).as_deref(), Some(want), "{p}");
            assert_eq!(required_env(&s), Some(var), "{p}");
        }
        // Key-only providers need the env var with any plan text.
        assert_eq!(
            required_env(&sub("openrouter", None)),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            provider_for(&sub("google", None)).as_deref(),
            Some("google")
        );
    }

    #[test]
    fn api_keys_and_key_only_providers_are_not_plans() {
        for (p, plan) in [
            ("anthropic", Some("API key")),
            ("openai", Some("api key")),
            ("openrouter", None),
            ("google", Some("Gemini Code Assist")),
            ("zai", Some("coding")),
            ("zai", Some("api")),
            ("glm53-exl3", None),
            ("openai-compatible", Some("API key")),
        ] {
            assert!(!is_plan(&sub(p, plan)), "{p} {plan:?}");
        }
        assert_eq!(refusal(&sub("anthropic", Some("API key"))), None);
        // The old word is a self-hosted server: the custom provider.
        assert_eq!(refusal(&sub("openai-compatible", Some("API key"))), None);
    }

    #[test]
    fn a_custom_provider_needs_a_model() {
        let custom = sub("glm53-exl3", None);
        assert!(start_refusal(&custom, None).is_some());
        assert!(start_refusal(&custom, Some(" ")).is_some());
        assert_eq!(start_refusal(&custom, Some("glm-5.3")), None);
        assert_eq!(start_refusal(&sub("openai", Some("API key")), None), None);
        assert!(start_refusal(&sub("anthropic", Some("Max")), Some("x")).is_some());
    }

    #[test]
    fn zai_plans_pick_the_builtin_or_the_custom_provider() {
        for plan in [None, Some("coding"), Some("Coding")] {
            let s = sub("zai", plan);
            assert_eq!(provider_for(&s).as_deref(), Some("zai"), "{plan:?}");
            assert_eq!(required_env(&s), Some("ZAI_API_KEY"));
            assert_eq!(start_refusal(&s, None), None);
        }
        let api = sub("zai", Some("api"));
        assert_eq!(provider_for(&api).as_deref(), Some("zai-api"));
        assert_eq!(required_env(&api), Some("ZAI_API_KEY"));
        assert_eq!(refusal(&api), None);
        assert_eq!(start_refusal(&api, None), None);
    }

    #[test]
    fn moonshot_plans_pick_the_builtin_or_the_custom_provider() {
        for (plan, provider, var) in [
            (None, "kimi-coding-intl", "KIMI_API_KEY"),
            (Some("coding"), "kimi-coding-intl", "KIMI_API_KEY"),
            (Some("coding-cn"), "kimi-coding", "KIMI_API_KEY"),
            (Some("api"), "moonshotai", "MOONSHOT_API_KEY"),
            (Some("api-cn"), "moonshotai-cn", "MOONSHOT_API_KEY"),
        ] {
            let s = sub("moonshot", plan);
            assert_eq!(provider_for(&s).as_deref(), Some(provider), "{plan:?}");
            assert_eq!(required_env(&s), Some(var), "{plan:?}");
            assert_eq!(start_refusal(&s, None), None, "{plan:?}");
        }
    }

    #[test]
    fn unknown_and_unsupported_providers() {
        assert_eq!(
            provider_for(&sub("glm53-exl3", None)).as_deref(),
            Some("glm53-exl3")
        );
        assert_eq!(required_env(&sub("glm53-exl3", None)), None);
        for p in ["openai-compatible", "selfhosted"] {
            assert_eq!(
                provider_for(&sub(p, None)).as_deref(),
                Some("terrarium-selfhosted")
            );
            assert_eq!(
                required_env(&sub(p, None)),
                Some("TERRARIUM_SELFHOSTED_API_KEY")
            );
            // No server: the start says so.
            assert!(
                start_refusal(&sub(p, None), Some("m"))
                    .unwrap()
                    .contains("no server")
            );
        }
        assert_eq!(provider_for(&sub("  ", None)), None);
        assert!(!is_api_key(&sub("openai", Some("ChatGPT Pro"))));
        assert!(is_api_key(&sub("openai", Some(" api KEY "))));
    }
}
