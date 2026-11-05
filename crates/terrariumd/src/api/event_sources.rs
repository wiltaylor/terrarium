//! `/api/event-sources/...` and `POST /api/events` (docs/api.md §20): the
//! webhook senders of the ingest listener (`crate::ingest`), and events
//! that a client adds to the log itself.
//!
//! Rights are those of schedules (§19.3): members read the sources of
//! their project, a project_admin manages them, a global source needs a
//! sysadmin. The secret is in the answers of create and rotate only.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use terrarium_core::ci;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventDraft, StoredEvent};
use crate::policy;
use crate::store::event_sources::{
    self, EventSource, ListFilter, NewSource, SourceKind, SourcePatch,
};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/event-sources", get(list).post(create))
        .route(
            "/event-sources/{id}",
            get(show).patch(update).delete(remove),
        )
        .route("/event-sources/{id}/rotate", post(rotate))
}

/// The emit route, `POST /api/events`. The router adds it to the `GET`
/// of the same path (the firehose).
pub async fn emit(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<EmitBody>,
) -> ApiResult<(StatusCode, Json<StoredEvent>)> {
    let kind = b.kind.trim().to_owned();
    if !ci::valid_kind(&kind) {
        return Err(ApiError::invalid(format!(
            "bad kind `{kind}`: use 1-100 of a-z, 0-9, `_`, `-`, in segments split by `.`, \
             starting with a letter"
        )));
    }
    let data = b.data.unwrap_or_else(|| json!({}));
    if !data.is_object() {
        return Err(ApiError::invalid("data: use a JSON object"));
    }
    let summary = ci::clean(b.summary.as_deref().unwrap_or(&kind), ci::MAX_SUMMARY);
    let project = match (&actor, b.project) {
        (Actor::Operator, Some(p)) => p.filter(|p| !p.is_empty()),
        (Actor::Operator, None) => None,
        (Actor::Agent(a), p) => {
            if !ci::external_kind(&kind) {
                return Err(ApiError::forbidden(format!(
                    "an agent emits only the kinds {}; `{kind}` is a kind of the daemon",
                    ci::EXTERNAL_PREFIXES
                        .iter()
                        .map(|p| format!("`{p}*`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            match p.flatten().filter(|p| !p.is_empty()) {
                Some(p) if p != a.project => {
                    return Err(ApiError::forbidden(format!(
                        "an agent emits into its own project only (`{}`), not `{p}`",
                        a.project
                    )));
                }
                _ => Some(a.project.clone()),
            }
        }
    };
    let agent = actor.agent().map(|a| (a.id.clone(), a.name.clone()));
    let e = state
        .db
        .write(actor.label(), move |t| {
            if let Some(p) = &project
                && crate::store::projects::find(t, p)?.is_none()
            {
                return Err(ApiError::not_found(format!("no project `{p}`")));
            }
            let mut d = EventDraft::new(kind, summary, data);
            if let Some(p) = project {
                d = d.project(p);
            }
            if let Some((id, name)) = agent {
                d = d.agent(id, name);
            }
            t.emit(d)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(e)))
}

/// The body of `POST /api/events`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmitBody {
    kind: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    data: Option<Value>,
    /// Absent: the agent's project (the operator: none). `null`: none.
    #[serde(default, deserialize_with = "explicit")]
    project: Option<Option<String>>,
}

/// `Some(None)` for an explicit `null`, `None` when the key is absent.
fn explicit<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Read the sources of `project` (`None`: the global ones).
fn read_scope(actor: &Actor, project: Option<&str>) -> ApiResult<()> {
    match project {
        Some(p) => policy::read_project(actor, p),
        None => policy::read_system(actor, "reading the global event sources"),
    }
}

/// Create, change, rotate, or delete a source of `project` (`None`:
/// global).
fn manage_scope(actor: &Actor, project: Option<&str>) -> ApiResult<()> {
    match project {
        Some(p) => policy::admin_project(actor, p, &format!("managing the event sources of `{p}`")),
        None => policy::sysadmin(actor, "managing a global event source"),
    }
}

/// A source as the API shows it: the row, its hook URL, and the setup
/// steps of its sender. `secret` only in the answers of create and rotate.
#[derive(Debug, Serialize)]
struct SourceView {
    #[serde(flatten)]
    source: EventSource,
    /// `/hooks/{project}/{name}`; `_` is the global scope.
    hook_path: String,
    /// The URL to give the sender: `TERRARIUM_INGEST_PUBLIC_URL` and the
    /// path. `null` when the daemon cannot tell it.
    hook_url: Option<String>,
    /// Why `hook_url` may be wrong or is `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    hook_note: Option<String>,
    setup: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

fn view(source: EventSource, secret: Option<String>) -> SourceView {
    let hook_path = crate::ingest::hook_path(&source);
    let (hook_url, hook_note) = crate::ingest::hook_url(&hook_path);
    SourceView {
        setup: setup_steps(source.kind, hook_url.as_deref().unwrap_or(&hook_path)),
        source,
        hook_path,
        hook_url,
        hook_note,
        secret,
    }
}

/// Short setup steps for the sender of a source.
fn setup_steps(kind: SourceKind, url: &str) -> Vec<String> {
    match kind {
        SourceKind::Github => vec![
            "GitHub: the repository (or organisation) > Settings > Webhooks > Add webhook.".into(),
            format!("Payload URL: {url}"),
            "Content type: application/json.".into(),
            "Secret: the secret of this source (shown at create and at rotate only).".into(),
            "Events: \"Let me select individual events\": Workflow runs, Workflow jobs, \
             Pushes, Pull requests."
                .into(),
            "GitHub sends a ping at once; the source shows it in last_seen.".into(),
        ],
        SourceKind::Forgejo => vec![
            "Forgejo: the repository > Settings > Webhooks > Add webhook > Forgejo.".into(),
            format!("Target URL: {url}"),
            "HTTP method: POST. POST content type: application/json.".into(),
            "Secret: the secret of this source (shown at create and at rotate only).".into(),
            "Trigger on: custom events: Push, Pull request, and the workflow \
             run and workflow job events (Forgejo 17), or the action run \
             events (Forgejo 12 to 16). Forgejo sends no commit status webhook."
                .into(),
            "Use \"Test delivery\" to check it; the source shows it in last_seen.".into(),
        ],
        SourceKind::Generic => vec![
            format!("POST {url} with content type application/json."),
            "Body: {\"kind\": \"ci.run.failed\", \"summary\": \"...\", \"data\": {...}}; the \
             kind starts with ci., git., pr., ext., or agent."
                .into(),
            "Auth: `Authorization: Bearer <secret>`, or `X-Terrarium-Signature: sha256=<hex>` \
             (HMAC-SHA256 of the body with the secret)."
                .into(),
            "Optional: `X-Terrarium-Delivery: <id>`; a repeated ID is dropped.".into(),
        ],
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    project: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<SourceView>>> {
    let global = read_scope(&actor, None).is_ok();
    let f = match q.project.filter(|p| !p.is_empty()) {
        Some(p) => {
            read_scope(&actor, Some(&p))?;
            ListFilter {
                project: Some(p),
                global,
                all: false,
            }
        }
        None if global => ListFilter {
            all: true,
            ..ListFilter::default()
        },
        None => ListFilter {
            project: actor.project().map(str::to_owned),
            ..ListFilter::default()
        },
    };
    let rows = state.db.read(move |c| event_sources::list(c, &f)).await?;
    Ok(Json(rows.into_iter().map(|s| view(s, None)).collect()))
}

/// Find the source of the path and check that the actor may read it.
async fn load(state: &Shared, actor: &Actor, key: String) -> ApiResult<EventSource> {
    let context = actor.project().map(str::to_owned);
    let s = state
        .db
        .read(move |c| event_sources::find(c, &key, context.as_deref()))
        .await?;
    read_scope(actor, s.project.as_deref())?;
    Ok(s)
}

/// The body of `POST /api/event-sources`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBody {
    name: String,
    /// Absent: the caller's project (the operator: global). `null`: global.
    #[serde(default, deserialize_with = "explicit")]
    project: Option<Option<String>>,
    kind: SourceKind,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<NewBody>,
) -> ApiResult<(StatusCode, Json<SourceView>)> {
    let project = match b.project {
        Some(p) => p.filter(|p| !p.is_empty()),
        None => actor.project().map(str::to_owned),
    };
    manage_scope(&actor, project.as_deref())?;
    let new = NewSource {
        name: b.name,
        project,
        kind: b.kind,
        enabled: b.enabled.unwrap_or(true),
        created_by: actor.label(),
    };
    let (s, secret) = state
        .db
        .write(actor.label(), move |t| event_sources::create(t, &new))
        .await?;
    Ok((StatusCode::CREATED, Json(view(s, Some(secret)))))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<SourceView>> {
    Ok(Json(view(load(&state, &actor, id).await?, None)))
}

/// The body of `PATCH /api/event-sources/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<PatchBody>,
) -> ApiResult<Json<SourceView>> {
    let s = load(&state, &actor, id).await?;
    manage_scope(&actor, s.project.as_deref())?;
    let patch = SourcePatch {
        name: b.name,
        enabled: b.enabled,
    };
    let s = state
        .db
        .write(actor.label(), move |t| {
            event_sources::update(t, &s.id, &patch)
        })
        .await?;
    Ok(Json(view(s, None)))
}

async fn rotate(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<SourceView>> {
    let s = load(&state, &actor, id).await?;
    manage_scope(&actor, s.project.as_deref())?;
    let (s, secret) = state
        .db
        .write(actor.label(), move |t| event_sources::rotate(t, &s.id))
        .await?;
    Ok(Json(view(s, Some(secret))))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let s = load(&state, &actor, id).await?;
    manage_scope(&actor, s.project.as_deref())?;
    state
        .db
        .write(actor.label(), move |t| event_sources::delete(t, &s.id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
