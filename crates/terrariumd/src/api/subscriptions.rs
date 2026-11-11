//! `/api/subscriptions/...`: plans, quota samples, probes, login sessions,
//! and API keys (PLAN §16, §17).

use axum::extract::{FromRequestParts, Path, Request, State, ws::WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use terrarium_core::HarnessId;
use terrarium_core::config::SubscriptionDef;

use super::{ApiJson, ApiQuery, Shared, is_websocket, streams};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::login_session::{self as login, ApiKey, Detected, LoginView, StartLogin};
use crate::policy;
use crate::probes::{self, ProbeOutcome};
use crate::store::projects::SOURCE_API;
use crate::store::subscriptions::{self as subs, QuotaSample, Subscription};

mod bedrock;

#[derive(Deserialize)]
struct KeySave {
    #[serde(flatten)]
    key: ApiKey,
    /// Opt-in because a test invocation bills tokens. The wizard requests it.
    #[serde(default)]
    test_connection: bool,
}
use crate::store::usage as ustore;
use terrarium_core::selfhosted::{self, Api, SelfHosted};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/subscriptions", get(list).post(create))
        .route("/subscriptions/detect", get(detect))
        .route("/subscriptions/fetch-models", post(fetch_models))
        .route("/subscriptions/login", post(login_start))
        .route(
            "/subscriptions/login/{id}",
            get(login_show).delete(login_cancel),
        )
        .route("/subscriptions/login/{id}/input", post(login_input))
        .route("/subscriptions/login/{id}/pty", get(login_pty))
        .route(
            "/subscriptions/{name}",
            get(show).delete(remove).patch(update),
        )
        .route("/subscriptions/{name}/enable", post(enable))
        .route("/subscriptions/{name}/disable", post(disable))
        .route("/subscriptions/{name}/stop-all", post(stop_all))
        .route("/subscriptions/{name}/quota", get(quota))
        .route("/subscriptions/{name}/models", get(models))
        .route("/subscriptions/{name}/probe", post(probe))
        .route("/subscriptions/{name}/verify", post(verify))
        .route("/subscriptions/{name}/logout", post(logout))
        .route("/subscriptions/{name}/default", post(make_default))
        .route("/subscriptions/{name}/api-key", post(api_key))
        .route("/subscriptions/{name}/cloud", axum::routing::put(set_cloud))
        .route("/subscriptions/{name}/label", axum::routing::put(set_label))
}

/// The newest sample of one window, for lists.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowView {
    pub window: String,
    pub used_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// When the probe read it.
    pub at: String,
    /// The money left, for a window that is a credit balance
    /// (`openrouter-credits`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<Balance>,
}

/// A credit balance, from the `raw` of a credits window, or of the monthly
/// `budget` window of a cloud subscription (`crate::cloud_budget`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Balance {
    pub total: f64,
    pub used: f64,
    pub remaining: f64,
    pub currency: String,
}

impl Balance {
    fn from_raw(raw: &serde_json::Value) -> Option<Self> {
        let num = |k: &str, budget: &str| {
            raw.get(k)
                .or_else(|| raw.get(budget))
                .and_then(serde_json::Value::as_f64)
        };
        Some(Self {
            total: num("total_credits", "budget_usd")?,
            used: num("total_usage", "used_usd")?,
            remaining: num("remaining", "remaining_usd")?,
            currency: raw
                .get("currency")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("USD")
                .to_owned(),
        })
    }
}

impl From<QuotaSample> for WindowView {
    fn from(q: QuotaSample) -> Self {
        Self {
            balance: Balance::from_raw(&q.raw),
            window: q.window,
            used_pct: q.used_pct,
            resets_at: q.resets_at,
            at: q.at,
        }
    }
}

/// A subscription as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubscriptionView {
    pub session_period: SessionPeriod,
    pub sessions: Vec<crate::store::session_refs::SubscriptionSession>,
    #[serde(flatten)]
    pub sub: Subscription,
    /// The name for people: `label` when the operator set one, else the
    /// vendor's product and plan name
    /// (`terrarium_core::providers::display_name`), for example
    /// `Claude Ultra`. `name` stays the id.
    pub display_name: String,
    /// The display name that the operator set
    /// (`PUT /api/subscriptions/{name}/label`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The newest sample per window.
    pub windows: Vec<WindowView>,
    /// No probe: the only numbers are local accounting.
    pub local_estimate_only: bool,
    /// The provider has no quota at all (a self-hosted server): the quota
    /// UI shows the usage counts only.
    pub no_quota: bool,
    /// The streams of a self-hosted subscription with `max_concurrent`
    /// (`crate::stream_limit`): the limit, the agents that run a turn now,
    /// and the agents whose work waits. Absent with no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streams: Option<crate::stream_limit::StreamUsage>,
    /// The public status page of the provider
    /// (`terrarium_core::providers::status_url`), or the `status_url` of a
    /// self-hosted server. Absent when none is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_url: Option<String>,
    /// An API key is stored for it (never shown).
    pub has_api_key: bool,
    /// Agents whose turns wait after a usage-limit error on this
    /// subscription (`crate::usage_limit`).
    pub held_agents: usize,
    /// The model each harness gets on this subscription when the agent
    /// sets no `model`, by harness name. Only the harnesses where the
    /// daemon picks the model: pi (the provider's default,
    /// `terrarium_adapter_pi::flags::default_model`; for Moonshot, the
    /// plan's model, `terrarium_core::moonshot::Plan::default_model`) and
    /// Claude Code on Z.ai (`terrarium_core::zai::model_for`). No pi entry when pi
    /// refuses the subscription. Empty (`{}`) when the harnesses keep their
    /// own defaults; always present, so a client can tell an older daemon.
    pub default_models: BTreeMap<String, String>,
    /// The plan account of the subscription (PLAN §16.3), when it has one.
    /// No secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountView>,
    /// The agents whose current subscription this is.
    pub agents: Vec<AgentOn>,
    /// The account is the default account of its harness (PLAN §16.3): an
    /// agent of that harness with no subscription runs on it.
    pub default: bool,
}

/// The plan account of a subscription, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountView {
    /// The harness whose login the account holds.
    pub harness: terrarium_core::HarnessId,
    /// The account has the credential file.
    pub has_login: bool,
    /// The login name, when the harness says it (Copilot), or the email of
    /// a Claude login.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// `migration` (the shared login moved here at a start) or `login`.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// An agent on a subscription.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentOn {
    pub id: String,
    pub name: String,
    pub project: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionPeriod {
    pub since: String,
    pub until: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod session_period_tests {
    use super::*;

    #[test]
    fn current_month_and_active_plan_window() {
        let now = "2026-11-08T12:00:00Z".parse().unwrap();
        let month = session_period(&[], now);
        assert_eq!(month.since, "2026-11-01T00:00:00+00:00");
        assert_eq!(month.until, "2026-12-01T00:00:00+00:00");
        let window = |name: &str, end: &str| WindowView {
            window: name.into(),
            used_pct: 1.0,
            resets_at: Some(end.into()),
            at: now.to_rfc3339(),
            balance: None,
        };
        let week = session_period(
            &[
                window("5h", "2026-11-08T14:00:00Z"),
                window("7d", "2026-11-10T00:00:00Z"),
            ],
            now,
        );
        assert_eq!(week.since, "2026-11-03T00:00:00+00:00");
        assert_eq!(
            session_period(&[window("168h", "2026-11-10T00:00:00Z")], now),
            week
        );
        assert_eq!(
            session_period(
                &[window("999999999999999999d", "2026-11-10T00:00:00Z")],
                now
            ),
            month
        );
        assert_eq!(
            session_period(&[window("7d", "2026-10-10T00:00:00Z")], now),
            month
        );
        let billing = session_period(&[window("monthly", "2026-11-15T00:00:00Z")], now);
        assert_eq!(billing.since, "2026-10-15T00:00:00+00:00");
    }
}

fn session_period(windows: &[WindowView], now: chrono::DateTime<chrono::Utc>) -> SessionPeriod {
    use chrono::{Datelike, TimeZone};
    // Prefer the longest active plan window, not its short burst limit.
    let window = windows
        .iter()
        .filter_map(|w| {
            let end = chrono::DateTime::parse_from_rfc3339(w.resets_at.as_deref()?)
                .ok()?
                .with_timezone(&chrono::Utc);
            let duration = match w.window.as_str() {
                "monthly" => end - end.checked_sub_months(chrono::Months::new(1))?,
                name => {
                    let hours = name.strip_suffix('h').and_then(|n| n.parse::<i64>().ok());
                    let days = name.strip_suffix('d').and_then(|n| n.parse::<i64>().ok());
                    let duration = hours
                        .and_then(chrono::Duration::try_hours)
                        .or_else(|| days.and_then(chrono::Duration::try_days))?;
                    if duration <= chrono::Duration::zero() {
                        return None;
                    }
                    duration
                }
            };
            let start = end.checked_sub_signed(duration)?;
            (start <= now && now < end).then_some((duration, start, end))
        })
        .max_by_key(|(duration, _, _)| *duration);
    let (since, until) = if let Some((_, start, end)) = window {
        (start, end)
    } else {
        let start = chrono::Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(now);
        let end = start + chrono::Months::new(1);
        (start, end)
    };
    SessionPeriod {
        since: since.to_rfc3339(),
        until: until.to_rfc3339(),
    }
}

fn view(
    c: &rusqlite::Connection,
    sub: Subscription,
    state_dir: &std::path::Path,
    streams: &crate::stream_limit::StreamGate,
) -> ApiResult<SubscriptionView> {
    let has_api_key = sub.env_var.is_some() && login::stored_key(state_dir, &sub.name).is_some();
    view_with_key_presence(c, sub, state_dir, streams, has_api_key)
}

fn view_with_key_presence(
    c: &rusqlite::Connection,
    sub: Subscription,
    state_dir: &std::path::Path,
    streams: &crate::stream_limit::StreamGate,
    has_api_key: bool,
) -> ApiResult<SubscriptionView> {
    let windows: Vec<WindowView> = subs::latest_windows(c, &sub.name)?
        .into_iter()
        .map(WindowView::from)
        .collect();
    let session_period = session_period(&windows, chrono::Utc::now());
    let sessions = crate::store::session_refs::on_subscription(
        c,
        &sub.name,
        &session_period.since,
        &session_period.until,
    )?;
    let label = subs::label(c, &sub.name)?;
    let display_name = subs::display_name(c, &sub)?;
    let cause = crate::usage_limit::cause(&sub.name);
    let held_agents = ustore::holds(c)?
        .iter()
        .filter(|h| h.cause == cause)
        .count();
    let account = crate::accounts::read_meta(state_dir, &sub.name).map(|m| AccountView {
        harness: m.harness,
        has_login: crate::accounts::has_credential(state_dir, &sub.name, m.harness),
        login: m.login.clone().or_else(|| {
            m.oauth_account
                .as_ref()
                .and_then(|a| a.get("emailAddress"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }),
        source: m.source,
        created_at: m.created_at,
        updated_at: m.updated_at,
    });
    let agents = ustore::agents_on(c, &sub.name)?
        .into_iter()
        .map(|(id, name, project)| AgentOn { id, name, project })
        .collect();
    Ok(SubscriptionView {
        session_period,
        sessions,
        display_name,
        label,
        default: subs::is_default(c, &sub.name)?,
        account,
        agents,
        default_models: crate::models::default_models(&sub),
        local_estimate_only: sub.probe.is_none(),
        no_quota: sub.is_selfhosted() && sub.provider != "litellm",
        streams: streams.usage(&sub.name),
        status_url: terrarium_core::providers::status_url_of(
            &sub.provider,
            sub.selfhosted.as_ref(),
        ),
        has_api_key,
        windows,
        held_agents,
        sub,
    })
}

async fn load(state: &Shared, name: String) -> ApiResult<SubscriptionView> {
    let dir = state.state_dir.clone();
    let st = state.clone();
    state
        .db
        .read(move |c| {
            let s = subs::get(c, &name)?;
            view(c, s, &dir, &st.usage.streams)
        })
        .await
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<SubscriptionView>>> {
    policy::read_subscriptions(&actor)?;
    let dir = state.state_dir.clone();
    let st = state.clone();
    let v = state
        .db
        .read(move |c| {
            subs::list(c)?
                .into_iter()
                .map(|s| view(c, s, &dir, &st.usage.streams))
                .collect::<ApiResult<Vec<_>>>()
        })
        .await?;
    Ok(Json(v))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<SubscriptionView>> {
    // An agent may read its own subscription.
    if let Some(a) = actor.agent() {
        let id = a.id.clone();
        let mine = state
            .db
            .read(move |c| crate::store::agents::get(c, &id))
            .await?
            .agent
            .subscription
            .is_some_and(|s| s == name);
        if !mine {
            policy::read_subscriptions(&actor)?;
        }
    }
    Ok(Json(load(&state, name).await?))
}

/// The query of `GET /api/subscriptions/{name}/models`.
#[derive(Debug, Default, Deserialize)]
struct ModelsQuery {
    /// Read the list again instead of the cached one.
    #[serde(default)]
    refresh: bool,
}

/// `GET /api/subscriptions/{name}/models`: the models the subscription can
/// use, from the provider, from the harness login, from pi's catalogue, or
/// from the default table (`crate::models`). Cached for six hours.
async fn models(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<ModelsQuery>,
) -> ApiResult<Json<crate::models::ModelList>> {
    let key = name.clone();
    let (sub, projects) = state
        .db
        .read(move |c| {
            let s = subs::get(c, &key)?;
            let mut stmt =
                c.prepare("SELECT DISTINCT project FROM agents WHERE subscription = ?1")?;
            let projects = stmt
                .query_map([&key], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((s, projects))
        })
        .await?;
    policy::read_subscription_models(&actor, &projects)?;
    let cfg = &state.usage.config;
    // The Codex list asks for the client version; ask only for a Codex
    // subscription (the report runs the binaries at most once in 10 min).
    let codex_version = if sub.harnesses.contains(&HarnessId::Codex) {
        state
            .supervisor
            .harness_versions(false)
            .await
            .harnesses
            .into_iter()
            .find(|h| h.harness == HarnessId::Codex)
            .and_then(|h| h.installed)
    } else {
        None
    };
    let src = crate::models::Sources {
        http: cfg.http.as_ref(),
        state_dir: &state.state_dir,
        pi: cfg.program(HarnessId::Pi),
        pi_home: cfg.home(HarnessId::Pi),
        homes: [HarnessId::Claude, HarnessId::Codex, HarnessId::Copilot]
            .into_iter()
            .map(|h| (h, cfg.home(h)))
            .collect(),
        codex_version,
    };
    Ok(Json(
        crate::models::list(&state.usage.models, &src, &sub, q.refresh).await,
    ))
}

/// `POST /api/subscriptions`: the fields of `subscriptions/<name>.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSubscription {
    name: String,
    provider: String,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    harnesses: Vec<HarnessId>,
    #[serde(default)]
    probe: Option<String>,
    #[serde(default)]
    alert_at: Option<u8>,
    /// The server of a `selfhosted` subscription
    /// (`terrarium_core::selfhosted::SelfHosted`).
    #[serde(default)]
    selfhosted: Option<SelfHosted>,
    /// The settings of a cloud subscription (`bedrock`, `gcp`, `foundry`;
    /// `terrarium_core::cloud::CloudSettings`).
    #[serde(default)]
    cloud: Option<terrarium_core::cloud::CloudSettings>,
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<NewSubscription>,
) -> ApiResult<(StatusCode, Json<SubscriptionView>)> {
    policy::manage_subscriptions(&actor, "defining a subscription")?;
    if let Some(p) = &b.probe
        && probes::by_name(p).is_none()
    {
        return Err(ApiError::invalid(format!(
            "unknown probe `{p}`; known: {}",
            probes::all()
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if b.alert_at.is_some_and(|a| a == 0 || a > 100) {
        return Err(ApiError::invalid("alert_at is a percent, 1 to 100"));
    }
    // A cloud subscription is for Claude Code only: no harnesses given
    // means Claude Code.
    let harnesses = if b.harnesses.is_empty() && terrarium_core::cloud::is_cloud(&b.provider) {
        terrarium_core::cloud::HARNESSES.to_vec()
    } else {
        b.harnesses
    };
    let def = SubscriptionDef {
        name: b.name,
        provider: b.provider,
        plan: b.plan,
        harnesses,
        probe: b.probe,
        alert_at: b.alert_at.unwrap_or(80),
        selfhosted: b.selfhosted,
        cloud: b.cloud,
    };
    // `openai-compatible` is the old word of `selfhosted` (OpenAI style).
    let def = if selfhosted::is_alias(&def.provider) {
        SubscriptionDef {
            provider: selfhosted::PROVIDER.into(),
            selfhosted: def.selfhosted.map(|mut s| {
                s.api = Api::Openai;
                s
            }),
            ..def
        }
    } else {
        def
    };
    let name = def.name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            subs::check_name(&def.name)?;
            if subs::find(t, &def.name)?.is_some() {
                return Err(ApiError::conflict(format!(
                    "subscription `{}` exists",
                    def.name
                )));
            }
            let s = subs::upsert_def(t, &def, SOURCE_API)?;
            t.audit(
                "subscription.create",
                &def.name,
                None,
                json!({"provider": s.provider, "probe": def.probe, "selfhosted": s.selfhosted}),
            )
        })
        .await?;
    crate::stream_limit::refresh(&state).await;
    Ok((StatusCode::CREATED, Json(load(&state, name).await?)))
}

/// `null` clears a field of a `PATCH`; a missing field keeps it.
fn nullable<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The `selfhosted` fields of `PATCH /api/subscriptions/{name}`. A missing
/// field keeps its value; `null` clears an optional one.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelfHostedPatch {
    #[serde(default)]
    api: Option<Api>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default, deserialize_with = "nullable")]
    default_model: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    context_window: Option<Option<u64>>,
    #[serde(default, deserialize_with = "nullable")]
    max_output_tokens: Option<Option<u64>>,
    #[serde(default, deserialize_with = "nullable")]
    max_concurrent: Option<Option<u32>>,
    #[serde(default, deserialize_with = "nullable")]
    status_url: Option<Option<String>>,
}

impl SelfHostedPatch {
    fn apply(self, mut s: SelfHosted) -> SelfHosted {
        if let Some(v) = self.api {
            s.api = v;
        }
        if let Some(v) = self.base_url {
            s.base_url = v;
        }
        if let Some(v) = self.models {
            s.models = v;
        }
        if let Some(v) = self.default_model {
            s.default_model = v;
        }
        if let Some(v) = self.context_window {
            s.context_window = v;
        }
        if let Some(v) = self.max_output_tokens {
            s.max_output_tokens = v;
        }
        if let Some(v) = self.max_concurrent {
            s.max_concurrent = v;
        }
        if let Some(v) = self.status_url {
            s.status_url = v;
        }
        s
    }
}

/// `PATCH /api/subscriptions/{name}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionPatch {
    #[serde(default)]
    alert_at: Option<u8>,
    #[serde(default)]
    harnesses: Option<Vec<HarnessId>>,
    #[serde(default)]
    selfhosted: Option<SelfHostedPatch>,
}

/// `PATCH /api/subscriptions/{name}`: change an API subscription
/// (`alert_at`; the `harnesses` and the server of a self-hosted one). A
/// file subscription changes in its file (`409 file_managed`).
async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<SubscriptionPatch>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::manage_subscriptions(&actor, "changing a subscription")?;
    if b.alert_at.is_some_and(|a| a == 0 || a > 100) {
        return Err(ApiError::invalid("alert_at is a percent, 1 to 100"));
    }
    let n = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            let s = subs::get(t, &n)?;
            if s.source == crate::store::projects::SOURCE_FILE {
                return Err(ApiError::new(
                    crate::error::Code::FileManaged,
                    format!(
                        "subscription `{n}` is declared in subscriptions/{n}.toml; change the                          file, then POST /api/reload"
                    ),
                ));
            }
            if let Some(a) = b.alert_at {
                subs::set_alert_at(t, &n, a)?;
            }
            if b.selfhosted.is_some() || (b.harnesses.is_some() && s.is_selfhosted()) {
                let current = s.selfhosted.clone().unwrap_or_default();
                let next = b.selfhosted.unwrap_or_default().apply(current);
                subs::set_selfhosted(t, &n, next, b.harnesses.clone())?;
            } else if b.harnesses.is_some() {
                return Err(ApiError::invalid(
                    "`harnesses` changes here only for a self-hosted subscription; change the                      definition of another one",
                ));
            }
            let after = subs::get(t, &n)?;
            t.audit(
                "subscription.update",
                &n,
                None,
                json!({"alert_at": after.alert_at, "harnesses": after.harnesses, "selfhosted": after.selfhosted}),
            )
        })
        .await?;
    crate::stream_limit::refresh(&state).await;
    Ok(Json(load(&state, name).await?))
}

/// Enable or disable a subscription (PLAN §16): the audit row, the
/// `subscription.enabled` or `subscription.disabled` event, and on the
/// enable a wake of the agents on it that have work.
async fn set_enabled(
    state: &Shared,
    actor: &Actor,
    name: String,
    enabled: bool,
) -> ApiResult<SubscriptionView> {
    let n = name.clone();
    let changed = state
        .db
        .write(actor.label(), move |t| {
            let changed = subs::set_enabled(t, &n, enabled)?;
            if changed {
                let kind = if enabled {
                    "subscription.enabled"
                } else {
                    "subscription.disabled"
                };
                t.audit(kind, &n, None, json!({}))?;
                let word = if enabled { "enabled" } else { "disabled" };
                t.emit(crate::events::EventDraft::new(
                    kind,
                    format!("{n}: {word}"),
                    json!({"subscription": n, "enabled": enabled}),
                ))?;
            }
            Ok(changed)
        })
        .await?;
    if changed && enabled {
        // The agents with work that waited for the enable wake now. The
        // stream limit applies to their turns.
        let key = name.clone();
        let ids = state.db.read(move |c| ustore::agents_on(c, &key)).await?;
        for (id, _, _) in ids {
            if let Ok(id) = id.parse() {
                crate::lifecycle::wake_with_work(state, Some(id), "subscription_enabled").await;
            }
        }
    }
    load(state, name).await
}

/// `POST /api/subscriptions/{name}/enable`.
async fn enable(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::manage_subscriptions(&actor, "enabling a subscription")?;
    Ok(Json(set_enabled(&state, &actor, name, true).await?))
}

/// `POST /api/subscriptions/{name}/disable`: no agent on it starts or
/// wakes, and no new agent takes it. Agents that run go on.
async fn disable(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::manage_subscriptions(&actor, "disabling a subscription")?;
    Ok(Json(set_enabled(&state, &actor, name, false).await?))
}

/// The body of `POST /api/subscriptions/{name}/stop-all`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopAll {
    /// Disable the subscription first, so no agent wakes again.
    #[serde(default)]
    disable: bool,
}

/// One agent of a stop-all.
#[derive(Debug, Clone, Serialize)]
struct StoppedAgent {
    id: String,
    name: String,
    project: String,
    /// The error of a stop that failed. Absent when it stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `POST /api/subscriptions/{name}/stop-all`: stop every running agent on
/// the subscription now, with the graceful stop of the agent stop (then
/// the hard stop after its time limit). With `disable: true` the
/// subscription is disabled first. The answer lists the agents.
async fn stop_all(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    policy::manage_subscriptions(&actor, "stopping the agents of a subscription")?;
    // The body is optional: no body is `{}`.
    let b: StopAll = if body.iter().all(u8::is_ascii_whitespace) {
        StopAll::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad body: {e}")))?
    };
    let key = name.clone();
    let on = state
        .db
        .read(move |c| {
            subs::get(c, &key)?;
            ustore::agents_on(c, &key)
        })
        .await?;
    if b.disable {
        set_enabled(&state, &actor, name.clone(), false).await?;
    }
    let mut stopped = Vec::new();
    for (id, agent, project) in on {
        let Ok(aid) = id.parse::<terrarium_core::AgentId>() else {
            continue;
        };
        let live = state
            .supervisor
            .status(aid)
            .await
            .is_some_and(|s| s.is_live() || s == terrarium_core::AgentStatus::Restarting);
        if !live {
            continue;
        }
        let error = state
            .supervisor
            .stop(aid)
            .await
            .err()
            .map(|e| e.to_string());
        stopped.push(StoppedAgent {
            id,
            name: agent,
            project,
            error,
        });
    }
    let n = name.clone();
    let agents = serde_json::to_value(&stopped)?;
    let disabled = b.disable;
    state
        .db
        .write(actor.label(), move |t| {
            let count = agents.as_array().map_or(0, Vec::len);
            t.audit(
                "subscription.stop_all",
                &n,
                None,
                json!({"agents": agents, "disabled": disabled}),
            )?;
            t.emit(crate::events::EventDraft::new(
                "subscription.stop_all",
                format!("{n}: stop all ({count} agents)"),
                json!({"subscription": n, "agents": agents, "disabled": disabled}),
            ))
        })
        .await?;
    Ok(Json(json!({
        "subscription": load(&state, name).await?,
        "stopped": stopped,
    })))
}

/// The body of `POST /api/subscriptions/fetch-models`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchModels {
    #[serde(default)]
    api: Option<Api>,
    #[serde(default)]
    base_url: Option<String>,
    /// A key to send (the wizard, before the key is stored). Never stored
    /// and never logged.
    #[serde(default)]
    key: Option<String>,
    /// Use the server and the stored key of this subscription. A
    /// `base_url` must then be its own: the stored key never goes to
    /// another host.
    #[serde(default)]
    subscription: Option<String>,
    /// Read the server again instead of the cache.
    #[serde(default)]
    refresh: bool,
}

/// `POST /api/subscriptions/fetch-models`: the model list of a self-hosted
/// server (`crate::models::selfhosted`), for the wizard before the
/// subscription exists, or for an edit. The operator and sysadmin only:
/// it is a request from the daemon to a URL that the caller gives.
async fn fetch_models(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<FetchModels>,
) -> ApiResult<Json<Value>> {
    policy::manage_subscriptions(&actor, "fetching the models of a self-hosted server")?;
    let stored = match &b.subscription {
        Some(n) => {
            let k = n.clone();
            let s = state.db.read(move |c| subs::get(c, &k)).await?;
            let Some(sh) = s.selfhosted.clone().filter(|_| s.is_selfhosted()) else {
                return Err(ApiError::invalid(format!(
                    "subscription `{n}` is not a self-hosted server"
                )));
            };
            Some((sh, login::stored_key(&state.state_dir, n)))
        }
        None => None,
    };
    let (sh, key) = match stored {
        Some((mut sh, stored_key)) => {
            if let Some(u) = &b.base_url {
                let u =
                    selfhosted::check_http_url("base_url", u, false).map_err(ApiError::invalid)?;
                if u != sh.base_url {
                    return Err(ApiError::invalid(
                        "a fetch with the stored key goes to the stored base_url only; leave                          `base_url` out, or leave `subscription` out and give the key",
                    ));
                }
            }
            if let Some(api) = b.api {
                sh.api = api;
            }
            (sh, b.key.clone().or(stored_key))
        }
        None => {
            let Some(base_url) = b.base_url.clone() else {
                return Err(ApiError::invalid("give `base_url` (or `subscription`)"));
            };
            let sh = SelfHosted {
                api: b.api.unwrap_or_default(),
                base_url,
                ..SelfHosted::default()
            }
            .checked()
            .map_err(ApiError::invalid)?;
            (sh, b.key.clone())
        }
    };
    let cache = &state.usage.models.fetched;
    let (models, cached) = match (!b.refresh).then(|| cache.get(&sh)).flatten() {
        Some(m) => (m, true),
        None => {
            let m = crate::models::selfhosted::fetch(&sh, key.as_deref())
                .await
                .map_err(|e| ApiError::new(crate::error::Code::Unavailable, e))?;
            cache.put(&sh, &m);
            (m, false)
        }
    };
    let (url, n) = (sh.models_url(), models.len());
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "subscription.fetch_models",
                &url,
                None,
                json!({"models": n, "cached": cached}),
            )
        })
        .await?;
    Ok(Json(json!({
        "api": sh.api,
        "base_url": sh.base_url,
        "models": models,
        "cached": cached,
        "fetched_at": crate::db::now(),
    })))
}

/// `PUT /api/subscriptions/{name}/label`: `{"label": "..."}` sets the
/// display name; `{"label": null}` (or only spaces) goes back to the name
/// from the vendor table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelBody {
    label: Option<String>,
}

async fn set_label(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<LabelBody>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::manage_subscriptions(&actor, "naming a subscription")?;
    let n = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            let label = subs::set_label(t, &n, b.label.as_deref())?;
            t.audit("subscription.label", &n, None, json!({"label": label}))
        })
        .await?;
    Ok(Json(load(&state, name).await?))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    policy::manage_subscriptions(&actor, "deleting a subscription")?;
    let account = crate::accounts::exists(&state.state_dir, &name);
    if account {
        refuse_if_used(&state, &name, "delete it").await?;
    }
    let n = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            let was_default = subs::is_default(t, &n)?;
            subs::delete(t, &n)?;
            t.audit(
                "subscription.delete",
                &n,
                None,
                json!({"account_removed": account, "default_cleared": was_default}),
            )
        })
        .await?;
    if account {
        let dir = state.state_dir.clone();
        tokio::task::spawn_blocking(move || crate::accounts::remove(&dir, &name))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::internal(format!("cannot remove the account: {e}")))?;
        settle_defaults(&state, None).await;
    }
    crate::stream_limit::refresh(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `409 conflict` while an agent runs on the account of `name`: swap it
/// first (`POST /api/agents/{id}/subscription`).
async fn refuse_if_used(state: &Shared, name: &str, what: &str) -> ApiResult<()> {
    let key = name.to_owned();
    let on = state.db.read(move |c| ustore::agents_on(c, &key)).await?;
    if on.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = on.iter().map(|(_, n, p)| format!("{p}/{n}")).collect();
    Err(ApiError::conflict(format!(
        "agents {} use the account of `{name}`; move them to another account \
         (terrarium admin agent swap <agent> <subscription>) before you {what}",
        names.join(", ")
    )))
}

#[derive(Debug, Default, Deserialize)]
struct QuotaQuery {
    since: Option<String>,
    window: Option<String>,
    limit: Option<usize>,
}

/// `GET /api/subscriptions/{name}/quota?since=&window=&limit=`: samples,
/// oldest first. The default `since` is 24 hours ago.
async fn quota(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<QuotaQuery>,
) -> ApiResult<Json<Vec<QuotaSample>>> {
    policy::read_subscriptions(&actor)?;
    let since =
        crate::time_param::opt_time_param("since", q.since.as_deref())?.unwrap_or_else(|| {
            crate::time_param::stored(chrono::Utc::now() - chrono::Duration::hours(24))
        });
    let limit = q.limit.unwrap_or(1000).clamp(1, 10_000);
    let v = state
        .db
        .read(move |c| {
            subs::get(c, &name)?;
            subs::samples(c, &name, q.window.as_deref(), &since, limit)
        })
        .await?;
    Ok(Json(v))
}

async fn probe(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<ProbeOutcome>> {
    policy::manage_subscriptions(&actor, "running a quota probe")?;
    Ok(Json(probes::run_subscription(&state, &name).await?))
}

/// `POST /api/subscriptions/{name}/verify`: run the harness status command.
async fn verify(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    policy::manage_subscriptions(&actor, "verifying a subscription login")?;
    let sub = load(&state, name.clone()).await?.sub;
    let key_only = key_only_provider(&sub.provider);
    if sub.env_var.is_some() && login::stored_key(&state.state_dir, &sub.name).is_some() {
        let n = name.clone();
        let status = if key_only.is_some() {
            subs::API_KEY
        } else {
            subs::LOGGED_IN
        };
        state
            .db
            .write(actor.label(), move |t| {
                subs::set_login_status(t, &n, status)
            })
            .await?;
        return Ok(Json(
            json!({"subscription": load(&state, name).await?, "status": {"logged_in": true, "detail": "a stored API key"}}),
        ));
    }
    // A self-hosted server has no login, and a key is optional.
    if sub.is_selfhosted() {
        let n = name.clone();
        state
            .db
            .write(actor.label(), move |t| {
                subs::set_login_status(t, &n, subs::API_KEY)
            })
            .await?;
        return Ok(Json(
            json!({"subscription": load(&state, name).await?, "status": {"logged_in": true, "detail": "a self-hosted server: no login, no stored key"}}),
        ));
    }
    // A Z.ai or Moonshot subscription is a key only. Its first harness
    // would report the shared login of that harness, which is not this
    // subscription.
    if let Some(label) = key_only {
        let n = name.clone();
        state
            .db
            .write(actor.label(), move |t| {
                subs::set_login_status(t, &n, subs::MISSING)
            })
            .await?;
        return Ok(Json(
            json!({"subscription": load(&state, name).await?, "status": {"logged_in": false, "detail": format!("no stored {label} API key")}}),
        ));
    }
    let st = match crate::accounts::read_meta(&state.state_dir, &sub.name) {
        // A plan account (PLAN §16.3): check the account itself.
        Some(meta) => {
            let kind = login::kind_for(meta.harness);
            let pi = (kind == login::LoginKind::Pi).then(|| sub.provider.clone());
            login::check_status_in(
                &state,
                kind,
                pi.as_deref(),
                &login::CheckHome::Account(sub.name.clone()),
            )
            .await
        }
        None => {
            let Some((kind, provider)) = login::kind_of(&sub) else {
                return Err(ApiError::invalid(format!(
                    "subscription `{name}` names no harness to check"
                )));
            };
            login::check_status(&state, kind, provider.as_deref()).await
        }
    };
    let (n, logged_in, plan) = (name.clone(), st.logged_in, st.plan.clone());
    state
        .db
        .write(actor.label(), move |t| {
            subs::record_login_check(t, &n, logged_in)?;
            if let Some(p) = &plan
                && subs::get(t, &n)?.plan.is_none()
            {
                subs::set_plan(t, &n, p)?;
            }
            // A renamed or changed plan (Max 20x is Ultra now).
            subs::refresh_plan(t, &n, plan.as_deref())?;
            t.audit(
                "subscription.verify",
                &n,
                None,
                json!({"logged_in": logged_in}),
            )
        })
        .await?;
    Ok(Json(
        json!({"subscription": load(&state, name).await?, "status": st}),
    ))
}

async fn logout(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::operator_only(&actor, "logging a subscription out")?;
    let sub = load(&state, name.clone()).await?.sub;
    // The harness logout would end the shared login of the harness, which
    // an API-key subscription does not use.
    if let Some(label) = key_only_provider(&sub.provider) {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` is a {label} API key and has no logout; store another key, or delete the subscription"
        )));
    }
    // A plan account: its logout removes the account dir, and nothing
    // else. No harness logout runs, so no other account and no shared file
    // changes (PLAN §16.3).
    if crate::accounts::exists(&state.state_dir, &sub.name) {
        refuse_if_used(&state, &sub.name, "log it out").await?;
        let (dir, n) = (state.state_dir.clone(), sub.name.clone());
        tokio::task::spawn_blocking(move || crate::accounts::remove(&dir, &n))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::internal(format!("cannot remove the account: {e}")))?;
        let n = name.clone();
        state
            .db
            .write(actor.label(), move |t| {
                subs::set_login_status(t, &n, subs::MISSING)?;
                // `settle_defaults` below picks the new default.
                let cleared = subs::clear_default(t, &n)?.is_some();
                t.audit(
                    "subscription.logout",
                    &n,
                    None,
                    json!({"account_removed": true, "default_cleared": cleared}),
                )
            })
            .await?;
        settle_defaults(&state, None).await;
        return Ok(Json(load(&state, name).await?));
    }
    let Some((kind, _)) = login::kind_of(&sub) else {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` names no harness"
        )));
    };
    let line = login::logout(&state, kind).await?;
    let n = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            subs::set_login_status(t, &n, subs::MISSING)?;
            t.audit(
                "subscription.logout",
                &n,
                None,
                json!({"kind": kind, "output": line}),
            )
        })
        .await?;
    Ok(Json(load(&state, name).await?))
}

/// `POST /api/subscriptions/{name}/default`: make the plan account of
/// `name` the default account of its harness (PLAN §16.3). The old default
/// stops being one. `422` for a subscription with no plan account (an API
/// key, or no login yet), and for a pi account.
async fn make_default(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::manage_subscriptions(&actor, "setting the default account")?;
    let sub = load(&state, name.clone()).await?.sub;
    if sub.env_var.is_some() {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` is an API key; only a plan account can be the default"
        )));
    }
    let Some(meta) = crate::accounts::read_meta(&state.state_dir, &name) else {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` has no plan account; log in with the wizard first"
        )));
    };
    let h = meta.harness;
    if !subs::DEFAULT_HARNESSES.contains(&h) || !crate::accounts::plan_provider(h, &sub.provider) {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` is not a plan account of Claude Code, Codex, or Copilot \
             (harness {h}, provider {}); only those have a default account",
            sub.provider
        )));
    }
    let n = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            subs::set_default(t, h, &n).map(|_| ())
        })
        .await?;
    Ok(Json(load(&state, name).await?))
}

/// Give each harness with plan accounts and no default its default
/// (`subs::settle_defaults`): at the start, after the migration moves an
/// account, and after a login (`fresh`). Logs a failure; never fails.
pub async fn settle_defaults(state: &Shared, fresh: Option<String>) {
    let dir = state.state_dir.clone();
    let accounts = match tokio::task::spawn_blocking(move || {
        crate::accounts::default_candidates(&dir)
    })
    .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("cannot list the plan accounts for the defaults: {e}");
            return;
        }
    };
    let r = state
        .db
        .write("system", move |t| {
            subs::settle_defaults(t, &accounts, fresh.as_deref())
        })
        .await;
    match r {
        Ok(changes) => {
            for c in changes {
                tracing::info!(harness = %c.harness, to = ?c.to, "set the default plan account");
            }
        }
        Err(e) => tracing::warn!("cannot set the default plan accounts: {e}"),
    }
}

/// Replace cloud settings without reading or writing the credential file.
async fn set_cloud(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(settings): ApiJson<terrarium_core::cloud::CloudSettings>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::operator_only(&actor, "changing cloud settings")?;
    let n = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            let sub = subs::get(t, &n)?;
            if sub.source != SOURCE_API {
                return Err(ApiError::invalid(format!(
                    "subscription `{n}` comes from a file; set its `[cloud]` table there"
                )));
            }
            let kind = sub.cloud_kind().ok_or_else(|| {
                ApiError::invalid(format!("subscription `{n}` is not a cloud subscription"))
            })?;
            terrarium_core::cloud::check(kind, &settings).map_err(ApiError::invalid)?;
            subs::set_cloud(t, &n, &settings)?;
            t.audit("subscription.cloud", &n, None, json!({"cloud": settings}))
        })
        .await?;
    let dir = state.state_dir.clone();
    let st = state.clone();
    let response = state
        .db
        .read(move |c| {
            let sub = subs::get(c, &name)?;
            // Settings updates need only file metadata, never its secret bytes.
            let has_key = sub.env_var.is_some() && login::key_path(&dir, &name).is_file();
            view_with_key_presence(c, sub, &dir, &st.usage.streams, has_key)
        })
        .await?;
    Ok(Json(response))
}

/// `POST /api/subscriptions/{name}/api-key`. The answer never has the key.
async fn api_key(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(save): ApiJson<KeySave>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::operator_only(&actor, "storing an API key")?;
    let KeySave {
        key,
        test_connection,
    } = save;
    if test_connection && key.provider != "bedrock" {
        return Err(ApiError::invalid("test_connection is for bedrock only"));
    }
    if key.key.trim().is_empty() {
        return Err(ApiError::invalid("the key is empty"));
    }
    let Some((var, url_var)) = login::env_var_for(&key.provider) else {
        return Err(ApiError::invalid(format!(
            "unknown provider `{}`; use anthropic, openai, openai-compatible, google, openrouter, xai, groq, github, zai, moonshot, bedrock, gcp, or foundry",
            key.provider
        )));
    };
    let cloud_kind = terrarium_core::cloud::kind(&key.provider);
    // A cloud key (Claude Code only): the key and `key_id` are checked
    // here, without showing them; the settings below.
    let var = match cloud_kind {
        Some(kind) => {
            terrarium_core::cloud::check_key(kind, key.key.trim(), key.key_id.as_deref())
                .map_err(ApiError::invalid)?;
            if key.base_url.is_some() || !key.models.is_empty() {
                return Err(ApiError::invalid(
                    "a cloud key takes `cloud.base_url` and `cloud.models`, not `base_url` and `models`",
                ));
            }
            kind.key_var(key.key_id.is_some())
        }
        None if key.key_id.is_some() || key.cloud.is_some() => {
            return Err(ApiError::invalid(format!(
                "`key_id` and `cloud` are for bedrock, gcp, and foundry, not `{}`",
                key.provider
            )));
        }
        None => var,
    };
    let moonshot = terrarium_core::moonshot::is_moonshot(&key.provider);
    // The plan word of a provider with plans: Z.ai and Moonshot.
    let key_plan: Option<&'static str> = if terrarium_core::zai::is_zai(&key.provider) {
        Some(
            terrarium_core::zai::parse_plan(key.plan.as_deref())
                .map_err(ApiError::invalid)?
                .as_str(),
        )
    } else if moonshot {
        Some(
            terrarium_core::moonshot::parse_plan(key.plan.as_deref())
                .map_err(ApiError::invalid)?
                .as_str(),
        )
    } else if key.plan.is_some() {
        return Err(ApiError::invalid(format!(
            "`plan` is only for providers zai and moonshot, not `{}`",
            key.provider
        )));
    } else {
        None
    };
    if url_var.is_some() && key.base_url.as_deref().is_none_or(str::is_empty) {
        return Err(ApiError::invalid(format!(
            "provider `{}` needs base_url",
            key.provider
        )));
    }
    let n = name.clone();
    let exists = state.db.read(move |c| subs::find(c, &n)).await?;
    // A Moonshot key never goes to a harness other than pi: refuse it on a
    // subscription of another provider or of another harness.
    if moonshot && let Some(s) = &exists {
        let why = if terrarium_core::moonshot::is_moonshot(&s.provider) {
            terrarium_core::moonshot::harness_refusal(&s.harnesses)
        } else {
            Some(format!("it is a {} subscription", s.provider))
        };
        if let Some(why) = why {
            return Err(ApiError::invalid(format!(
                "cannot store a moonshot key on subscription `{name}`: {why}"
            )));
        }
    }
    // A self-hosted key goes on a self-hosted subscription only, and a new
    // one needs the server's base URL.
    let is_sh = selfhosted::is_selfhosted(&key.provider);
    match &exists {
        Some(s)
            if s.is_selfhosted() != is_sh
                || ((s.provider == "litellm") != (key.provider == "litellm")) =>
        {
            return Err(ApiError::invalid(format!(
                "cannot store a {} key on subscription `{name}`: it is a {} subscription",
                key.provider, s.provider
            )));
        }
        None if is_sh && key.base_url.as_deref().is_none_or(str::is_empty) => {
            return Err(ApiError::invalid(format!(
                "provider `{}` needs base_url for a new subscription (or create it first with \
                 POST /api/subscriptions and `selfhosted`)",
                key.provider
            )));
        }
        _ => {}
    }
    // A cloud key goes on a cloud subscription of the same kind only. The
    // settings: the ones given, else the stored ones; checked for the kind.
    let cloud_settings = match cloud_kind {
        Some(kind) => {
            if let Some(s) = &exists
                && s.cloud_kind() != Some(kind)
            {
                return Err(ApiError::invalid(format!(
                    "cannot store a {} key on subscription `{name}`: it is a {} subscription",
                    key.provider, s.provider
                )));
            }
            if key.cloud.is_some() && exists.as_ref().is_some_and(|s| s.source != SOURCE_API) {
                return Err(ApiError::invalid(format!(
                    "subscription `{name}` comes from a file; set its `[cloud]` table there"
                )));
            }
            // A body without `models` keeps the stored model map.
            let stored = exists.as_ref().and_then(|s| s.cloud.clone());
            let mut settings = match key.cloud.clone() {
                Some(mut c) => {
                    if c.models.is_empty()
                        && let Some(st) = &stored
                    {
                        c.models = st.models.clone();
                    }
                    c
                }
                None => stored.unwrap_or_default(),
            };
            if kind == terrarium_core::cloud::Kind::Bedrock {
                let mut models = terrarium_core::cloud::bedrock_models();
                models.extend(settings.models);
                settings.models = models;
                settings.region.get_or_insert_with(|| "us-east-1".into());
            }
            terrarium_core::cloud::check(kind, &settings)
                .map_err(|e| ApiError::invalid(format!("subscription `{name}`: {e}")))?;
            Some(settings)
        }
        None => None,
    };
    // Before any key or definition write. Never return an AWS response body,
    // request error, URL, signature or credential in the diagnostic.
    if test_connection {
        bedrock::test(
            &key,
            cloud_settings
                .as_ref()
                .ok_or_else(|| ApiError::invalid("missing cloud settings"))?,
        )
        .await?;
    }
    let source = exists.as_ref().map(|s| s.source.clone());
    // Validate before writing the credential file, which is outside the DB transaction.
    if key.provider == "litellm" && exists.is_none() {
        let sh = SelfHosted {
            api: Api::Anthropic,
            base_url: key.base_url.clone().unwrap_or_default(),
            models: key.models.clone(),
            ..SelfHosted::default()
        }
        .checked()
        .map_err(ApiError::invalid)?;
        if !sh.base_url.to_ascii_lowercase().starts_with("https://") {
            return Err(ApiError::invalid("LiteLLM base_url must use https"));
        }
    }
    let dir = state.state_dir.clone();
    // The file keeps the secrets; the settings go to the definition.
    let k = ApiKey {
        cloud: None,
        key: if cloud_kind.is_some() {
            key.key.trim().to_owned()
        } else {
            key.key.clone()
        },
        ..key.clone()
    };
    let (n, provider, base_url, models) = (
        name.clone(),
        key.provider.clone(),
        key.base_url.clone(),
        key.models.clone(),
    );
    state
        .db
        .write(actor.label(), move |t| {
            // The key file, then the matcher, in the write: the new key
            // joins the redaction matcher before any other write (PLAN
            // §16b.6, S9 and S11).
            login::store_key(&dir, &n, &k)
                .map_err(|e| ApiError::internal(format!("cannot store the key: {e}")))?;
            t.rebuild_redactor(&dir);
            if let (Some(kind), Some(settings)) = (cloud_kind, cloud_settings.as_ref()) {
                // Claude Code only; never a default or a pool pick
                // (`terrarium_core::cloud`).
                if source.is_none() {
                    let def = SubscriptionDef {
                        name: n.clone(),
                        provider: kind.as_str().into(),
                        plan: None,
                        harnesses: terrarium_core::cloud::HARNESSES.to_vec(),
                        probe: None,
                        alert_at: 80,
                        selfhosted: None,
                        cloud: Some(settings.clone()),
                    };
                    subs::upsert_def(t, &def, SOURCE_API)?;
                } else if source.as_deref() == Some(SOURCE_API) {
                    subs::set_cloud(t, &n, settings)?;
                }
            } else if let Some(plan) = key_plan {
                // Z.ai: Claude Code and pi. Moonshot: pi only. No probe in
                // the definition: the read gives the default probe of the
                // plan (`subs::default_probe`).
                if source.is_none() {
                    let harnesses = if moonshot {
                        terrarium_core::moonshot::HARNESSES.to_vec()
                    } else {
                        vec![HarnessId::Claude, HarnessId::Pi]
                    };
                    let def = SubscriptionDef {
                        name: n.clone(),
                        provider: provider.clone(),
                        plan: Some(plan.into()),
                        harnesses,
                        probe: None,
                        alert_at: 80,
                        selfhosted: None,
                        cloud: None,
                    };
                    subs::upsert_def(t, &def, SOURCE_API)?;
                } else if source.as_deref() == Some(SOURCE_API) {
                    subs::set_plan(t, &n, plan)?;
                }
            } else if source.is_none() && is_sh {
                // The old wizard path (`openai-compatible` with a base URL):
                // a self-hosted OpenAI-style server.
                let def = SubscriptionDef {
                    name: n.clone(),
                    provider: if provider == "litellm" { "litellm" } else { selfhosted::PROVIDER }.into(),
                    plan: None,
                    harnesses: Vec::new(),
                    probe: None,
                    alert_at: 80,
                    selfhosted: Some(SelfHosted {
                        api: if provider == "litellm" { Api::Anthropic } else { Api::Openai },
                        base_url: base_url.clone().unwrap_or_default(),
                        models: models.clone(),
                        ..SelfHosted::default()
                    }),
                    cloud: None,
                };
                subs::upsert_def(t, &def, SOURCE_API)?;
            } else if source.is_none() {
                let harness = if provider == "github" { HarnessId::Copilot } else { HarnessId::Pi };
                let def = SubscriptionDef {
                    name: n.clone(),
                    provider: provider.clone(),
                    plan: Some("API key".into()),
                    harnesses: vec![harness],
                    probe: if harness == HarnessId::Copilot {
                        Some(probes::github::NAME.into())
                    } else {
                        None
                    },
                    alert_at: 80,
                    selfhosted: None,
                    cloud: None,
                };
                subs::upsert_def(t, &def, SOURCE_API)?;
            }
            // A file subscription keeps its definition; the env var is
            // daemon state.
            subs::set_env_var(t, &n, Some(var))?;
            let status = if key_plan.is_some() || is_sh || cloud_kind.is_some() {
                subs::API_KEY
            } else {
                subs::LOGGED_IN
            };
            subs::set_login_status(t, &n, status)?;
            t.audit(
                "subscription.api_key",
                &n,
                None,
                json!({"provider": provider, "plan": key_plan, "env_var": var, "base_url": base_url, "models": models,
                       "cloud": cloud_settings}),
            )
        })
        .await?;
    Ok(Json(load(&state, name).await?))
}

/// The name of a provider whose subscription is an API key only (no
/// harness login): Z.ai, Moonshot, self-hosted, and the cloud providers.
fn key_only_provider(provider: &str) -> Option<&'static str> {
    if let Some(k) = terrarium_core::cloud::kind(provider) {
        Some(k.display_name())
    } else if terrarium_core::zai::is_zai(provider) {
        Some("Z.ai")
    } else if terrarium_core::moonshot::is_moonshot(provider) {
        Some("Moonshot")
    } else if selfhosted::is_selfhosted(provider) {
        Some("self-hosted")
    } else {
        None
    }
}

async fn detect(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<Detected>>> {
    policy::manage_subscriptions(&actor, "checking the harness logins")?;
    let all = login::detect_all(&state).await?;
    Ok(Json(
        all.into_iter()
            .filter(|d| d.status.logged_in && d.subscription.is_none())
            .collect(),
    ))
}

async fn login_start(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(req): ApiJson<StartLogin>,
) -> ApiResult<(StatusCode, Json<LoginView>)> {
    policy::operator_only(&actor, "a subscription login")?;
    Ok((StatusCode::CREATED, Json(login::start(&state, req).await?)))
}

async fn login_show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<LoginView>> {
    policy::operator_only(&actor, "a subscription login")?;
    Ok(Json(login::view(&state, &id).await?))
}

async fn login_cancel(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    policy::operator_only(&actor, "a subscription login")?;
    login::cancel(&state, &id).await?;
    let i = id.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("login.cancel", &i, None, json!({}))
        })
        .await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeInput {
    code: String,
}

async fn login_input(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<CodeInput>,
) -> ApiResult<StatusCode> {
    policy::operator_only(&actor, "a subscription login")?;
    login::input(&state, &id, &b.code).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn login_pty(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    policy::operator_only(&actor, "the login terminal")?;
    if !is_websocket(&req) {
        return Err(ApiError::bad_request(
            "the login PTY is a WebSocket; send an upgrade request",
        ));
    }
    let (mut parts, _) = req.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|e| ApiError::bad_request(e.body_text()))?;
    let att = login::attach(&state, &id)?;
    let i = id.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("login.attach", &i, None, json!({}))
        })
        .await?;
    Ok(ws.on_upgrade(move |socket| streams::pty_session(socket, att)))
}
