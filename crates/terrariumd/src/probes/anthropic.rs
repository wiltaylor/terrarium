//! Anthropic: the OAuth usage endpoint behind Claude Code's `/usage`.
//!
//! `GET https://api.anthropic.com/api/oauth/usage` with the Claude Code
//! OAuth access token (`<claude home>/.credentials.json`,
//! `claudeAiOauth.accessToken`) and `anthropic-beta: oauth-2025-04-20`.
//! The answer has one object per window: `five_hour` and `seven_day`, and
//! on some plans `seven_day_opus`, `seven_day_sonnet`,
//! `seven_day_oauth_apps`. Each has `utilization` (percent) and
//! `resets_at`. Verified 2026-10-24 (docs/compat.md, "Usage probes").
//!
//! The Ultra plan (the Max 20x plan before 2026-10-30) has no 5-hour
//! limit: `five_hour` is `null`. The `limits` list names each limit by
//! `kind`: `session` (5 hours), `weekly_all` (7 days, all models), and
//! `weekly_scoped` (7 days, one model in `scope.model.display_name`).
//! The probe reports the object windows, then each `limits` entry that
//! has no object window. `extra_usage` is a window (`extra`) only when it
//! is on and has a `utilization`. The other objects (code names such as
//! `iguana_necktie`) have no known meaning and are not windows.
//! Verified 2026-11-01 (`tests/fixtures/probes/anthropic_usage_ultra.json`).

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body, read_json};

pub const NAME: &str = "anthropic-oauth-usage";
pub const URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The windows to report, and their names.
const WINDOWS: [(&str, &str); 6] = [
    ("five_hour", "5h"),
    ("seven_day", "7d"),
    ("seven_day_opus", "7d_opus"),
    ("seven_day_sonnet", "7d_sonnet"),
    ("seven_day_oauth_apps", "7d_oauth_apps"),
    ("seven_day_cowork", "7d_cowork"),
];

pub struct AnthropicOauthUsage;

/// The window name of one `limits` entry, or `None` for a kind that the
/// probe does not know.
fn limit_window(l: &Value) -> Option<String> {
    match l.get("kind").and_then(Value::as_str)? {
        "session" => Some("5h".into()),
        "weekly_all" => Some("7d".into()),
        "weekly_scoped" => {
            let scope = l.get("scope")?;
            let name = scope
                .pointer("/model/display_name")
                .or_else(|| scope.pointer("/model/id"))
                .or_else(|| scope.pointer("/surface/display_name"))
                .or_else(|| scope.get("surface"))
                .and_then(Value::as_str)?;
            let slug: String = name
                .trim()
                .to_ascii_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            (!slug.is_empty()).then(|| format!("7d_{slug}"))
        }
        _ => None,
    }
}

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
        let mut out: Vec<QuotaWindow> = Vec::new();
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
        for l in v
            .get("limits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(window) = limit_window(l) else {
                continue;
            };
            let Some(pct) = l.get("percent").and_then(Value::as_f64) else {
                continue;
            };
            if out.iter().any(|w| w.window == window) {
                continue;
            }
            out.push(QuotaWindow {
                window,
                used_pct: pct,
                resets_at: l.get("resets_at").and_then(time),
                raw: l.clone(),
            });
        }
        if let Some(x) = v
            .get("extra_usage")
            .filter(|x| x.get("is_enabled").and_then(Value::as_bool) == Some(true))
            && let Some(pct) = x.get("utilization").and_then(Value::as_f64)
        {
            out.push(QuotaWindow {
                window: "extra".into(),
                used_pct: pct,
                resets_at: x.get("resets_at").and_then(time),
                raw: x.clone(),
            });
        }
        if out.is_empty() {
            return Err(ProbeError::Parse(
                "no usage window (five_hour, seven_day, or limits) in the answer".into(),
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
    /// An Ultra plan, 2026-11-01: no 5-hour window.
    const ULTRA: &str = include_str!("../../tests/fixtures/probes/anthropic_usage_ultra.json");

    fn names(w: &[QuotaWindow]) -> Vec<(&str, f64)> {
        w.iter().map(|w| (w.window.as_str(), w.used_pct)).collect()
    }

    #[test]
    fn parses_the_ultra_answer() {
        let w = AnthropicOauthUsage.parse(ULTRA).unwrap();
        assert_eq!(names(&w), vec![("7d", 5.0), ("7d_fable", 0.0)]);
        assert_eq!(
            w[0].resets_at.unwrap().to_rfc3339(),
            "2026-11-07T02:00:00.937522+00:00"
        );
        assert_eq!(w[1].raw["kind"], "weekly_scoped");
        assert!(!w.iter().any(|w| w.window == "5h"));
    }

    #[test]
    fn maps_the_limits_list_and_extra_usage() {
        let body = r#"{"five_hour": null, "seven_day": null,
            "extra_usage": {"is_enabled": true, "utilization": 12.5, "monthly_limit": 5000},
            "limits": [
              {"kind": "session", "percent": 40, "resets_at": "2026-11-01T05:00:00+00:00"},
              {"kind": "weekly_all", "percent": 9},
              {"kind": "weekly_scoped", "percent": 3, "scope": {"model": {"display_name": "Claude Opus"}}},
              {"kind": "weekly_scoped", "percent": 1, "scope": null},
              {"kind": "monthly_new_thing", "percent": 50}
            ]}"#;
        let w = AnthropicOauthUsage.parse(body).unwrap();
        assert_eq!(
            names(&w),
            vec![
                ("5h", 40.0),
                ("7d", 9.0),
                ("7d_claude_opus", 3.0),
                ("extra", 12.5)
            ]
        );
        // Off: no window.
        let off = r#"{"seven_day": {"utilization": 1.0},
            "extra_usage": {"is_enabled": false, "utilization": 3.0}}"#;
        assert_eq!(
            names(&AnthropicOauthUsage.parse(off).unwrap()),
            vec![("7d", 1.0)]
        );
    }

    #[test]
    fn parses_the_recorded_answer() {
        let w = AnthropicOauthUsage.parse(FIXTURE).unwrap();
        // The objects, then the scoped weekly limit from `limits`.
        assert_eq!(w.len(), 3);
        assert_eq!((w[2].window.as_str(), w[2].used_pct), ("7d_fable", 3.0));
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
        assert_eq!(w.len(), 3);
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
