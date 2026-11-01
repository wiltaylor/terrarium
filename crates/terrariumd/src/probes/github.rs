//! GitHub Copilot: the premium-request quota snapshot behind Copilot's
//! usage display.
//!
//! `GET https://api.github.com/copilot_internal/user` with
//! `Authorization: token <github token>`. `quota_snapshots` has
//! `premium_interactions`, `chat`, and `completions`, each with
//! `percent_remaining`, `entitlement`, `remaining`, and `unlimited`.
//! `quota_reset_date_utc` is the monthly reset. Verified 2026-10-24 with a
//! GitHub CLI OAuth token (docs/compat.md, "Usage probes").
//!
//! The token: a stored API key (a fine-grained PAT with the Copilot
//! Requests permission), else the plain-text fallback in
//! `<copilot home>/config.json` (Copilot keeps the token in the system
//! credential store when it has one, which a container does not), else
//! `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, or `GITHUB_TOKEN` in the daemon env.
//! The plain-text fallback keeps the token in `authTokens.<key>.token`
//! (verified 2026-10-25 with a container login, docs/compat.md).
//!
//! The answer does not name the plan. [`copilot_tier`] reads it from
//! `access_type_sku` and `copilot_plan`, with the map of VS Code
//! (`src/vs/workbench/services/chat/common/chatEntitlementService.ts`,
//! `doResolveEntitlement`, read 2026-11-01). The probe puts the tier in the
//! `raw` of each window as `level`, as the Z.ai probe does, and the display
//! name shows it (`GitHub Copilot Pro`). Verified 2026-11-01: a Pro
//! account answers `copilot_plan: "individual"`, `access_type_sku:
//! "yearly_subscriber_quota"` (docs/compat.md, "GitHub Copilot").

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body};

pub const NAME: &str = "github-copilot-quota";
pub const URL: &str = "https://api.github.com/copilot_internal/user";

/// Env vars that Copilot reads for a token, in its order.
pub const TOKEN_ENV: [&str; 3] = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

pub struct CopilotQuota;

/// Remove `//` line comments: Copilot's `config.json` starts with some.
pub fn strip_line_comments(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A token in the JSON of Copilot's `config.json`, if it has one. The
/// real file keeps it in `authTokens.<key>.token` (the adapter reads
/// that); the snake-case keys after it are older guesses.
pub fn token_in_config(v: &Value) -> Option<String> {
    if let Some(t) = terrarium_adapter_copilot::auth::token_in_config(v) {
        return Some(t);
    }
    let first_str = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(m) => m.values().find_map(|x| x.as_str().map(str::to_owned)),
            _ => None,
        }
    };
    ["copilot_tokens", "github_token", "oauth_token", "token"]
        .iter()
        .find_map(|k| v.get(*k).and_then(first_str))
}

/// Read `config.json` of a Copilot home. `Ok(None)` when it has no token.
pub fn config_token(home: &Path) -> Result<Option<String>, ProbeError> {
    let path = home.join("config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ProbeError::NoCredential(format!(
                "cannot read {}: {}",
                path.display(),
                e.kind()
            )));
        }
    };
    let v: Value = serde_json::from_str(&strip_line_comments(&text))
        .map_err(|e| ProbeError::NoCredential(format!("{} is not JSON: {e}", path.display())))?;
    Ok(token_in_config(&v))
}

/// The plan key of a quota answer (`free`, `student`, `pro`, `pro+`,
/// `max`, `business`, `enterprise`) for
/// `terrarium_core::providers::copilot_plan_name`. The order is the order
/// of VS Code: the SKU of a free plan first, then `copilot_plan`. `None`
/// for a value that VS Code does not know, and for an account that can
/// still sign up for Copilot Free (it has no plan).
pub fn copilot_tier(v: &Value) -> Option<&'static str> {
    let sku = v.get("access_type_sku").and_then(Value::as_str);
    match sku {
        Some("free_limited_copilot") => return Some("free"),
        Some("free_educational_quota") => return Some("student"),
        _ => {}
    }
    if v.get("can_signup_for_limited").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    Some(match v.get("copilot_plan").and_then(Value::as_str)? {
        "individual_edu" => "student",
        "individual" => "pro",
        "individual_pro" => "pro+",
        "individual_max" => "max",
        "business" => "business",
        "enterprise" => "enterprise",
        _ => return None,
    })
}

fn percent_used(s: &Value) -> Option<f64> {
    if let Some(r) = s.get("percent_remaining").and_then(Value::as_f64) {
        return Some((100.0 - r).max(0.0));
    }
    let ent = s.get("entitlement").and_then(Value::as_f64)?;
    let rem = s.get("remaining").and_then(Value::as_f64)?;
    (ent > 0.0).then(|| ((ent - rem) / ent * 100.0).max(0.0))
}

impl QuotaProbe for CopilotQuota {
    fn name(&self) -> &'static str {
        NAME
    }

    fn harness(&self) -> HarnessId {
        HarnessId::Copilot
    }

    fn accepts_api_key(&self) -> bool {
        true
    }

    fn credentials(&self, home: &Path) -> Result<Credentials, ProbeError> {
        // A plan account's store (PLAN §16.3) holds the token alone.
        let account = home.join(crate::accounts::TOKEN_FILE);
        if let Ok(b) = crate::accounts::read_nofollow(&account)
            && let Ok(t) = String::from_utf8(b)
            && !t.trim().is_empty()
        {
            return Ok(Credentials::new(t.trim()));
        }
        if let Some(t) = config_token(home)? {
            return Ok(Credentials::new(t));
        }
        TOKEN_ENV
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .map(Credentials::new)
            .ok_or_else(|| {
                ProbeError::NoCredential(format!(
                    "no token in {}/config.json or in {}; Copilot keeps its login in the system \
                     credential store, so store a PAT with POST /api/subscriptions/<name>/api-key",
                    home.display(),
                    TOKEN_ENV.join(", ")
                ))
            })
    }

    fn request(&self, creds: &Credentials) -> HttpRequest {
        HttpRequest {
            url: URL.into(),
            headers: vec![
                ("Authorization".into(), format!("token {}", creds.token())),
                ("Accept".into(), "application/json".into()),
            ],
        }
    }

    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v = json_body(body)?;
        let snaps = v
            .get("quota_snapshots")
            .and_then(Value::as_object)
            .ok_or_else(|| ProbeError::Parse("no quota_snapshots in the answer".into()))?;
        let resets_at = v
            .get("quota_reset_date_utc")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let level = copilot_tier(&v);
        let mut out = Vec::new();
        for (key, window) in [
            ("premium_interactions", "monthly"),
            ("chat", "monthly_chat"),
            ("completions", "monthly_completions"),
        ] {
            let Some(s) = snaps.get(key) else { continue };
            if s.get("unlimited").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(pct) = percent_used(s) else { continue };
            let mut raw = s.clone();
            if let (Some(l), Some(o)) = (level, raw.as_object_mut()) {
                o.insert("level".into(), l.into());
            }
            out.push(QuotaWindow {
                window: window.into(),
                used_pct: (pct * 10.0).round() / 10.0,
                resets_at,
                raw,
            });
        }
        if out.is_empty() {
            return Err(ProbeError::Parse(
                "no limited quota in quota_snapshots (all unlimited?)".into(),
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::probes::tests::FakeHttp;

    const FIXTURE: &str = include_str!("../../tests/fixtures/probes/github_copilot_user.json");

    #[test]
    fn parses_the_recorded_answer() {
        let w = CopilotQuota.parse(FIXTURE).unwrap();
        // chat and completions are unlimited on this plan.
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].window, "monthly");
        assert!((w[0].used_pct - 22.7).abs() < 1e-9);
        assert_eq!(
            w[0].resets_at.unwrap().to_rfc3339(),
            "2026-11-01T00:00:00+00:00"
        );
        assert_eq!(w[0].raw["entitlement"], 300);
        // `copilot_plan: "individual"` is Copilot Pro.
        assert_eq!(w[0].raw["level"], "pro");
        assert!(CopilotQuota.parse(r#"{"login": "x"}"#).is_err());
    }

    /// The answer of a Pro account, read 2026-11-01 (sanitised: no login,
    /// ids, organizations, or enterprises).
    const PRO: &str = include_str!("../../tests/fixtures/probes/github_copilot_user_pro.json");

    #[test]
    fn a_pro_answer_has_the_pro_level() {
        let w = CopilotQuota.parse(PRO).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].window, "monthly");
        assert_eq!(w[0].used_pct, 0.0);
        assert_eq!(w[0].raw["level"], "pro");
        assert_eq!(
            terrarium_core::providers::display_name("github", None, Some("pro"), None, "g"),
            "GitHub Copilot Pro"
        );
    }

    #[test]
    fn the_tier_follows_vs_code() {
        let t = |sku: &str, plan: &str| {
            copilot_tier(&serde_json::json!({"access_type_sku": sku, "copilot_plan": plan}))
        };
        assert_eq!(t("yearly_subscriber_quota", "individual"), Some("pro"));
        assert_eq!(t("free_limited_copilot", "individual"), Some("free"));
        assert_eq!(t("free_educational_quota", "individual"), Some("student"));
        assert_eq!(t("x", "individual_edu"), Some("student"));
        assert_eq!(t("x", "individual_pro"), Some("pro+"));
        assert_eq!(t("x", "individual_max"), Some("max"));
        assert_eq!(t("x", "business"), Some("business"));
        assert_eq!(t("x", "enterprise"), Some("enterprise"));
        // No guess for a value that VS Code does not know.
        assert_eq!(t("x", "individual_ultra"), None);
        assert_eq!(copilot_tier(&serde_json::json!({})), None);
        // An account that can still sign up for Copilot Free has no plan.
        assert_eq!(
            copilot_tier(
                &serde_json::json!({"copilot_plan": "individual", "can_signup_for_limited": true})
            ),
            None
        );
        // Each name is one that the display name table knows.
        for p in [
            "free",
            "student",
            "pro",
            "pro+",
            "max",
            "business",
            "enterprise",
        ] {
            assert!(
                terrarium_core::providers::copilot_plan_name(p).is_some(),
                "{p}"
            );
        }
        // An unknown plan leaves no level, and the windows stay.
        let body = FIXTURE.replace("\"individual\"", "\"individual_new\"");
        let w = CopilotQuota.parse(&body).unwrap();
        assert!(w[0].raw.get("level").is_none());
    }

    #[test]
    fn config_token_skips_comments() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(config_token(tmp.path()).unwrap(), None);
        std::fs::write(
            tmp.path().join("config.json"),
            "// User settings belong in settings.json.\n// This file is managed automatically.\n{\"firstLaunchAt\": \"x\", \"trustedFolders\": []}",
        )
        .unwrap();
        assert_eq!(config_token(tmp.path()).unwrap(), None);
        std::fs::write(
            tmp.path().join("config.json"),
            "// managed\n{\"copilot_tokens\": {\"https://github.com:me\": \"gho_x\"}}",
        )
        .unwrap();
        assert_eq!(config_token(tmp.path()).unwrap().as_deref(), Some("gho_x"));
        let c = CopilotQuota.credentials(tmp.path()).unwrap();
        assert_eq!(c.token(), "gho_x");
        // The shape of a real container login.
        std::fs::write(
            tmp.path().join("config.json"),
            "// User settings belong in settings.json.\n// This file is managed automatically.\n{\n  \"authTokens\": {\n    \"https://github.com:octo:github\": {\"token\": \"gho_real\"},\n    \"https://github.com:octo\": {\"token\": \"gho_real\"}\n  },\n  \"lastLoggedInUser\": {\"host\": \"https://github.com\", \"login\": \"octo\"}\n}\n",
        )
        .unwrap();
        assert_eq!(
            config_token(tmp.path()).unwrap().as_deref(),
            Some("gho_real")
        );
    }

    #[tokio::test]
    async fn run_uses_the_token_scheme() {
        let http = FakeHttp::with(URL, 200, FIXTURE);
        CopilotQuota
            .run(&http, &Credentials::new("gho_x"))
            .await
            .unwrap();
        let seen = http.seen.lock().unwrap();
        assert!(
            seen[0]
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "token gho_x")
        );
    }
}
