//! Login state for the Subscriptions page (PLAN §16).
//!
//! `codex login status` prints one line to stderr, and nothing to stdout
//! (`spikes/codex/auth-status`):
//!
//! - logged in: `Logged in using ChatGPT`, exit 0;
//! - no login: `Not logged in`, exit 1.
//!
//! The plan is not in that output. `auth.json` has it as the claim
//! `https://api.openai.com/auth.chatgpt_plan_type` of `tokens.id_token`
//! (`pro`). The same value is in `token_count.rate_limits.plan_type` of
//! every turn ([`crate::usage`]).

use std::path::Path;
use std::process::Command;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CodexError, Result};

/// The result of `codex login status`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuthStatus {
    pub logged_in: bool,
    /// The text after "Logged in using ", for example `ChatGPT`. Only the
    /// ChatGPT form was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// The output, trimmed: stderr, else stdout.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Read the result of `codex login status`. Exit 0 with a "Logged in"
/// line is logged in. Anything else is not.
pub fn parse_auth_status(exit_code: Option<i32>, stdout: &str, stderr: &str) -> AuthStatus {
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    }
    .to_owned();
    let line = message
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("Logged in"))
        .map(str::trim);
    let logged_in = exit_code == Some(0) && line.is_some();
    let method = line
        .filter(|_| logged_in)
        .and_then(|l| l.strip_prefix("Logged in using "))
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());
    AuthStatus {
        logged_in,
        method,
        message,
        exit_code,
    }
}

/// Env var prefixes that a parent Codex session or an API key setup can
/// leave behind. They would change which login the child reads.
const STRIP_PREFIXES: [&str; 2] = ["CODEX", "OPENAI"];

/// Run `<program> login status` against `codex_home` and read it. This
/// blocks.
pub fn auth_status(program: &str, codex_home: &Path) -> Result<AuthStatus> {
    let mut cmd = Command::new(program);
    cmd.args(["login", "status"]);
    for (key, _) in std::env::vars_os() {
        if let Some(k) = key.to_str()
            && STRIP_PREFIXES.iter().any(|p| k.starts_with(p))
        {
            cmd.env_remove(&key);
        }
    }
    cmd.env("CODEX_HOME", codex_home);
    let out = cmd.output().map_err(|e| CodexError::Spawn {
        command: format!("{program} login status"),
        source: e,
    })?;
    Ok(parse_auth_status(
        out.status.code(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    ))
}

/// The plan from `auth.json`. No token leaves this function.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Plan {
    /// For example `chatgpt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    /// For example `pro`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    /// The ID-token claim `chatgpt_subscription_active_until`, as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_until: Option<String>,
}

/// The claims object of the OpenAI auth namespace.
const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// The payload of a JWT, without a signature check. The token is only
/// read for its plan claim.
fn jwt_payload(token: &str) -> Option<Value> {
    let part = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Parse the plan fields from the text of `auth.json`.
pub fn parse_plan(text: &str) -> Result<Option<Plan>> {
    let v: Value = serde_json::from_str(text).map_err(|e| CodexError::json("auth.json", e))?;
    let auth_mode = v
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let claims = v
        .get("tokens")
        .and_then(|t| t.get("id_token"))
        .and_then(Value::as_str)
        .and_then(jwt_payload);
    let auth = claims.as_ref().and_then(|c| c.get(AUTH_CLAIM));
    let claim = |k: &str| {
        auth.and_then(|a| a.get(k)).and_then(|x| match x {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        })
    };
    let plan = Plan {
        auth_mode,
        plan_type: claim("chatgpt_plan_type"),
        active_until: claim("chatgpt_subscription_active_until"),
    };
    Ok((plan != Plan::default()).then_some(plan))
}

/// Read the plan from `<codex_home>/auth.json`. `None` when the file is not
/// there or has no plan fields.
pub fn plan(codex_home: &Path) -> Result<Option<Plan>> {
    let path = codex_home.join("auth.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_plan(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CodexError::io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_login_status() {
        // From `spikes/codex/auth-status/captured/results.json`.
        let real = parse_auth_status(Some(0), "", "Logged in using ChatGPT");
        assert!(real.logged_in);
        assert_eq!(real.method.as_deref(), Some("ChatGPT"));
        assert_eq!(real.message, "Logged in using ChatGPT");
        let empty = parse_auth_status(Some(1), "", "Not logged in");
        assert!(!empty.logged_in);
        assert_eq!(empty.method, None);
        assert_eq!(empty.exit_code, Some(1));
        // A crash with the right text is still not a login.
        assert!(!parse_auth_status(Some(2), "", "Logged in using ChatGPT").logged_in);
        assert!(!parse_auth_status(None, "", "").logged_in);
        // A warning line before the status line.
        let warned = parse_auth_status(
            Some(0),
            "",
            "WARNING: proceeding, even though we could not create PATH aliases\nLogged in using an API key",
        );
        assert!(warned.logged_in);
        assert_eq!(warned.method.as_deref(), Some("an API key"));
    }

    fn jwt(claims: &Value) -> String {
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            e.encode(br#"{"alg":"RS256"}"#),
            e.encode(claims.to_string())
        )
    }

    #[test]
    fn plan_reads_the_id_token_claim() {
        let id = jwt(&serde_json::json!({
            "email": "<email>",
            "https://api.openai.com/auth": {"chatgpt_plan_type": "pro", "chatgpt_account_id": "<id>",
                         "chatgpt_subscription_active_until": "2026-11-17T00:00:00+00:00"}
        }));
        let text = serde_json::json!({
            "auth_mode": "chatgpt", "OPENAI_API_KEY": null, "last_refresh": "x",
            "tokens": {"id_token": id, "access_token": "<redacted>",
                       "refresh_token": "<redacted>", "account_id": "<id>"}
        })
        .to_string();
        assert_eq!(
            parse_plan(&text).expect("parse"),
            Some(Plan {
                auth_mode: Some("chatgpt".into()),
                plan_type: Some("pro".into()),
                active_until: Some("2026-11-17T00:00:00+00:00".into()),
            })
        );
        assert_eq!(parse_plan("{}").expect("empty"), None);
        assert!(parse_plan("not json").is_err());
        assert_eq!(
            parse_plan(r#"{"auth_mode":"apikey","tokens":{"id_token":"bad"}}"#)
                .expect("bad token")
                .and_then(|p| p.auth_mode)
                .as_deref(),
            Some("apikey")
        );

        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(plan(tmp.path()).expect("missing"), None);
        std::fs::write(tmp.path().join("auth.json"), &text).expect("write");
        assert_eq!(
            plan(tmp.path())
                .expect("read")
                .and_then(|p| p.plan_type)
                .as_deref(),
            Some("pro")
        );
    }
}
