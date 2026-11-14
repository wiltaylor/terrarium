//! The harness version report (`GET /api/harnesses`).
//!
//! `harnesses.toml` pins each harness: the version of the first install
//! (`terrarium_core::harnesses`). The report has the version that each
//! binary reports (`Adapter::detect_version`), and with `?latest=true` the
//! latest release on npm. Terrarium keeps no range of supported versions:
//! the harnesses release too often. Any installed version can start an
//! agent, and the compatibility suite (`just ci::compat`) tests a new one.
//!
//! The setting `TERRARIUM_HARNESS_VERSION_POLICY` (`warn`, `refuse`) of
//! older releases has no effect. The daemon logs one line when it is set
//! ([`ENV_POLICY`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use terrarium_core::{Adapter, HarnessId};

/// The env var of the removed start policy. The daemon ignores it.
pub const ENV_POLICY: &str = "TERRARIUM_HARNESS_VERSION_POLICY";

/// How long a detected version stays in the cache. An update through the
/// API refreshes it at once; the TTL covers an `npm install -g` by hand in
/// the tools dir.
pub const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// One row of `GET /api/harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessVersion {
    pub harness: HarnessId,
    /// The npm package (`harnesses.toml`).
    pub package: String,
    /// The base version: the version of the first install
    /// (`harnesses.toml`). It is not a pin: the runtime pin is
    /// `auto_update.pinned_version`.
    pub base: String,
    /// Deprecated: the same value as `base`, for older clients. It is not
    /// the runtime pin.
    #[serde(default)]
    pub pinned: String,
    /// The version that the binary reports. `None` when it did not answer.
    pub installed: Option<String>,
    /// Why the installed version is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the daemon ran `<binary> --version`.
    pub checked_at: Option<DateTime<Utc>>,
    /// `?latest=true` only: the latest release on npm, `null` when npm did
    /// not answer (then `latest_error` says why). Absent without
    /// `latest=true`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub latest: Option<Option<Latest>>,
    /// `?latest=true` only: the bump from the installed version (else the
    /// pin) to `latest`, `null` when the latest is not known
    /// ([`update_kind`]).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub update: Option<Option<UpdateKind>>,
    /// `?latest=true` only: why the latest is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_error: Option<String>,
    /// The active version dir in the tools dir: the version that the next
    /// launch runs. Absent without a tools dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// The complete version dirs in the tools dir, oldest first. Absent
    /// without a tools dir.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<String>,
    /// The live agents on the harness and the version that each runs.
    /// Absent when none runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentOnVersion>,
    /// The update policy and the last auto-update check
    /// (`crate::harness_auto_update`). Absent from a supervisor's own
    /// report; `GET /api/harnesses` adds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<crate::harness_auto_update::HarnessAutoUpdate>,
}

/// A live agent and the harness version it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOnVersion {
    /// `project/name`.
    pub agent: String,
    pub id: String,
    /// The version dir that its launch resolved; `null` when not known (an
    /// agent that started before the version dirs, or a harness from
    /// `PATH`).
    pub version: Option<String>,
}

/// A field that is present (even as `null`) is `Some`.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The latest release of a package on npm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latest {
    /// `dist-tags.latest`.
    pub version: String,
    /// `time[<version>]`, when the registry gives it.
    pub published_at: Option<DateTime<Utc>>,
}

/// The bump from the pin to the latest release, as `scripts/harness.py`
/// judges it (`judge` and `bump_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    /// The pin is the latest (or newer).
    None,
    /// A newer release with the same compatibility key: a patch or a minor.
    Minor,
    /// A newer release that changes the first non-zero part: `2.x` to
    /// `3.0.0`, or `0.160.x` to `0.161.0`.
    Major,
}

/// The part of a version that stays the same for a compatible version: the
/// first non-zero part (Cargo's rule; `compat_key` in `scripts/harness.py`).
fn compat_key(v: &Version) -> (u64, u64, u64) {
    if v.major != 0 {
        (v.major, 0, 0)
    } else if v.minor != 0 {
        (0, v.minor, 0)
    } else {
        (0, 0, v.patch)
    }
}

/// The bump from `pinned` to `latest`. A version that does not parse
/// counts as a minor update when the two strings differ, as in
/// `scripts/harness.py`.
pub fn update_kind(pinned: &str, latest: &str) -> UpdateKind {
    let parse = |s: &str| Version::parse(s.trim().trim_start_matches('v'));
    match (parse(pinned), parse(latest)) {
        (Ok(p), Ok(l)) if l <= p => UpdateKind::None,
        (Ok(p), Ok(l)) if compat_key(&p) != compat_key(&l) => UpdateKind::Major,
        (Ok(_), Ok(_)) => UpdateKind::Minor,
        _ if pinned == latest => UpdateKind::None,
        _ => UpdateKind::Minor,
    }
}

/// How long a latest version from npm stays in the cache.
pub const LATEST_TTL: Duration = Duration::from_secs(60 * 60);
/// How long a failed npm lookup stays in the cache, so a dashboard that
/// polls does not hammer an unreachable registry.
pub const LATEST_ERROR_TTL: Duration = Duration::from_secs(5 * 60);
/// The npm registry.
pub const NPM_REGISTRY: &str = "https://registry.npmjs.org";

/// The npm registry of the lookups: `NPM_CONFIG_REGISTRY` (or
/// `npm_config_registry`), as npm itself reads it for the installs, else
/// [`NPM_REGISTRY`].
pub fn registry_from_env() -> String {
    ["NPM_CONFIG_REGISTRY", "npm_config_registry"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| NPM_REGISTRY.to_owned())
}

/// The registry URL of a package's document. The `/` of a scoped name is
/// `%2f`.
pub fn npm_url(registry: &str, package: &str) -> String {
    format!(
        "{}/{}",
        registry.trim_end_matches('/'),
        package.replace('/', "%2f")
    )
}

/// Read `dist-tags.latest` and its `time` from a registry document.
pub fn parse_npm(body: &str) -> Result<Latest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the npm answer is not JSON: {e}"))?;
    let version = v["dist-tags"]["latest"]
        .as_str()
        .ok_or("the npm answer has no dist-tags.latest")?
        .to_owned();
    let published_at = v["time"][&version]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    Ok(Latest {
        version,
        published_at,
    })
}

#[derive(Clone)]
struct Looked {
    at: Instant,
    result: Result<Latest, String>,
}

/// The latest release of each package on npm, cached for [`LATEST_TTL`]
/// (`GET /api/harnesses?latest=true`). A failure never fails the request:
/// the row gets `latest: null` and `latest_error`.
pub struct LatestCache {
    http: Mutex<Arc<dyn crate::probes::HttpGet>>,
    registry: String,
    entries: Mutex<HashMap<String, Looked>>,
}

impl Default for LatestCache {
    fn default() -> Self {
        // A registry document can be large (`@openai/codex`: 18 MB), so
        // the timeout is longer than the probes' 20 s.
        Self::new(
            Arc::new(crate::probes::ReqwestHttp::with_timeout(
                Duration::from_secs(90),
            )),
            &registry_from_env(),
        )
    }
}

impl LatestCache {
    pub fn new(http: Arc<dyn crate::probes::HttpGet>, registry: &str) -> Self {
        Self {
            http: Mutex::new(http),
            registry: registry.to_owned(),
            entries: Mutex::default(),
        }
    }

    /// Replace the HTTP client and forget the cache. For tests.
    pub fn set_http(&self, http: Arc<dyn crate::probes::HttpGet>) {
        *self
            .http
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = http;
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// The latest release of `package`. `refresh` asks npm again.
    pub async fn lookup(&self, package: &str, refresh: bool) -> Result<Latest, String> {
        if !refresh {
            let cached = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(package)
                .filter(|l| {
                    let ttl = if l.result.is_ok() {
                        LATEST_TTL
                    } else {
                        LATEST_ERROR_TTL
                    };
                    l.at.elapsed() < ttl
                })
                .cloned();
            if let Some(l) = cached {
                return l.result;
            }
        }
        let http = self
            .http
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let url = npm_url(&self.registry, package);
        let result = match http
            .get(crate::probes::HttpRequest {
                url: url.clone(),
                headers: vec![("accept".into(), "application/json".into())],
            })
            .await
        {
            Err(e) => Err(format!("cannot reach npm: {e}")),
            Ok(r) if !(200..300).contains(&r.status) => {
                Err(format!("npm answered {} for {url}", r.status))
            }
            Ok(r) => parse_npm(&r.body),
        };
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                package.to_owned(),
                Looked {
                    at: Instant::now(),
                    result: result.clone(),
                },
            );
        result
    }

    /// Add `latest`, `update`, and `latest_error` to each row. The lookups
    /// run at the same time.
    pub async fn fill(&self, report: &mut HarnessReport, refresh: bool) {
        let found = futures_util::future::join_all(
            report
                .harnesses
                .iter()
                .map(|h| self.lookup(&h.package, refresh)),
        )
        .await;
        for (h, r) in report.harnesses.iter_mut().zip(found) {
            match r {
                Ok(l) => {
                    let from = h.installed.as_deref().unwrap_or(&h.base);
                    h.update = Some(Some(update_kind(from, &l.version)));
                    h.latest = Some(Some(l));
                    h.latest_error = None;
                }
                Err(e) => {
                    h.latest = Some(None);
                    h.update = Some(None);
                    h.latest_error = Some(e);
                }
            }
        }
    }
}

/// The answer of `GET /api/harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessReport {
    pub harnesses: Vec<HarnessVersion>,
    /// The schedule of the background check and the next check
    /// (`TERRARIUM_HARNESS_AUTO_UPDATE`). `GET /api/harnesses` adds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<crate::harness_auto_update::ScheduleInfo>,
}

/// The rows for the pins alone, for a supervisor that cannot run the
/// harnesses (the fake one).
pub fn pins_only() -> HarnessReport {
    HarnessReport {
        harnesses: terrarium_core::harnesses::PINS
            .iter()
            .map(|p| HarnessVersion {
                harness: p.harness,
                package: p.package.to_owned(),
                base: p.version.to_owned(),
                pinned: p.version.to_owned(),
                installed: None,
                error: Some("this supervisor does not run harnesses".into()),
                checked_at: None,
                latest: None,
                update: None,
                latest_error: None,
                active: None,
                versions: Vec::new(),
                agents: Vec::new(),
                auto_update: None,
            })
            .collect(),
        auto_update: None,
    }
}

#[derive(Debug, Clone)]
struct Detected {
    at: Instant,
    checked_at: DateTime<Utc>,
    version: Result<Version, String>,
}

/// The detected version of each harness, cached for [`CACHE_TTL`].
#[derive(Debug, Default)]
pub struct VersionCache {
    entries: Mutex<HashMap<HarnessId, Detected>>,
}

impl VersionCache {
    /// The installed version of harness `id`, which `adapter` runs. `refresh` runs the
    /// binary again. The run blocks, so it goes to the blocking pool.
    pub async fn detect(
        &self,
        id: HarnessId,
        adapter: Arc<dyn Adapter>,
        refresh: bool,
    ) -> (Result<Version, String>, DateTime<Utc>) {
        if !refresh {
            let cached = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .filter(|d| d.at.elapsed() < CACHE_TTL)
                .cloned();
            if let Some(d) = cached {
                return (d.version, d.checked_at);
            }
        }
        let version = tokio::task::spawn_blocking(move || adapter.detect_version())
            .await
            .map_err(|e| format!("the version check did not finish: {e}"))
            .and_then(|r| r.map_err(|e| e.to_string()));
        let checked_at = Utc::now();
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                Detected {
                    at: Instant::now(),
                    checked_at,
                    version: version.clone(),
                },
            );
        (version, checked_at)
    }

    /// The report row of `id`, which `adapter` runs.
    pub async fn row(
        &self,
        id: HarnessId,
        adapter: Arc<dyn Adapter>,
        refresh: bool,
    ) -> HarnessVersion {
        let pin = terrarium_core::harnesses::pin(id);
        let (installed, checked_at) = self.detect(id, adapter, refresh).await;
        let (installed, error) = match installed {
            Ok(v) => (Some(v.to_string()), None),
            Err(e) => (None, Some(e)),
        };
        HarnessVersion {
            harness: id,
            package: pin.package.to_owned(),
            base: pin.version.to_owned(),
            pinned: pin.version.to_owned(),
            installed,
            error,
            checked_at: Some(checked_at),
            latest: None,
            update: None,
            latest_error: None,
            active: None,
            versions: Vec::new(),
            agents: Vec::new(),
            auto_update: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn update_kind_follows_harness_py() {
        use UpdateKind::*;
        assert_eq!(update_kind("2.1.310", "2.1.310"), None);
        assert_eq!(
            update_kind("2.1.310", "2.1.300"),
            None,
            "older is not an update"
        );
        assert_eq!(update_kind("2.1.310", "2.1.311"), Minor, "patch");
        assert_eq!(update_kind("2.1.310", "2.2.0"), Minor);
        assert_eq!(update_kind("2.1.310", "3.0.0"), Major);
        // 0.x: the second number is the major.
        assert_eq!(update_kind("0.164.0", "0.164.2"), Minor);
        assert_eq!(update_kind("0.164.0", "0.165.0"), Major);
        assert_eq!(update_kind("0.0.3", "0.0.4"), Major);
        assert_eq!(update_kind("1.0.103", "1.0.110"), Minor);
        assert_eq!(update_kind("1.4.0", "2.0.0"), Major);
        // A version that does not parse: different strings are a minor.
        assert_eq!(update_kind("nightly", "nightly"), None);
        assert_eq!(update_kind("nightly", "1.0.0"), Minor);
        assert_eq!(
            serde_json::to_value(Major).expect("json"),
            serde_json::json!("major")
        );
    }

    #[test]
    fn npm_url_escapes_the_scope() {
        assert_eq!(
            npm_url("https://registry.npmjs.org/", "@openai/codex"),
            "https://registry.npmjs.org/@openai%2fcodex"
        );
        assert_eq!(
            npm_url(NPM_REGISTRY, "copilot-custom-system-prompt"),
            "https://registry.npmjs.org/copilot-custom-system-prompt"
        );
    }

    /// A registry document, trimmed (`npm view` fields only).
    const CODEX_DOC: &str = r#"{"name":"@openai/codex","dist-tags":{"latest":"0.165.0","alpha":"0.166.0-alpha.1"},
        "time":{"created":"2025-04-16T00:00:00.000Z","0.164.0":"2026-10-20T18:01:02.123Z","0.165.0":"2026-10-25T09:30:00.000Z"},
        "versions":{"0.164.0":{},"0.165.0":{}}}"#;

    #[test]
    fn parse_npm_reads_the_latest_tag_and_its_time() {
        let l = parse_npm(CODEX_DOC).expect("parse");
        assert_eq!(l.version, "0.165.0");
        assert_eq!(
            l.published_at.map(|t| t.to_rfc3339()),
            Some("2026-10-25T09:30:00+00:00".into())
        );
        let no_time = parse_npm(r#"{"dist-tags":{"latest":"1.0.0"}}"#).expect("parse");
        assert_eq!(no_time.published_at, None);
        assert!(
            parse_npm(r#"{"dist-tags":{}}"#)
                .unwrap_err()
                .contains("dist-tags.latest")
        );
        assert!(parse_npm("<html>").unwrap_err().contains("not JSON"));
    }

    /// Answers from a table by URL; counts the calls.
    #[derive(Default)]
    struct FakeNpm {
        answers: Mutex<HashMap<String, Result<crate::probes::HttpResponse, String>>>,
        calls: Mutex<Vec<String>>,
    }

    impl crate::probes::HttpGet for FakeNpm {
        fn get(
            &self,
            req: crate::probes::HttpRequest,
        ) -> futures_util::future::BoxFuture<'_, Result<crate::probes::HttpResponse, String>>
        {
            self.calls.lock().unwrap().push(req.url.clone());
            let a = self
                .answers
                .lock()
                .unwrap()
                .get(&req.url)
                .cloned()
                .unwrap_or_else(|| Err("connection refused".into()));
            Box::pin(async move { a })
        }
    }

    fn ok(body: &str) -> Result<crate::probes::HttpResponse, String> {
        Ok(crate::probes::HttpResponse {
            status: 200,
            body: body.into(),
        })
    }

    #[tokio::test]
    async fn fill_adds_latest_and_update_and_caches_for_an_hour() {
        let npm = Arc::new(FakeNpm::default());
        let base = "http://npm.test";
        {
            let mut a = npm.answers.lock().unwrap();
            a.insert(
                npm_url(base, "@anthropic-ai/claude-code"),
                ok(r#"{"dist-tags":{"latest":"2.1.310"},"time":{"2.1.310":"2026-10-01T00:00:00Z"}}"#),
            );
            a.insert(npm_url(base, "@openai/codex"), ok(CODEX_DOC));
            a.insert(
                npm_url(base, "@github/copilot"),
                Ok(crate::probes::HttpResponse {
                    status: 503,
                    body: "down".into(),
                }),
            );
            // pi: no answer at all (connection refused).
        }
        let cache = LatestCache::new(npm.clone(), base);
        let mut r = pins_only();
        for h in &mut r.harnesses {
            h.base = match h.harness {
                HarnessId::Codex => "0.164.0".into(),
                _ => h.base.clone(),
            };
        }
        let claude_pin = r.harnesses[0].base.clone();
        cache.fill(&mut r, false).await;
        let row = |id: HarnessId| r.harnesses.iter().find(|h| h.harness == id).expect("row");

        let codex = row(HarnessId::Codex);
        assert_eq!(
            codex.latest.clone().flatten().expect("latest").version,
            "0.165.0"
        );
        assert_eq!(codex.update, Some(Some(UpdateKind::Major)));
        assert_eq!(codex.latest_error, None);
        let claude = row(HarnessId::Claude);
        assert_eq!(
            claude.update,
            Some(Some(update_kind(&claude_pin, "2.1.310")))
        );
        let copilot = row(HarnessId::Copilot);
        assert_eq!(copilot.latest, Some(None));
        assert_eq!(copilot.update, Some(None));
        assert!(copilot.latest_error.as_deref().unwrap().contains("503"));
        let pi = row(HarnessId::Pi);
        assert!(
            pi.latest_error
                .as_deref()
                .unwrap()
                .contains("connection refused")
        );

        // The JSON: `latest` is present as null on a failure.
        let v = serde_json::to_value(&r).expect("json");
        let rows = v["harnesses"].as_array().expect("rows");
        let cop = rows
            .iter()
            .find(|h| h["harness"] == "copilot")
            .expect("copilot");
        assert!(cop["latest"].is_null() && cop.get("latest").is_some());
        assert!(cop["update"].is_null() && cop.get("update").is_some());
        let cod = rows
            .iter()
            .find(|h| h["harness"] == "codex")
            .expect("codex");
        assert_eq!(
            cod["latest"],
            serde_json::json!({"version": "0.165.0", "published_at": "2026-10-25T09:30:00Z"})
        );
        assert_eq!(cod["update"], "major");
        // The JSON reads back the same, `null` included.
        let back: HarnessReport = serde_json::from_value(v).expect("back");
        assert_eq!(back, r);

        // The second fill is from the cache; `refresh` asks again.
        // One npm call per harness (grok too, TER-1192).
        let n = npm.calls.lock().unwrap().len();
        assert_eq!(n, HarnessId::ALL.len());
        cache.fill(&mut r, false).await;
        assert_eq!(npm.calls.lock().unwrap().len(), n);
        cache.fill(&mut r, true).await;
        assert_eq!(npm.calls.lock().unwrap().len(), 2 * n);

        // An installed version (after an update) wins over the pin.
        for h in &mut r.harnesses {
            if h.harness == HarnessId::Codex {
                h.installed = Some("0.165.0".into());
            }
        }
        cache.fill(&mut r, false).await;
        assert_eq!(
            row_update(&r, HarnessId::Codex),
            Some(Some(UpdateKind::None))
        );
    }

    fn row_update(r: &HarnessReport, id: HarnessId) -> Option<Option<UpdateKind>> {
        r.harnesses
            .iter()
            .find(|h| h.harness == id)
            .and_then(|h| h.update)
    }

    #[test]
    fn the_report_has_no_range_and_no_policy() {
        let v = serde_json::to_value(pins_only()).expect("json");
        assert!(v.get("policy").is_none(), "{v}");
        for row in v["harnesses"].as_array().expect("rows") {
            for k in ["tested", "in_range"] {
                assert!(row.get(k).is_none(), "{k}: {row}");
            }
        }
    }

    #[test]
    fn without_latest_the_row_has_no_npm_fields() {
        let v = serde_json::to_value(pins_only()).expect("json");
        let row = &v["harnesses"][0];
        for k in ["latest", "update", "latest_error"] {
            assert!(row.get(k).is_none(), "{k}: {row}");
        }
    }

    #[test]
    fn pins_only_lists_every_harness() {
        let r = pins_only();
        assert_eq!(r.harnesses.len(), HarnessId::ALL.len());
        assert!(
            r.harnesses
                .iter()
                .all(|h| h.installed.is_none() && h.error.is_some())
        );
    }
}
