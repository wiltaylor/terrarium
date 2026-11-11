//! LiteLLM virtual-key budget, from the subscription's own proxy only.
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::json;
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow};

pub const NAME: &str = "litellm-key-spend";
const NO_SPEND: &str = "no spend from LiteLLM";

pub struct LiteLlmKeySpend {
    url: String,
}

pub fn registered() -> LiteLlmKeySpend {
    LiteLlmKeySpend { url: String::new() }
}

impl LiteLlmKeySpend {
    pub fn new(base: &str) -> Result<Self, ProbeError> {
        let base = terrarium_core::selfhosted::check_http_url("base_url", base, false)
            .map_err(|_| unavailable())?;
        if !base.starts_with("https://") {
            return Err(unavailable());
        }
        Ok(Self {
            url: format!("{}/key/info", base.trim_end_matches('/')),
        })
    }
}

pub fn unavailable() -> ProbeError {
    ProbeError::Parse(NO_SPEND.into())
}

impl QuotaProbe for LiteLlmKeySpend {
    fn name(&self) -> &'static str {
        NAME
    }
    fn harness(&self) -> HarnessId {
        HarnessId::Pi
    }
    fn credentials(&self, _home: &Path) -> Result<Credentials, ProbeError> {
        Err(ProbeError::NoCredential(NO_SPEND.into()))
    }
    fn accepts_api_key(&self) -> bool {
        true
    }
    fn request(&self, creds: &Credentials) -> HttpRequest {
        HttpRequest {
            url: self.url.clone(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", creds.token())),
                ("Accept".into(), "application/json".into()),
            ],
        }
    }
    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v: serde_json::Value = serde_json::from_str(body).map_err(|_| unavailable())?;
        let info = &v["info"];
        let spend = info["spend"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(unavailable)?;
        let budget = info["max_budget"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(unavailable)?;
        let duration = info["budget_duration"].as_str();
        let resets_at = if duration.is_some_and(|s| !s.is_empty()) {
            Some(
                DateTime::parse_from_rfc3339(
                    info["budget_reset_at"].as_str().ok_or_else(unavailable)?,
                )
                .map_err(|_| unavailable())?
                .with_timezone(&Utc),
            )
        } else {
            None
        };
        let used_pct = if budget > 0.0 {
            (spend / budget * 100.0).clamp(0.0, 100.0)
        } else if spend > 0.0 {
            100.0
        } else {
            0.0
        };
        Ok(vec![QuotaWindow {
            window: "key-spend".into(),
            used_pct,
            resets_at,
            raw: json!({"spend": spend, "max_budget": budget,
                "budget_duration": duration, "currency": "USD"}),
        }])
    }
}

/// A separate client: no redirects, HTTPS only, and a 1 MiB response cap.
pub async fn fetch(
    probe: &LiteLlmKeySpend,
    creds: &Credentials,
) -> Result<Vec<QuotaWindow>, ProbeError> {
    creds.check_fresh(Utc::now())?;
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| unavailable())?;
    let mut response = client
        .get(&probe.url)
        .bearer_auth(creds.token())
        .send()
        .await
        .map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if body.len() + chunk.len() > 1024 * 1024 {
            return Err(unavailable());
        }
        body.extend_from_slice(&chunk);
    }
    probe.parse(std::str::from_utf8(&body).map_err(|_| unavailable())?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn budget_and_reset() {
        let p = LiteLlmKeySpend::new("https://proxy.example/prefix/").unwrap();
        let w = p.parse(r#"{"info":{"spend":2.5,"max_budget":10,"budget_duration":"1d","budget_reset_at":"2026-11-12T02:00:00+02:00","key":"must-not-store"}}"#).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].used_pct, 25.0);
        assert_eq!(
            w[0].resets_at.unwrap().to_rfc3339(),
            "2026-11-12T00:00:00+00:00"
        );
        assert_eq!(w[0].raw["spend"], 2.5);
        assert!(!w[0].raw.to_string().contains("must-not-store"));
        let req = p.request(&Credentials::new("test-key"));
        assert_eq!(req.url, "https://proxy.example/prefix/key/info");
        assert_eq!(req.headers[0].1, "Bearer test-key");
    }
    #[test]
    fn incomplete_and_invalid_answers() {
        let p = LiteLlmKeySpend::new("https://proxy.example").unwrap();
        for body in [
            "not JSON",
            "{}",
            r#"{"info":{"spend":null,"max_budget":10}}"#,
            r#"{"info":{"spend":-1,"max_budget":10}}"#,
            r#"{"info":{"spend":1,"max_budget":10,"budget_duration":"1d","budget_reset_at":"bad"}}"#,
        ] {
            assert_eq!(p.parse(body).unwrap_err(), unavailable());
        }
        for (spend, budget, pct) in [(0, 0, 0.0), (1, 0, 100.0), (12, 10, 100.0)] {
            let w = p
                .parse(&json!({"info":{"spend":spend,"max_budget":budget}}).to_string())
                .unwrap();
            assert_eq!(w[0].used_pct, pct);
            assert_eq!(w[0].resets_at, None);
        }
    }
    #[test]
    fn rejects_unsafe_urls() {
        for base in [
            "http://proxy.example",
            "https://user:password@proxy.example",
            "https://proxy.example?key=secret",
            "https://proxy.example/#secret",
        ] {
            assert!(LiteLlmKeySpend::new(base).is_err());
        }
    }
}
