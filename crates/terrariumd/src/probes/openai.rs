//! OpenAI Codex: the ChatGPT-plan rate-limit windows that Codex shows in
//! `/status`.
//!
//! `GET https://chatgpt.com/backend-api/wham/usage` with the Codex OAuth
//! access token and the `ChatGPT-Account-Id` header (both from
//! `<codex home>/auth.json`, `tokens.access_token` and `tokens.account_id`).
//! `rate_limit.primary_window` and `rate_limit.secondary_window` each have
//! `used_percent`, `limit_window_seconds`, and `reset_at` (Unix seconds). A
//! window can be `null`. Verified 2026-10-24 (docs/compat.md, "Usage
//! probes").

use std::path::Path;

use chrono::{TimeZone, Utc};
use serde_json::Value;
use terrarium_core::HarnessId;

use super::{
    Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body, read_json,
    window_name,
};

pub const NAME: &str = "openai-codex-usage";
pub const URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub struct OpenAiCodexUsage;

/// Read the token and the account id from the JSON of `auth.json`.
pub fn parse_credentials(v: &Value) -> Result<Credentials, ProbeError> {
    let tokens = v
        .get("tokens")
        .filter(|t| t.is_object())
        .ok_or_else(|| ProbeError::NoCredential("no ChatGPT tokens in auth.json".into()))?;
    let token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ProbeError::NoCredential("no access token in auth.json".into()))?;
    let mut c = Credentials::new(token);
    c.account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(c)
}

fn window(w: &Value) -> Option<QuotaWindow> {
    let pct = w.get("used_percent").and_then(Value::as_f64)?;
    let secs = w
        .get("limit_window_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(QuotaWindow {
        window: window_name(secs),
        used_pct: pct,
        resets_at: w
            .get("reset_at")
            .and_then(Value::as_i64)
            .and_then(|s| Utc.timestamp_opt(s, 0).single()),
        raw: w.clone(),
    })
}

impl QuotaProbe for OpenAiCodexUsage {
    fn name(&self) -> &'static str {
        NAME
    }

    fn harness(&self) -> HarnessId {
        HarnessId::Codex
    }

    fn credentials(&self, home: &Path) -> Result<Credentials, ProbeError> {
        parse_credentials(&read_json(&home.join("auth.json"))?)
    }

    fn request(&self, creds: &Credentials) -> HttpRequest {
        let mut headers = vec![
            ("Authorization".into(), format!("Bearer {}", creds.token())),
            ("Accept".into(), "application/json".into()),
        ];
        if let Some(a) = &creds.account_id {
            headers.push(("ChatGPT-Account-Id".into(), a.clone()));
        }
        HttpRequest {
            url: URL.into(),
            headers,
        }
    }

    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v = json_body(body)?;
        let rl = v
            .get("rate_limit")
            .filter(|r| r.is_object())
            .ok_or_else(|| ProbeError::Parse("no rate_limit in the answer".into()))?;
        let mut out: Vec<QuotaWindow> = ["primary_window", "secondary_window"]
            .iter()
            .filter_map(|k| rl.get(*k).and_then(window))
            .collect();
        // Two windows of one length would collide; name the second apart.
        if out.len() == 2 && out[0].window == out[1].window {
            out[1].window.push_str("_secondary");
        }
        if out.is_empty() {
            return Err(ProbeError::Parse(
                "no rate-limit window in the answer".into(),
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

    const ONE: &str = include_str!("../../tests/fixtures/probes/openai_wham_usage.json");
    const TWO: &str =
        include_str!("../../tests/fixtures/probes/openai_wham_usage_two_windows.json");

    #[test]
    fn parses_the_recorded_answer() {
        let w = OpenAiCodexUsage.parse(ONE).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("7d", 0.0));
        assert_eq!(w[0].resets_at.unwrap().timestamp(), 1_793_396_550);
    }

    #[test]
    fn parses_primary_and_secondary() {
        let w = OpenAiCodexUsage.parse(TWO).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("5h", 100.0));
        assert_eq!((w[1].window.as_str(), w[1].used_pct), ("7d", 41.5));
        assert!(
            OpenAiCodexUsage
                .parse(r#"{"rate_limit": {"primary_window": null}}"#)
                .is_err()
        );
        assert!(OpenAiCodexUsage.parse(r#"{"plan_type": "free"}"#).is_err());
    }

    #[tokio::test]
    async fn reads_auth_json_and_sends_the_account() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("auth.json"),
            r#"{"auth_mode": "chatgpt", "OPENAI_API_KEY": null,
               "tokens": {"id_token": "x", "access_token": "tok", "refresh_token": "r", "account_id": "acct"},
               "last_refresh": "2026-10-17T09:02:00Z"}"#,
        )
        .unwrap();
        let c = OpenAiCodexUsage.credentials(tmp.path()).unwrap();
        let http = FakeHttp::with(URL, 200, ONE);
        OpenAiCodexUsage.run(&http, &c).await.unwrap();
        let seen = http.seen.lock().unwrap();
        assert!(
            seen[0]
                .headers
                .iter()
                .any(|(k, v)| k == "ChatGPT-Account-Id" && v == "acct")
        );
        std::fs::write(tmp.path().join("auth.json"), r#"{"OPENAI_API_KEY": "sk"}"#).unwrap();
        assert!(OpenAiCodexUsage.credentials(tmp.path()).is_err());
    }
}
