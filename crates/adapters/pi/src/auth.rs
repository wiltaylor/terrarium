//! Login state for the Subscriptions page (PLAN §16.1).
//!
//! `pi auth check --provider <p> --json --no-refresh` prints one JSON line:
//! `{"status":"ready","provider":"…","authType":"oauth"|"api_key"}` with
//! exit code 0, or `{"status":"not_ready","provider":"…","reason":
//! "credentials_not_configured"}` with exit code 1.
//!
//! **`ready` means only that a credential is configured.** pi does not call
//! the provider. The spikes saw `ready` for a LAN server that was down and
//! for an Anthropic token that the API refused (`docs/compat.md`, "Auth
//! and token reuse"). [`AuthCheck::verified_with_provider`] is always
//! false to say so.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{PiError, Result};

/// The result of `pi auth check` for one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCheck {
    /// The pi provider id.
    pub provider: String,
    /// pi's `status` is `ready`: a credential for the provider is in
    /// `auth.json`, in `models.json`, or in the env. It does not say that
    /// the provider accepts it.
    pub credential_present: bool,
    /// `oauth` or `api_key`, when a credential is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    /// Why not, for example `credentials_not_configured`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Always false: `pi auth check` never calls the provider. A probe or a
    /// real turn is the only test that the credential works.
    pub verified_with_provider: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    status: String,
    provider: Option<String>,
    auth_type: Option<String>,
    reason: Option<String>,
}

/// Parse the JSON from `pi auth check --json`. `provider` fills in a
/// missing `provider` field.
pub fn parse_auth_check(stdout: &str, provider: &str) -> Result<AuthCheck> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or(stdout);
    let raw: Raw =
        serde_json::from_str(line.trim()).map_err(|e| PiError::json("pi auth check", e))?;
    Ok(AuthCheck {
        provider: raw.provider.unwrap_or_else(|| provider.to_owned()),
        credential_present: raw.status == "ready",
        auth_type: raw.auth_type,
        reason: raw.reason,
        verified_with_provider: false,
    })
}

/// Run `<program> auth check --provider <provider> --json --no-refresh`
/// against the pi home `agent_dir` (the daemon's view). The child starts
/// from an empty env plus `PATH`, `HOME`, the home, and `extra_env` (the
/// API key of the subscription, when it has one), so that a key in the
/// daemon's own env does not count. This blocks for about a second.
pub fn auth_check(
    program: &str,
    agent_dir: &Path,
    provider: &str,
    extra_env: &[(String, String)],
) -> Result<AuthCheck> {
    let mut cmd = Command::new(program);
    cmd.args([
        "auth",
        "check",
        "--provider",
        provider,
        "--json",
        "--no-refresh",
    ])
    .env_clear();
    for key in ["PATH", "HOME"] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    cmd.env(crate::PI_0_85.config_env, agent_dir)
        .env("PI_OFFLINE", "1")
        .env("PI_TELEMETRY", "0");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| PiError::Spawn {
        command: format!("{program} auth check --provider {provider}"),
        source: e,
    })?;
    parse_auth_check(&String::from_utf8_lossy(&out.stdout), provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_outputs_parse() {
        // From `spikes/pi/auth-status/captured/results.json`.
        let ready = parse_auth_check(
            r#"{"status":"ready","provider":"glm53-exl3","authType":"api_key"}"#,
            "x",
        )
        .expect("ready");
        assert_eq!(
            ready,
            AuthCheck {
                provider: "glm53-exl3".into(),
                credential_present: true,
                auth_type: Some("api_key".into()),
                reason: None,
                verified_with_provider: false,
            }
        );
        let not = parse_auth_check(
            "{\"status\":\"not_ready\",\"provider\":\"openai-codex\",\"reason\":\"credentials_not_configured\"}\n",
            "openai-codex",
        )
        .expect("not ready");
        assert!(!not.credential_present);
        assert_eq!(not.reason.as_deref(), Some("credentials_not_configured"));
        assert!(parse_auth_check("ready", "x").is_err());
    }

    /// Runs the real binary against an empty home.
    #[test]
    #[ignore = "needs the pi binary on PATH"]
    fn smoke_auth_check_empty_home() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::write(tmp.path().join("auth.json"), "{}").expect("auth");
        let c = auth_check("pi", tmp.path(), "openai-codex", &[]).expect("check");
        assert!(!c.credential_present);
        let c = auth_check(
            "pi",
            tmp.path(),
            "openrouter",
            &[("OPENROUTER_API_KEY".into(), "sk-test".into())],
        )
        .expect("check");
        assert!(c.credential_present);
        assert!(!c.verified_with_provider);
    }
}
