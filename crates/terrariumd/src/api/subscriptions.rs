//! `/api/subscriptions/...`: plans, quota samples, probes, login sessions,
//! and API keys (PLAN §16, §17).

use axum::extract::{FromRequestParts, Path, Request, State, ws::WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
}

impl From<QuotaSample> for WindowView {
    fn from(q: QuotaSample) -> Self {
        Self {
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
    Ok(SubscriptionView {
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
    state
        .db
        .write(actor.label(), move |t| {
            subs::delete(t, &name)?;
            t.audit("subscription.delete", &name, None, json!({}))
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
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
    if sub.env_var.is_some() && login::stored_key(&state.state_dir, &sub.name).is_some() {
        let n = name.clone();
        state
            .db
            .write(actor.label(), move |t| {
                subs::set_login_status(t, &n, subs::LOGGED_IN)
            })
            .await?;
        return Ok(Json(
            json!({"subscription": load(&state, name).await?, "status": {"logged_in": true, "detail": "a stored API key"}}),
        ));
    }
    let Some((kind, provider)) = login::kind_of(&sub) else {
        return Err(ApiError::invalid(format!(
            "subscription `{name}` names no harness to check"
        )));
    };
    let st = login::check_status(&state, kind, provider.as_deref()).await;
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
            "unknown provider `{}`; use anthropic, openai, openai-compatible, google, openrouter, xai, groq, or github",
            key.provider
        )));
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
            if source.is_none() {
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
            subs::set_login_status(t, &n, subs::LOGGED_IN)?;
            t.audit(
                "subscription.api_key",
                &n,
                None,
                json!({"provider": provider, "env_var": var, "base_url": base_url, "models": models}),
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
