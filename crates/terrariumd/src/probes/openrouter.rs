//! OpenRouter: the credit balance of the account.
//!
//! `GET https://openrouter.ai/api/v1/credits` with the stored OpenRouter
//! key (`POST /api/subscriptions/{name}/api-key`) in `Authorization: Bearer
//! <key>`. A normal key may read it; it needs no management key. The answer
//! is `{data: {total_credits, total_usage}}` in US dollars. Verified live
//! 2026-10-26 (docs/compat.md, "OpenRouter credits").
//!
//! Credits do not reset, so the one window, `credits`, has no reset time.
//! `used_pct` is the share of the bought credits that is used; at 100 the
//! account has no money left, and the quota policy acts on it as on a
//! full window. `raw` keeps the dollar amounts and `remaining`.

use std::path::Path;

use serde_json::{Value, json};
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body};

pub const NAME: &str = "openrouter-credits";
pub const URL: &str = "https://openrouter.ai/api/v1/credits";

pub struct OpenRouterCredits;

impl QuotaProbe for OpenRouterCredits {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Unused: the probe reads the stored key, not a harness home.
    fn harness(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn credentials(&self, _home: &Path) -> Result<Credentials, ProbeError> {
        Err(ProbeError::NoCredential(
            "no stored OpenRouter key; POST /api/subscriptions/{name}/api-key".into(),
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
        let data = v.get("data").unwrap_or(&v);
        let num = |k: &str| {
            data.get(k)
                .and_then(Value::as_f64)
                .ok_or_else(|| ProbeError::Parse(format!("no data.{k} in the answer")))
        };
        let (total, used) = (num("total_credits")?, num("total_usage")?);
        let used_pct = if total > 0.0 {
            (used / total * 100.0).clamp(0.0, 100.0)
        } else if used > 0.0 {
            100.0
        } else {
            0.0
        };
        Ok(vec![QuotaWindow {
            window: "credits".into(),
            used_pct,
            resets_at: None,
            raw: json!({
                "total_credits": total,
                "total_usage": used,
                "remaining": (total - used).max(0.0),
                "currency": "USD",
            }),
        }])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::probes::tests::FakeHttp;

    /// The shape of the live answer (2026-10-26), other amounts.
    const BODY: &str = r#"{"data":{"total_credits":60,"total_usage":45}}"#;

    #[test]
    fn openrouter_parses_the_balance() {
        let w = OpenRouterCredits.parse(BODY).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("credits", 75.0));
        assert_eq!(w[0].resets_at, None);
        assert_eq!(w[0].raw["remaining"], 15.0);
        assert_eq!(w[0].raw["total_credits"], 60.0);
    }

    #[test]
    fn openrouter_edge_amounts() {
        let pct = |b: &str| OpenRouterCredits.parse(b).unwrap()[0].used_pct;
        assert_eq!(pct(r#"{"data":{"total_credits":0,"total_usage":0}}"#), 0.0);
        assert_eq!(
            pct(r#"{"data":{"total_credits":0,"total_usage":1.5}}"#),
            100.0
        );
        // Usage past the credits (a negative balance) shows as full.
        let w = OpenRouterCredits
            .parse(r#"{"data":{"total_credits":10,"total_usage":12}}"#)
            .unwrap();
        assert_eq!(
            (w[0].used_pct, &w[0].raw["remaining"]),
            (100.0, &json!(0.0))
        );
        assert!(OpenRouterCredits.parse(r#"{"data":{}}"#).is_err());
        assert!(OpenRouterCredits.parse("<html>").is_err());
    }

    #[tokio::test]
    async fn openrouter_sends_the_bearer_key() {
        let http = FakeHttp::with(URL, 200, BODY);
        let w = OpenRouterCredits
            .run(&http, &Credentials::new("k"))
            .await
            .unwrap();
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
            OpenRouterCredits.run(&http, &Credentials::new("k")).await,
            Err(ProbeError::Status { status: 401, .. })
        ));
    }
}
