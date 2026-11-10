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
use terrarium_core::selfhosted::{self, SelfHosted};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::json_col;
use crate::store::projects::{SOURCE_API, SOURCE_FILE};

/// Names that collide with the fixed routes under `/api/subscriptions/`.
pub const RESERVED_NAMES: [&str; 3] = ["login", "detect", "fetch-models"];

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
    /// The server of a `selfhosted` subscription
    /// (`terrarium_core::selfhosted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selfhosted: Option<SelfHosted>,
    /// False: no agent on the subscription starts or wakes, and no new
    /// agent takes it. Agents that run go on. Daemon state, not part of
    /// the definition.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The settings of a cloud subscription (`terrarium_core::cloud`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<terrarium_core::cloud::CloudSettings>,
    pub created_at: String,
    pub updated_at: String,
}

impl Subscription {
    /// The definition columns, as the adapters read them.
    pub fn def(&self) -> SubscriptionDef {
        SubscriptionDef {
            name: self.name.clone(),
            provider: self.provider.clone(),
            plan: self.plan.clone(),
            harnesses: self.harnesses.clone(),
            probe: self.probe.clone(),
            alert_at: self.alert_at,
            selfhosted: self.selfhosted.clone(),
            cloud: self.cloud.clone(),
        }
    }

    /// True for a `selfhosted` subscription.
    pub fn is_selfhosted(&self) -> bool {
        selfhosted::is_selfhosted(&self.provider)
    }

    /// The kind of a cloud subscription (`bedrock`, `gcp`, `foundry`). A
    /// cloud subscription is never in the automatic account choice: an
    /// agent or a template uses it only when it names it.
    pub fn cloud_kind(&self) -> Option<terrarium_core::cloud::Kind> {
        terrarium_core::cloud::kind(&self.provider)
    }

    /// The most agents of a `selfhosted` subscription that may run a turn
    /// at the same time, when it sets a limit.
    pub fn max_concurrent(&self) -> Option<u32> {
        self.selfhosted.as_ref().and_then(|s| s.max_concurrent)
    }

    /// True when the subscription is a plan login (Claude Pro/Max,
    /// ChatGPT, Copilot, any OAuth login), not an API key, as pi sees it
    /// (`terrarium_adapter_pi::provider::is_plan`).
    pub fn is_plan(&self) -> bool {
        terrarium_adapter_pi::provider::is_plan(&self.def())
    }
}

/// The harnesses that may spend a subscription of `def`: its `harnesses`,
/// without `pi` when it is a plan login. pi uses API keys only (PLAN
/// §16.3), so a plan never lists pi, also when its file or an older row
/// says so.
pub fn spendable_harnesses(def: &SubscriptionDef) -> Vec<HarnessId> {
    let mut out = def.harnesses.clone();
    if out.contains(&HarnessId::Pi) && terrarium_adapter_pi::provider::is_plan(def) {
        out.retain(|h| *h != HarnessId::Pi);
    }
    out
}

fn yes() -> bool {
    true
}

/// Why `harness` must not spend `sub`: pi and a plan login (PLAN §16.3),
/// a harness that a self-hosted server does not serve
/// (`terrarium_core::selfhosted`, for example Copilot, or Claude Code on
/// an OpenAI-style server), and a harness other than Claude Code on a
/// cloud subscription (`terrarium_core::cloud`).
pub fn plan_refusal(harness: HarnessId, sub: &Subscription) -> Option<String> {
    if sub.cloud_kind().is_some() {
        return terrarium_core::cloud::harness_refusal(&[harness])
            .map(|why| format!("subscription `{}`: {why}", sub.name));
    }
    if sub.is_selfhosted() {
        let api = sub.selfhosted.as_ref().map(|s| s.api).unwrap_or_default();
        return (!api.harnesses().contains(&harness)).then(|| {
            format!(
                "subscription `{}`: {}",
                sub.name,
                selfhosted::refusal_text(api, harness)
            )
        });
    }
    (harness == HarnessId::Pi && sub.is_plan()).then(|| {
        format!(
            "subscription `{}`: {}",
            sub.name,
            terrarium_adapter_pi::provider::PLAN_REFUSAL
        )
    })
}

/// Why an agent with `model` cannot run on `sub`: a cloud subscription
/// (`terrarium_core::cloud`) whose model map has no entry for the name,
/// which is not an alias either ([`terrarium_core::cloud::model_refusal`]).
pub fn model_refusal(sub: &Subscription, model: Option<&str>) -> Option<String> {
    sub.cloud_kind()?;
    let models = sub.cloud.as_ref().map(|c| c.models.clone()).unwrap_or_default();
    terrarium_core::cloud::model_refusal(&sub.name, &models, model)
}

/// Why no new work may start on `sub`: it is disabled.
pub fn disabled_refusal(sub: &Subscription) -> Option<String> {
    (!sub.enabled).then(|| {
        format!(
            "subscription `{}` is disabled; enable it first (terrarium admin subscription \
             enable {})",
            sub.name, sub.name
        )
    })
}

/// The status reason of an agent whose subscription is disabled.
pub const REASON_DISABLED: &str = "subscription_disabled";

/// The probe of a subscription that names none: `zai-usage` for a Z.ai
/// GLM Coding Plan (the `api` plan has no quota endpoint),
/// `moonshot-balance` or `moonshot-cn-balance` for a Moonshot Open
/// Platform plan, `kimi-code-usage` for a Kimi Code plan, and
/// `openrouter-credits` for OpenRouter. Applied when a row is read, so rows
/// written before the probe existed get it too.
pub fn default_probe(provider: &str, plan: Option<&str>) -> Option<String> {
    use terrarium_core::zai;
    if zai::is_zai(provider) && zai::plan_or_default(plan) == zai::Plan::Coding {
        return Some(crate::probes::zai::NAME.to_owned());
    }
    if terrarium_core::moonshot::is_moonshot(provider) {
        use terrarium_core::moonshot::{Plan, plan_or_default};
        return match plan_or_default(plan) {
            Plan::Api => Some(crate::probes::moonshot::NAME.to_owned()),
            Plan::ApiCn => Some(crate::probes::moonshot::NAME_CN.to_owned()),
            Plan::Coding | Plan::CodingCn => Some(crate::probes::kimi::NAME.to_owned()),
        };
    }
    provider
        .trim()
        .eq_ignore_ascii_case("openrouter")
        .then(|| crate::probes::openrouter::NAME.to_owned())
}

fn row(r: &Row<'_>) -> rusqlite::Result<Subscription> {
    let harnesses: String = r.get("harnesses")?;
    let alert_at: i64 = r.get("alert_at")?;
    let provider: String = r.get("provider")?;
    let plan: Option<String> = r.get("plan")?;
    let probe: Option<String> = r.get("probe")?;
    let sh: Option<String> = r.get("selfhosted")?;
    let cloud: Option<String> = r.get("cloud")?;
    let mut s = Subscription {
        cloud: cloud.and_then(|t| serde_json::from_str(&t).ok()),
        selfhosted: sh.and_then(|t| serde_json::from_str(&t).ok()),
        enabled: r.get("enabled")?,
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
    };
    // A row from before pi took API keys only can list pi on a plan.
    s.harnesses = spendable_harnesses(&s.def());
    Ok(s)
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
    let def = &normalize_selfhosted(t, def)?;
    if terrarium_core::zai::is_zai(&def.provider) {
        terrarium_core::zai::parse_plan(def.plan.as_deref()).map_err(ApiError::invalid)?;
    }
    check_cloud(def)?;
    // Moonshot is for pi only (an operator decision; `docs/PLAN.md` §16).
    if terrarium_core::moonshot::is_moonshot(&def.provider) {
        terrarium_core::moonshot::parse_plan(def.plan.as_deref()).map_err(ApiError::invalid)?;
        if let Some(why) = terrarium_core::moonshot::harness_refusal(&def.harnesses) {
            return Err(ApiError::invalid(format!(
                "subscription `{}`: {why}",
                def.name
            )));
        }
    }
    let at = now();
    let harnesses = serde_json::to_string(&spendable_harnesses(def))?;
    let sh = def
        .selfhosted
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let cloud = def.cloud.as_ref().map(serde_json::to_string).transpose()?;
    t.execute(
        "INSERT INTO subscriptions (name, provider, plan, harnesses, probe, alert_at, source,
                                    created_at, updated_at, selfhosted, cloud)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10)
         ON CONFLICT (name) DO UPDATE SET
             provider = excluded.provider, plan = excluded.plan,
             harnesses = excluded.harnesses, probe = excluded.probe,
             alert_at = excluded.alert_at,
             selfhosted = coalesce(excluded.selfhosted, subscriptions.selfhosted),
             cloud = coalesce(excluded.cloud, subscriptions.cloud),
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
            at,
            sh,
            cloud
        ],
    )?;
    get(t, &def.name)
}

/// A cloud definition (`bedrock`, `gcp`, `foundry`) is for Claude Code
/// only and has checked `[cloud]` settings; a definition of another
/// provider has no `[cloud]`. A cloud definition with no `[cloud]` keeps
/// the stored settings (the upsert keeps them), so it is checked only
/// when it has them.
fn check_cloud(def: &SubscriptionDef) -> ApiResult<()> {
    use terrarium_core::cloud;
    let Some(kind) = cloud::kind(&def.provider) else {
        if def.cloud.is_some() {
            return Err(ApiError::invalid(format!(
                "subscription `{}`: `cloud` is only for the providers bedrock, gcp, and foundry, \
                 not `{}`",
                def.name, def.provider
            )));
        }
        return Ok(());
    };
    if let Some(why) = cloud::harness_refusal(&def.harnesses) {
        return Err(ApiError::invalid(format!(
            "subscription `{}`: {why}",
            def.name
        )));
    }
    if def.plan.is_some() {
        return Err(ApiError::invalid(format!(
            "subscription `{}`: a cloud subscription has no plan",
            def.name
        )));
    }
    if let Some(s) = &def.cloud {
        cloud::check(kind, s)
            .map_err(|e| ApiError::invalid(format!("subscription `{}`: {e}", def.name)))?;
    }
    Ok(())
}

/// Set the `[cloud]` settings of a subscription (the key endpoint gives
/// them with the key).
pub fn set_cloud(
    t: &mut Tx<'_>,
    name: &str,
    cloud: &terrarium_core::cloud::CloudSettings,
) -> ApiResult<()> {
    t.execute(
        "UPDATE subscriptions SET cloud = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, serde_json::to_string(cloud)?, now()],
    )?;
    Ok(())
}

/// A `selfhosted` (or `openai-compatible`) definition in its stored form:
/// the provider `selfhosted`, a checked server, and harnesses that the
/// API style serves (none given: all of them). A definition with no
/// server keeps the stored one (a file from before `[selfhosted]`, whose
/// base URL the migration moved from the key file); with neither, `422`.
/// Every other definition with a `selfhosted` table is `422`.
fn normalize_selfhosted(t: &Tx<'_>, def: &SubscriptionDef) -> ApiResult<SubscriptionDef> {
    if !selfhosted::is_selfhosted(&def.provider) {
        if def.selfhosted.is_some() {
            return Err(ApiError::invalid(format!(
                "subscription `{}`: `selfhosted` is only for the provider selfhosted, not `{}`",
                def.name, def.provider
            )));
        }
        return Ok(def.clone());
    }
    let mut d = def.clone();
    d.provider = selfhosted::PROVIDER.to_owned();
    if d.plan.is_some() && !terrarium_adapter_pi::provider::is_api_key(&d) {
        return Err(ApiError::invalid(format!(
            "subscription `{}`: a self-hosted subscription has no plan",
            d.name
        )));
    }
    d.plan = None;
    let stored = find(t, &d.name)?.and_then(|s| s.selfhosted);
    let sh = match d.selfhosted.take().or(stored) {
        Some(s) => s
            .checked()
            .map_err(|e| ApiError::invalid(format!("subscription `{}`: {e}", d.name)))?,
        None => {
            return Err(ApiError::invalid(format!(
                "subscription `{}`: a self-hosted subscription needs `selfhosted.base_url`",
                d.name
            )));
        }
    };
    if d.harnesses.is_empty() {
        d.harnesses = sh.api.harnesses().to_vec();
    }
    if let Some(why) = selfhosted::harnesses_refusal(sh.api, &d.harnesses) {
        return Err(ApiError::invalid(format!(
            "subscription `{}`: {why}",
            d.name
        )));
    }
    if d.probe.is_some() {
        return Err(ApiError::invalid(format!(
            "subscription `{}`: a self-hosted server has no quota probe",
            d.name
        )));
    }
    d.selfhosted = Some(sh);
    Ok(d)
}

/// Replace the server of a `selfhosted` subscription (`PATCH`), and the
/// harnesses when `harnesses` is given. Checked as a create is.
pub fn set_selfhosted(
    t: &mut Tx<'_>,
    name: &str,
    sh: SelfHosted,
    harnesses: Option<Vec<HarnessId>>,
) -> ApiResult<Subscription> {
    let s = get(t, name)?;
    if !s.is_selfhosted() {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` is {}, not selfhosted",
            s.provider
        )));
    }
    let mut def = s.def();
    def.selfhosted = Some(sh);
    def.harnesses = harnesses.unwrap_or_else(|| {
        // A new API style can drop harnesses: keep the ones it serves.
        let api = def.selfhosted.as_ref().map(|x| x.api).unwrap_or_default();
        s.harnesses
            .iter()
            .copied()
            .filter(|h| api.harnesses().contains(h))
            .collect()
    });
    let def = normalize_selfhosted(t, &def)?;
    let sh = def
        .selfhosted
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    t.execute(
        "UPDATE subscriptions SET selfhosted = ?2, harnesses = ?3, updated_at = ?4
         WHERE name = ?1",
        params![name, sh, serde_json::to_string(&def.harnesses)?, now()],
    )?;
    get(t, name)
}

/// Set the alert threshold of a subscription.
pub fn set_alert_at(t: &mut Tx<'_>, name: &str, alert_at: u8) -> ApiResult<()> {
    t.execute(
        "UPDATE subscriptions SET alert_at = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, i64::from(alert_at), now()],
    )?;
    Ok(())
}

/// Enable or disable a subscription. Returns true when it changed.
pub fn set_enabled(t: &mut Tx<'_>, name: &str, enabled: bool) -> ApiResult<bool> {
    let s = get(t, name)?;
    if s.enabled == enabled {
        return Ok(false);
    }
    t.execute(
        "UPDATE subscriptions SET enabled = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, enabled, now()],
    )?;
    Ok(true)
}

/// Give each `selfhosted` row with no server one: a row that migration 46
/// renamed from `openai-compatible` keeps its base URL and models in the
/// key file (`login_session::ApiKey`). Once, at the start; idempotent.
/// Returns the names it changed.
pub fn migrate_openai_compatible(
    t: &mut Tx<'_>,
    state_dir: &std::path::Path,
) -> ApiResult<Vec<String>> {
    let rows: Vec<String> = {
        let mut stmt =
            t.prepare("SELECT name FROM subscriptions WHERE provider = ?1 AND selfhosted IS NULL")?;
        let rows = stmt.query_map([selfhosted::PROVIDER], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut out = Vec::new();
    for name in rows {
        let Some(key) = crate::login_session::read_key(state_dir, &name) else {
            continue;
        };
        let Some(base_url) = key.base_url.clone() else {
            continue;
        };
        let sh = SelfHosted {
            api: selfhosted::Api::Openai,
            base_url,
            models: key.models.clone(),
            ..SelfHosted::default()
        };
        let Ok(sh) = sh.checked() else {
            tracing::warn!(subscription = %name, "the stored base URL of the openai-compatible subscription does not parse; set it again");
            continue;
        };
        let current = get(t, &name)?;
        let harnesses: Vec<HarnessId> = current
            .harnesses
            .iter()
            .copied()
            .filter(|h| sh.api.harnesses().contains(h))
            .collect();
        let harnesses = if harnesses.is_empty() {
            sh.api.harnesses().to_vec()
        } else {
            harnesses
        };
        t.execute(
            "UPDATE subscriptions SET selfhosted = ?2, harnesses = ?3, plan = NULL,
                    env_var = CASE WHEN env_var IS NULL THEN NULL ELSE ?4 END, updated_at = ?5
             WHERE name = ?1",
            params![
                name,
                serde_json::to_string(&sh)?,
                serde_json::to_string(&harnesses)?,
                selfhosted::KEY_ENV,
                now()
            ],
        )?;
        t.audit(
            "subscription.migrate",
            &name,
            None,
            serde_json::json!({"from": "openai-compatible", "to": "selfhosted", "api": "openai"}),
        )?;
        out.push(name);
    }
    Ok(out)
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
    clear_default(t, name)?;
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

/// The longest display name that the operator can give.
pub const LABEL_MAX: usize = 80;

/// The display name that the operator gave a subscription, if any
/// (migration 47).
pub fn label(conn: &Connection, name: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT label FROM subscriptions WHERE name = ?1",
            [name],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Set or clear (`None`, or only spaces) the display name of a
/// subscription. It is text for people only: the name, the ids, and the
/// keys stay. Returns the stored value.
pub fn set_label(t: &mut Tx<'_>, name: &str, label: Option<&str>) -> ApiResult<Option<String>> {
    get(t, name)?;
    let label = label.map(str::trim).filter(|l| !l.is_empty());
    if let Some(l) = label {
        if l.chars().count() > LABEL_MAX {
            return Err(ApiError::invalid(format!(
                "the label is longer than {LABEL_MAX} characters"
            )));
        }
        if l.chars().any(char::is_control) {
            return Err(ApiError::invalid("the label has a control character"));
        }
    }
    t.execute(
        "UPDATE subscriptions SET label = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, label, now()],
    )?;
    Ok(label.map(str::to_owned))
}

/// The name of a subscription for people: the operator's label, else the
/// vendor's product and plan name
/// (`terrarium_core::providers::display_name`), with the plan tier that
/// the last probe answer names (the `level` of the Z.ai, GitHub Copilot,
/// and Kimi Code probes).
pub fn display_name(conn: &Connection, sub: &Subscription) -> ApiResult<String> {
    if let Some(l) = label(conn, &sub.name)? {
        return Ok(l);
    }
    let tier = latest_windows(conn, &sub.name)?.iter().find_map(|q| {
        q.raw
            .get("level")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    Ok(terrarium_core::providers::display_name(
        &sub.provider,
        sub.plan.as_deref(),
        tier.as_deref(),
        sub.selfhosted.as_ref(),
        &sub.name,
    ))
}

/// Take the plan that a login check read (`login_session::claude_plan`,
/// `codex_plan`) when it differs from the stored one: a plan that the
/// vendor renamed (Anthropic's Max 20x is Ultra since 2026-10-30) or
/// changed shows at the next check, with no new login. Only for a plan
/// login (Anthropic, OpenAI, GitHub) and not for a file subscription
/// (the file owns its plan). Returns true when it changed.
pub fn refresh_plan(t: &mut Tx<'_>, name: &str, plan: Option<&str>) -> ApiResult<bool> {
    let Some(plan) = plan.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(false);
    };
    let s = get(t, name)?;
    let login_plan = ["anthropic", "openai", "github"]
        .iter()
        .any(|p| s.provider.trim().eq_ignore_ascii_case(p));
    if !login_plan || s.source == SOURCE_FILE || s.plan.as_deref() == Some(plan) {
        return Ok(false);
    }
    set_plan(t, name, plan)?;
    Ok(true)
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
///
/// One pass over the subscription's index entries (TER-394). The older
/// correlated `max(id)` subquery scanned all samples of a window for each
/// sample, quadratic in the never-pruned `quota_samples`, and held the
/// database lock for many seconds on spawns and subscription lists.
pub fn latest_windows(conn: &Connection, subscription: &str) -> ApiResult<Vec<QuotaSample>> {
    let mut stmt = conn.prepare(
        "SELECT q.* FROM (SELECT max(id) AS id FROM quota_samples
                          WHERE subscription = ?1 GROUP BY window) m
         JOIN quota_samples q ON q.id = m.id
         ORDER BY q.window",
    )?;
    let rows = stmt.query_map([subscription], sample_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Remove each window of a subscription that is not in `keep`: its
/// samples and its quota notice rows. A successful probe calls it with
/// the windows of its answer, so a window that the provider stopped
/// reporting (the 5-hour window of an Anthropic Ultra plan) leaves the
/// lists, and no quota line can notify for it. Returns the removed window
/// names, by name.
pub fn retire_windows(t: &mut Tx<'_>, subscription: &str, keep: &[&str]) -> ApiResult<Vec<String>> {
    let mut stmt = t.prepare(
        "SELECT DISTINCT window FROM quota_samples WHERE subscription = ?1 ORDER BY window",
    )?;
    let had = stmt
        .query_map([subscription], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let gone: Vec<String> = had
        .into_iter()
        .filter(|w| !keep.contains(&w.as_str()))
        .collect();
    for w in &gone {
        t.execute(
            "DELETE FROM quota_samples WHERE subscription = ?1 AND window = ?2",
            params![subscription, w],
        )?;
        t.execute(
            "DELETE FROM quota_notices WHERE subscription = ?1 AND window = ?2",
            params![subscription, w],
        )?;
    }
    Ok(gone)
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

/// The most `quota_samples` rows that one prune transaction deletes
/// (TER-395): the daemon has one connection behind a mutex, so each batch
/// holds the lock only briefly.
pub const SAMPLE_PRUNE_BATCH: usize = 500;

/// [`sample_windows`]: one range of `quota_samples_sub`.
const SAMPLE_WINDOWS_SQL: &str = "SELECT window, max(id) FROM quota_samples
     WHERE subscription = ?1 GROUP BY window ORDER BY window";

/// [`prune_window_samples`]: one range of `quota_samples_sub`, then the
/// rows by ID.
const PRUNE_WINDOW_SQL: &str = "DELETE FROM quota_samples WHERE id IN (
     SELECT id FROM quota_samples
     WHERE subscription = ?1 AND window = ?2 AND at < ?3 AND id < ?4
     ORDER BY at LIMIT ?5)";

/// The windows of a subscription with the ID of each window's newest
/// sample (the row that [`latest_windows`] reads), by window name.
pub fn sample_windows(conn: &Connection, subscription: &str) -> ApiResult<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(SAMPLE_WINDOWS_SQL)?;
    let rows = stmt.query_map([subscription], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Delete at most `batch` samples of one window that are older than
/// `before` (an RFC 3339 time as stored) and older than the sample
/// `newest` (from [`sample_windows`]), which always stays. Returns the
/// rows deleted. The range read uses `quota_samples_sub`.
pub fn prune_window_samples(
    t: &Tx<'_>,
    subscription: &str,
    window: &str,
    newest: i64,
    before: &str,
    batch: usize,
) -> ApiResult<usize> {
    let batch = i64::try_from(batch).unwrap_or(i64::MAX);
    Ok(t.execute(
        PRUNE_WINDOW_SQL,
        params![subscription, window, before, newest, batch],
    )?)
}

// ---- Default plan accounts (PLAN §16.3) ----

/// The harnesses that have a default plan account. A pi agent names its
/// provider through a subscription, so pi has none.
pub const DEFAULT_HARNESSES: [HarnessId; 3] =
    [HarnessId::Claude, HarnessId::Codex, HarnessId::Copilot];

/// One plan account with a login, for [`settle_defaults`]. The caller
/// reads the account dirs (`crate::accounts::list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRef {
    pub subscription: String,
    pub harness: HarnessId,
    /// `created_at` of `account.json`. `None`: the subscription's
    /// `created_at` counts.
    pub created_at: Option<String>,
}

/// A change of the default account of one harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DefaultChange {
    pub harness: HarnessId,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// The row of `harness`: `None` when there is no row (the daemon has not
/// looked yet), `Some(None)` when the default went away.
fn default_row(conn: &Connection, harness: HarnessId) -> ApiResult<Option<Option<String>>> {
    Ok(conn
        .query_row(
            "SELECT subscription FROM account_defaults WHERE harness = ?1",
            [harness.as_str()],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?)
}

/// The default account of `harness`, when it has one.
pub fn default_for(conn: &Connection, harness: HarnessId) -> ApiResult<Option<String>> {
    Ok(default_row(conn, harness)?.flatten())
}

/// True when `name` is the default account of a harness.
pub fn is_default(conn: &Connection, name: &str) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM account_defaults WHERE subscription = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Write the default of `harness`, audit it, and emit a `subscription`
/// event. The caller checks that `name` is a plan account of `harness`.
/// `reason`: `set` (the API), `first_account` (a login made the first
/// account of the harness), or `backfill` (the oldest account, at a start).
fn write_default(
    t: &mut Tx<'_>,
    harness: HarnessId,
    name: &str,
    reason: &str,
) -> ApiResult<DefaultChange> {
    let from = default_for(t, harness)?;
    t.execute(
        "INSERT INTO account_defaults (harness, subscription, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (harness) DO UPDATE SET subscription = excluded.subscription,
             updated_at = excluded.updated_at",
        params![harness.as_str(), name, now()],
    )?;
    let change = DefaultChange {
        harness,
        from,
        to: Some(name.to_owned()),
    };
    let detail = serde_json::json!({
        "harness": harness, "from": change.from, "to": name, "reason": reason,
    });
    t.audit("subscription.default", name, None, detail.clone())?;
    let mut data = detail;
    data["change"] = "default".into();
    t.emit(crate::events::EventDraft::new(
        "subscription",
        format!("{name} is the default {harness} account"),
        data,
    ))?;
    Ok(change)
}

/// Make `name` the default account of `harness`. The old default stops
/// being one in the same transaction.
pub fn set_default(t: &mut Tx<'_>, harness: HarnessId, name: &str) -> ApiResult<DefaultChange> {
    if !DEFAULT_HARNESSES.contains(&harness) {
        return Err(ApiError::invalid(format!(
            "{harness} has no default account: a {harness} agent names its provider with a subscription"
        )));
    }
    get(t, name)?;
    write_default(t, harness, name, "set")
}

/// `name` stops being a default (its delete or its logout). The caller
/// then runs [`settle_defaults`], which makes the oldest account left the
/// default.
pub fn clear_default(t: &mut Tx<'_>, name: &str) -> ApiResult<Option<DefaultChange>> {
    let harness: Option<String> = t
        .query_row(
            "SELECT harness FROM account_defaults WHERE subscription = ?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    let Some(harness) = harness.and_then(|h| h.parse::<HarnessId>().ok()) else {
        return Ok(None);
    };
    t.execute(
        "UPDATE account_defaults SET subscription = NULL, updated_at = ?2 WHERE harness = ?1",
        params![harness.as_str(), now()],
    )?;
    t.emit(crate::events::EventDraft::new(
        "subscription",
        format!("{harness} has no default account: {name} is gone"),
        serde_json::json!({"change": "default_cleared", "harness": harness, "from": name, "to": null}),
    ))?;
    Ok(Some(DefaultChange {
        harness,
        from: Some(name.to_owned()),
        to: None,
    }))
}

/// Give a default to each harness that needs one (PLAN §16.3). `accounts`
/// are the plan accounts with a login.
///
/// - A harness with accounts and no row gets its oldest account (by the
///   `created_at` of the account, else of the subscription; then by name).
///   This is the backfill of the first start with defaults, and the first
///   account that the migration moves.
/// - A harness whose default went away (a NULL row, or a default with no
///   account dir) gets its oldest account left, also when one is left
///   (PLAN §16.3).
///
/// A second account never changes a default. Idempotent.
pub fn settle_defaults(
    t: &mut Tx<'_>,
    accounts: &[AccountRef],
    fresh: Option<&str>,
) -> ApiResult<Vec<DefaultChange>> {
    let mut out = Vec::new();
    for h in DEFAULT_HARNESSES {
        let mut mine: Vec<(String, &str)> = Vec::new();
        for a in accounts.iter().filter(|a| a.harness == h) {
            // An account dir with no subscription row cannot be a default.
            let Some(sub) = find(t, &a.subscription)? else {
                continue;
            };
            let at = a.created_at.clone().unwrap_or(sub.created_at);
            mine.push((at, &a.subscription));
        }
        mine.sort();
        let oldest = mine.first().map(|(_, n)| *n);
        let pick = match default_row(t, h)? {
            None => oldest.map(|n| (n, "backfill")),
            // The default went away (a delete or a logout): the oldest
            // account left takes over, also when it is the only one.
            Some(None) => oldest.map(|n| {
                if fresh == Some(n) && mine.len() == 1 {
                    (n, "first_account")
                } else {
                    (n, "replaced")
                }
            }),
            // A default whose account dir is gone counts as no default.
            Some(Some(d)) if !mine.iter().any(|(_, n)| *n == d) => oldest.map(|n| (n, "replaced")),
            Some(Some(_)) => None,
        };
        if let Some((name, reason)) = pick {
            out.push(write_default(t, h, name, reason)?);
        }
    }
    Ok(out)
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
        // pi uses API keys only: a plan that lists pi loses it.
        assert_eq!(s.harnesses, [HarnessId::Claude]);
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

    /// A row stored before pi took API keys only keeps `pi` in its
    /// column; the read drops it from a plan, and keeps it on an API key.
    #[tokio::test]
    async fn a_stored_plan_row_loses_pi_on_read() {
        let db = db();
        db.write("system", |t| {
            for (name, provider, plan) in [
                ("max", "anthropic", Some("Max 20x")),
                ("chatgpt", "openai", Some("ChatGPT Pro")),
                ("pi-openai-codex", "openai-codex", None),
                ("or", "openrouter", Some("API key")),
                ("ak", "anthropic", Some("API key")),
                ("glm", "zai", Some("coding")),
            ] {
                t.execute(
                    "INSERT INTO subscriptions (name, provider, plan, harnesses, alert_at, source,
                                                created_at, updated_at)
                     VALUES (?1, ?2, ?3, '[\"claude\",\"pi\"]', 80, 'api', '', '')",
                    params![name, provider, plan],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let got: Vec<(String, Vec<HarnessId>)> = db
            .read(list)
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.name, s.harnesses))
            .collect();
        let (c, p) = (HarnessId::Claude, HarnessId::Pi);
        assert_eq!(
            got,
            [
                ("ak".to_owned(), vec![c, p]),
                ("chatgpt".to_owned(), vec![c]),
                ("glm".to_owned(), vec![c, p]),
                ("max".to_owned(), vec![c]),
                ("or".to_owned(), vec![c, p]),
                ("pi-openai-codex".to_owned(), vec![c]),
            ]
        );
        let max = db.read(|c| get(c, "max")).await.unwrap();
        let why = plan_refusal(HarnessId::Pi, &max).unwrap();
        assert!(why.contains("pi uses API keys only"), "{why}");
        assert_eq!(plan_refusal(HarnessId::Claude, &max), None);
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

    /// TER-394: the newest window rows come from one pass over the index,
    /// with no correlated subquery per sample, so many samples stay fast.
    #[tokio::test]
    async fn latest_windows_stays_linear_with_many_samples() {
        let db = db();
        db.write("system", |t| {
            upsert_def(t, &def("s1"), SOURCE_API)?;
            upsert_def(t, &def("s2"), SOURCE_API)?;
            t.execute_batch(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<30000)
                 INSERT INTO quota_samples (subscription, window, used_pct, at)
                 SELECT 's1', CASE x % 3 WHEN 0 THEN '5h' WHEN 1 THEN '7d' ELSE '7d_opus' END,
                        x % 100, '2026-11-01T00:00:00Z' FROM n;",
            )?;
            insert_sample(t, "s1", "7d", 42.0, None, &Value::Null)?;
            insert_sample(t, "s2", "7d", 7.0, None, &Value::Null)
        })
        .await
        .unwrap();
        let started = std::time::Instant::now();
        let w = db.read(|c| latest_windows(c, "s1")).await.unwrap();
        let took = started.elapsed();
        assert_eq!(
            w.iter()
                .map(|w| (w.window.as_str(), w.used_pct))
                .collect::<Vec<_>>(),
            vec![("5h", 0.0), ("7d", 42.0), ("7d_opus", 99.0)]
        );
        // The quadratic query took about 20 s here; the linear one is
        // well under a second even on a loaded debug build.
        assert!(took < std::time::Duration::from_secs(3), "{took:?}");
        let plan: Vec<String> = db
            .read(|c| {
                let mut stmt = c.prepare(
                    "EXPLAIN QUERY PLAN SELECT q.* FROM (SELECT max(id) AS id FROM quota_samples
                          WHERE subscription = ?1 GROUP BY window) m
                     JOIN quota_samples q ON q.id = m.id ORDER BY q.window",
                )?;
                let rows = stmt.query_map(["s1"], |r| r.get::<_, String>(3))?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .await
            .unwrap();
        assert!(!plan.iter().any(|p| p.contains("CORRELATED")), "{plan:?}");
    }

    /// TER-395: the prune reads only index ranges of `quota_samples_sub`
    /// and rows by ID, never all of `quota_samples`, so each batch holds
    /// the lock briefly however large the table is.
    #[tokio::test]
    async fn the_sample_prune_queries_use_the_index() {
        let db = db();
        db.write("system", |t| {
            upsert_def(t, &def("s1"), SOURCE_API)?;
            insert_sample(t, "s1", "5h", 10.0, None, &Value::Null)?;
            insert_sample(t, "s1", "7d", 20.0, None, &Value::Null)
        })
        .await
        .unwrap();
        for (sql, args) in [(SAMPLE_WINDOWS_SQL, 1), (PRUNE_WINDOW_SQL, 5)] {
            let plan: Vec<String> = db
                .read(move |c| {
                    let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let values: Vec<rusqlite::types::Value> =
                        vec![rusqlite::types::Value::Text("s1".into()); args];
                    let rows = stmt.query_map(rusqlite::params_from_iter(values), |r| {
                        r.get::<_, String>(3)
                    })?;
                    Ok(rows.collect::<Result<_, _>>()?)
                })
                .await
                .unwrap();
            assert!(
                plan.iter().any(|p| p.contains("quota_samples_sub")),
                "{sql}: {plan:?}"
            );
            assert!(
                !plan.iter().any(|p| p.contains("SCAN quota_samples")),
                "{sql}: {plan:?}"
            );
        }
    }

    /// A login check renames the plan of an API subscription (Max 20x is
    /// Ultra now); a file subscription and a key plan keep theirs.
    #[tokio::test]
    async fn a_login_check_refreshes_the_plan_label() {
        let db = db();
        let changed = db
            .write("system", |t| {
                upsert_def(t, &def("api"), SOURCE_API)?;
                upsert_def(t, &def("file"), SOURCE_FILE)?;
                let mut z = def("z");
                z.provider = "zai".into();
                z.plan = Some("coding".into());
                z.harnesses = vec![HarnessId::Claude];
                upsert_def(t, &z, SOURCE_API)?;
                Ok(vec![
                    refresh_plan(t, "api", Some("Ultra"))?,
                    refresh_plan(t, "api", Some("Ultra"))?,
                    refresh_plan(t, "api", None)?,
                    refresh_plan(t, "file", Some("Ultra"))?,
                    refresh_plan(t, "z", Some("Max"))?,
                ])
            })
            .await
            .unwrap();
        assert_eq!(changed, vec![true, false, false, false, false]);
        let plan = |n: &'static str| db.read(move |c| Ok(get(c, n)?.plan));
        assert_eq!(plan("api").await.unwrap().as_deref(), Some("Ultra"));
        assert_eq!(plan("file").await.unwrap().as_deref(), Some("Max 20x"));
        assert_eq!(plan("z").await.unwrap().as_deref(), Some("coding"));
    }

    /// Any probe: a window that the last answer does not have goes, with
    /// its samples and its quota notice rows. Other subscriptions keep
    /// theirs.
    #[tokio::test]
    async fn a_window_that_the_probe_stops_reporting_goes() {
        let db = db();
        db.write("system", |t| {
            for name in ["s1", "s2"] {
                upsert_def(t, &def(name), SOURCE_API)?;
                for w in ["5h", "7d", "7d_opus"] {
                    insert_sample(t, name, w, 95.0, None, &Value::Null)?;
                    t.execute(
                        "INSERT INTO quota_notices (subscription, window, threshold, used_pct, at)
                         VALUES (?1, ?2, 90, 95, '2026-11-01T00:00:00Z')",
                        params![name, w],
                    )?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
        let gone = db
            .write("system", |t| retire_windows(t, "s1", &["7d", "7d_fable"]))
            .await
            .unwrap();
        assert_eq!(gone, vec!["5h".to_owned(), "7d_opus".to_owned()]);
        let w = db.read(|c| latest_windows(c, "s1")).await.unwrap();
        assert_eq!(
            w.iter().map(|w| w.window.as_str()).collect::<Vec<_>>(),
            vec!["7d"]
        );
        assert!(
            db.read(|c| samples(c, "s1", Some("5h"), "", 10))
                .await
                .unwrap()
                .is_empty()
        );
        let notices = |sub: &'static str| {
            db.read(move |c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM quota_notices WHERE subscription = ?1",
                    [sub],
                    |r| r.get::<_, i64>(0),
                )?)
            })
        };
        assert_eq!(notices("s1").await.unwrap(), 1);
        assert_eq!(notices("s2").await.unwrap(), 3);
        assert_eq!(db.read(|c| latest_windows(c, "s2")).await.unwrap().len(), 3);
        // Nothing more to remove.
        let gone = db
            .write("system", |t| retire_windows(t, "s1", &["7d"]))
            .await
            .unwrap();
        assert!(gone.is_empty());
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
            selfhosted: None,
            cloud: None,
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
    async fn moonshot_plans_and_harnesses_are_checked() {
        let db = db();
        let ms = |plan: Option<&str>, harnesses: Vec<HarnessId>| SubscriptionDef {
            name: "kimi".into(),
            provider: "moonshot".into(),
            plan: plan.map(str::to_owned),
            harnesses,
            probe: None,
            alert_at: 80,
            selfhosted: None,
            cloud: None,
        };
        for plan in [
            None,
            Some("coding"),
            Some("coding-cn"),
            Some("api"),
            Some("api-cn"),
        ] {
            let d = ms(plan, vec![HarnessId::Pi]);
            db.write("system", move |t| upsert_def(t, &d, SOURCE_API))
                .await
                .unwrap();
        }
        let d = ms(Some("Allegretto"), vec![HarnessId::Pi]);
        let err = db
            .write("system", move |t| upsert_def(t, &d, SOURCE_API))
            .await
            .unwrap_err();
        assert_eq!(err.code, crate::error::Code::Invalid);
        assert!(err.message.contains("coding-cn"), "{}", err.message);
        // pi only: Claude Code, Codex and Copilot are refused, alone or
        // beside pi, from the API and from a file.
        for h in [HarnessId::Claude, HarnessId::Codex, HarnessId::Copilot] {
            for (harnesses, source) in [(vec![h], SOURCE_API), (vec![HarnessId::Pi, h], "file")] {
                let d = ms(Some("api"), harnesses);
                let err = db
                    .write("system", move |t| upsert_def(t, &d, source))
                    .await
                    .unwrap_err();
                assert_eq!(err.code, crate::error::Code::Invalid);
                assert!(err.message.contains("pi harness only"), "{}", err.message);
                assert!(err.message.contains(h.as_str()), "{}", err.message);
            }
        }
        assert_eq!(
            db.read(|c| get(c, "kimi")).await.unwrap().harnesses,
            [HarnessId::Pi]
        );
    }

    #[tokio::test]
    async fn moonshot_plans_get_the_probe_of_their_product() {
        assert_eq!(
            default_probe("moonshot", Some("api")).as_deref(),
            Some("moonshot-balance")
        );
        assert_eq!(
            default_probe("Moonshot", Some("api-cn")).as_deref(),
            Some("moonshot-cn-balance")
        );
        // Both Kimi Code plans read the same usage endpoint.
        for plan in [Some("coding"), Some("coding-cn"), None] {
            assert_eq!(
                default_probe("moonshot", plan).as_deref(),
                Some("kimi-code-usage"),
                "{plan:?}"
            );
        }
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
            selfhosted: None,
            cloud: None,
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

    fn acct(name: &str, harness: HarnessId, at: Option<&str>) -> AccountRef {
        AccountRef {
            subscription: name.into(),
            harness,
            created_at: at.map(str::to_owned),
        }
    }

    async fn defaults(db: &Db) -> Vec<(String, Option<String>)> {
        db.read(|c| {
            let mut stmt =
                c.prepare("SELECT harness, subscription FROM account_defaults ORDER BY harness")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
    }

    async fn default_audits(db: &Db) -> Vec<Value> {
        db.read(|c| {
            let mut stmt = c.prepare(
                "SELECT detail FROM audit WHERE action = 'subscription.default' ORDER BY rowid",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows
                .map(|r| r.map(|d| serde_json::from_str(&d).unwrap()))
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
    }

    /// PLAN §16.3: the first start with defaults makes the oldest account
    /// of each harness its default, once. pi gets none.
    #[tokio::test]
    async fn the_backfill_picks_the_oldest_account_once() {
        let db = db();
        db.write("system", |t| {
            for n in [
                "max-new",
                "max-old",
                "max-nometa",
                "pro",
                "gh",
                "pi-a",
                "orphan",
            ] {
                upsert_def(t, &def(n), SOURCE_API)?;
            }
            // `orphan` has no row: its account cannot be a default.
            delete(t, "orphan")
        })
        .await
        .unwrap();
        let accounts = vec![
            acct(
                "max-new",
                HarnessId::Claude,
                Some("2026-10-02T00:00:00.000Z"),
            ),
            acct(
                "max-old",
                HarnessId::Claude,
                Some("2026-10-01T00:00:00.000Z"),
            ),
            // No `created_at`: the subscription's counts (now, the newest).
            acct("max-nometa", HarnessId::Claude, None),
            acct("orphan", HarnessId::Codex, Some("2000-01-01T00:00:00.000Z")),
            acct("pro", HarnessId::Codex, Some("2026-10-05T00:00:00.000Z")),
            acct("gh", HarnessId::Copilot, None),
            acct("pi-a", HarnessId::Pi, Some("2000-01-01T00:00:00.000Z")),
        ];
        let a = accounts.clone();
        let changes = db
            .write("system", move |t| settle_defaults(t, &a, None))
            .await
            .unwrap();
        assert_eq!(changes.len(), 3, "{changes:?}");
        assert_eq!(
            defaults(&db).await,
            [
                ("claude".to_owned(), Some("max-old".to_owned())),
                ("codex".to_owned(), Some("pro".to_owned())),
                ("copilot".to_owned(), Some("gh".to_owned())),
            ]
        );
        let audits = default_audits(&db).await;
        assert_eq!(audits.len(), 3);
        assert_eq!(audits[0]["reason"], "backfill");
        assert_eq!(audits[0]["from"], Value::Null);
        // Idempotent: a second pass changes nothing.
        let a = accounts.clone();
        let again = db
            .write("system", move |t| settle_defaults(t, &a, None))
            .await
            .unwrap();
        assert!(again.is_empty());
        assert_eq!(default_audits(&db).await.len(), 3);
        assert!(db.read(|c| is_default(c, "max-old")).await.unwrap());
        assert!(!db.read(|c| is_default(c, "max-new")).await.unwrap());
        assert_eq!(
            db.read(|c| default_for(c, HarnessId::Pi)).await.unwrap(),
            None
        );
    }

    /// The first login of a harness becomes its default; a second login
    /// keeps it. The default of a deleted or logged-out account is not
    /// replaced, also not by a later start.
    #[tokio::test]
    async fn the_first_account_is_the_default_and_the_oldest_replaces_a_gone_one() {
        let db = db();
        db.write("system", |t| {
            for n in ["max-a", "max-b", "max-c"] {
                upsert_def(t, &def(n), SOURCE_API)?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let first = vec![acct(
            "max-a",
            HarnessId::Claude,
            Some("2026-10-01T00:00:00.000Z"),
        )];
        db.write("system", move |t| settle_defaults(t, &first, Some("max-a")))
            .await
            .unwrap();
        assert_eq!(
            db.read(|c| default_for(c, HarnessId::Claude))
                .await
                .unwrap(),
            Some("max-a".into())
        );
        // A second and a third login: the default stays.
        let three = vec![
            acct("max-a", HarnessId::Claude, Some("2026-10-01T00:00:00.000Z")),
            acct("max-b", HarnessId::Claude, Some("2026-09-01T00:00:00.000Z")),
            acct("max-c", HarnessId::Claude, Some("2026-10-03T00:00:00.000Z")),
        ];
        let a = three.clone();
        let changes = db
            .write("system", move |t| settle_defaults(t, &a, Some("max-b")))
            .await
            .unwrap();
        assert!(changes.is_empty());
        assert_eq!(
            db.read(|c| default_for(c, HarnessId::Claude))
                .await
                .unwrap(),
            Some("max-a".into())
        );

        // Delete the default: the oldest account left (max-b, from
        // September) takes over at the next settle.
        db.write("operator", |t| delete(t, "max-a")).await.unwrap();
        assert_eq!(defaults(&db).await, [("claude".to_owned(), None)]);
        let rest = three[1..].to_vec();
        let a = rest.clone();
        let changes = db
            .write("system", move |t| settle_defaults(t, &a, None))
            .await
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            db.read(|c| default_for(c, HarnessId::Claude))
                .await
                .unwrap(),
            Some("max-b".into())
        );
        // A default whose account dir is gone counts as gone too.
        let only_c = vec![three[2].clone()];
        db.write("system", move |t| settle_defaults(t, &only_c, None))
            .await
            .unwrap();
        assert_eq!(
            db.read(|c| default_for(c, HarnessId::Claude))
                .await
                .unwrap(),
            Some("max-c".into())
        );

        // clear_default empties the row; the caller then settles.
        let change = db
            .write("operator", |t| clear_default(t, "max-c"))
            .await
            .unwrap();
        assert_eq!(
            change,
            Some(DefaultChange {
                harness: HarnessId::Claude,
                from: Some("max-c".into()),
                to: None
            })
        );
        // Not a default: nothing to clear.
        assert_eq!(
            db.write("operator", |t| clear_default(t, "max-b"))
                .await
                .unwrap(),
            None
        );
        // One account left: it becomes the default.
        let one = vec![acct("max-b", HarnessId::Claude, None)];
        db.write("system", move |t| settle_defaults(t, &one, None))
            .await
            .unwrap();
        assert_eq!(
            db.read(|c| default_for(c, HarnessId::Claude))
                .await
                .unwrap(),
            Some("max-b".into())
        );
    }

    /// Setting a default replaces the old one in one transaction, with an
    /// audit row (from, to) and a `subscription` event.
    #[tokio::test]
    async fn setting_a_default_replaces_the_old_one() {
        let db = db();
        db.write("system", |t| {
            upsert_def(t, &def("max-a"), SOURCE_API)?;
            upsert_def(t, &def("max-b"), SOURCE_API)?;
            set_default(t, HarnessId::Claude, "max-a")?;
            set_default(t, HarnessId::Claude, "max-b")
        })
        .await
        .unwrap();
        assert!(!db.read(|c| is_default(c, "max-a")).await.unwrap());
        assert!(db.read(|c| is_default(c, "max-b")).await.unwrap());
        let audits = default_audits(&db).await;
        assert_eq!(audits[1]["from"], "max-a");
        assert_eq!(audits[1]["to"], "max-b");
        assert_eq!(audits[1]["harness"], "claude");
        let kinds: Vec<(String, String)> = db
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT kind, payload FROM events WHERE kind = 'subscription' ORDER BY seq",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap();
        assert_eq!(kinds.len(), 2);
        let last: Value = serde_json::from_str(&kinds[1].1).unwrap();
        assert_eq!(last["change"], "default");
        assert_eq!(last["from"], "max-a");
        // pi has no default; an unknown name is 404.
        let e = db
            .write("operator", |t| set_default(t, HarnessId::Pi, "max-a"))
            .await
            .unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
        let e = db
            .write("operator", |t| set_default(t, HarnessId::Claude, "nope"))
            .await
            .unwrap_err();
        assert_eq!(e.code, crate::error::Code::NotFound);
    }
}
