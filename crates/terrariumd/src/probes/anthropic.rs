//! Anthropic: the OAuth usage endpoint behind Claude Code's `/usage`.
//!
//! `GET https://api.anthropic.com/api/oauth/usage` with the Claude Code
//! OAuth access token (`<claude home>/.credentials.json`,
//! `claudeAiOauth.accessToken`) and `anthropic-beta: oauth-2025-04-20`.
//! The answer has one object per window: `five_hour` and `seven_day`, and
//! on some plans `seven_day_opus`, `seven_day_sonnet`,
//! `seven_day_oauth_apps`. Each has `utilization` (percent) and
//! `resets_at`. Verified 2026-10-24 (docs/compat.md, "Usage probes").

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body, read_json};

pub const NAME: &str = "anthropic-oauth-usage";
pub const URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The windows to report, and their names.
const WINDOWS: [(&str, &str); 5] = [
    ("five_hour", "5h"),
    ("seven_day", "7d"),
    ("seven_day_opus", "7d_opus"),
    ("seven_day_sonnet", "7d_sonnet"),
    ("seven_day_oauth_apps", "7d_oauth_apps"),
];

pub struct AnthropicOauthUsage;

/// Read the access token and its expiry from the text of
/// `.credentials.json`.
pub fn parse_credentials(v: &Value) -> Result<Credentials, ProbeError> {
    let oauth = v
        .get("claudeAiOauth")
        .ok_or_else(|| ProbeError::NoCredential("no claudeAiOauth in .credentials.json".into()))?;
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ProbeError::NoCredential("no OAuth access token".into()))?;
    let mut c = Credentials::new(token);
    c.expires_at = oauth
        .get("expiresAt")
        .and_then(Value::as_i64)
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single());
    Ok(c)
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

impl QuotaProbe for AnthropicOauthUsage {
    fn name(&self) -> &'static str {
        NAME
    }

    fn harness(&self) -> HarnessId {
        HarnessId::Claude
    }

    fn credentials(&self, home: &Path) -> Result<Credentials, ProbeError> {
        parse_credentials(&read_json(&home.join(".credentials.json"))?)
    }

    fn request(&self, creds: &Credentials) -> HttpRequest {
        HttpRequest {
            url: URL.into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", creds.token())),
                ("anthropic-beta".into(), "oauth-2025-04-20".into()),
                ("Accept".into(), "application/json".into()),
            ],
        }
    }

    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v = json_body(body)?;
        let mut out = Vec::new();
        for (key, window) in WINDOWS {
            let Some(w) = v.get(key).filter(|w| w.is_object()) else {
                continue;
            };
            let Some(pct) = w.get("utilization").and_then(Value::as_f64) else {
                continue;
            };
            out.push(QuotaWindow {
                window: window.into(),
                used_pct: pct,
                resets_at: w.get("resets_at").and_then(time),
                raw: w.clone(),
            });
        }
        if out.is_empty() {
            return Err(ProbeError::Parse(
                "no five_hour or seven_day window in the answer".into(),
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

    const FIXTURE: &str = include_str!("../../tests/fixtures/probes/anthropic_oauth_usage.json");

    #[test]
    fn parses_the_recorded_answer() {
        let w = AnthropicOauthUsage.parse(FIXTURE).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("5h", 16.0));
        assert_eq!(
            w[0].resets_at.unwrap().to_rfc3339(),
            "2026-10-24T10:59:59.514909+00:00"
        );
        assert_eq!((w[1].window.as_str(), w[1].used_pct), ("7d", 10.0));
        assert_eq!(w[1].raw["utilization"], 10.0);
        assert!(AnthropicOauthUsage.parse("{}").is_err());
        assert!(AnthropicOauthUsage.parse("<html>").is_err());
    }

    #[test]
    fn reads_credentials_from_the_home() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            AnthropicOauthUsage.credentials(tmp.path()),
            Err(ProbeError::NoCredential(_))
        ));
        std::fs::write(
            tmp.path().join(".credentials.json"),
            r#"{"claudeAiOauth": {"accessToken": "tok", "refreshToken": "r", "expiresAt": 4102444800000,
               "subscriptionType": "max", "rateLimitTier": "default_claude_max_20x"}}"#,
        )
        .unwrap();
        let c = AnthropicOauthUsage.credentials(tmp.path()).unwrap();
        assert_eq!(c.token(), "tok");
        assert_eq!(c.expires_at.unwrap().timestamp(), 4_102_444_800);
    }

    #[tokio::test]
    async fn run_sends_the_oauth_headers() {
        let http = FakeHttp::with(URL, 200, FIXTURE);
        let w = AnthropicOauthUsage
            .run(&http, &Credentials::new("tok"))
            .await
            .unwrap();
        assert_eq!(w.len(), 2);
        let seen = http.seen.lock().unwrap();
        let h = &seen[0].headers;
        assert!(
            h.iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer tok")
        );
        assert!(
            h.iter()
                .any(|(k, v)| k == "anthropic-beta" && v == "oauth-2025-04-20")
        );
    }
}
