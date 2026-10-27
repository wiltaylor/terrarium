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
use crate::store::usage as ustore;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/subscriptions", get(list).post(create))
        .route("/subscriptions/detect", get(detect))
        .route("/subscriptions/login", post(login_start))
        .route(
            "/subscriptions/login/{id}",
            get(login_show).delete(login_cancel),
        )
        .route("/subscriptions/login/{id}/input", post(login_input))
        .route("/subscriptions/login/{id}/pty", get(login_pty))
        .route("/subscriptions/{name}", get(show).delete(remove))
        .route("/subscriptions/{name}/quota", get(quota))
        .route("/subscriptions/{name}/models", get(models))
        .route("/subscriptions/{name}/probe", post(probe))
        .route("/subscriptions/{name}/verify", post(verify))
        .route("/subscriptions/{name}/logout", post(logout))
        .route("/subscriptions/{name}/api-key", post(api_key))
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

/// A credit balance, from the `raw` of a credits window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Balance {
    pub total: f64,
    pub used: f64,
    pub remaining: f64,
    pub currency: String,
}

impl Balance {
    fn from_raw(raw: &serde_json::Value) -> Option<Self> {
        let num = |k: &str| raw.get(k).and_then(serde_json::Value::as_f64);
        Some(Self {
            total: num("total_credits")?,
            used: num("total_usage")?,
            remaining: num("remaining")?,
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
    #[serde(flatten)]
    pub sub: Subscription,
    /// The newest sample per window.
    pub windows: Vec<WindowView>,
    /// No probe: the only numbers are local accounting.
    pub local_estimate_only: bool,
    /// An API key is stored for it (never shown).
    pub has_api_key: bool,
    /// Agents whose turns wait for a reset of this subscription.
    pub held_agents: usize,
    /// The model each harness gets on this subscription when the agent
    /// sets no `model`, by harness name. Only the harnesses where the
    /// daemon picks the model: pi (the provider's default,
    /// `terrarium_adapter_pi::flags::default_model`) and Claude Code on
    /// Z.ai (`terrarium_core::zai::model_for`). No pi entry when pi
    /// refuses the subscription. Empty (`{}`) when the harnesses keep their
    /// own defaults; always present, so a client can tell an older daemon.
    pub default_models: BTreeMap<String, String>,
    /// The plan account of the subscription (PLAN §16.3), when it has one.
    /// No secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountView>,
    /// The agents whose current subscription this is.
    pub agents: Vec<AgentOn>,
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

fn view(
    c: &rusqlite::Connection,
    sub: Subscription,
    state_dir: &std::path::Path,
) -> ApiResult<SubscriptionView> {
    let windows = subs::latest_windows(c, &sub.name)?
        .into_iter()
        .map(WindowView::from)
        .collect();
    let cause = format!("quota:{}", sub.name);
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
        account,
        agents,
        default_models: crate::models::default_models(&sub),
        local_estimate_only: sub.probe.is_none(),
        has_api_key: sub.env_var.is_some() && login::stored_key(state_dir, &sub.name).is_some(),
        windows,
        held_agents,
        sub,
    })
}

async fn load(state: &Shared, name: String) -> ApiResult<SubscriptionView> {
    let dir = state.state_dir.clone();
    state
        .db
        .read(move |c| {
            let s = subs::get(c, &name)?;
            view(c, s, &dir)
        })
        .await
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<SubscriptionView>>> {
    policy::read_subscriptions(&actor)?;
    let dir = state.state_dir.clone();
    let v = state
        .db
        .read(move |c| {
            subs::list(c)?
                .into_iter()
                .map(|s| view(c, s, &dir))
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
    let def = SubscriptionDef {
        name: b.name,
        provider: b.provider,
        plan: b.plan,
        harnesses: b.harnesses,
        probe: b.probe,
        alert_at: b.alert_at.unwrap_or(80),
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
            subs::upsert_def(t, &def, SOURCE_API)?;
            t.audit(
                "subscription.create",
                &def.name,
                None,
                json!({"provider": def.provider, "probe": def.probe}),
            )
        })
        .await?;
    Ok((StatusCode::CREATED, Json(load(&state, name).await?)))
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
            subs::delete(t, &n)?;
            t.audit(
                "subscription.delete",
                &n,
                None,
                json!({"account_removed": account}),
            )
        })
        .await?;
    if account {
        let dir = state.state_dir.clone();
        tokio::task::spawn_blocking(move || crate::accounts::remove(&dir, &name))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::internal(format!("cannot remove the account: {e}")))?;
    }
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
    let zai = terrarium_core::zai::is_zai(&sub.provider);
    if sub.env_var.is_some() && login::stored_key(&state.state_dir, &sub.name).is_some() {
        let n = name.clone();
        let status = if zai { subs::API_KEY } else { subs::LOGGED_IN };
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
    // A Z.ai subscription is a key only. Its first harness (claude) would
    // report the shared Claude login, which is not this subscription.
    if zai {
        let n = name.clone();
        state
            .db
            .write(actor.label(), move |t| {
                subs::set_login_status(t, &n, subs::MISSING)
            })
            .await?;
        return Ok(Json(
            json!({"subscription": load(&state, name).await?, "status": {"logged_in": false, "detail": "no stored Z.ai API key"}}),
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
            if let Some(p) = plan
                && subs::get(t, &n)?.plan.is_none()
            {
                subs::set_plan(t, &n, &p)?;
            }
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
    if terrarium_core::zai::is_zai(&sub.provider) {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` is a Z.ai API key and has no logout; store another key, or delete the subscription"
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
                t.audit(
                    "subscription.logout",
                    &n,
                    None,
                    json!({"account_removed": true}),
                )
            })
            .await?;
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

/// `POST /api/subscriptions/{name}/api-key`. The answer never has the key.
async fn api_key(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(key): ApiJson<ApiKey>,
) -> ApiResult<Json<SubscriptionView>> {
    policy::operator_only(&actor, "storing an API key")?;
    if key.key.trim().is_empty() {
        return Err(ApiError::invalid("the key is empty"));
    }
    let Some((var, url_var)) = login::env_var_for(&key.provider) else {
        return Err(ApiError::invalid(format!(
            "unknown provider `{}`; use anthropic, openai, openai-compatible, google, openrouter, xai, groq, github, or zai",
            key.provider
        )));
    };
    let zai = terrarium_core::zai::is_zai(&key.provider);
    let zai_plan = if zai {
        Some(terrarium_core::zai::parse_plan(key.plan.as_deref()).map_err(ApiError::invalid)?)
    } else if key.plan.is_some() {
        return Err(ApiError::invalid(format!(
            "`plan` is only for provider zai, not `{}`",
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
    let source = exists.as_ref().map(|s| s.source.clone());
    let dir = state.state_dir.clone();
    let (n, k) = (name.clone(), key.clone());
    tokio::task::spawn_blocking(move || login::store_key(&dir, &n, &k))
        .await
        .map_err(|e| ApiError::internal(format!("key task failed: {e}")))?
        .map_err(|e| ApiError::internal(format!("cannot store the key: {e}")))?;
    let (n, provider, base_url, models) = (
        name.clone(),
        key.provider.clone(),
        key.base_url.clone(),
        key.models.clone(),
    );
    state
        .db
        .write(actor.label(), move |t| {
            if let Some(plan) = zai_plan {
                // Z.ai: Claude Code and pi. No probe: Z.ai documents no
                // usage endpoint (`docs/compat.md`, "Z.ai").
                if source.is_none() {
                    let def = SubscriptionDef {
                        name: n.clone(),
                        provider: provider.clone(),
                        plan: Some(plan.as_str().into()),
                        harnesses: vec![HarnessId::Claude, HarnessId::Pi],
                        probe: None,
                        alert_at: 80,
                    };
                    subs::upsert_def(t, &def, SOURCE_API)?;
                } else if source.as_deref() == Some(SOURCE_API) {
                    subs::set_plan(t, &n, plan.as_str())?;
                }
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
                };
                subs::upsert_def(t, &def, SOURCE_API)?;
            }
            // A file subscription keeps its definition; the env var is
            // daemon state.
            subs::set_env_var(t, &n, Some(var))?;
            let status = if zai_plan.is_some() { subs::API_KEY } else { subs::LOGGED_IN };
            subs::set_login_status(t, &n, status)?;
            t.audit(
                "subscription.api_key",
                &n,
                None,
                json!({"provider": provider, "plan": zai_plan.map(|p| p.as_str()), "env_var": var, "base_url": base_url, "models": models}),
            )
        })
        .await?;
    Ok(Json(load(&state, name).await?))
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
