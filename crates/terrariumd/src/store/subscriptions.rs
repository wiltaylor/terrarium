//! Subscriptions, quota samples, and login sessions (PLAN §16).
//!
//! A subscription comes from `subscriptions/<name>.toml` (`source = file`)
//! or from the API and the login wizard (`source = api`). The definition
//! columns of a file subscription follow the file on each sync. The daemon
//! columns (login status, probe status, the env var of a stored key) stay.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::HarnessId;
use terrarium_core::config::SubscriptionDef;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::json_col;
use crate::store::projects::{SOURCE_API, SOURCE_FILE};

/// Names that collide with the fixed routes under `/api/subscriptions/`.
pub const RESERVED_NAMES: [&str; 2] = ["login", "detect"];

/// Login state words of the Subscriptions page.
pub const LOGGED_IN: &str = "logged_in";
pub const MISSING: &str = "missing";
pub const EXPIRING: &str = "expiring";
pub const EXPIRED: &str = "expired";
/// A stored API key that no harness status command can check (Z.ai). The
/// key is there; whether the provider takes it shows on the first turn.
pub const API_KEY: &str = "api_key";

/// Probe state words.
pub const PROBE_OK: &str = "ok";
pub const PROBE_FAILED: &str = "failed";
pub const PROBE_NONE: &str = "none";

/// One subscription row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub name: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub harnesses: Vec<HarnessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    pub alert_at: u8,
    /// `file`, `api`, or `fleet:<name>` (owned by a fleet).
    pub source: String,
    /// The env var that carries the stored API key into the agents on this
    /// subscription. The key itself is never in the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    /// `logged_in`, `expiring`, `expired`, `missing`, `unknown`, or
    /// `api_key` (a stored Z.ai key; no harness login to check).
    pub login_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    /// `ok`, `failed`, or `none` (no probe, or not run yet).
    pub probe_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// The probe of a subscription that names none: `zai-usage` for a Z.ai
/// GLM Coding Plan (the `api` plan has no quota endpoint). Applied when a
/// row is read, so rows written before the probe existed get it too.
pub fn default_probe(provider: &str, plan: Option<&str>) -> Option<String> {
    use terrarium_core::zai;
    (zai::is_zai(provider) && zai::plan_or_default(plan) == zai::Plan::Coding)
        .then(|| crate::probes::zai::NAME.to_owned())
}

fn row(r: &Row<'_>) -> rusqlite::Result<Subscription> {
    let harnesses: String = r.get("harnesses")?;
    let alert_at: i64 = r.get("alert_at")?;
    let provider: String = r.get("provider")?;
    let plan: Option<String> = r.get("plan")?;
    let probe: Option<String> = r.get("probe")?;
    Ok(Subscription {
        name: r.get("name")?,
        probe: probe.or_else(|| default_probe(&provider, plan.as_deref())),
        provider,
        plan,
        harnesses: json_col(&harnesses),
        alert_at: u8::try_from(alert_at.clamp(0, 255)).unwrap_or(80),
        source: r.get("source")?,
        env_var: r.get("env_var")?,
        login_status: r.get("login_status")?,
        verified_at: r.get("verified_at")?,
        probe_status: r.get("probe_status")?,
        probe_error: r.get("probe_error")?,
        last_probe_at: r.get("last_probe_at")?,
        last_success_at: r.get("last_success_at")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub fn find(conn: &Connection, name: &str) -> ApiResult<Option<Subscription>> {
    Ok(conn
        .query_row("SELECT * FROM subscriptions WHERE name = ?1", [name], row)
        .optional()?)
}

pub fn get(conn: &Connection, name: &str) -> ApiResult<Subscription> {
    find(conn, name)?.ok_or_else(|| ApiError::not_found(format!("no subscription `{name}`")))
}

/// Every subscription, by name.
pub fn list(conn: &Connection) -> ApiResult<Vec<Subscription>> {
    let mut stmt = conn.prepare("SELECT * FROM subscriptions ORDER BY name")?;
    let rows = stmt.query_map([], row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Check a subscription name: the project name rules, and not a reserved
/// route word.
pub fn check_name(name: &str) -> ApiResult<()> {
    crate::store::check_name("subscription", name)?;
    if RESERVED_NAMES.contains(&name) {
        return Err(ApiError::invalid(format!(
            "`{name}` is a reserved word; pick another subscription name"
        )));
    }
    Ok(())
}

/// Insert or update the definition columns of a subscription. A row from
/// another source becomes `source`, except that an `api` write keeps the
/// `fleet:<name>` of a row a fleet owns (the fleet stays the owner, and
/// the change shows as drift).
pub fn upsert_def(t: &mut Tx<'_>, def: &SubscriptionDef, source: &str) -> ApiResult<Subscription> {
    check_name(&def.name)?;
    if def.provider.trim().is_empty() {
        return Err(ApiError::invalid("the subscription needs a provider"));
    }
    if terrarium_core::zai::is_zai(&def.provider) {
        terrarium_core::zai::parse_plan(def.plan.as_deref()).map_err(ApiError::invalid)?;
    }
    let at = now();
    let harnesses = serde_json::to_string(&def.harnesses)?;
    t.execute(
        "INSERT INTO subscriptions (name, provider, plan, harnesses, probe, alert_at, source,
                                    created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT (name) DO UPDATE SET
             provider = excluded.provider, plan = excluded.plan,
             harnesses = excluded.harnesses, probe = excluded.probe,
             alert_at = excluded.alert_at,
             source = CASE WHEN excluded.source = 'api' AND subscriptions.source GLOB 'fleet:*'
                           THEN subscriptions.source ELSE excluded.source END,
             updated_at = excluded.updated_at",
        params![
            def.name,
            def.provider,
            def.plan,
            harnesses,
            def.probe,
            i64::from(def.alert_at),
            source,
            at
        ],
    )?;
    get(t, &def.name)
}

/// What a file sync changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SyncChanges {
    pub upserted: Vec<String>,
    /// File subscriptions whose file is gone. They become `api` rows, so
    /// their samples stay.
    pub released: Vec<String>,
    pub errors: Vec<String>,
}

/// Sync `subscriptions/*.toml` into the table.
pub fn sync_files(t: &mut Tx<'_>, defs: &[SubscriptionDef]) -> ApiResult<SyncChanges> {
    let mut changes = SyncChanges::default();
    for def in defs {
        match upsert_def(t, def, SOURCE_FILE) {
            Ok(_) => changes.upserted.push(def.name.clone()),
            Err(e) => changes
                .errors
                .push(format!("subscriptions/{}.toml: {}", def.name, e.message)),
        }
    }
    let names: Vec<String> = {
        let mut stmt = t.prepare("SELECT name FROM subscriptions WHERE source = ?1")?;
        let rows = stmt.query_map([SOURCE_FILE], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for name in names {
        if !defs.iter().any(|d| d.name == name) {
            t.execute(
                "UPDATE subscriptions SET source = ?2, updated_at = ?3 WHERE name = ?1",
                params![name, SOURCE_API, now()],
            )?;
            changes.released.push(name);
        }
    }
    Ok(changes)
}

/// Delete an API subscription. Its quota samples go with it; usage samples
/// stay (they name the subscription as text).
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<()> {
    let s = get(t, name)?;
    if s.source == SOURCE_FILE {
        return Err(ApiError::new(
            crate::error::Code::FileManaged,
            format!(
                "subscription `{name}` is declared in subscriptions/{name}.toml; delete the file, then POST /api/reload"
            ),
        ));
    }
    t.execute("DELETE FROM subscriptions WHERE name = ?1", [name])?;
    Ok(())
}

pub fn set_login_status(t: &mut Tx<'_>, name: &str, status: &str) -> ApiResult<()> {
    let at = now();
    t.execute(
        "UPDATE subscriptions SET login_status = ?2, verified_at = ?3, updated_at = ?3 WHERE name = ?1",
        params![name, status, at],
    )?;
    Ok(())
}

/// Record what a harness status command said about the login of `name`
/// (the verify path and the start-up check, PLAN §16.1). Logged in:
/// `logged_in`. Logged out after a login was known (`logged_in`,
/// `expiring`, `expired`): `expired`, see [`mark_expired`]. Logged out with
/// no known login: `missing`. Returns true when this call mailed the
/// operator.
pub fn record_login_check(t: &mut Tx<'_>, name: &str, logged_in: bool) -> ApiResult<bool> {
    if logged_in {
        set_login_status(t, name, LOGGED_IN)?;
        return Ok(false);
    }
    let current = get(t, name)?.login_status;
    if matches!(current.as_str(), LOGGED_IN | EXPIRING | EXPIRED) {
        return mark_expired(t, name, "the harness status command says it is logged out");
    }
    set_login_status(t, name, MISSING)?;
    Ok(false)
}

/// Set `expired` on `name`. On the change into `expired` (and only then),
/// mail the operator from `system` and emit a `subscription.expired`
/// event. Returns true when it mailed.
pub fn mark_expired(t: &mut Tx<'_>, name: &str, why: &str) -> ApiResult<bool> {
    let sub = get(t, name)?;
    set_login_status(t, name, EXPIRED)?;
    if sub.login_status == EXPIRED {
        return Ok(false);
    }
    let subject = format!("Login expired: {name}");
    let body = format!(
        "The login of subscription `{name}` ({}) expired: {why}.\n\nAgents on it cannot \
         spend it until you log in again. Open Subscriptions in the dashboard and use \
         Re-login, then Verify now.",
        sub.provider
    );
    crate::store::mail::send(
        t,
        crate::store::mail::Outgoing {
            from: &crate::store::mail::Address::System,
            to: &crate::store::mail::Address::Operator,
            subject: &subject,
            body: &body,
            thread: None,
            in_reply_to: None,
        },
    )?;
    t.emit(crate::events::EventDraft::new(
        "subscription.expired",
        format!("{name}: login expired"),
        serde_json::json!({"subscription": name, "from": sub.login_status, "reason": why}),
    ))?;
    Ok(true)
}

/// A probe proved the login: an `expired` login is `logged_in` again.
/// Other states stay, because a probe can use another credential.
pub fn clear_expired(t: &mut Tx<'_>, name: &str) -> ApiResult<()> {
    if get(t, name)?.login_status == EXPIRED {
        set_login_status(t, name, LOGGED_IN)?;
    }
    Ok(())
}

pub fn set_plan(t: &mut Tx<'_>, name: &str, plan: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE subscriptions SET plan = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, plan, now()],
    )?;
    Ok(())
}

pub fn set_env_var(t: &mut Tx<'_>, name: &str, env_var: Option<&str>) -> ApiResult<()> {
    t.execute(
        "UPDATE subscriptions SET env_var = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, env_var, now()],
    )?;
    Ok(())
}

/// Record a probe run: `Ok(())` sets `ok` and `last_success_at`; an error
/// sets `failed` and keeps the last success time.
pub fn set_probe_result(t: &mut Tx<'_>, name: &str, result: Result<(), &str>) -> ApiResult<()> {
    let at = now();
    match result {
        Ok(()) => t.execute(
            "UPDATE subscriptions SET probe_status = 'ok', probe_error = NULL, last_probe_at = ?2,
                    last_success_at = ?2 WHERE name = ?1",
            params![name, at],
        )?,
        Err(e) => t.execute(
            "UPDATE subscriptions SET probe_status = 'failed', probe_error = ?2, last_probe_at = ?3
             WHERE name = ?1",
            params![name, e, at],
        )?,
    };
    Ok(())
}

/// One stored quota sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSample {
    pub subscription: String,
    pub window: String,
    pub used_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub raw: Value,
    pub at: String,
}

fn sample_row(r: &Row<'_>) -> rusqlite::Result<QuotaSample> {
    let raw: String = r.get("raw")?;
    Ok(QuotaSample {
        subscription: r.get("subscription")?,
        window: r.get("window")?,
        used_pct: r.get("used_pct")?,
        resets_at: r.get("resets_at")?,
        raw: serde_json::from_str(&raw).unwrap_or(Value::Null),
        at: r.get("at")?,
    })
}

pub fn insert_sample(
    t: &mut Tx<'_>,
    subscription: &str,
    window: &str,
    used_pct: f64,
    resets_at: Option<&str>,
    raw: &Value,
) -> ApiResult<()> {
    t.execute(
        "INSERT INTO quota_samples (subscription, window, used_pct, resets_at, raw, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            subscription,
            window,
            used_pct,
            resets_at,
            raw.to_string(),
            now()
        ],
    )?;
    Ok(())
}

/// The newest sample of each window of a subscription, by window name.
pub fn latest_windows(conn: &Connection, subscription: &str) -> ApiResult<Vec<QuotaSample>> {
    let mut stmt = conn.prepare(
        "SELECT q.* FROM quota_samples q
         WHERE q.subscription = ?1
           AND q.id = (SELECT max(id) FROM quota_samples
                       WHERE subscription = q.subscription AND window = q.window)
         ORDER BY q.window",
    )?;
    let rows = stmt.query_map([subscription], sample_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Samples of a subscription after `since` (an RFC 3339 time), oldest
/// first, at most `limit`.
pub fn samples(
    conn: &Connection,
    subscription: &str,
    window: Option<&str>,
    since: &str,
    limit: usize,
) -> ApiResult<Vec<QuotaSample>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM quota_samples
         WHERE subscription = ?1 AND at > ?2 AND (?3 IS NULL OR window = ?3)
         ORDER BY id LIMIT ?4",
    )?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = stmt.query_map(params![subscription, since, window, limit], sample_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A login session row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginRow {
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn login_row(r: &Row<'_>) -> rusqlite::Result<LoginRow> {
    Ok(LoginRow {
        id: r.get("id")?,
        kind: r.get("kind")?,
        provider: r.get("provider")?,
        state: r.get("state")?,
        url: r.get("url")?,
        device_code: r.get("device_code")?,
        subscription: r.get("subscription")?,
        started_at: r.get("started_at")?,
        finished_at: r.get("finished_at")?,
        error: r.get("error")?,
    })
}

pub fn insert_login(t: &mut Tx<'_>, id: &str, kind: &str, provider: Option<&str>) -> ApiResult<()> {
    t.execute(
        "INSERT INTO login_sessions (id, kind, provider, state, started_at)
         VALUES (?1, ?2, ?3, 'starting', ?4)",
        params![id, kind, provider, now()],
    )?;
    Ok(())
}

/// The final states of a login session.
pub const FINAL_LOGIN_STATES: [&str; 4] = ["done", "failed", "cancelled", "expired"];

/// Write the visible state of a login session. A final state sets
/// `finished_at` and clears `url` and `device_code`: the code must not stay
/// in the database and its backups (`docs/security-review.md` F6).
pub fn update_login(t: &mut Tx<'_>, l: &LoginRow) -> ApiResult<()> {
    let final_state = FINAL_LOGIN_STATES.contains(&l.state.as_str());
    t.execute(
        "UPDATE login_sessions SET state = ?2,
                url = CASE WHEN ?7 THEN NULL ELSE ?3 END,
                device_code = CASE WHEN ?7 THEN NULL ELSE ?4 END,
                subscription = ?5, error = ?6,
                finished_at = CASE WHEN ?7 THEN coalesce(finished_at, ?8) ELSE NULL END
         WHERE id = ?1",
        params![
            l.id,
            l.state,
            l.url,
            l.device_code,
            l.subscription,
            l.error,
            final_state,
            now()
        ],
    )?;
    Ok(())
}

pub fn get_login(conn: &Connection, id: &str) -> ApiResult<LoginRow> {
    conn.query_row(
        "SELECT * FROM login_sessions WHERE id = ?1",
        [id],
        login_row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("no login session `{id}`")))
}

/// Login sessions that a daemon stop left open become `expired`.
/// Also clears the `url` and `device_code` of every ended session: rows
/// from a version that kept them (`docs/security-review.md` F6).
pub fn expire_open_logins(t: &mut Tx<'_>) -> ApiResult<usize> {
    let n = t.execute(
        "UPDATE login_sessions SET state = 'expired', finished_at = ?1,
                error = coalesce(error, 'the daemon stopped during the login')
         WHERE state IN ('starting', 'waiting', 'verifying')",
        [now()],
    )?;
    t.execute(
        "UPDATE login_sessions SET url = NULL, device_code = NULL
         WHERE state NOT IN ('starting', 'waiting', 'verifying')
           AND (url IS NOT NULL OR device_code IS NOT NULL)",
        [],
    )?;
    Ok(n)
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

    fn def(name: &str) -> SubscriptionDef {
        toml::from_str(&format!(
            "name = \"{name}\"\nprovider = \"anthropic\"\nplan = \"Max 20x\"\nharnesses = [\"claude\", \"pi\"]\nprobe = \"anthropic-oauth-usage\"\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn sync_upserts_and_releases_file_rows() {
        let db = db();
        let changes = db
            .write("system", |t| sync_files(t, &[def("anthropic-max")]))
            .await
            .unwrap();
        assert_eq!(changes.upserted, ["anthropic-max"]);
        let s = db.read(|c| get(c, "anthropic-max")).await.unwrap();
        assert_eq!(s.source, "file");
        assert_eq!(s.harnesses, [HarnessId::Claude, HarnessId::Pi]);
        assert_eq!(s.alert_at, 80);
        assert_eq!(s.probe_status, "none");
        // Daemon columns survive a sync.
        db.write("system", |t| {
            set_env_var(t, "anthropic-max", Some("ANTHROPIC_API_KEY"))?;
            set_probe_result(t, "anthropic-max", Ok(()))
        })
        .await
        .unwrap();
        db.write("system", |t| sync_files(t, &[def("anthropic-max")]))
            .await
            .unwrap();
        let s = db.read(|c| get(c, "anthropic-max")).await.unwrap();
        assert_eq!(s.env_var.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(s.probe_status, "ok");
        assert!(s.last_success_at.is_some());
        // The file is gone: the row stays as an API row.
        let changes = db.write("system", |t| sync_files(t, &[])).await.unwrap();
        assert_eq!(changes.released, ["anthropic-max"]);
        assert_eq!(
            db.read(|c| get(c, "anthropic-max")).await.unwrap().source,
            "api"
        );
    }

    #[tokio::test]
    async fn failed_probe_keeps_last_success() {
        let db = db();
        db.write("system", |t| {
            upsert_def(t, &def("s1"), SOURCE_API)?;
            set_probe_result(t, "s1", Ok(()))?;
            set_probe_result(t, "s1", Err("401"))
        })
        .await
        .unwrap();
        let s = db.read(|c| get(c, "s1")).await.unwrap();
        assert_eq!(s.probe_status, "failed");
        assert_eq!(s.probe_error.as_deref(), Some("401"));
        assert!(s.last_success_at.is_some());
    }

    #[tokio::test]
    async fn latest_window_per_name() {
        let db = db();
        db.write("system", |t| {
            upsert_def(t, &def("s1"), SOURCE_API)?;
            insert_sample(
                t,
                "s1",
                "5h",
                10.0,
                Some("2026-10-24T10:00:00Z"),
                &Value::Null,
            )?;
            insert_sample(t, "s1", "7d", 5.0, None, &Value::Null)?;
            insert_sample(
                t,
                "s1",
                "5h",
                20.0,
                Some("2026-10-24T10:00:00Z"),
                &Value::Null,
            )
        })
        .await
        .unwrap();
        let w = db.read(|c| latest_windows(c, "s1")).await.unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].window.as_str(), w[0].used_pct), ("5h", 20.0));
        assert_eq!((w[1].window.as_str(), w[1].used_pct), ("7d", 5.0));
        let all = db
            .read(|c| samples(c, "s1", Some("5h"), "", 10))
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn logout_after_login_expires_once_and_mails_once() {
        let db = db();
        let ops = |c: &Connection| {
            let f = crate::store::mail::MailFilter {
                to: Some("operator".into()),
                ..Default::default()
            };
            crate::store::mail::list(c, &f, &crate::store::Page::default())
        };
        db.write("system", |t| {
            upsert_def(t, &def("s1"), SOURCE_API)?;
            // Never logged in: missing, no mail.
            assert!(!record_login_check(t, "s1", false)?);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            db.read(|c| get(c, "s1")).await.unwrap().login_status,
            MISSING
        );
        assert!(db.read(ops).await.unwrap().items.is_empty());
        db.write("system", |t| {
            record_login_check(t, "s1", true)?;
            // Logged out after a login: expired, one mail.
            assert!(record_login_check(t, "s1", false)?);
            assert!(!record_login_check(t, "s1", false)?);
            // A refused probe on an expired login does not mail again.
            assert!(!mark_expired(t, "s1", "HTTP 401")?);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            db.read(|c| get(c, "s1")).await.unwrap().login_status,
            EXPIRED
        );
        let mail = db.read(ops).await.unwrap().items;
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].from, "system");
        assert_eq!(mail[0].subject, "Login expired: s1");
        // A good probe clears it; the next refusal mails again.
        db.write("system", |t| {
            clear_expired(t, "s1")?;
            assert_eq!(get(t, "s1")?.login_status, LOGGED_IN);
            assert!(mark_expired(t, "s1", "HTTP 401")?);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(db.read(ops).await.unwrap().items.len(), 2);
    }

    #[tokio::test]
    async fn zai_plans_are_checked() {
        let db = db();
        let zai = |plan: Option<&str>| SubscriptionDef {
            name: "glm".into(),
            provider: "zai".into(),
            plan: plan.map(str::to_owned),
            harnesses: vec![HarnessId::Claude, HarnessId::Pi],
            probe: None,
            alert_at: 80,
        };
        for plan in [None, Some("coding"), Some("api")] {
            let d = zai(plan);
            db.write("system", move |t| upsert_def(t, &d, SOURCE_API))
                .await
                .unwrap();
        }
        let d = zai(Some("Max 20x"));
        let err = db
            .write("system", move |t| upsert_def(t, &d, SOURCE_API))
            .await
            .unwrap_err();
        assert_eq!(err.code, crate::error::Code::Invalid);
        assert!(err.message.contains("coding"), "{}", err.message);
        // The new login state word is stored.
        db.write("system", |t| set_login_status(t, "glm", API_KEY))
            .await
            .unwrap();
        assert_eq!(
            db.read(|c| get(c, "glm")).await.unwrap().login_status,
            API_KEY
        );
    }

    #[tokio::test]
    async fn zai_coding_plan_gets_the_usage_probe() {
        let db = db();
        let zai = |name: &str, plan: Option<&str>, probe: Option<&str>| SubscriptionDef {
            name: name.into(),
            provider: "zai".into(),
            plan: plan.map(str::to_owned),
            harnesses: vec![HarnessId::Claude],
            probe: probe.map(str::to_owned),
            alert_at: 80,
        };
        db.write("t", move |t| {
            upsert_def(t, &zai("zc", Some("coding"), None), "api")?;
            upsert_def(t, &zai("zn", None, None), "api")?;
            upsert_def(t, &zai("za", Some("api"), None), "api")?;
            upsert_def(t, &zai("zx", Some("coding"), Some("other")), "api")?;
            Ok(())
        })
        .await
        .unwrap();
        let probe = |n: &'static str| {
            let db = db.clone();
            async move { db.read(move |c| get(c, n)).await.unwrap().probe }
        };
        assert_eq!(probe("zc").await.as_deref(), Some("zai-usage"));
        assert_eq!(probe("zn").await.as_deref(), Some("zai-usage"));
        assert_eq!(probe("za").await, None);
        assert_eq!(probe("zx").await.as_deref(), Some("other"));
        // The stored column stays NULL: the default is applied on read.
        let stored: Option<String> = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT probe FROM subscriptions WHERE name = 'zc'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(stored, None);
        assert_eq!(default_probe("anthropic", None), None);
    }

    #[tokio::test]
    async fn names_are_checked() {
        let db = db();
        for bad in ["login", "detect", "Bad"] {
            let mut d = def("x");
            d.name = bad.into();
            assert!(
                db.write("system", move |t| upsert_def(t, &d, SOURCE_API))
                    .await
                    .is_err()
            );
        }
    }
}
