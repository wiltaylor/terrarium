//! Moonshot Kimi Code (the coding membership): the usage windows.
//!
//! `GET https://api.kimi.com/coding/v1/usages` with the stored Kimi Code
//! key (`sk-kimi-...`, `POST /api/subscriptions/{name}/api-key`) in
//! `Authorization: Bearer <key>`. Moonshot does not document the endpoint.
//! The official `kimi-cli` reads it for its `/usage` command
//! (`src/kimi_cli/ui/shell/usage.py`), at the base URL of its Kimi Code
//! platform (`src/kimi_cli/auth/platforms.py`). Verified live 2026-10-30
//! with an overseas key on `api.kimi.com` (`docs/compat.md`, "Kimi Code
//! usage"). The overseas chat host `api.kimi.ai` was not verified for
//! this path, so both plans (`coding`, `coding-cn`) use `api.kimi.com`.
//!
//! The answer has these keys, each optional:
//!
//! - `limits`: a list. Each item has `window: {duration, timeUnit}` (for
//!   example 300 x `TIME_UNIT_MINUTE`, the 5-hour window) and the numbers,
//!   in `detail` or in the item: `limit`, `used` or `remaining`, and a
//!   reset (`resetTime`, `reset_at`, `reset_time`, `resetAt`: an ISO time
//!   or epoch seconds or ms; or `reset_in`, `resetIn`: seconds from now).
//!   The numbers can be strings. With no `used`, used is `limit -
//!   remaining`.
//! - `usage`: the same numbers for the weekly window of the legacy plans.
//! - `usages`: `limit_5h`, `limit_7d`, `limit_month_total`,
//!   `limit_month_code`, each `{used_ratio (0 to 1), reset_time}`. The
//!   probe uses an entry only for a window that `limits` and `usage` do not
//!   give, so a window never comes twice.
//! - `booster_wallet` (older `boosterWallet`): the pay-as-you-go top-up. It
//!   is not a window. It stays in the `raw` of the first window, under
//!   `booster_wallet`.
//!
//! The tier: after the usage answer, the probe sends `GET
//! https://api.kimi.com/coding/v1/me` with the same key. The official
//! Kimi Code CLI reads it for the profile
//! (https://github.com/MoonshotAI/kimi-code,
//! `packages/oauth/src/managed-userinfo.ts`). Its `user_level_name` (for
//! example `Plus`) goes in the `raw` of each window as `level`, as the
//! Z.ai probe does, and the display name shows it (`Kimi Code Plus`).
//! Verified live 2026-11-01 with the Kimi Code key (docs/compat.md, "Kimi
//! Code usage"). The tier is best effort: a failed `/me` does not fail the
//! probe.

use std::path::Path;

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{Map, Value, json};
use terrarium_core::HarnessId;

use super::{
    Credentials, HttpGet, HttpRequest, ProbeError, QuotaProbe, QuotaWindow, json_body, window_name,
};
use crate::supervisor::BoxFuture;

pub const NAME: &str = "kimi-code-usage";
pub const URL: &str = "https://api.kimi.com/coding/v1/usages";
/// The profile, with the membership tier (`user_level_name`).
pub const ME_URL: &str = "https://api.kimi.com/coding/v1/me";

/// The prefix of a Kimi Code key (from third-party clients; Moonshot does
/// not document it).
const KEY_PREFIX: &str = "sk-kimi-";

/// The reset fields of an entry, in the order the probe reads them.
const RESET_AT: [&str; 4] = ["resetTime", "reset_at", "reset_time", "resetAt"];
const RESET_IN: [&str; 2] = ["reset_in", "resetIn"];

pub struct KimiCodeUsage;

/// A number, or a string that holds a number.
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// An epoch time: milliseconds above 10^12, else seconds.
fn epoch(n: f64) -> Option<DateTime<Utc>> {
    if n <= 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let ms = if n > 1e12 { n } else { n * 1000.0 } as i64;
    Utc.timestamp_millis_opt(ms).single()
}

/// The reset time of an entry, or `None`.
fn reset_time(o: &Map<String, Value>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    for k in RESET_AT {
        let at = match o.get(k) {
            Some(Value::String(s)) => DateTime::parse_from_rfc3339(s.trim())
                .map(|t| t.with_timezone(&Utc))
                .ok()
                .or_else(|| s.trim().parse().ok().and_then(epoch)),
            Some(Value::Number(n)) => n.as_f64().and_then(epoch),
            _ => None,
        };
        if at.is_some() {
            return at;
        }
    }
    RESET_IN.iter().find_map(|k| {
        #[allow(clippy::cast_possible_truncation)]
        num(o.get(*k)).map(|s| now + Duration::milliseconds((s * 1000.0) as i64))
    })
}

/// The window name of `{duration, timeUnit}`, or `None` for an unknown
/// unit.
fn window_label(w: &Value) -> Option<String> {
    let n = num(w.get("duration"))?;
    let unit = w
        .get("timeUnit")
        .or_else(|| w.get("time_unit"))
        .and_then(Value::as_str)?;
    let secs = match unit.trim_start_matches("TIME_UNIT_") {
        "SECOND" => 1.0,
        "MINUTE" => 60.0,
        "HOUR" => 3600.0,
        "DAY" => 86400.0,
        "WEEK" => 604_800.0,
        "MONTH" if n == 1.0 => return Some("monthly".into()),
        "MONTH" => return Some(format!("{n}mo")),
        _ => return None,
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(window_name((n * secs) as u64))
}

/// The window of one `limits` item or of `usage`, or `None` when it has
/// no limit.
fn counted(name: String, source: &str, item: &Value, now: DateTime<Utc>) -> Option<QuotaWindow> {
    let d = item.get("detail").filter(|d| d.is_object()).unwrap_or(item);
    let o = d.as_object()?;
    let limit = num(o.get("limit")).filter(|l| *l > 0.0)?;
    let remaining = num(o.get("remaining"));
    let used = num(o.get("used")).or_else(|| remaining.map(|r| limit - r))?;
    Some(QuotaWindow {
        window: name,
        used_pct: used / limit * 100.0,
        resets_at: reset_time(o, now).or_else(|| item.as_object().and_then(|i| reset_time(i, now))),
        raw: json!({"source": source, "limit": limit, "used": used, "remaining": remaining}),
    })
}

/// The window name of a `usages` key.
fn ratio_label(key: &str) -> String {
    match key {
        "limit_5h" => "5h".into(),
        "limit_7d" => "7d".into(),
        "limit_month_total" => "monthly".into(),
        "limit_month_code" => "monthly-code".into(),
        k => k.trim_start_matches("limit_").to_owned(),
    }
}

/// The order of the `usages` windows: the known ones first.
fn ratio_rank(key: &str) -> usize {
    [
        "limit_5h",
        "limit_7d",
        "limit_month_total",
        "limit_month_code",
    ]
    .iter()
    .position(|k| *k == key)
    .unwrap_or(usize::MAX)
}

/// What the operator must change after a non-2xx answer. `kimi_key` is
/// true when the key has the Kimi Code prefix.
fn refusal(status: u16, body: &str, kimi_key: bool) -> ProbeError {
    let wrong_kind = "a Kimi Code key (sk-kimi-..., from the Kimi Code Console) is needed; an Open \
                      Platform key (platform.kimi.ai) belongs on a moonshot subscription with plan \
                      `api` or `api-cn`";
    let hint = match status {
        401 if kimi_key => "Kimi Code refused the key: it is revoked or mistyped; store a new key \
                            with POST /api/subscriptions/{name}/api-key"
            .to_owned(),
        401 => format!("Kimi Code refused the key: {wrong_kind}"),
        403 if body.contains("\"cloudflare_error\"") => {
            "Cloudflare refused the request before Kimi Code read it; see docs/compat.md, \"Kimi \
             Code usage\""
                .to_owned()
        }
        403 if kimi_key => "the key has no Kimi Code access: the membership ended, or its tier \
                            has no coding quota"
            .to_owned(),
        403 => format!("the key has no Kimi Code access: {wrong_kind}"),
        404 => "no usage endpoint at this URL: Kimi Code moved it; see docs/compat.md, \"Kimi \
                Code usage\""
            .to_owned(),
        _ => {
            return ProbeError::Status {
                status,
                url: URL.into(),
            };
        }
    };
    ProbeError::StatusHint {
        status,
        url: URL.into(),
        hint,
    }
}

/// The `user_level_name` of a `/me` answer (`Plus`), or `None`.
pub fn level_of(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("user_level_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A request with the Kimi Code key.
fn bearer(url: &str, creds: &Credentials) -> HttpRequest {
    HttpRequest {
        url: url.into(),
        headers: vec![
            ("Authorization".into(), format!("Bearer {}", creds.token())),
            ("Accept".into(), "application/json".into()),
        ],
    }
}

impl KimiCodeUsage {
    /// Parse an answer read at `now` (for `reset_in`).
    pub fn parse_at(&self, body: &str, now: DateTime<Utc>) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v = json_body(body)?;
        if !v.is_object() {
            return Err(ProbeError::Parse("the answer is not an object".into()));
        }
        let mut out: Vec<QuotaWindow> = Vec::new();
        let push = |w: QuotaWindow, out: &mut Vec<QuotaWindow>| {
            if !out.iter().any(|o| o.window == w.window) {
                out.push(w);
            }
        };
        for item in v
            .get("limits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = item.get("window").and_then(window_label) else {
                continue;
            };
            if let Some(w) = counted(name, "limits", item, now) {
                push(w, &mut out);
            }
        }
        if let Some(u) = v.get("usage") {
            let name = u
                .get("window")
                .and_then(window_label)
                .unwrap_or_else(|| "7d".into());
            if let Some(w) = counted(name, "usage", u, now) {
                push(w, &mut out);
            }
        }
        let mut ratios: Vec<(&String, &Value)> = v
            .get("usages")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .collect();
        ratios.sort_by_key(|(k, _)| (ratio_rank(k), k.as_str()));
        for (k, e) in ratios {
            let (Some(o), Some(ratio)) = (e.as_object(), num(e.get("used_ratio"))) else {
                continue;
            };
            let w = QuotaWindow {
                window: ratio_label(k),
                used_pct: ratio * 100.0,
                resets_at: reset_time(o, now),
                raw: json!({"source": format!("usages.{k}"), "used_ratio": ratio}),
            };
            push(w, &mut out);
        }
        let Some(first) = out.first_mut() else {
            return Err(ProbeError::Parse(
                "no usage window in the answer (no limits, usage, or usages)".into(),
            ));
        };
        let wallet = v.get("booster_wallet").or_else(|| v.get("boosterWallet"));
        if let (Some(b), Some(o)) = (wallet, first.raw.as_object_mut()) {
            o.insert("booster_wallet".into(), b.clone());
        }
        Ok(out)
    }
}

impl QuotaProbe for KimiCodeUsage {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Unused: the probe reads the stored key, not a harness home.
    fn harness(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn credentials(&self, _home: &Path) -> Result<Credentials, ProbeError> {
        Err(ProbeError::NoCredential(
            "no stored Kimi Code key; POST /api/subscriptions/{name}/api-key".into(),
        ))
    }

    fn accepts_api_key(&self) -> bool {
        true
    }

    fn request(&self, creds: &Credentials) -> HttpRequest {
        bearer(URL, creds)
    }

    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        self.parse_at(body, Utc::now())
    }

    /// The default run, with a hint on a 401, 403, or 404, and then the
    /// tier from `/me` (best effort).
    fn run<'a>(
        &'a self,
        http: &'a dyn HttpGet,
        creds: &'a Credentials,
    ) -> BoxFuture<'a, Result<Vec<QuotaWindow>, ProbeError>> {
        Box::pin(async move {
            creds.check_fresh(Utc::now())?;
            let resp = http
                .get(self.request(creds))
                .await
                .map_err(ProbeError::Http)?;
            if !(200..300).contains(&resp.status) {
                let kimi_key = creds.token().trim().starts_with(KEY_PREFIX);
                return Err(refusal(resp.status, &resp.body, kimi_key));
            }
            let mut out = self.parse(&resp.body)?;
            if let Ok(me) = http.get(bearer(ME_URL, creds)).await
                && (200..300).contains(&me.status)
                && let Some(level) = level_of(&me.body)
            {
                for w in &mut out {
                    if let Some(o) = w.raw.as_object_mut() {
                        o.insert("level".into(), Value::String(level.clone()));
                    }
                }
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::probes::tests::FakeHttp;

    const FIXTURE: &str = include_str!("../../tests/fixtures/probes/kimi_code_usages.json");

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        at("2026-10-30T01:00:00Z")
    }

    fn names(w: &[QuotaWindow]) -> Vec<&str> {
        w.iter().map(|q| q.window.as_str()).collect()
    }

    #[test]
    fn kimi_parses_the_recorded_answer() {
        let w = KimiCodeUsage.parse_at(FIXTURE, now()).unwrap();
        // The 5h window comes from `limits`; `usages.limit_5h` adds no
        // second row.
        assert_eq!(names(&w), ["5h", "monthly", "monthly-code"]);
        assert_eq!(w[0].used_pct, 0.0);
        assert_eq!(w[0].resets_at, Some(at("2026-10-30T04:59:49.350524Z")));
        assert_eq!(w[0].raw["source"], "limits");
        assert_eq!(w[0].raw["limit"], 100.0);
        assert_eq!(w[0].raw["used"], 0.0);
        assert_eq!(w[1].resets_at, Some(at("2026-11-23T00:00:00Z")));
        assert_eq!(w[1].raw["source"], "usages.limit_month_total");
        assert!(w[0].raw.get("booster_wallet").is_none());
    }

    #[test]
    fn kimi_parses_the_older_shape() {
        // Numbers in the item (no `detail`), `used`, a weekly `usage`,
        // camelCase `boosterWallet`, and ratios that `limits` and `usage`
        // already give.
        let body = r#"{
            "usage": {"limit": 2000, "used": 500, "resetAt": 1793750400},
            "limits": [{"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"},
                        "limit": 200, "used": 50, "reset_in": 3600}],
            "usages": {"limit_5h": {"used_ratio": 0.9}, "limit_7d": {"used_ratio": 0.9}},
            "boosterWallet": {"status": "STATUS_ENABLED", "monthlyUsed": {"priceInCents": "120", "currency": "CNY"}}
        }"#;
        let w = KimiCodeUsage.parse_at(body, now()).unwrap();
        assert_eq!(names(&w), ["5h", "7d"]);
        assert_eq!(w[0].used_pct, 25.0);
        assert_eq!(w[0].resets_at, Some(at("2026-10-30T02:00:00Z")));
        assert_eq!(w[1].used_pct, 25.0);
        assert_eq!(w[1].raw["source"], "usage");
        assert_eq!(w[1].resets_at.unwrap().timestamp(), 1_793_750_400);
        assert_eq!(w[0].raw["booster_wallet"]["status"], "STATUS_ENABLED");
        assert_eq!(
            w[0].raw["booster_wallet"]["monthlyUsed"]["priceInCents"],
            "120"
        );
    }

    #[test]
    fn kimi_used_is_limit_minus_remaining() {
        let body = r#"{"limits": [{"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": "100", "remaining": "40", "reset_time": "1793750400000"}}],
            "booster_wallet": {"status": "STATUS_DISABLED"}}"#;
        let w = KimiCodeUsage.parse_at(body, now()).unwrap();
        assert_eq!(names(&w), ["5h"]);
        assert_eq!(w[0].used_pct, 60.0);
        assert_eq!(w[0].raw["used"], 60.0);
        assert_eq!(w[0].raw["remaining"], 40.0);
        // Epoch ms in a string.
        assert_eq!(w[0].resets_at.unwrap().timestamp(), 1_793_750_400);
        assert_eq!(w[0].raw["booster_wallet"]["status"], "STATUS_DISABLED");
    }

    #[test]
    fn kimi_falls_back_to_the_ratios() {
        let body = r#"{"usage": {}, "limits": [],
            "usages": {"limit_5h": {"used_ratio": 0.25, "reset_time": "2026-10-30T04:00:00Z"},
                       "limit_7d": {"used_ratio": "0.5"}, "limit_1d": {"used_ratio": 1}}}"#;
        let w = KimiCodeUsage.parse_at(body, now()).unwrap();
        assert_eq!(names(&w), ["5h", "7d", "1d"]);
        assert_eq!(w[0].used_pct, 25.0);
        assert_eq!(w[0].resets_at, Some(at("2026-10-30T04:00:00Z")));
        assert_eq!(w[1].used_pct, 50.0);
        assert_eq!(w[1].resets_at, None);
        assert_eq!(w[2].used_pct, 100.0);
    }

    #[test]
    fn kimi_window_labels() {
        let l = |d: u64, u: &str| window_label(&json!({"duration": d, "timeUnit": u}));
        assert_eq!(l(300, "TIME_UNIT_MINUTE").as_deref(), Some("5h"));
        assert_eq!(l(5, "TIME_UNIT_HOUR").as_deref(), Some("5h"));
        assert_eq!(l(7, "TIME_UNIT_DAY").as_deref(), Some("7d"));
        assert_eq!(l(1, "TIME_UNIT_WEEK").as_deref(), Some("7d"));
        assert_eq!(l(1, "TIME_UNIT_MONTH").as_deref(), Some("monthly"));
        assert_eq!(l(1, "TIME_UNIT_FORTNIGHT"), None);
        assert_eq!(ratio_label("limit_month_code"), "monthly-code");
    }

    #[test]
    fn kimi_refuses_an_answer_with_no_window() {
        assert!(KimiCodeUsage.parse("<html>").is_err());
        assert!(KimiCodeUsage.parse("[]").is_err());
        assert!(matches!(
            KimiCodeUsage.parse(r#"{"booster_wallet": {}}"#),
            Err(ProbeError::Parse(m)) if m.contains("no usage window")
        ));
        // A limit of 0 is no window.
        assert!(
            KimiCodeUsage
                .parse(r#"{"limits": [{"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"}, "limit": 0, "used": 0}]}"#)
                .is_err()
        );
    }

    #[tokio::test]
    async fn kimi_sends_the_bearer_key() {
        // No `/me` answer: the windows come with no level.
        let http = FakeHttp::with(URL, 200, FIXTURE);
        let w = KimiCodeUsage
            .run(&http, &Credentials::new("sk-kimi-k"))
            .await
            .unwrap();
        assert_eq!(w.len(), 3);
        assert!(w.iter().all(|q| q.raw.get("level").is_none()));
        let seen = http.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].url, ME_URL);
        for req in seen.iter() {
            assert!(
                req.headers
                    .iter()
                    .any(|(k, v)| k == "Authorization" && v == "Bearer sk-kimi-k")
            );
            // No user agent is spoofed.
            assert!(
                !req.headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            );
        }
    }

    /// The `/me` answer of a Plus membership, read 2026-11-01 (sanitised:
    /// no ids, nickname, avatar, email, or times).
    const ME: &str = include_str!("../../tests/fixtures/probes/kimi_code_me.json");

    #[tokio::test]
    async fn kimi_reads_the_tier_from_me() {
        let http = FakeHttp::with(URL, 200, FIXTURE);
        http.answers.lock().unwrap().insert(
            ME_URL.into(),
            crate::probes::HttpResponse {
                status: 200,
                body: ME.into(),
            },
        );
        let w = KimiCodeUsage
            .run(&http, &Credentials::new("sk-kimi-k"))
            .await
            .unwrap();
        assert!(w.iter().all(|q| q.raw["level"] == "Plus"));
        assert_eq!(
            terrarium_core::providers::display_name(
                "moonshot",
                Some("coding"),
                Some("Plus"),
                None,
                "k"
            ),
            "Kimi Code Plus"
        );
        // A refused `/me` does not fail the probe.
        http.answers.lock().unwrap().insert(
            ME_URL.into(),
            crate::probes::HttpResponse {
                status: 404,
                body: "{}".into(),
            },
        );
        let w = KimiCodeUsage
            .run(&http, &Credentials::new("sk-kimi-k"))
            .await
            .unwrap();
        assert!(w.iter().all(|q| q.raw.get("level").is_none()));
        assert_eq!(level_of(r#"{"user_level_name": ""}"#), None);
        assert_eq!(level_of("<html>"), None);
    }

    #[tokio::test]
    async fn kimi_maps_refusals_to_hints() {
        let run = |status: u16, body: &'static str, key: &'static str| async move {
            let http = FakeHttp::with(URL, status, body);
            KimiCodeUsage
                .run(&http, &Credentials::new(key))
                .await
                .unwrap_err()
        };
        let hint = |e: ProbeError| match e {
            ProbeError::StatusHint { hint, .. } => hint,
            e => panic!("{e:?}"),
        };
        let e = run(401, "{}", "sk-open-platform").await;
        assert!(matches!(e, ProbeError::StatusHint { status: 401, .. }));
        let h = hint(e);
        assert!(h.contains("sk-kimi-") && h.contains("`api`"), "{h}");
        let h = hint(run(401, "{}", "sk-kimi-x").await);
        assert!(h.contains("revoked"), "{h}");
        let h = hint(
            run(
                403,
                r#"{"error_code":1010,"cloudflare_error":true}"#,
                "sk-kimi-x",
            )
            .await,
        );
        assert!(h.contains("Cloudflare"), "{h}");
        let h = hint(run(403, "{}", "sk-kimi-x").await);
        assert!(h.contains("membership"), "{h}");
        let h = hint(run(403, "{}", "sk-x").await);
        assert!(h.contains("Open Platform"), "{h}");
        let h = hint(run(404, "{}", "sk-kimi-x").await);
        assert!(h.contains("moved"), "{h}");
        assert_eq!(
            run(500, "", "sk-kimi-x").await,
            ProbeError::Status {
                status: 500,
                url: URL.into()
            }
        );
        // The message never holds the key.
        assert!(
            !run(401, "{}", "sk-kimi-secret")
                .await
                .to_string()
                .contains("secret")
        );
    }

    #[test]
    fn kimi_reads_only_a_stored_key() {
        assert!(KimiCodeUsage.accepts_api_key());
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            KimiCodeUsage.credentials(tmp.path()),
            Err(ProbeError::NoCredential(_))
        ));
    }
}
