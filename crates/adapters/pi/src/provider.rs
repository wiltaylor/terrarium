//! Subscriptions to pi providers (PLAN §16).
//!
//! An agent names its subscription. The subscription names a provider in
//! Terrarium's words (`SubscriptionDef::provider`: the words of the login
//! wizard and of `terrariumd`'s `env_var_for`). pi has its own provider
//! ids. [`provider_for`] maps one to the other, and [`required_env`] names
//! the env var that carries an API key.
//!
//! Two kinds of credential:
//!
//! - **A plan login (OAuth).** It lives in the shared `auth.json` of the pi
//!   home. All agents share it, and pi refreshes it under a file lock. No
//!   env var is needed.
//! - **An API key.** The daemon keeps it in its credential store and
//!   injects it into the env of the agents on that subscription only. The
//!   login wizard marks such a subscription with `plan = "API key"`
//!   ([`is_api_key`]).
//!
//! `openai` is the one word with two meanings: a ChatGPT plan that Codex
//! logged in (pi `openai-codex`, OAuth) or an OpenAI API key (pi `openai`,
//! `OPENAI_API_KEY`). The plan field decides.
//!
//! Verified through pi (`docs/compat.md`, "Auth and token reuse"): only
//! `openai-codex` (with a copy of the Codex access token). `anthropic` with
//! Claude Code's OAuth token was refused ("You're out of extra usage"):
//! third-party use of a Claude Pro/Max plan is billed as extra usage. The
//! API-key providers are not tested; the env var names are from
//! `pi --help`.

use terrarium_core::config::SubscriptionDef;

/// The plan value that the login wizard gives an API-key subscription.
pub const API_KEY_PLAN: &str = "API key";

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
    /// pi's provider id for a plan login.
    oauth: Option<&'static str>,
    /// pi's provider id and env var for an API key.
    api_key: Option<(&'static str, &'static str)>,
}

const TABLE: &[Row] = &[
    Row {
        names: &["anthropic", "claude"],
        oauth: Some("anthropic"),
        api_key: Some(("anthropic", "ANTHROPIC_API_KEY")),
    },
    Row {
        names: &["openai"],
        oauth: Some("openai-codex"),
        api_key: Some(("openai", "OPENAI_API_KEY")),
    },
    Row {
        names: &["openai-codex", "codex", "chatgpt"],
        oauth: Some("openai-codex"),
        api_key: None,
    },
    Row {
        names: &["github", "github-copilot", "copilot"],
        oauth: Some("github-copilot"),
        api_key: None,
    },
    Row {
        names: &["google", "gemini"],
        oauth: None,
        api_key: Some(("google", "GEMINI_API_KEY")),
    },
    Row {
        names: &["google-gemini-cli"],
        oauth: Some("google-gemini-cli"),
        api_key: None,
    },
    Row {
        names: &["openrouter"],
        oauth: None,
        api_key: Some(("openrouter", "OPENROUTER_API_KEY")),
    },
    Row {
        names: &["xai"],
        oauth: None,
        api_key: Some(("xai", "XAI_API_KEY")),
    },
    Row {
        names: &["groq"],
        oauth: None,
        api_key: Some(("groq", "GROQ_API_KEY")),
    },
    Row {
        names: &["deepseek"],
        oauth: None,
        api_key: Some(("deepseek", "DEEPSEEK_API_KEY")),
    },
    Row {
        names: &["mistral"],
        oauth: None,
        api_key: Some(("mistral", "MISTRAL_API_KEY")),
    },
    // An OpenAI-compatible endpoint needs a provider entry in the shared
    // `models.json` (a base URL and a model list). pi does not read a base
    // URL from the env. Not supported yet: no provider flag.
    Row {
        names: &["openai-compatible"],
        oauth: None,
        api_key: None,
    },
];

fn row(provider: &str) -> Option<&'static Row> {
    let p = provider.trim();
    TABLE
        .iter()
        .find(|r| r.names.iter().any(|n| n.eq_ignore_ascii_case(p)))
}

/// The pi provider id (`--provider`) for a subscription.
///
/// A provider that is not in the table goes through as it is: it can be a
/// custom provider in the shared `models.json` (the spike host has three).
/// `None` when the provider is empty or cannot be used through pi.
pub fn provider_for(sub: &SubscriptionDef) -> Option<String> {
    let name = sub.provider.trim();
    if name.is_empty() {
        return None;
    }
    let Some(r) = row(name) else {
        return Some(name.to_owned());
    };
    let id = if is_api_key(sub) {
        r.api_key.map(|(id, _)| id).or(r.oauth)
    } else {
        r.oauth.or(r.api_key.map(|(id, _)| id))
    };
    id.map(str::to_owned)
}

/// The env var that must carry the subscription's API key, or `None` when
/// the credential is a plan login in the shared `auth.json`.
///
/// The daemon injects the value from its credential store
/// (`<state>/credentials/subscriptions/<name>.json`) into
/// `LaunchSpec::env`, for the agents on this subscription only. The names
/// agree with `terrariumd`'s `login_session::env_var_for`.
pub fn required_env(sub: &SubscriptionDef) -> Option<&'static str> {
    let r = row(&sub.provider)?;
    match (r.oauth, r.api_key) {
        // A provider with only API keys always needs the env var.
        (None, Some((_, var))) => Some(var),
        (Some(_), Some((_, var))) if is_api_key(sub) => Some(var),
        _ => None,
    }
}

/// Why pi must not spend this subscription, or `None` when it can.
///
/// A Claude Pro/Max plan login through pi is billed as extra usage, not
/// from the plan (`docs/compat.md`, "pi: facts that contradict the plan",
/// 1). The daemon refuses such a start; an Anthropic API key is fine.
pub fn refusal(sub: &SubscriptionDef) -> Option<&'static str> {
    let anthropic = row(&sub.provider).is_some_and(|r| r.oauth == Some("anthropic"));
    if anthropic && !is_api_key(sub) {
        return Some(
            "pi cannot spend a Claude plan: Anthropic bills third-party use of a Pro/Max \
             login as extra usage. Use the claude harness, or an Anthropic API key",
        );
    }
    if provider_for(sub).is_none() {
        return Some("pi has no provider for this subscription");
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
        }
    }

    #[test]
    fn plan_logins_map_to_oauth_providers() {
        for (p, want) in [
            ("anthropic", "anthropic"),
            ("openai", "openai-codex"),
            ("openai-codex", "openai-codex"),
            ("github", "github-copilot"),
            ("copilot", "github-copilot"),
            ("google-gemini-cli", "google-gemini-cli"),
        ] {
            let s = sub(p, Some("Pro"));
            assert_eq!(provider_for(&s).as_deref(), Some(want), "{p}");
            assert_eq!(required_env(&s), None, "{p}");
        }
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
    fn claude_plans_are_refused() {
        assert!(refusal(&sub("anthropic", Some("Max 20x"))).is_some());
        assert!(refusal(&sub("claude", None)).is_some());
        assert_eq!(refusal(&sub("anthropic", Some("API key"))), None);
        assert_eq!(refusal(&sub("openai", Some("Pro"))), None);
        assert!(refusal(&sub("openai-compatible", Some("API key"))).is_some());
    }

    #[test]
    fn unknown_and_unsupported_providers() {
        assert_eq!(
            provider_for(&sub("glm53-exl3", None)).as_deref(),
            Some("glm53-exl3")
        );
        assert_eq!(required_env(&sub("glm53-exl3", None)), None);
        assert_eq!(
            provider_for(&sub("openai-compatible", Some("API key"))),
            None
        );
        assert_eq!(provider_for(&sub("  ", None)), None);
        assert!(!is_api_key(&sub("openai", Some("ChatGPT Pro"))));
        assert!(is_api_key(&sub("openai", Some(" api KEY "))));
    }
}
