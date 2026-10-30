//! Moonshot AI (Kimi Open Platform): the account balance.
//!
//! `GET https://api.moonshot.ai/v1/users/me/balance` (global, US dollars)
//! or `GET https://api.moonshot.cn/v1/users/me/balance` (China, yuan) with
//! the stored Moonshot key (`POST /api/subscriptions/{name}/api-key`) in
//! `Authorization: Bearer <key>`. Documented at
//! <https://platform.kimi.ai/docs/api/balance> and
//! <https://platform.kimi.com/docs/api/balance>. Not verified live: no key
//! was available (`docs/compat.md`, "Moonshot").
//!
//! The answer is `{code, data: {available_balance, voucher_balance,
//! cash_balance}, scode, status}`; `code` 0 is success. The docs say that
//! at an `available_balance` of 0 or less the inference API refuses calls.
//!
//! Only the Open Platform plans (`api`, `api-cn`) get this probe. The Kimi
//! Code plans (`coding`, `coding-cn`) get `kimi-code-usage`
//! ([`super::kimi`]).
//!
//! A balance has no total, so the one window, `balance`, is 0 % used while
//! money is left and 100 % at 0 or less. The quota policy then acts on it
//! as on a full window. It has no reset time. `raw` keeps the amounts and
//! the currency.

use std::path::Path;

use serde_json::{Value, json};
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body};

/// The probe of the global Open Platform (`api` plan).
pub const NAME: &str = "moonshot-balance";
pub const URL: &str = "https://api.moonshot.ai/v1/users/me/balance";
/// The probe of the China Open Platform (`api-cn` plan).
pub const NAME_CN: &str = "moonshot-cn-balance";
pub const URL_CN: &str = "https://api.moonshot.cn/v1/users/me/balance";

/// The balance probe of one region.
pub struct MoonshotBalance {
    name: &'static str,
    url: &'static str,
    currency: &'static str,
}

/// The global probe.
pub const GLOBAL: MoonshotBalance = MoonshotBalance {
    name: NAME,
    url: URL,
    currency: "USD",
};
/// The China probe.
pub const CHINA: MoonshotBalance = MoonshotBalance {
    name: NAME_CN,
    url: URL_CN,
    currency: "CNY",
};

impl QuotaProbe for MoonshotBalance {
    fn name(&self) -> &'static str {
        self.name
    }

    /// Unused: the probe reads the stored key, not a harness home.
    fn harness(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn credentials(&self, _home: &Path) -> Result<Credentials, ProbeError> {
        Err(ProbeError::NoCredential(
            "no stored Moonshot key; POST /api/subscriptions/{name}/api-key".into(),
        ))
    }

    fn accepts_api_key(&self) -> bool {
        true
    }

    fn request(&self, creds: &Credentials) -> HttpRequest {
        HttpRequest {
            url: self.url.into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", creds.token())),
                ("Accept".into(), "application/json".into()),
            ],
        }
    }

    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v = json_body(body)?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 || v.get("status").and_then(Value::as_bool) == Some(false) {
            let scode = v.get("scode").and_then(Value::as_str).unwrap_or("none");
            return Err(ProbeError::Parse(format!(
                "Moonshot answered code {code} (scode {scode})"
            )));
        }
        let data = v
            .get("data")
            .ok_or_else(|| ProbeError::Parse("no data in the answer".into()))?;
        let num = |k: &str| data.get(k).and_then(Value::as_f64);
        let available = num("available_balance")
            .ok_or_else(|| ProbeError::Parse("no data.available_balance in the answer".into()))?;
        Ok(vec![QuotaWindow {
            window: "balance".into(),
            used_pct: if available > 0.0 { 0.0 } else { 100.0 },
            resets_at: None,
            raw: json!({
                "available_balance": available,
                "voucher_balance": num("voucher_balance"),
                "cash_balance": num("cash_balance"),
                "currency": self.currency,
            }),
        }])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::probes::tests::FakeHttp;

    /// The example answer of the docs.
    const BODY: &str = r#"{"code":0,"data":{"available_balance":49.58894,"voucher_balance":46.58893,"cash_balance":3.00001},"scode":"0x0","status":true}"#;

    #[test]
    fn moonshot_parses_the_documented_answer() {
        let w = GLOBAL.parse(BODY).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("balance", 0.0));
        assert_eq!(w[0].resets_at, None);
        assert_eq!(w[0].raw["available_balance"], 49.58894);
        assert_eq!(w[0].raw["cash_balance"], 3.00001);
        assert_eq!(w[0].raw["currency"], "USD");
        assert_eq!(CHINA.parse(BODY).unwrap()[0].raw["currency"], "CNY");
    }

    #[test]
    fn moonshot_no_money_is_a_full_window() {
        let pct = |b: &str| GLOBAL.parse(b).unwrap()[0].used_pct;
        assert_eq!(
            pct(
                r#"{"code":0,"data":{"available_balance":0,"voucher_balance":0,"cash_balance":0}}"#
            ),
            100.0
        );
        assert_eq!(
            pct(
                r#"{"code":0,"data":{"available_balance":-1.5,"voucher_balance":0,"cash_balance":-1.5}}"#
            ),
            100.0
        );
    }

    #[test]
    fn moonshot_maps_error_answers() {
        assert!(matches!(
            GLOBAL.parse(r#"{"code":1,"scode":"0x1","status":false}"#),
            Err(ProbeError::Parse(m)) if m.contains("code 1") && m.contains("0x1")
        ));
        assert!(GLOBAL.parse(r#"{"code":0,"data":{}}"#).is_err());
        assert!(GLOBAL.parse(r#"{"code":0}"#).is_err());
        assert!(GLOBAL.parse("<html>").is_err());
    }

    #[tokio::test]
    async fn moonshot_sends_the_bearer_key_to_its_region() {
        for (p, url) in [(&GLOBAL, URL), (&CHINA, URL_CN)] {
            let http = FakeHttp::with(url, 200, BODY);
            let w = p.run(&http, &Credentials::new("k")).await.unwrap();
            assert_eq!(w.len(), 1);
            let auth = {
                let seen = http.seen.lock().unwrap();
                seen[0].url == url
                    && seen[0]
                        .headers
                        .iter()
                        .any(|(k, v)| k == "Authorization" && v == "Bearer k")
            };
            assert!(auth, "{url}");
            let http = FakeHttp::with(url, 401, "{}");
            assert!(matches!(
                p.run(&http, &Credentials::new("k")).await,
                Err(ProbeError::Status { status: 401, .. })
            ));
        }
    }

    #[test]
    fn moonshot_reads_only_a_stored_key() {
        assert!(GLOBAL.accepts_api_key());
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            GLOBAL.credentials(tmp.path()),
            Err(ProbeError::NoCredential(_))
        ));
    }
}
