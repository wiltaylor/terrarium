//! Facts about each subscription provider that the daemon, the CLI, and
//! the dashboard share.

use crate::selfhosted;

/// The public status page of a provider, or `None` when the provider has
/// no official one that Terrarium knows. The dashboard only links to it;
/// nothing fetches it.
///
/// Sources (read 2026-11-01): each page is the provider's own. Z.ai has
/// no public status page (its docs link none, and `status.z.ai` does not
/// resolve), so it has no entry. Moonshot's page (`status.moonshot.cn`)
/// covers the Open Platform API and Kimi Code. A `selfhosted`
/// subscription has its own `status_url` field
/// (`crate::selfhosted::SelfHosted::status_url`).
pub fn status_url(provider: &str) -> Option<&'static str> {
    let p = provider.trim().to_ascii_lowercase();
    Some(match p.as_str() {
        "anthropic" | "claude" => "https://status.anthropic.com",
        "openai" | "openai-codex" | "codex" | "chatgpt" => "https://status.openai.com",
        "github" | "github-copilot" | "copilot" => "https://www.githubstatus.com",
        "openrouter" => "https://status.openrouter.ai",
        "moonshot" => "https://status.moonshot.cn",
        _ => return None,
    })
}

/// The status page of a subscription: the provider's ([`status_url`]),
/// or the `status_url` of a self-hosted server.
pub fn status_url_of(
    provider: &str,
    selfhosted: Option<&selfhosted::SelfHosted>,
) -> Option<String> {
    if selfhosted::is_selfhosted(provider) {
        return selfhosted.and_then(|s| s.status_url.clone());
    }
    status_url(provider).map(str::to_owned)
}

// ---- Display names ----
//
// The name of a subscription on the dashboard and in the CLI is the
// vendor's own name of the product and the plan, with its spelling.
// Sources (read 2026-11-01; docs/compat.md, "Display names"):
//
// - Anthropic: https://claude.com/pricing (plans Free, Pro, Max, Ultra)
//   and https://support.claude.com/en/articles/17728952-what-is-the-ultra-plan
//   ("The Max 20x plan is now called the Ultra plan"). The $100 plan
//   ("5x Pro usage") is "Max". The API is the "Claude API".
// - OpenAI: https://learn.chatgpt.com/docs/pricing ("ChatGPT Free, Go,
//   Plus, Pro, Business, Edu, or Enterprise plan"). The `team` plan claim
//   of older logins is the plan that OpenAI calls Business now.
// - GitHub: https://github.com/features/copilot/plans ("GitHub Copilot",
//   plans Free, Pro, Pro+, Max, Business, Enterprise). The probe answer
//   does not say the plan, so the tier shows only when the stored plan
//   names it.
// - Z.ai: https://docs.z.ai/devpack/overview ("GLM Coding Plan", tiers
//   Lite, Pro, Max). The tier is the `level` of the quota probe answer.
// - Moonshot: https://www.kimi.com/code ("Kimi Code") and
//   https://platform.kimi.ai/ ("Kimi API Platform").
// - OpenRouter: https://openrouter.ai/pricing ("OpenRouter").

/// The name that Anthropic uses today for a Claude plan, from a
/// `rateLimitTier` (`default_claude_max_20x`), a subscription type
/// (`max`), or a stored plan label (`Max 20x`). `None` when the text names
/// no plan that Anthropic has.
pub fn claude_plan_name(raw: &str) -> Option<&'static str> {
    let k: String = raw
        .to_ascii_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let k = k.strip_prefix("claude").unwrap_or(&k);
    let k = k.strip_prefix("defaultclaude").unwrap_or(k);
    Some(match k {
        _ if k.contains("ultra") || k.contains("max20x") => "Ultra",
        _ if k.starts_with("max") => "Max",
        "pro" | "claudepro" => "Pro",
        "free" => "Free",
        "team" => "Team",
        "enterprise" => "Enterprise",
        _ => return None,
    })
}

/// The ChatGPT plan name from a `chatgpt_plan_type` claim or a stored
/// plan (`pro`).
pub fn chatgpt_plan_name(raw: &str) -> Option<&'static str> {
    Some(match raw.trim().to_ascii_lowercase().as_str() {
        "free" => "Free",
        "go" => "Go",
        "plus" => "Plus",
        "pro" => "Pro",
        "team" | "business" => "Business",
        "edu" | "education" => "Edu",
        "enterprise" => "Enterprise",
        _ => return None,
    })
}

/// The GitHub Copilot plan name from a stored plan (`pro+`).
pub fn copilot_plan_name(raw: &str) -> Option<&'static str> {
    Some(match raw.trim().to_ascii_lowercase().as_str() {
        "free" => "Free",
        "pro" => "Pro",
        "pro+" | "pro_plus" | "proplus" | "pro-plus" => "Pro+",
        "max" => "Max",
        "business" => "Business",
        "enterprise" => "Enterprise",
        _ => return None,
    })
}

/// The GLM Coding Plan tier from the `level` of the Z.ai quota answer.
pub fn glm_coding_tier(raw: &str) -> Option<&'static str> {
    Some(match raw.trim().to_ascii_lowercase().as_str() {
        "lite" => "Lite",
        "pro" => "Pro",
        "max" => "Max",
        _ => return None,
    })
}

/// True for the plan word of an API key subscription.
fn is_api_key(plan: Option<&str>) -> bool {
    plan.is_some_and(|p| {
        let p = p.trim().to_ascii_lowercase();
        p == "api key" || p == "api" || p == "api-cn"
    })
}

/// The host of a base URL: `gpu.lan:8000` from `http://gpu.lan:8000/v1`.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?;
    (!host.is_empty()).then_some(host)
}

/// The display name of a subscription: the vendor's product and plan
/// name (`Claude Ultra`, `ChatGPT Pro`, `GitHub Copilot`, `GLM Coding
/// Plan Max`, `Kimi Code`, `OpenRouter`). `tier` is a plan hint from the
/// last probe answer (the `level` of Z.ai). A provider that the table
/// does not know shows as `<provider> <plan>`; a self-hosted server as
/// `Self-hosted <host>`. `name` is the subscription name, the last
/// fallback.
pub fn display_name(
    provider: &str,
    plan: Option<&str>,
    tier: Option<&str>,
    selfhosted: Option<&selfhosted::SelfHosted>,
    name: &str,
) -> String {
    let p = provider.trim().to_ascii_lowercase();
    let plan = plan.map(str::trim).filter(|s| !s.is_empty());
    let with = |product: &str, tier: Option<&str>| match tier {
        Some(t) => format!("{product} {t}"),
        None => product.to_owned(),
    };
    if selfhosted::is_selfhosted(&p) {
        let host = selfhosted.and_then(|s| host_of(&s.base_url));
        return format!("Self-hosted {}", host.unwrap_or(name));
    }
    match p.as_str() {
        "anthropic" | "claude" if is_api_key(plan) => "Claude API".into(),
        "anthropic" | "claude" => with("Claude", plan.and_then(claude_plan_name)),
        "openai" | "openai-codex" | "codex" | "chatgpt" if is_api_key(plan) => "OpenAI API".into(),
        "openai" | "openai-codex" | "codex" | "chatgpt" => {
            with("ChatGPT", plan.and_then(chatgpt_plan_name))
        }
        "github" | "github-copilot" | "copilot" => {
            with("GitHub Copilot", plan.and_then(copilot_plan_name))
        }
        "zai" | "z.ai" if is_api_key(plan) => "Z.ai".into(),
        "zai" | "z.ai" => with("GLM Coding Plan", tier.and_then(glm_coding_tier)),
        "moonshot" if is_api_key(plan) => "Kimi API Platform".into(),
        "moonshot" => "Kimi Code".into(),
        "openrouter" => "OpenRouter".into(),
        _ => match plan {
            Some(pl) => format!("{} {pl}", provider.trim()),
            None if !provider.trim().is_empty() => provider.trim().to_owned(),
            None => name.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_table() {
        assert_eq!(
            status_url("anthropic"),
            Some("https://status.anthropic.com")
        );
        assert_eq!(status_url(" OpenAI "), Some("https://status.openai.com"));
        assert_eq!(status_url("github"), Some("https://www.githubstatus.com"));
        assert_eq!(
            status_url("openrouter"),
            Some("https://status.openrouter.ai")
        );
        assert_eq!(status_url("moonshot"), Some("https://status.moonshot.cn"));
        // No official page known: no link.
        assert_eq!(status_url("zai"), None);
        assert_eq!(status_url("selfhosted"), None);
        for p in ["anthropic", "openai", "github", "openrouter", "moonshot"] {
            assert!(
                status_url(p).is_some_and(|u| u.starts_with("https://")),
                "{p}"
            );
        }
        let sh = selfhosted::SelfHosted {
            status_url: Some("http://gpu.lan/status".into()),
            ..Default::default()
        };
        assert_eq!(
            status_url_of("selfhosted", Some(&sh)).as_deref(),
            Some("http://gpu.lan/status")
        );
        assert_eq!(status_url_of("selfhosted", None), None);
        assert_eq!(
            status_url_of("anthropic", Some(&sh)).as_deref(),
            Some("https://status.anthropic.com")
        );
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    fn name(provider: &str, plan: Option<&str>) -> String {
        display_name(provider, plan, None, None, "sub")
    }

    #[test]
    fn claude_plans_use_the_names_of_today() {
        // The Max 20x plan is the Ultra plan since 2026-10-30.
        for raw in [
            "default_claude_max_20x",
            "Max 20x",
            "max_20x",
            "default_claude_ultra",
            "Ultra",
        ] {
            assert_eq!(claude_plan_name(raw), Some("Ultra"), "{raw}");
        }
        for raw in ["default_claude_max_5x", "Max 5x", "max", "Max"] {
            assert_eq!(claude_plan_name(raw), Some("Max"), "{raw}");
        }
        assert_eq!(claude_plan_name("pro"), Some("Pro"));
        assert_eq!(claude_plan_name("default_claude_pro"), Some("Pro"));
        assert_eq!(claude_plan_name("something"), None);
        assert_eq!(name("anthropic", Some("Max 20x")), "Claude Ultra");
        assert_eq!(name("anthropic", Some("Ultra")), "Claude Ultra");
        assert_eq!(name("anthropic", Some("Max 5x")), "Claude Max");
        assert_eq!(name("anthropic", Some("Pro")), "Claude Pro");
        assert_eq!(name("anthropic", None), "Claude");
        assert_eq!(name("anthropic", Some("API key")), "Claude API");
    }

    #[test]
    fn the_name_table() {
        assert_eq!(name("openai", Some("pro")), "ChatGPT Pro");
        assert_eq!(name("openai", Some("plus")), "ChatGPT Plus");
        assert_eq!(name("openai", Some("team")), "ChatGPT Business");
        assert_eq!(name("openai", None), "ChatGPT");
        assert_eq!(name("openai", Some("API key")), "OpenAI API");
        assert_eq!(name("github", None), "GitHub Copilot");
        assert_eq!(name("github", Some("API key")), "GitHub Copilot");
        assert_eq!(name("github", Some("pro+")), "GitHub Copilot Pro+");
        assert_eq!(name("zai", Some("coding")), "GLM Coding Plan");
        assert_eq!(
            display_name("zai", Some("coding"), Some("max"), None, "z"),
            "GLM Coding Plan Max"
        );
        assert_eq!(
            display_name("zai", Some("coding"), Some("odd"), None, "z"),
            "GLM Coding Plan"
        );
        assert_eq!(name("zai", Some("api")), "Z.ai");
        assert_eq!(name("moonshot", Some("coding")), "Kimi Code");
        assert_eq!(name("moonshot", Some("coding-cn")), "Kimi Code");
        assert_eq!(name("moonshot", Some("api")), "Kimi API Platform");
        assert_eq!(name("moonshot", Some("api-cn")), "Kimi API Platform");
        assert_eq!(name("openrouter", Some("API key")), "OpenRouter");
        assert_eq!(name("acme", Some("Gold")), "acme Gold");
        assert_eq!(name("acme", None), "acme");
        let sh = selfhosted::SelfHosted {
            base_url: "http://gpu.lan:8000/v1".into(),
            ..Default::default()
        };
        assert_eq!(
            display_name("selfhosted", None, None, Some(&sh), "gpu"),
            "Self-hosted gpu.lan:8000"
        );
        assert_eq!(
            display_name("selfhosted", None, None, None, "gpu"),
            "Self-hosted gpu"
        );
    }
}
