//! The update policy of each harness, and the result of its last
//! auto-update check (PLAN §6, "Auto-update and pins";
//! `crate::harness_auto_update`).
//!
//! A harness with no row has the policy `auto`. `harnesses.toml` gives the
//! version of a fresh install; on a running daemon this table decides
//! what the background check does. The table is in the state dir, so a
//! deploy (which replaces the code dir) keeps it.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};

/// What the background check does with a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    /// Install the latest release on npm (majors too).
    #[default]
    Auto,
    /// Keep `pinned_version`; never update by itself.
    Pinned,
}

impl UpdatePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pinned => "pinned",
        }
    }
}

impl std::str::FromStr for UpdatePolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "pinned" => Ok(Self::Pinned),
            other => Err(format!("unknown update policy `{other}`")),
        }
    }
}

/// The result of the last check of one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    /// The active version is the latest release.
    Current,
    /// The check installed a newer release and switched to it.
    Updated,
    /// The policy is `pinned`: no install.
    Pinned,
    /// The install of a newer release failed; the version stays.
    Failed,
    /// npm did not answer, the harness is not installed, or a manual
    /// update ran.
    Skipped,
}

impl CheckResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Updated => "updated",
            Self::Pinned => "pinned",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

impl std::str::FromStr for CheckResult {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "current" => Ok(Self::Current),
            "updated" => Ok(Self::Updated),
            "pinned" => Ok(Self::Pinned),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            other => Err(format!("unknown check result `{other}`")),
        }
    }
}

/// One row: the policy and the last check of a harness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    pub harness: String,
    pub policy: UpdatePolicy,
    /// The version of a `pinned` policy.
    pub pinned_version: Option<String>,
    /// The active version before the last switch: the target of a
    /// rollback.
    pub previous: Option<String>,
    /// The latest release on npm at the last check.
    pub latest: Option<String>,
    /// When the last check ran (RFC 3339).
    pub checked_at: Option<String>,
    pub check_result: Option<CheckResult>,
    /// One line about the last check.
    pub check_detail: Option<String>,
    /// The version whose failed install was mailed.
    pub failed_version: Option<String>,
}

impl UpdateState {
    /// The state of a harness with no row.
    pub fn new(harness: &str) -> Self {
        Self {
            harness: harness.to_owned(),
            ..Self::default()
        }
    }
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<UpdateState> {
    let policy: String = r.get("policy")?;
    let result: Option<String> = r.get("check_result")?;
    Ok(UpdateState {
        harness: r.get("harness")?,
        policy: policy.parse().unwrap_or_default(),
        pinned_version: r.get("pinned_version")?,
        previous: r.get("previous")?,
        latest: r.get("latest")?,
        checked_at: r.get("checked_at")?,
        check_result: result.and_then(|s| s.parse().ok()),
        check_detail: r.get("check_detail")?,
        failed_version: r.get("failed_version")?,
    })
}

/// The state of `harness`; [`UpdateState::new`] when it has no row.
pub fn get(c: &Connection, harness: &str) -> ApiResult<UpdateState> {
    Ok(c.query_row(
        "SELECT * FROM harness_updates WHERE harness = ?1",
        [harness],
        from_row,
    )
    .optional()?
    .unwrap_or_else(|| UpdateState::new(harness)))
}

/// Every row, by harness.
pub fn all(c: &Connection) -> ApiResult<Vec<UpdateState>> {
    let mut st = c.prepare("SELECT * FROM harness_updates ORDER BY harness")?;
    let rows = st.query_map([], from_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The time of the newest check of any harness.
pub fn last_check(c: &Connection) -> ApiResult<Option<String>> {
    Ok(
        c.query_row("SELECT max(checked_at) FROM harness_updates", [], |r| {
            r.get(0)
        })?,
    )
}

/// Make sure that `harness` has a row.
fn ensure(t: &Tx<'_>, harness: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO harness_updates (harness, updated_at) VALUES (?1, ?2)
         ON CONFLICT (harness) DO NOTHING",
        params![harness, now()],
    )?;
    Ok(())
}

/// Set the policy of `harness`: `pinned` needs a version; `auto` clears it.
pub fn set_policy(
    t: &Tx<'_>,
    harness: &str,
    policy: UpdatePolicy,
    version: Option<&str>,
) -> ApiResult<()> {
    let version = match (policy, version) {
        (UpdatePolicy::Pinned, None) => {
            return Err(ApiError::invalid("a pin needs a version"));
        }
        (UpdatePolicy::Pinned, Some(v)) => Some(v),
        (UpdatePolicy::Auto, _) => None,
    };
    ensure(t, harness)?;
    t.execute(
        "UPDATE harness_updates SET policy = ?2, pinned_version = ?3, updated_at = ?4
         WHERE harness = ?1",
        params![harness, policy.as_str(), version, now()],
    )?;
    Ok(())
}

/// Record a switch of the active version from `from` to `to`: `from`
/// becomes the previous version, and a pinned harness is now pinned to
/// `to`. A switch to the same version changes nothing.
pub fn record_switch(t: &Tx<'_>, harness: &str, from: Option<&str>, to: &str) -> ApiResult<()> {
    ensure(t, harness)?;
    if let Some(from) = from.filter(|f| *f != to) {
        t.execute(
            "UPDATE harness_updates SET previous = ?2, updated_at = ?3 WHERE harness = ?1",
            params![harness, from, now()],
        )?;
    }
    t.execute(
        "UPDATE harness_updates SET pinned_version = ?2, updated_at = ?3
         WHERE harness = ?1 AND policy = 'pinned'",
        params![harness, to, now()],
    )?;
    t.execute(
        "UPDATE harness_updates SET failed_version = NULL WHERE harness = ?1",
        [harness],
    )?;
    Ok(())
}

/// Record the result of a check.
pub fn record_check(
    t: &Tx<'_>,
    harness: &str,
    at: &str,
    result: CheckResult,
    detail: &str,
    latest: Option<&str>,
) -> ApiResult<()> {
    ensure(t, harness)?;
    t.execute(
        "UPDATE harness_updates SET checked_at = ?2, check_result = ?3, check_detail = ?4,
                latest = coalesce(?5, latest), updated_at = ?6
         WHERE harness = ?1",
        params![harness, at, result.as_str(), detail, latest, now()],
    )?;
    Ok(())
}

/// Set the version whose failed install was mailed (`None` clears it).
pub fn set_failed(t: &Tx<'_>, harness: &str, version: Option<&str>) -> ApiResult<()> {
    ensure(t, harness)?;
    t.execute(
        "UPDATE harness_updates SET failed_version = ?2 WHERE harness = ?1",
        params![harness, version],
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events::EventHub;

    fn db() -> Db {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        Db::new(conn, EventHub::default())
    }

    async fn state(db: &Db, h: &'static str) -> UpdateState {
        db.read(move |c| get(c, h)).await.unwrap()
    }

    #[tokio::test]
    async fn no_row_is_auto_and_a_pin_needs_a_version() {
        let db = db();
        let s = state(&db, "codex").await;
        assert_eq!(s, UpdateState::new("codex"));
        assert_eq!(s.policy, UpdatePolicy::Auto);
        let e = db
            .write("operator", |t| {
                set_policy(t, "codex", UpdatePolicy::Pinned, None)
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("needs a version"), "{e}");
    }

    #[tokio::test]
    async fn pin_unpin_switch_and_check_round_trip() {
        let db = db();
        db.write("operator", |t| {
            set_policy(t, "codex", UpdatePolicy::Pinned, Some("0.164.0"))
        })
        .await
        .unwrap();
        let s = state(&db, "codex").await;
        assert_eq!(s.policy, UpdatePolicy::Pinned);
        assert_eq!(s.pinned_version.as_deref(), Some("0.164.0"));

        // A switch of a pinned harness moves the pin and keeps `previous`.
        db.write("operator", |t| {
            set_failed(t, "codex", Some("0.165.0"))?;
            record_switch(t, "codex", Some("0.164.0"), "0.164.2")
        })
        .await
        .unwrap();
        let s = state(&db, "codex").await;
        assert_eq!(s.previous.as_deref(), Some("0.164.0"));
        assert_eq!(s.pinned_version.as_deref(), Some("0.164.2"));
        assert_eq!(s.failed_version, None);
        // A switch to the same version keeps `previous`.
        db.write("operator", |t| {
            record_switch(t, "codex", Some("0.164.2"), "0.164.2")
        })
        .await
        .unwrap();
        assert_eq!(
            state(&db, "codex").await.previous.as_deref(),
            Some("0.164.0")
        );

        db.write("operator", |t| {
            set_policy(t, "codex", UpdatePolicy::Auto, Some("ignored"))
        })
        .await
        .unwrap();
        let s = state(&db, "codex").await;
        assert_eq!(s.policy, UpdatePolicy::Auto);
        assert_eq!(s.pinned_version, None);

        db.write("system", |t| {
            record_check(
                t,
                "codex",
                "2026-10-31T00:00:00.000Z",
                CheckResult::Current,
                "0.164.2 is the latest",
                Some("0.164.2"),
            )?;
            // npm did not answer: the last known latest stays.
            record_check(
                t,
                "pi",
                "2026-10-31T01:00:00.000Z",
                CheckResult::Skipped,
                "npm",
                None,
            )
        })
        .await
        .unwrap();
        let s = state(&db, "codex").await;
        assert_eq!(s.check_result, Some(CheckResult::Current));
        assert_eq!(s.latest.as_deref(), Some("0.164.2"));
        assert_eq!(
            db.read(last_check).await.unwrap().as_deref(),
            Some("2026-10-31T01:00:00.000Z")
        );
        assert_eq!(db.read(all).await.unwrap().len(), 2);
    }
}
