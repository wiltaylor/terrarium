//! `/api/usage...`: local accounting for the dashboard and for
//! `terrarium usage` (PLAN §16.2), and the agent status self-report.

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::AgentStatus;

use super::agents::load as load_agent;
use super::subscriptions::WindowView;
use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventDraft, StoredEvent};
use crate::policy;
use crate::quota_policy::{self, Budget};
use crate::store::subscriptions as subs;
use crate::store::usage::{self as ustore, Bucket, GroupBy, Hold, Totals, UsageFilter, UsagePoint};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/usage",
            get(series),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/usage/me",
            get(me),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/usage/project",
            get(project),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/cards/{id}/usage",
            get(card_usage),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/trouble",
            get(trouble_settings).put(update_trouble_settings),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::PUT, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/agents/{id}/status",
            post(self_report),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
}

async fn trouble_settings(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<crate::trouble::Settings>> {
    policy::read_project(&actor, &name)?;
    Ok(Json(
        state
            .db
            .read(move |c| {
                crate::store::projects::get(c, &name)?;
                crate::trouble::settings(c, &name)
            })
            .await?,
    ))
}

async fn update_trouble_settings(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(settings): ApiJson<crate::trouble::Settings>,
) -> ApiResult<Json<crate::trouble::Settings>> {
    policy::admin_project(&actor, &name, "changing trouble thresholds")?;
    let result = state
        .db
        .write(actor.label(), move |t| {
            crate::trouble::set_settings(t, &name, &settings)?;
            Ok(settings)
        })
        .await?;
    Ok(Json(result))
}

#[derive(Debug, Default, Deserialize)]
struct UsageQuery {
    subscription: Option<String>,
    project: Option<String>,
    /// An agent key: ULID, `project/name`, or a name.
    agent: Option<String>,
    model: Option<String>,
    /// A card ID (`TER-42`, any case).
    card: Option<String>,
    since: Option<String>,
    until: Option<String>,
    #[serde(default)]
    bucket: Bucket,
    group: Option<GroupBy>,
}

fn day_ago() -> String {
    (chrono::Utc::now() - chrono::Duration::hours(24))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn time_or(v: Option<&str>, what: &str) -> ApiResult<Option<String>> {
    crate::time_param::opt_time_param(what, v)
}

/// `GET /api/usage`: a series of token sums, one point per bucket (and per
/// group value). An agent sees its own project only, unless it is a
/// `sysadmin`.
async fn series(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<UsageQuery>,
) -> ApiResult<Json<Vec<UsagePoint>>> {
    let mut f = UsageFilter {
        subscription: q.subscription.filter(|s| !s.is_empty()),
        project: q.project.filter(|s| !s.is_empty()),
        agent: None,
        model: q.model.filter(|s| !s.is_empty()),
        card: q
            .card
            .filter(|s| !s.trim().is_empty())
            .map(|c| crate::store::board::normalise_id(&c)),
        since: Some(time_or(q.since.as_deref(), "since")?.unwrap_or_else(day_ago)),
        until: time_or(q.until.as_deref(), "until")?,
    };
    if let Some(key) = q.agent.filter(|s| !s.is_empty()) {
        let a = load_agent(&state, &actor, key).await?;
        f.agent = Some(a.agent.id.to_string());
    }
    if let Actor::Agent(a) = &actor
        && !a.reads_all()
    {
        match &f.project {
            Some(p) => policy::read_project(&actor, p)?,
            None => f.project = Some(a.project.clone()),
        }
    }
    let v = state
        .db
        .read(move |c| ustore::series(c, &f, q.bucket, q.group))
        .await?;
    Ok(Json(v))
}

/// `GET /api/usage/me`.
#[derive(Debug, Serialize)]
struct Me {
    agent: AgentRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription: Option<SubRef>,
    /// The UTC day so far.
    today: Today,
    /// Holds on this agent's turns (a plan at its limit, a budget).
    holds: Vec<Hold>,
}

#[derive(Debug, Serialize)]
struct AgentRef {
    id: String,
    name: String,
    project: String,
}

#[derive(Debug, Serialize)]
struct SubRef {
    name: String,
    /// The name for people (`store::subscriptions::display_name`).
    display_name: String,
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    probe_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_success_at: Option<String>,
    local_estimate_only: bool,
    windows: Vec<WindowView>,
}

#[derive(Debug, Serialize)]
struct Today {
    since: String,
    mine: Totals,
    /// All agents on the same subscription, by local accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription: Option<Totals>,
    /// My input + output tokens as a percent of the subscription's.
    #[serde(skip_serializing_if = "Option::is_none")]
    share_pct: Option<f64>,
}

fn share(part: u64, whole: u64) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    (whole > 0).then(|| (part as f64 / whole as f64 * 1000.0).round() / 10.0)
}

#[derive(Debug, Default, Deserialize)]
struct MeQuery {
    /// For the operator: the agent to report on.
    agent: Option<String>,
}

/// `GET /api/usage/me`: the caller's subscription windows and its share
/// today. The operator names the agent with `?agent=`.
async fn me(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<MeQuery>,
) -> ApiResult<Json<Me>> {
    let key = match (&actor, q.agent) {
        (_, Some(k)) if !k.is_empty() => k,
        (Actor::Agent(a), _) => a.id.clone(),
        (Actor::Operator, _) => {
            return Err(ApiError::bad_request(
                "/api/usage/me reports on the calling agent; the operator adds ?agent=",
            ));
        }
    };
    let a = load_agent(&state, &actor, key).await?;
    let id = a.agent.id.to_string();
    let sub_name = a.agent.subscription.clone();
    let since = ustore::today_start();
    let me = AgentRef {
        id: id.clone(),
        name: a.agent.name.clone(),
        project: a.agent.project.clone(),
    };
    let v = state
        .db
        .read(move |c| {
            let mine = ustore::totals(
                c,
                &UsageFilter {
                    agent: Some(id.clone()),
                    since: Some(since.clone()),
                    ..Default::default()
                },
            )?;
            let mut subscription = None;
            let mut sub_totals = None;
            if let Some(n) = &sub_name
                && let Some(s) = subs::find(c, n)?
            {
                let all = ustore::totals(
                    c,
                    &UsageFilter {
                        subscription: Some(n.clone()),
                        since: Some(since.clone()),
                        ..Default::default()
                    },
                )?;
                sub_totals = Some(all);
                subscription = Some(SubRef {
                    display_name: subs::display_name(c, &s)?,
                    windows: subs::latest_windows(c, n)?
                        .into_iter()
                        .map(WindowView::from)
                        .collect(),
                    local_estimate_only: s.probe.is_none(),
                    name: s.name,
                    provider: s.provider,
                    plan: s.plan,
                    probe_status: s.probe_status,
                    last_success_at: s.last_success_at,
                });
            }
            Ok(Me {
                agent: me,
                subscription,
                today: Today {
                    since,
                    share_pct: sub_totals.and_then(|t| share(mine.tokens(), t.tokens())),
                    subscription: sub_totals,
                    mine,
                },
                holds: ustore::holds_of(c, &id)?,
            })
        })
        .await?;
    Ok(Json(v))
}

#[derive(Debug, Default, Deserialize)]
struct ProjectQuery {
    project: Option<String>,
    since: Option<String>,
}

#[derive(Debug, Serialize)]
struct AgentUsage {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription: Option<String>,
    #[serde(flatten)]
    totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    share_pct: Option<f64>,
}

#[derive(Debug, Serialize)]
struct BudgetView {
    #[serde(flatten)]
    budget: Budget,
    used_tokens: u64,
    used_turns: u64,
}

#[derive(Debug, Serialize)]
struct ProjectUsage {
    project: String,
    since: String,
    total: Totals,
    agents: Vec<AgentUsage>,
    budgets: Vec<BudgetView>,
}

/// `GET /api/usage/project`: the per-agent split of the caller's project
/// (or `?project=`), since the start of the UTC day (or `?since=`).
async fn project(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ProjectQuery>,
) -> ApiResult<Json<ProjectUsage>> {
    let project = match (q.project.filter(|p| !p.is_empty()), actor.project()) {
        (Some(p), _) => p,
        (None, Some(p)) => p.to_owned(),
        (None, None) => return Err(ApiError::bad_request("give ?project=")),
    };
    policy::read_project(&actor, &project)?;
    let since = time_or(q.since.as_deref(), "since")?.unwrap_or_else(ustore::today_start);
    let budgets = quota_policy::budgets(&state.config_dir, &project);
    let p = project.clone();
    let s = since.clone();
    let v = state
        .db
        .read(move |c| {
            crate::store::projects::get(c, &p)?;
            let f = UsageFilter {
                project: Some(p.clone()),
                since: Some(s.clone()),
                ..Default::default()
            };
            let total = ustore::totals(c, &f)?;
            let mut agents: Vec<AgentUsage> = ustore::by_agent(c, &f)?
                .into_iter()
                .map(|(id, totals)| {
                    let (name, subscription) = crate::store::agents::get(c, &id)
                        .map(|a| (a.agent.name, a.agent.subscription))
                        .unwrap_or_else(|_| (id.clone(), None));
                    AgentUsage {
                        share_pct: share(totals.tokens(), total.tokens()),
                        id,
                        name,
                        subscription,
                        totals,
                    }
                })
                .collect();
            // Agents with no usage yet, so the table lists the whole project.
            for (id, name, subscription) in ustore::agents_of(c, &p, None)? {
                if !agents.iter().any(|a| a.id == id) {
                    agents.push(AgentUsage {
                        id,
                        name,
                        subscription,
                        totals: Totals::default(),
                        share_pct: None,
                    });
                }
            }
            let today = ustore::today_start();
            let budgets = budgets
                .into_iter()
                .map(|b| {
                    let t = ustore::totals(
                        c,
                        &UsageFilter {
                            project: Some(p.clone()),
                            subscription: b.subscription.clone(),
                            since: Some(today.clone()),
                            ..Default::default()
                        },
                    )?;
                    Ok(BudgetView {
                        budget: b,
                        used_tokens: t.tokens(),
                        used_turns: t.turns,
                    })
                })
                .collect::<ApiResult<Vec<_>>>()?;
            Ok(ProjectUsage {
                project: p,
                since: s,
                total,
                agents,
                budgets,
            })
        })
        .await?;
    Ok(Json(v))
}

#[derive(Debug, Default, Deserialize)]
struct CardUsageQuery {
    since: Option<String>,
    until: Option<String>,
}

#[derive(Debug, Serialize)]
struct ModelUsage {
    model: String,
    #[serde(flatten)]
    totals: Totals,
}

#[derive(Debug, Serialize)]
struct CardAgentUsage {
    id: String,
    name: String,
    #[serde(flatten)]
    totals: Totals,
}

#[derive(Debug, Serialize)]
struct CardUsage {
    #[serde(flatten)]
    trouble: crate::trouble::View,
    card: String,
    title: String,
    project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<String>,
    total: Totals,
    models: Vec<ModelUsage>,
    agents: Vec<CardAgentUsage>,
}

/// `GET /api/cards/{id}/usage`: the samples attributed to one card, in
/// total, by model, and by agent. All time unless `?since=` or `?until=`.
async fn card_usage(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<CardUsageQuery>,
) -> ApiResult<Json<CardUsage>> {
    let since = time_or(q.since.as_deref(), "since")?;
    let until = time_or(q.until.as_deref(), "until")?;
    let card = state
        .db
        .read(move |c| crate::store::board::get_card(c, &id))
        .await?;
    policy::read_project(&actor, &card.project)?;
    let v = state
        .db
        .read(move |c| {
            let f = UsageFilter {
                card: Some(card.id.clone()),
                since: since.clone(),
                until: until.clone(),
                ..Default::default()
            };
            let models = ustore::by_model(c, &f)?
                .into_iter()
                .map(|(model, totals)| ModelUsage { model, totals })
                .collect();
            let agents = ustore::by_agent(c, &f)?
                .into_iter()
                .map(|(id, totals)| CardAgentUsage {
                    name: crate::store::agents::known_name(c, &id)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| id.clone()),
                    id,
                    totals,
                })
                .collect();
            Ok(CardUsage {
                trouble: crate::trouble::view(c, &card.id, chrono::Utc::now())?,
                total: ustore::totals(c, &f)?,
                card: card.id,
                title: card.title,
                project: card.project,
                since,
                until,
                models,
                agents,
            })
        })
        .await?;
    Ok(Json(v))
}

/// `POST /api/agents/{id}/status`: `terrarium status set`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelfReport {
    status: AgentStatus,
    #[serde(default)]
    note: Option<String>,
}

/// An agent's own word on what it does. It is a `self_report` event; the
/// runner's status machine keeps the real status.
async fn self_report(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<SelfReport>,
) -> ApiResult<Json<StoredEvent>> {
    if !matches!(
        b.status,
        AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked
    ) {
        return Err(ApiError::invalid(
            "a self-report status is idle, working, or blocked",
        ));
    }
    let a = load_agent(&state, &actor, id).await?;
    let id = a.agent.id.to_string();
    if let Actor::Agent(me) = &actor
        && me.id != id
    {
        return Err(ApiError::forbidden("an agent reports its own status only"));
    }
    let note: Option<String> = b
        .note
        .map(|n| n.chars().take(500).collect::<String>())
        .filter(|n| !n.trim().is_empty());
    let (name, project) = (a.agent.name.clone(), a.agent.project.clone());
    let ev = state
        .db
        .write(actor.label(), move |t| {
            let session = crate::store::agents::current_session(t, &id)?;
            t.emit(
                EventDraft::new(
                    "self_report",
                    match &note {
                        Some(n) => format!("{name} says {}: {n}", b.status),
                        None => format!("{name} says {}", b.status),
                    },
                    json!({"status": b.status, "note": note}),
                )
                .agent(id.clone(), name.clone())
                .project(project)
                .session(session),
            )
        })
        .await?;
    Ok(Json(ev))
}
