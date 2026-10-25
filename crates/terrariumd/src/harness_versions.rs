//! The harness version guard and the version report (`GET /api/harnesses`).
//!
//! `harnesses.toml` pins each harness, and each adapter's `tested_version`
//! comes from it (`terrarium_core::harnesses`). At each launch the
//! supervisor compares the installed version (`Adapter::detect_version`)
//! with that range. Outside it, the policy decides
//! (`TERRARIUM_HARNESS_VERSION_POLICY`):
//!
//! - `warn` (the default): the agent starts. The daemon logs a warning and
//!   writes a `warning` event for the agent.
//! - `refuse`: the start fails with `422 invalid`, and the message says why.
//!
//! A version that cannot be read (the binary is missing, or prints no
//! version) never refuses a start: the launch reports a missing binary
//! itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use terrarium_core::{Adapter, HarnessId};

/// The env var of the policy.
pub const ENV_POLICY: &str = "TERRARIUM_HARNESS_VERSION_POLICY";

/// How long a detected version stays in the cache. A rebuilt image restarts
/// the daemon; the TTL covers a harness that changed on the host in
/// development (`just daemon::run`).
pub const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// What a launch does with a harness outside its tested range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum VersionPolicy {
    /// Start, with a log line and a `warning` event.
    #[default]
    Warn,
    /// Refuse the start (`422 invalid`).
    Refuse,
}

impl std::fmt::Display for VersionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Warn => "warn",
            Self::Refuse => "refuse",
        })
    }
}

/// One row of `GET /api/harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessVersion {
    pub harness: HarnessId,
    /// The npm package (`harnesses.toml`).
    pub package: String,
    /// The version the image installs (`harnesses.toml`).
    pub pinned: String,
    /// The range the adapter accepts.
    pub tested: String,
    /// The version that the binary reports. `None` when it did not answer.
    pub installed: Option<String>,
    /// `None` when the installed version is not known.
    pub in_range: Option<bool>,
    /// Why the installed version is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the daemon ran `<binary> --version`.
    pub checked_at: Option<DateTime<Utc>>,
}

/// The answer of `GET /api/harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessReport {
    pub policy: VersionPolicy,
    pub harnesses: Vec<HarnessVersion>,
}

/// The rows for the pins alone, for a supervisor that cannot run the
/// harnesses (the fake one).
pub fn pins_only(policy: VersionPolicy) -> HarnessReport {
    HarnessReport {
        policy,
        harnesses: terrarium_core::harnesses::PINS
            .iter()
            .map(|p| HarnessVersion {
                harness: p.harness,
                package: p.package.to_owned(),
                pinned: p.version.to_owned(),
                tested: p.tested.to_owned(),
                installed: None,
                in_range: None,
                error: Some("this supervisor does not run harnesses".into()),
                checked_at: None,
            })
            .collect(),
    }
}

/// The installed version compared with the tested range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    InRange(Version),
    OutOfRange(Version),
    /// The version could not be read; the reason.
    Unknown(String),
}

pub fn judge(installed: &Result<Version, String>, tested: &VersionReq) -> Verdict {
    match installed {
        Ok(v) if tested.matches(v) => Verdict::InRange(v.clone()),
        Ok(v) => Verdict::OutOfRange(v.clone()),
        Err(e) => Verdict::Unknown(e.clone()),
    }
}

/// The warning (and refusal) text for a harness outside its range.
pub fn out_of_range_message(
    harness: HarnessId,
    installed: &Version,
    tested: &VersionReq,
) -> String {
    let pin = terrarium_core::harnesses::pin(harness).version;
    format!(
        "{harness} {installed} is outside the tested range {tested} (harnesses.toml pins {pin}); \
         hooks and flags may differ. Rebuild the image (`just up`) or update the pin \
         (`just harness::update {harness}`, docs/getting-started.md \"Updating a harness\")"
    )
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
        let tested = adapter.tested_version();
        let pin = terrarium_core::harnesses::pin(id);
        let (installed, checked_at) = self.detect(id, adapter, refresh).await;
        let (in_range, error) = match judge(&installed, &tested) {
            Verdict::InRange(_) => (Some(true), None),
            Verdict::OutOfRange(_) => (Some(false), None),
            Verdict::Unknown(e) => (None, Some(e)),
        };
        HarnessVersion {
            harness: id,
            package: pin.package.to_owned(),
            pinned: pin.version.to_owned(),
            tested: tested.to_string(),
            installed: installed.ok().map(|v| v.to_string()),
            in_range,
            error,
            checked_at: Some(checked_at),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn judge_sorts_the_three_cases() {
        let req = VersionReq::parse(">=2.1.310, <2.2.0").expect("req");
        let v = |s: &str| Version::parse(s).expect("v");
        assert_eq!(
            judge(&Ok(v("2.1.311")), &req),
            Verdict::InRange(v("2.1.311"))
        );
        assert_eq!(
            judge(&Ok(v("2.2.0")), &req),
            Verdict::OutOfRange(v("2.2.0"))
        );
        assert_eq!(
            judge(&Err("cannot run claude".into()), &req),
            Verdict::Unknown("cannot run claude".into())
        );
    }

    #[test]
    fn the_message_names_the_pin_and_the_way_out() {
        let req = VersionReq::parse(">=2.1.310, <2.2.0").expect("req");
        let m = out_of_range_message(
            HarnessId::Claude,
            &Version::parse("2.3.0").expect("v"),
            &req,
        );
        assert!(
            m.starts_with("claude 2.3.0 is outside the tested range >=2.1.310, <2.2.0"),
            "{m}"
        );
        assert!(
            m.contains(terrarium_core::harnesses::claude::VERSION),
            "{m}"
        );
        assert!(m.contains("just harness::update claude"), "{m}");
    }

    #[test]
    fn the_policy_parses_and_prints() {
        use clap::ValueEnum;
        assert_eq!(
            VersionPolicy::from_str("refuse", true),
            Ok(VersionPolicy::Refuse)
        );
        assert_eq!(VersionPolicy::default(), VersionPolicy::Warn);
        assert_eq!(
            serde_json::to_value(VersionPolicy::Refuse).expect("json"),
            "refuse"
        );
        assert_eq!(VersionPolicy::Warn.to_string(), "warn");
    }

    #[test]
    fn pins_only_lists_every_harness() {
        let r = pins_only(VersionPolicy::Warn);
        assert_eq!(r.harnesses.len(), HarnessId::ALL.len());
        assert!(
            r.harnesses
                .iter()
                .all(|h| h.installed.is_none() && h.in_range.is_none())
        );
    }
}
