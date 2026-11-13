//! xAI as a subscription provider (PLAN §16, TER-1169 D1, TER-1191).
//!
//! One provider, `xai`, has two kinds of row:
//!
//! | `plan` | What it is | Harnesses |
//! |---|---|---|
//! | `API key` (or no plan, a row from before) | an xAI API key, pay per token | `["pi"]` |
//! | a tier: [`TIERS`] | the X Subscription (SuperGrok / X Premium), a plan login | `["grok"]` (Grok Build) |
//!
//! A tier row is a plan login. pi never spends it ([`refusal`], and pi's
//! own guard in `terrarium_adapter_pi::provider::is_plan`): the API is billed
//! apart from the plan. Grok Build never spends an API-key row: Terrarium
//! keeps the key out of its env. Each other mix of provider, plan, and
//! harness is refused by [`refusal`].
//!
//! The Grok Build login is a file, `~/.grok/auth.json` (mode 0600, written
//! and refreshed by the CLI). The daemon keeps it in the account of the row
//! (`terrariumd::accounts`). It is a private file, not an encrypted one.

use crate::HarnessId;

/// The provider word.
pub const PROVIDER: &str = "xai";

/// The plan of an API-key row (`terrarium_adapter_pi::provider::API_KEY_PLAN`).
pub const API_KEY_PLAN: &str = "API key";

/// The tiers of the X Subscription, as the operator writes them in `plan`.
pub const TIERS: [&str; 4] = ["Plus", "Premium", "Super", "Ultra"];

/// The harness of a tier row.
pub const PLAN_HARNESS: HarnessId = HarnessId::Grok;

/// The harness of an API-key row.
pub const KEY_HARNESS: HarnessId = HarnessId::Pi;

/// True for the provider `xai`.
pub fn is_xai(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(PROVIDER)
}

/// What the `plan` of an xai row says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// An API key: `API key`, or no plan (a row from before the tiers).
    ApiKey,
    /// A tier of the X Subscription, by its name in [`TIERS`].
    Tier(&'static str),
    /// Any other word: refused.
    Unknown,
}

/// Read the `plan` of an xai row. Case and outer spaces do not count.
pub fn plan(plan: Option<&str>) -> Plan {
    let Some(p) = plan.map(str::trim).filter(|p| !p.is_empty()) else {
        return Plan::ApiKey;
    };
    if p.eq_ignore_ascii_case(API_KEY_PLAN) {
        return Plan::ApiKey;
    }
    TIERS
        .iter()
        .find(|t| t.eq_ignore_ascii_case(p))
        .map_or(Plan::Unknown, |t| Plan::Tier(t))
}

/// True when the row is an xai plan login (a tier, or an unknown plan
/// word): pi must not spend it.
pub fn is_plan_row(provider: &str, plan_word: Option<&str>) -> bool {
    is_xai(provider) && plan(plan_word) != Plan::ApiKey
}

/// Why the combination of `provider`, `plan`, and `harnesses` is refused,
/// or `None` when it is allowed.
///
/// - `grok` spends only an xai tier row.
/// - An xai tier row is for `grok` only, and names it.
/// - An xai API-key row is for `pi` only.
/// - An xai row with another plan word is refused.
pub fn refusal(provider: &str, plan_word: Option<&str>, harnesses: &[HarnessId]) -> Option<String> {
    if !is_xai(provider) {
        return harnesses.contains(&PLAN_HARNESS).then(|| {
            format!(
                "the grok harness spends only an xai plan (provider `xai`, plan one of {}), \
                 not provider `{}`",
                TIERS.join(", "),
                provider.trim()
            )
        });
    }
    let only = |want: HarnessId, what: &str| -> Option<String> {
        let other: Vec<&str> = harnesses
            .iter()
            .filter(|h| **h != want)
            .map(|h| h.as_str())
            .collect();
        if !other.is_empty() {
            return Some(format!(
                "{what} is for the {want} harness only, not {}; set harnesses to [\"{want}\"]",
                other.join(", ")
            ));
        }
        None
    };
    match plan(plan_word) {
        Plan::ApiKey => only(KEY_HARNESS, "an xai API key"),
        Plan::Tier(t) => {
            if harnesses.is_empty() {
                return Some(format!(
                    "an xai {t} plan needs a harness; set harnesses to [\"{PLAN_HARNESS}\"]"
                ));
            }
            only(PLAN_HARNESS, &format!("an xai {t} plan"))
        }
        Plan::Unknown => Some(format!(
            "unknown xai plan `{}`; use one of {}, or `{API_KEY_PLAN}`",
            plan_word.unwrap_or_default().trim(),
            TIERS.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_are_read() {
        assert_eq!(plan(None), Plan::ApiKey);
        assert_eq!(plan(Some(" ")), Plan::ApiKey);
        assert_eq!(plan(Some("api key")), Plan::ApiKey);
        assert_eq!(plan(Some("super")), Plan::Tier("Super"));
        for t in TIERS {
            assert_eq!(plan(Some(t)), Plan::Tier(t));
        }
        assert_eq!(plan(Some("Heavy")), Plan::Unknown);
        assert!(is_plan_row("XAI", Some("Ultra")));
        assert!(is_plan_row("xai", Some("Heavy")));
        assert!(!is_plan_row("xai", Some("API key")));
        assert!(!is_plan_row("xai", None));
        assert!(!is_plan_row("anthropic", Some("Ultra")));
    }

    #[test]
    fn the_allowed_rows() {
        assert_eq!(refusal("xai", Some("Super"), &[HarnessId::Grok]), None);
        assert_eq!(refusal("xai", Some("API key"), &[HarnessId::Pi]), None);
        // A key row from before: no plan, pi.
        assert_eq!(refusal("xai", None, &[HarnessId::Pi]), None);
        assert_eq!(refusal("anthropic", None, &[HarnessId::Claude]), None);
    }

    #[test]
    fn the_refused_combinations() {
        let r = |p: &str, plan: Option<&str>, h: &[HarnessId]| refusal(p, plan, h).unwrap();
        assert!(r("xai", Some("Super"), &[HarnessId::Pi]).contains("grok harness only"));
        assert!(r("xai", Some("Plus"), &[HarnessId::Grok, HarnessId::Pi]).contains("not pi"));
        assert!(r("xai", Some("Premium"), &[]).contains("needs a harness"));
        assert!(r("xai", Some("API key"), &[HarnessId::Grok]).contains("pi harness only"));
        assert!(r("xai", None, &[HarnessId::Claude]).contains("pi harness only"));
        assert!(r("xai", Some("Heavy"), &[HarnessId::Grok]).contains("unknown xai plan"));
        assert!(r("anthropic", Some("Max"), &[HarnessId::Grok]).contains("only an xai plan"));
        assert!(r("openrouter", None, &[HarnessId::Pi, HarnessId::Grok]).contains("only an xai"));
    }
}
