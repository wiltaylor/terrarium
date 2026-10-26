//! Z.ai: the GLM Coding Plan quota endpoint.
//!
//! `GET https://api.z.ai/api/monitor/usage/quota/limit` with the stored
//! Z.ai API key (`POST /api/subscriptions/{name}/api-key`) in
//! `Authorization: Bearer <key>`. Z.ai does not document the endpoint;
//! its own `glm-plan-usage` plugin and third-party tools call it.
//! Verified live 2026-10-26 (docs/compat.md, "Z.ai usage").
//!
//! The answer is `{code, msg, success, data: {level, limits: [...]}}`.
//! Each limit has `type`, a period of `number` x `unit` (unit 3 = hours,
//! 5 = months, 6 = weeks), `percentage`, and, when the period has started,
//! `usage` (the limit), `currentValue`, `remaining`, and `nextResetTime`
//! (epoch ms). `TOKENS_LIMIT` and `CREDIT_LIMIT` are the prompt windows
//! (5 hours, and weekly on some plans). `TIME_LIMIT` is the monthly budget
//! of the MCP tools (web search, web reader, zread). A used-up tool budget
//! does not stop chat, so it is not a window: the quota policy would pause
//! the agents on it. Its numbers stay in the `raw` of the first window,
//! under `other_limits`.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body};

pub const NAME: &str = "zai-usage";
pub const URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";

/// The limit types that are prompt windows.
const WINDOW_TYPES: [&str; 2] = ["TOKENS_LIMIT", "CREDIT_LIMIT"];

pub struct ZaiUsage;

/// The window name of a period of `number` x `unit`.
pub fn window_label(unit: i64, number: i64) -> String {
    match (unit, number) {
        (3, n) => format!("{n}h"),
        (6, 1) => "7d".into(),
        (6, n) => format!("{}d", n * 7),
        (5, 1) => "monthly".into(),
        (5, n) => format!("{n}mo"),
        (u, n) => format!("unit{u}x{n}"),
    }
}

fn reset_time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_i64()
        .filter(|ms| *ms > 0)
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
}

/// The error in an answer whose `success` is false. A refused key is a
/// 401, so the subscription shows as expired.
fn answer_error(v: &Value) -> ProbeError {
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
    let msg = v.get("msg").and_then(Value::as_str).unwrap_or("no message");
    if code == 401 {
        return ProbeError::Status {
            status: 401,
            url: URL.into(),
        };
    }
    ProbeError::Parse(format!("Z.ai answered code {code}: {msg}"))
}

impl QuotaProbe for ZaiUsage {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Unused: the probe reads the stored key, not a harness home.
    fn harness(&self) -> HarnessId {
        HarnessId::Claude
    }

    fn credentials(&self, _home: &Path) -> Result<Credentials, ProbeError> {
        Err(ProbeError::NoCredential(
            "no stored Z.ai key; POST /api/subscriptions/{name}/api-key".into(),
        ))
    }

    fn accepts_api_key(&self) -> bool {
        true
    }

    fn request(&self, creds: &Credentials) -> HttpRequest {
        HttpRequest {
            url: URL.into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", creds.token())),
                ("Accept".into(), "application/json".into()),
            ],
        }
    }

    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v = json_body(body)?;
        let failed = v.get("success").and_then(Value::as_bool) == Some(false)
            || v.get("code")
                .and_then(Value::as_i64)
                .is_some_and(|c| c != 200);
        if failed {
            return Err(answer_error(&v));
        }
        let data = v
            .get("data")
            .ok_or_else(|| ProbeError::Parse("no data in the answer".into()))?;
        let level = data.get("level").cloned().unwrap_or(Value::Null);
        let limits = data
            .get("limits")
            .and_then(Value::as_array)
            .ok_or_else(|| ProbeError::Parse("no data.limits in the answer".into()))?;
        let mut out = Vec::new();
        let mut others = Vec::new();
        for l in limits {
            let kind = l.get("type").and_then(Value::as_str).unwrap_or("");
            let pct = l.get("percentage").and_then(Value::as_f64);
            let (true, Some(pct)) = (WINDOW_TYPES.contains(&kind), pct) else {
                others.push(l.clone());
                continue;
            };
            let unit = l.get("unit").and_then(Value::as_i64).unwrap_or(0);
            let number = l.get("number").and_then(Value::as_i64).unwrap_or(0);
            let mut raw = l.clone();
            if let Some(o) = raw.as_object_mut() {
                o.insert("level".into(), level.clone());
            }
            out.push(QuotaWindow {
                window: window_label(unit, number),
                used_pct: pct,
                resets_at: l.get("nextResetTime").and_then(reset_time),
                raw,
            });
        }
        let Some(first) = out.first_mut() else {
            return Err(ProbeError::Parse(
                "no TOKENS_LIMIT or CREDIT_LIMIT window in the answer".into(),
            ));
        };
        if !others.is_empty()
            && let Some(o) = first.raw.as_object_mut()
        {
            o.insert("other_limits".into(), json!(others));
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::probes::tests::FakeHttp;

    const FIXTURE: &str = include_str!("../../tests/fixtures/probes/zai_quota_limit.json");

    /// A Pro plan answer posted in getpaseo/paseo#5057 (2026-09-18), with a
    /// weekly window added.
    const CREDIT: &str = r#"{"code":200,"success":true,"data":{"level":"pro","limits":[
        {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":12000,"currentValue":402,
         "remaining":11615,"percentage":3,"nextResetTime":1789757854621},
        {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":60000,"currentValue":30000,
         "remaining":30000,"percentage":50,"nextResetTime":1790000000000}]}}"#;

    #[test]
    fn zai_parses_the_recorded_answer() {
        let w = ZaiUsage.parse(FIXTURE).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("5h", 0.0));
        // The window has not started: no reset time.
        assert_eq!(w[0].resets_at, None);
        assert_eq!(w[0].raw["level"], "max");
        // The MCP tool budget is kept in raw, not as a window.
        let other = &w[0].raw["other_limits"][0];
        assert_eq!(other["type"], "TIME_LIMIT");
        assert_eq!(other["usage"], 4000);
    }

    #[test]
    fn zai_parses_credit_windows() {
        let w = ZaiUsage.parse(CREDIT).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("5h", 3.0));
        assert_eq!(
            w[0].resets_at.unwrap().timestamp_millis(),
            1_789_757_854_621
        );
        assert_eq!((w[1].window.as_str(), w[1].used_pct), ("7d", 50.0));
        assert_eq!(w[0].raw["currentValue"], 402);
        assert!(w[0].raw.get("other_limits").is_none());
    }

    #[test]
    fn zai_window_labels() {
        assert_eq!(window_label(3, 5), "5h");
        assert_eq!(window_label(6, 1), "7d");
        assert_eq!(window_label(6, 2), "14d");
        assert_eq!(window_label(5, 1), "monthly");
        assert_eq!(window_label(9, 2), "unit9x2");
    }

    #[test]
    fn zai_maps_error_answers() {
        assert!(matches!(
            ZaiUsage.parse(r#"{"code":1001,"msg":"bad token","success":false}"#),
            Err(ProbeError::Parse(m)) if m.contains("1001") && m.contains("bad token")
        ));
        assert_eq!(
            ZaiUsage.parse(r#"{"code":401,"msg":"token expired","success":false}"#),
            Err(ProbeError::Status {
                status: 401,
                url: URL.into()
            })
        );
        assert!(ZaiUsage.parse("<html>").is_err());
        assert!(
            ZaiUsage
                .parse(r#"{"code":200,"data":{"limits":[]}}"#)
                .is_err()
        );
        // Only the MCP budget: no prompt window.
        assert!(
            ZaiUsage
                .parse(r#"{"code":200,"data":{"limits":[{"type":"TIME_LIMIT","unit":5,"number":1,"percentage":10}]}}"#)
                .is_err()
        );
    }

    #[tokio::test]
    async fn zai_sends_the_bearer_key() {
        let http = FakeHttp::with(URL, 200, FIXTURE);
        let w = ZaiUsage.run(&http, &Credentials::new("k")).await.unwrap();
        assert_eq!(w.len(), 1);
        let auth = {
            let seen = http.seen.lock().unwrap();
            seen[0]
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer k")
        };
        assert!(auth);
        let http = FakeHttp::with(URL, 401, "{}");
        assert!(matches!(
            ZaiUsage.run(&http, &Credentials::new("k")).await,
            Err(ProbeError::Status { status: 401, .. })
        ));
    }

    #[test]
    fn zai_reads_only_a_stored_key() {
        assert!(ZaiUsage.accepts_api_key());
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            ZaiUsage.credentials(tmp.path()),
            Err(ProbeError::NoCredential(_))
        ));
    }
}
