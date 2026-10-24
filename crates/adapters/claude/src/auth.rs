//! Login state for the Subscriptions page (PLAN §16).
//!
//! `claude auth status` prints JSON by default. The exit code is 0 when
//! logged in and 1 when not; both print JSON. The plan tier is not in that
//! output. `.credentials.json` has it as `claudeAiOauth.rateLimitTier`.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{ClaudeError, Result};

/// The output of `claude auth status`. `email`, `org_id`, and `org_name`
/// are `None` until the first interactive start writes `oauthAccount` into
/// `.claude.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub logged_in: bool,
    /// For example `claude.ai`, or `none` when logged out.
    #[serde(default)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub api_provider: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub org_id: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
    /// For example `max` or `pro`.
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub config_directory: Option<String>,
}

/// Parse the JSON from `claude auth status`.
pub fn parse_auth_status(stdout: &str) -> Result<AuthStatus> {
    serde_json::from_str(stdout.trim()).map_err(|e| ClaudeError::json("claude auth status", e))
}

/// Env var prefixes that a parent Claude Code session or an API key setup
/// can leave behind. They would change which login the child reads.
const STRIP_PREFIXES: [&str; 2] = ["CLAUDE", "ANTHROPIC"];

/// Run `<program> auth status` against `config_dir` and parse it. This
/// blocks for a few seconds.
pub fn auth_status(program: &str, config_dir: &Path) -> Result<AuthStatus> {
    let mut cmd = Command::new(program);
    cmd.args(["auth", "status"]);
    for (key, _) in std::env::vars_os() {
        if let Some(k) = key.to_str()
            && STRIP_PREFIXES.iter().any(|p| k.starts_with(p))
        {
            cmd.env_remove(&key);
        }
    }
    cmd.env("CLAUDE_CONFIG_DIR", config_dir);
    let out = cmd.output().map_err(|e| ClaudeError::Spawn {
        command: format!("{program} auth status"),
        source: e,
    })?;
    parse_auth_status(&String::from_utf8_lossy(&out.stdout))
}

/// The plan from `.credentials.json`. No token leaves this function.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanTier {
    /// For example `default_claude_max_20x`.
    pub rate_limit_tier: Option<String>,
    /// For example `max`.
    pub subscription_type: Option<String>,
}

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OauthPart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OauthPart {
    rate_limit_tier: Option<String>,
    subscription_type: Option<String>,
}

/// Parse the plan fields from the text of `.credentials.json`.
pub fn parse_plan_tier(text: &str) -> Result<Option<PlanTier>> {
    let c: Credentials =
        serde_json::from_str(text).map_err(|e| ClaudeError::json(".credentials.json", e))?;
    Ok(c.oauth.map(|o| PlanTier {
        rate_limit_tier: o.rate_limit_tier,
        subscription_type: o.subscription_type,
    }))
}

/// Read the plan tier from `<config_dir>/.credentials.json`. `None` when
/// the file or the OAuth block is not there.
pub fn plan_tier(config_dir: &Path) -> Result<Option<PlanTier>> {
    let path = config_dir.join(".credentials.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_plan_tier(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ClaudeError::io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From `spikes/claude/auth-status/captured/results.json`.
    const LOGGED_IN: &str = r#"{
  "loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
  "analyticsDisabled": true, "projectsDirectory": "$HOME/.claude/projects",
  "configDirectory": "$HOME/.claude", "email": "<email>", "orgId": "<redacted>",
  "orgName": "<redacted>", "subscriptionType": "max"
}"#;
    const COPIED: &str = r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
  "analyticsDisabled": true, "email": null, "orgId": null, "orgName": null, "subscriptionType": "max"}"#;
    const LOGGED_OUT: &str = r#"{"loggedIn": false, "authMethod": "none", "apiProvider": "firstParty",
  "analyticsDisabled": true, "projectsDirectory": "$CLAUDE_CONFIG_DIR/projects",
  "configDirectory": "$CLAUDE_CONFIG_DIR"}"#;

    #[test]
    fn auth_status_parses() {
        let a = parse_auth_status(LOGGED_IN).expect("in");
        assert!(a.logged_in);
        assert_eq!(a.email.as_deref(), Some("<email>"));
        assert_eq!(a.subscription_type.as_deref(), Some("max"));
        assert_eq!(a.config_directory.as_deref(), Some("$HOME/.claude"));
        let c = parse_auth_status(COPIED).expect("copied");
        assert!(c.logged_in && c.email.is_none() && c.org_name.is_none());
        let o = parse_auth_status(LOGGED_OUT).expect("out");
        assert!(!o.logged_in);
        assert_eq!(o.auth_method.as_deref(), Some("none"));
        assert!(parse_auth_status("Login method: Claude Max account").is_err());
    }

    #[test]
    fn plan_tier_reads_credentials_without_tokens() {
        let text = r#"{"claudeAiOauth": {"accessToken": "<redacted>", "refreshToken": "<redacted>",
            "expiresAt": 1, "scopes": [], "subscriptionType": "max",
            "rateLimitTier": "default_claude_max_20x"}}"#;
        assert_eq!(
            parse_plan_tier(text).expect("parse"),
            Some(PlanTier {
                rate_limit_tier: Some("default_claude_max_20x".into()),
                subscription_type: Some("max".into()),
            })
        );
        assert_eq!(parse_plan_tier("{}").expect("empty"), None);

        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(plan_tier(tmp.path()).expect("missing"), None);
        std::fs::write(tmp.path().join(".credentials.json"), text).expect("write");
        assert_eq!(
            plan_tier(tmp.path())
                .expect("read")
                .and_then(|p| p.rate_limit_tier)
                .as_deref(),
            Some("default_claude_max_20x")
        );
    }
}
