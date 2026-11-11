//! Cloud providers for Claude Code (Cloud C1, TER-630): Amazon Bedrock
//! (`bedrock`), Google Cloud Vertex AI (`gcp`), and Microsoft Foundry
//! (`foundry`).
//!
//! A cloud subscription is for the Claude Code harness only. It has:
//!
//! - [`CloudSettings`] in the definition (`[cloud]`, no secret): the
//!   region, the project, the resource or base URL, the model map, and
//!   the monthly budget in USD.
//! - A key file (`POST /api/subscriptions/{name}/api-key`): the secret.
//!   Bedrock: the secret access key with `key_id` (the access key ID), or
//!   an Amazon Bedrock API key without `key_id`. GCP: the service-account
//!   JSON. Foundry: the API key.
//!
//! [`claude_env`] gives the env of Claude Code. The daemon hands it to
//! `terrarium exec-env` in the sandbox (`credential_env_for`), never to
//! the launch spec. The GCP JSON is a file ([`CloudEnv::files`]): the
//! sandbox writes it with mode 0600 and points
//! `GOOGLE_APPLICATION_CREDENTIALS` to it.
//!
//! A cloud subscription bills per token. The automatic account choice (the
//! auto-swap after a usage limit, the spawn pick from a pool) never takes
//! one: an agent or a template uses it only when it names it.
//!
//! The env var names are from the Claude Code docs, read 2026-11-10:
//! <https://code.claude.com/docs/en/amazon-bedrock>,
//! <https://code.claude.com/docs/en/google-vertex-ai>, and
//! <https://code.claude.com/docs/en/microsoft-foundry>. Not verified
//! live: no cloud account was available (`docs/compat.md`, "Cloud
//! providers").

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::HarnessId;

/// The harnesses that may spend a cloud subscription.
pub const HARNESSES: [HarnessId; 1] = [HarnessId::Claude];

/// The quota window of the monthly budget.
pub const BUDGET_WINDOW: &str = "budget";

/// The percent of the budget that sends the alert.
pub const ALERT_PCT: f64 = 80.0;

/// Bedrock: an IAM access key ID (with the secret access key).
pub const AWS_ACCESS_KEY_ID: &str = "AWS_ACCESS_KEY_ID";
/// Bedrock: the secret access key.
pub const AWS_SECRET_ACCESS_KEY: &str = "AWS_SECRET_ACCESS_KEY";
/// Bedrock: an Amazon Bedrock API key (a bearer token).
pub const AWS_BEARER_TOKEN_BEDROCK: &str = "AWS_BEARER_TOKEN_BEDROCK";
/// GCP: the path of the service-account JSON.
pub const GOOGLE_APPLICATION_CREDENTIALS: &str = "GOOGLE_APPLICATION_CREDENTIALS";

/// Vertex publisher model IDs. Explicit settings override these defaults.
pub fn vertex_models() -> BTreeMap<String, String> {
    [
        ("opus", "claude-opus-5-5"),
        ("sonnet", "claude-sonnet-4-6"),
        ("haiku", "claude-haiku-4-5@20251001"),
    ]
    .into_iter()
    .map(|(tier, id)| (tier.into(), id.into()))
    .collect()
}
/// Foundry: the API key.
pub const ANTHROPIC_FOUNDRY_API_KEY: &str = "ANTHROPIC_FOUNDRY_API_KEY";

/// A cloud provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bedrock,
    Gcp,
    Foundry,
}

impl Kind {
    /// Every kind, in the order the wizard shows them.
    pub const ALL: [Kind; 3] = [Kind::Bedrock, Kind::Gcp, Kind::Foundry];

    /// The provider word of a subscription.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bedrock => "bedrock",
            Self::Gcp => "gcp",
            Self::Foundry => "foundry",
        }
    }

    /// The vendor's name of the product.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Bedrock => "Amazon Bedrock",
            Self::Gcp => "Google Cloud Vertex AI",
            Self::Foundry => "Microsoft Foundry",
        }
    }

    /// The switch var that turns the provider on in Claude Code.
    pub fn use_var(self) -> &'static str {
        match self {
            Self::Bedrock => "CLAUDE_CODE_USE_BEDROCK",
            Self::Gcp => "CLAUDE_CODE_USE_VERTEX",
            Self::Foundry => "CLAUDE_CODE_USE_FOUNDRY",
        }
    }

    /// The env var that carries the stored key. Bedrock: the secret access
    /// key with an access key ID, else the Bedrock API key. GCP: the file
    /// var.
    pub fn key_var(self, with_key_id: bool) -> &'static str {
        match self {
            Self::Bedrock if with_key_id => AWS_SECRET_ACCESS_KEY,
            Self::Bedrock => AWS_BEARER_TOKEN_BEDROCK,
            Self::Gcp => GOOGLE_APPLICATION_CREDENTIALS,
            Self::Foundry => ANTHROPIC_FOUNDRY_API_KEY,
        }
    }
}

/// The cloud kind of a provider word, `None` for another provider.
pub fn kind(provider: &str) -> Option<Kind> {
    let p = provider.trim();
    Kind::ALL
        .into_iter()
        .find(|k| p.eq_ignore_ascii_case(k.as_str()))
}

/// True when `provider` is a cloud provider.
pub fn is_cloud(provider: &str) -> bool {
    kind(provider).is_some()
}

/// Why a cloud subscription cannot list `harnesses`: one that is not
/// Claude Code.
pub fn harness_refusal(harnesses: &[HarnessId]) -> Option<String> {
    let other: Vec<&str> = harnesses
        .iter()
        .filter(|h| !HARNESSES.contains(h))
        .map(|h| h.as_str())
        .collect();
    (!other.is_empty()).then(|| {
        format!(
            "a cloud subscription is for the claude harness only, not {}; set harnesses to [\"claude\"]",
            other.join(", ")
        )
    })
}

/// The settings of a cloud subscription (`[cloud]` in the definition).
/// No secret: the key is in the key file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudSettings {
    /// Bedrock: `AWS_REGION`. GCP: `CLOUD_ML_REGION` (`global`, `us`,
    /// `us-east5`). Not for Foundry. Claude Code has a default for each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// GCP: `ANTHROPIC_VERTEX_PROJECT_ID`. Needed for GCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Foundry: `ANTHROPIC_FOUNDRY_RESOURCE`, the resource name alone.
    /// Foundry needs this or `base_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// An endpoint over the default: `ANTHROPIC_BEDROCK_BASE_URL`,
    /// `ANTHROPIC_VERTEX_BASE_URL`, or `ANTHROPIC_FOUNDRY_BASE_URL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Terrarium model names (`opus`, `sonnet`, `haiku`, or a Claude id
    /// such as `claude-opus-5-5`) to cloud model ids (a Bedrock inference
    /// profile id or ARN, a Vertex model id, a Foundry deployment name).
    /// See [`model_for`] and [`tier_env`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, String>,
    /// The monthly budget in whole USD. `None`: no budget window, no hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<u32>,
}

/// A region or location word: what Claude Code takes (no slash, dot, or
/// space).
fn region_ok(r: &str) -> bool {
    !r.is_empty()
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A model id, a project id, or a resource name: no space, no control
/// character.
fn word_ok(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Check the settings of a `kind` subscription. An error names the field.
pub fn check(kind: Kind, s: &CloudSettings) -> Result<(), String> {
    let name = kind.as_str();
    if let Some(r) = &s.region {
        if kind == Kind::Foundry {
            return Err("`cloud.region` is not for foundry; use `resource` or `base_url`".into());
        }
        if !region_ok(r) {
            return Err(format!(
                "`cloud.region` `{r}` is not a region name (lowercase letters, digits, and `-`)"
            ));
        }
    }
    if let Some(u) = &s.base_url
        && !(u.starts_with("https://") && word_ok(u))
    {
        return Err("`cloud.base_url` must be an https:// URL".into());
    }
    match kind {
        Kind::Gcp => match s.project_id.as_deref() {
            Some(p) if word_ok(p) => {}
            Some(_) => return Err("`cloud.project_id` has a space".into()),
            None => return Err("a gcp subscription needs `cloud.project_id`".into()),
        },
        _ if s.project_id.is_some() => {
            return Err(format!("`cloud.project_id` is for gcp only, not {name}"));
        }
        _ => {}
    }
    match (kind, s.resource.as_deref(), s.base_url.is_some()) {
        (Kind::Foundry, None, false) => {
            return Err("a foundry subscription needs `cloud.resource` or `cloud.base_url`".into());
        }
        (Kind::Foundry, Some(_), true) => {
            return Err("give `cloud.resource` or `cloud.base_url`, not both".into());
        }
        (Kind::Foundry, Some(r), false) if !word_ok(r) || r.contains(['.', '/', ':']) => {
            return Err(format!(
                "`cloud.resource` `{r}` must be the Foundry resource name alone, not a URL or host"
            ));
        }
        (Kind::Bedrock | Kind::Gcp, Some(_), _) => {
            return Err(format!("`cloud.resource` is for foundry only, not {name}"));
        }
        _ => {}
    }
    for (k, v) in &s.models {
        if !word_ok(k) || k.to_ascii_lowercase() != *k {
            return Err(format!(
                "`cloud.models` key `{k}` must be a lowercase model name"
            ));
        }
        if !word_ok(v) {
            return Err(format!(
                "`cloud.models.{k}` must be a model id with no space"
            ));
        }
    }
    if s.budget_usd == Some(0) {
        return Err("`cloud.budget_usd` must be at least 1; leave it out for no budget".into());
    }
    Ok(())
}

/// Check a key before it is stored. The error never shows the key.
pub fn check_key(kind: Kind, key: &str, key_id: Option<&str>) -> Result<(), String> {
    match kind {
        Kind::Gcp => {
            let v: serde_json::Value = serde_json::from_str(key)
                .map_err(|_| "the gcp key must be the service-account JSON".to_owned())?;
            if !v.get("type").is_some_and(serde_json::Value::is_string) {
                return Err(
                    "the gcp key must be a JSON object with `type` (service_account)".into(),
                );
            }
        }
        Kind::Bedrock | Kind::Foundry if !word_ok(key) => {
            return Err(format!(
                "the {} key has a space or a line break",
                kind.as_str()
            ));
        }
        _ => {}
    }
    match (kind, key_id) {
        (Kind::Bedrock, Some(id)) if !word_ok(id) => {
            Err("`key_id` (the AWS access key ID) has a space".into())
        }
        (Kind::Gcp | Kind::Foundry, Some(_)) => Err(format!(
            "`key_id` is for bedrock only, not {}",
            kind.as_str()
        )),
        _ => Ok(()),
    }
}

/// The env of Claude Code on a cloud subscription.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CloudEnv {
    /// Env vars. Some values are secrets (the keys).
    pub vars: Vec<(String, String)>,
    /// Files to write in the sandbox with mode 0600: the var gets the
    /// path of the file. The content is a secret.
    pub files: Vec<(String, String)>,
}

/// The `ANTHROPIC_DEFAULT_*_MODEL` vars of the model map: one for each of
/// the keys `opus`, `sonnet`, and `haiku`.
pub fn tier_env(models: &BTreeMap<String, String>) -> Vec<(String, String)> {
    [
        ("opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
        ("sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
        ("haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
    ]
    .into_iter()
    .filter_map(|(tier, var)| models.get(tier).map(|m| (var.to_owned(), m.clone())))
    .collect()
}

/// The cloud model id for the `model` of an agent: the model map entry of
/// that name (case does not count), else the model as it is (Claude Code
/// maps an alias with [`tier_env`], and a Claude id with its own table).
/// No model: `None`, and Claude Code takes its default (or the pinned
/// `opus`).
pub fn model_for(models: &BTreeMap<String, String>, model: Option<&str>) -> Option<String> {
    let m = model.map(str::trim).filter(|m| !m.is_empty())?;
    Some(
        models
            .get(&m.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| m.to_owned()),
    )
}

/// The model names that need no entry in the model map: Claude Code maps
/// them with [`tier_env`] or its own default for the cloud.
pub const ALIASES: [&str; 3] = ["opus", "sonnet", "haiku"];

/// Why an agent on cloud subscription `sub` cannot use `model`: a name
/// that is not an alias ([`ALIASES`]) and not a key of the model map. Such
/// a name would fail only at the first turn, with a cloud API error. No
/// model passes.
pub fn model_refusal(
    sub: &str,
    models: &BTreeMap<String, String>,
    model: Option<&str>,
) -> Option<String> {
    let m = model.map(str::trim).filter(|m| !m.is_empty())?;
    let key = m.to_ascii_lowercase();
    if ALIASES.contains(&key.as_str()) || models.contains_key(&key) {
        return None;
    }
    Some(format!(
        "model `{m}` is not in the model map of cloud subscription `{sub}`; add `cloud.models.{key}` \
         (the cloud model id), or use opus, sonnet, or haiku"
    ))
}

/// The env of Claude Code for a `kind` subscription with the settings `s`,
/// the stored `key`, and the Bedrock access key ID `key_id`.
pub fn claude_env(kind: Kind, s: &CloudSettings, key: &str, key_id: Option<&str>) -> CloudEnv {
    let mut vars: Vec<(String, String)> = vec![(kind.use_var().into(), "1".into())];
    let mut files = Vec::new();
    let mut add = |k: &str, v: &str| vars.push((k.to_owned(), v.to_owned()));
    match kind {
        Kind::Bedrock => {
            if let Some(r) = &s.region {
                add("AWS_REGION", r);
            }
            if let Some(u) = &s.base_url {
                add("ANTHROPIC_BEDROCK_BASE_URL", u);
            }
            match key_id {
                Some(id) => {
                    add(AWS_ACCESS_KEY_ID, id);
                    add(AWS_SECRET_ACCESS_KEY, key);
                }
                None => add(AWS_BEARER_TOKEN_BEDROCK, key),
            }
        }
        Kind::Gcp => {
            if let Some(r) = &s.region {
                add("CLOUD_ML_REGION", r);
            }
            if let Some(p) = &s.project_id {
                add("ANTHROPIC_VERTEX_PROJECT_ID", p);
            }
            if let Some(u) = &s.base_url {
                add("ANTHROPIC_VERTEX_BASE_URL", u);
            }
            files.push((GOOGLE_APPLICATION_CREDENTIALS.to_owned(), key.to_owned()));
        }
        Kind::Foundry => {
            if let Some(r) = &s.resource {
                add("ANTHROPIC_FOUNDRY_RESOURCE", r);
            }
            if let Some(u) = &s.base_url {
                add("ANTHROPIC_FOUNDRY_BASE_URL", u);
            }
            add(ANTHROPIC_FOUNDRY_API_KEY, key);
        }
    }
    vars.extend(tier_env(&s.models));
    CloudEnv { vars, files }
}

// ---- The monthly budget ----

/// The budget window of a month: like the OpenRouter credits window, in
/// USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetWindow {
    pub budget_usd: f64,
    pub used_usd: f64,
    pub remaining_usd: f64,
    /// 0 to 100.
    pub used_pct: f64,
}

impl BudgetWindow {
    /// The alert line ([`ALERT_PCT`]) is passed.
    pub fn alert(&self) -> bool {
        self.used_pct >= ALERT_PCT
    }

    /// The budget is used up: new turns on the subscription wait.
    pub fn exhausted(&self) -> bool {
        self.used_pct >= 100.0
    }
}

/// The window of `used_usd` spent against `budget_usd`.
pub fn budget_window(budget_usd: u32, used_usd: f64) -> BudgetWindow {
    let budget = f64::from(budget_usd);
    let used = if used_usd.is_finite() {
        used_usd.max(0.0)
    } else {
        0.0
    };
    let used_pct = if budget > 0.0 {
        (used / budget * 100.0).clamp(0.0, 100.0)
    } else {
        100.0
    };
    BudgetWindow {
        budget_usd: budget,
        used_usd: used,
        remaining_usd: (budget - used).max(0.0),
        used_pct,
    }
}

/// The month of `at`, `2026-11`: the key of the spend rows.
pub fn month_key(at: DateTime<Utc>) -> String {
    at.format("%Y-%m").to_string()
}

/// The start of the UTC month of `at`.
pub fn month_start(at: DateTime<Utc>) -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(at.year(), at.month(), 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map_or(at, |t| t.and_utc())
}

/// The start of the next UTC month: the reset of the budget window.
pub fn next_month_start(at: DateTime<Utc>) -> DateTime<Utc> {
    let (y, m) = if at.month() == 12 {
        (at.year() + 1, 1)
    } else {
        (at.year(), at.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map_or(at, |t| t.and_utc())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    fn models() -> BTreeMap<String, String> {
        [
            ("opus", "us.anthropic.claude-opus-5-5"),
            ("sonnet", "us.anthropic.claude-sonnet-4-6"),
            (
                "claude-opus-5-5",
                "arn:aws:bedrock:us-east-2:1:application-inference-profile/x",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    #[test]
    fn kinds_parse_and_are_for_claude_only() {
        assert_eq!(kind(" Bedrock "), Some(Kind::Bedrock));
        assert_eq!(kind("gcp"), Some(Kind::Gcp));
        assert_eq!(kind("foundry"), Some(Kind::Foundry));
        assert_eq!(kind("anthropic"), None);
        assert!(!is_cloud("openrouter"));
        assert_eq!(harness_refusal(&[HarnessId::Claude]), None);
        assert!(
            harness_refusal(&[HarnessId::Claude, HarnessId::Pi])
                .unwrap()
                .contains("pi")
        );
        // The dashboard logos (TER-642) use the same words and names.
        for (k, word, name) in [
            (Kind::Bedrock, "bedrock", "Amazon Bedrock"),
            (Kind::Gcp, "gcp", "Google Cloud Vertex AI"),
            (Kind::Foundry, "foundry", "Microsoft Foundry"),
        ] {
            assert_eq!((k.as_str(), k.display_name()), (word, name));
        }
    }

    #[test]
    fn bedrock_env_with_an_access_key_or_an_api_key() {
        let s = CloudSettings {
            region: Some("us-east-1".into()),
            models: models(),
            ..Default::default()
        };
        let env = claude_env(Kind::Bedrock, &s, "secret-1", Some("AKIA1"));
        assert_eq!(get(&env.vars, "CLAUDE_CODE_USE_BEDROCK"), Some("1"));
        assert_eq!(get(&env.vars, "AWS_REGION"), Some("us-east-1"));
        assert_eq!(get(&env.vars, "AWS_ACCESS_KEY_ID"), Some("AKIA1"));
        assert_eq!(get(&env.vars, "AWS_SECRET_ACCESS_KEY"), Some("secret-1"));
        assert_eq!(get(&env.vars, "AWS_BEARER_TOKEN_BEDROCK"), None);
        assert_eq!(
            get(&env.vars, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some("us.anthropic.claude-opus-5-5")
        );
        assert_eq!(get(&env.vars, "ANTHROPIC_DEFAULT_HAIKU_MODEL"), None);
        assert!(env.files.is_empty());
        assert!(get(&env.vars, "ANTHROPIC_API_KEY").is_none());
        let env = claude_env(Kind::Bedrock, &s, "bedrock-api-key", None);
        assert_eq!(
            get(&env.vars, "AWS_BEARER_TOKEN_BEDROCK"),
            Some("bedrock-api-key")
        );
        assert_eq!(get(&env.vars, "AWS_SECRET_ACCESS_KEY"), None);
        assert_eq!(Kind::Bedrock.key_var(true), "AWS_SECRET_ACCESS_KEY");
        assert_eq!(Kind::Bedrock.key_var(false), "AWS_BEARER_TOKEN_BEDROCK");
    }

    #[test]
    fn gcp_env_puts_the_json_in_a_file_not_in_a_var() {
        let s = CloudSettings {
            region: Some("global".into()),
            project_id: Some("proj-1".into()),
            models: vertex_models(),
            ..Default::default()
        };
        let json = r#"{"type":"service_account","private_key":"PK"}"#;
        let env = claude_env(Kind::Gcp, &s, json, None);
        assert_eq!(get(&env.vars, "CLAUDE_CODE_USE_VERTEX"), Some("1"));
        assert_eq!(get(&env.vars, "CLOUD_ML_REGION"), Some("global"));
        assert_eq!(
            get(&env.vars, "ANTHROPIC_VERTEX_PROJECT_ID"),
            Some("proj-1")
        );
        assert_eq!(
            env.files,
            [("GOOGLE_APPLICATION_CREDENTIALS".to_owned(), json.to_owned())]
        );
        assert!(env.vars.iter().all(|(_, v)| !v.contains("PK")));
        assert_eq!(s.models.len(), 3);
        for (tier, id) in [
            ("opus", "claude-opus-5-5"),
            ("sonnet", "claude-sonnet-4-6"),
            ("haiku", "claude-haiku-4-5@20251001"),
        ] {
            assert_eq!(model_for(&s.models, Some(tier)).as_deref(), Some(id));
        }
        for pair in tier_env(&s.models) {
            assert!(env.vars.contains(&pair));
        }
    }

    #[test]
    fn foundry_env_with_a_resource_or_a_base_url() {
        let s = CloudSettings {
            resource: Some("my-res".into()),
            ..Default::default()
        };
        let env = claude_env(Kind::Foundry, &s, "fk", None);
        assert_eq!(get(&env.vars, "CLAUDE_CODE_USE_FOUNDRY"), Some("1"));
        assert_eq!(get(&env.vars, "ANTHROPIC_FOUNDRY_RESOURCE"), Some("my-res"));
        assert_eq!(get(&env.vars, "ANTHROPIC_FOUNDRY_API_KEY"), Some("fk"));
        assert_eq!(get(&env.vars, "ANTHROPIC_FOUNDRY_BASE_URL"), None);
        let s = CloudSettings {
            base_url: Some("https://r.services.ai.azure.com/anthropic".into()),
            ..Default::default()
        };
        let env = claude_env(Kind::Foundry, &s, "fk", None);
        assert_eq!(get(&env.vars, "ANTHROPIC_FOUNDRY_RESOURCE"), None);
        assert_eq!(
            get(&env.vars, "ANTHROPIC_FOUNDRY_BASE_URL"),
            Some("https://r.services.ai.azure.com/anthropic")
        );
    }

    #[test]
    fn settings_are_checked_per_kind() {
        let ok = |k, s: CloudSettings| check(k, &s);
        assert!(ok(Kind::Bedrock, CloudSettings::default()).is_ok());
        assert!(
            ok(
                Kind::Bedrock,
                CloudSettings {
                    region: Some("us.east".into()),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .contains("region")
        );
        assert!(
            ok(Kind::Gcp, CloudSettings::default())
                .unwrap_err()
                .contains("project_id")
        );
        assert!(
            ok(Kind::Foundry, CloudSettings::default())
                .unwrap_err()
                .contains("resource")
        );
        let url = CloudSettings {
            resource: Some("r.services.ai.azure.com".into()),
            ..Default::default()
        };
        assert!(ok(Kind::Foundry, url).unwrap_err().contains("name alone"));
        let both = CloudSettings {
            resource: Some("r".into()),
            base_url: Some("https://x".into()),
            ..Default::default()
        };
        assert!(ok(Kind::Foundry, both).unwrap_err().contains("not both"));
        let http = CloudSettings {
            base_url: Some("http://x".into()),
            ..Default::default()
        };
        assert!(ok(Kind::Bedrock, http).unwrap_err().contains("https"));
        let zero = CloudSettings {
            budget_usd: Some(0),
            ..Default::default()
        };
        assert!(ok(Kind::Bedrock, zero).unwrap_err().contains("budget"));
        let mut m = BTreeMap::new();
        m.insert("Opus".to_owned(), "x".to_owned());
        let upper = CloudSettings {
            models: m,
            ..Default::default()
        };
        assert!(ok(Kind::Bedrock, upper).unwrap_err().contains("lowercase"));
    }

    #[test]
    fn keys_are_checked_without_showing_them() {
        let e = check_key(Kind::Gcp, "not json secret", None).unwrap_err();
        assert!(!e.contains("secret"), "{e}");
        assert!(check_key(Kind::Gcp, r#"{"type":"service_account"}"#, None).is_ok());
        assert!(check_key(Kind::Gcp, r#"{"x":1}"#, None).is_err());
        let e = check_key(Kind::Foundry, "a b-secret", None).unwrap_err();
        assert!(!e.contains("secret"), "{e}");
        assert!(check_key(Kind::Foundry, "k", Some("id")).is_err());
        assert!(check_key(Kind::Bedrock, "k", Some("AKIA")).is_ok());
    }

    #[test]
    fn the_model_map() {
        let m = models();
        assert_eq!(model_for(&m, None), None);
        assert_eq!(model_for(&m, Some(" ")), None);
        assert_eq!(
            model_for(&m, Some("opus")).as_deref(),
            Some("us.anthropic.claude-opus-5-5")
        );
        assert_eq!(
            model_for(&m, Some("Sonnet")).as_deref(),
            Some("us.anthropic.claude-sonnet-4-6")
        );
        assert_eq!(
            model_for(&m, Some("claude-opus-5-5")).as_deref(),
            Some("arn:aws:bedrock:us-east-2:1:application-inference-profile/x")
        );
        // Not in the map: as it is, and Claude Code maps it.
        assert_eq!(model_for(&m, Some("haiku")).as_deref(), Some("haiku"));
        assert_eq!(
            model_for(&m, Some("claude-sonnet-5")).as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(model_refusal("aws", &m, None), None);
        assert_eq!(model_refusal("aws", &m, Some("Haiku")), None);
        assert_eq!(model_refusal("aws", &m, Some("claude-opus-5-5")), None);
        let why = model_refusal("aws", &m, Some("claude-sonnet-5")).unwrap();
        assert!(why.contains("`aws`"), "{why}");
        assert!(why.contains("cloud.models.claude-sonnet-5"), "{why}");
        assert!(model_refusal("aws", &BTreeMap::new(), Some("opsu")).is_some());
        let env = tier_env(&m);
        assert_eq!(
            env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            [
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "ANTHROPIC_DEFAULT_SONNET_MODEL"
            ]
        );
    }

    #[test]
    fn the_budget_window() {
        let w = budget_window(100, 25.5);
        assert_eq!(w.used_usd, 25.5);
        assert_eq!(w.remaining_usd, 74.5);
        assert_eq!(w.used_pct, 25.5);
        assert!(!w.alert() && !w.exhausted());
        let w = budget_window(100, 80.0);
        assert!(w.alert() && !w.exhausted());
        let w = budget_window(50, 75.0);
        assert_eq!((w.used_pct, w.remaining_usd), (100.0, 0.0));
        assert!(w.exhausted());
        assert_eq!(budget_window(10, f64::NAN).used_usd, 0.0);
        assert_eq!(budget_window(10, -3.0).used_pct, 0.0);
    }

    #[test]
    fn months() {
        let at = DateTime::parse_from_rfc3339("2026-12-31T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(month_key(at), "2026-12");
        assert_eq!(month_start(at).to_rfc3339(), "2026-12-01T00:00:00+00:00");
        assert_eq!(
            next_month_start(at).to_rfc3339(),
            "2027-01-01T00:00:00+00:00"
        );
        let at = DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            next_month_start(at).to_rfc3339(),
            "2026-12-01T00:00:00+00:00"
        );
    }
}
