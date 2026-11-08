//! `/api/projects/{name}/board`, `/api/projects/{name}/cards`, `/api/cards/...`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::longpoll::{self, WaitQuery};
use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::ApiResult;
use crate::events::{self, EventFilter};
use crate::policy;
use crate::store::Page;
use crate::store::board::{self, Board, Card, CardFilter, CardPatch, ColumnSpec, NewCard, NewLink};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/projects/{name}/board", get(show_board).patch(patch_board))
        .route("/projects/{name}/cards", get(list_cards).post(create_card))
        .route("/projects/{name}/github-issues/import", post(import_issue))
        .route("/cards", get(active_cards))
        .route("/cards/{id}", get(show).patch(update).delete(archive))
        .route("/cards/{id}/move", post(move_card))
        .route("/cards/{id}/assign", post(assign))
        .route("/cards/{id}/claim", post(claim))
        .route("/cards/{id}/comments", get(comments).post(comment))
        .route("/cards/{id}/links", post(link))
        .route("/cards/{id}/events", get(card_events))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportIssue {
    repo: String,
    number: u64,
}

async fn import_issue(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<ImportIssue>,
) -> ApiResult<Json<Card>> {
    Ok(Json(
        crate::github_issues::import(&state, actor, name, body.repo, body.number).await?,
    ))
}

async fn show_board(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(query): ApiQuery<BoardQuery>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Json<serde_json::Value>> {
    policy::read_project(&actor, &name)?;
    Ok(Json(
        state
            .db
            .read(move |c| board::board_summary(c, &name, query.done, &page))
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
struct BoardQuery {
    #[serde(default)]
    done: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardPatch {
    columns: Vec<ColumnSpec>,
}

async fn patch_board(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<BoardPatch>,
) -> ApiResult<Json<Board>> {
    let b = state
        .db
        .write(actor.label(), move |t| {
            board::set_columns(t, &actor, &name, &body.columns)
        })
        .await?;
    Ok(Json(b))
}

async fn list_cards(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(f): ApiQuery<CardFilter>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let rows = longpoll::hold(&state, wait, &["card"], || {
        let (actor, name, page, f) = (actor.clone(), name.clone(), page.clone(), f.clone());
        state.db.read(move |c| {
            crate::store::projects::get(c, &name)?;
            let assignee = match f.assignee.as_deref() {
                None => None,
                Some("me") => Some(actor.address()),
                Some(a) => Some(crate::store::mail::resolve_known(c, a, Some(&name))?.key()),
            };
            board::list_cards(c, &name, &f, assignee.as_deref(), &page)
        })
    })
    .await?;
    Ok(paged(rows))
}

#[derive(Debug, Default, Deserialize)]
struct ActiveQuery {
    /// Comma-separated roles or column names. Default `working,research`.
    #[serde(default)]
    columns: Option<String>,
}

/// `GET /api/cards?columns=working,research` (TER-43): the cards that are
/// not archived in those columns of every board that the caller may read:
/// each project for the operator and a sysadmin, the caller's own project
/// for another agent. Each card has `column_kind` and `assignee_status`.
async fn active_cards(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ActiveQuery>,
) -> ApiResult<Json<Vec<board::ActiveCard>>> {
    let wanted: Vec<String> = q
        .columns
        .as_deref()
        .unwrap_or("working,research")
        .split(',')
        .map(|c| c.trim().to_owned())
        .filter(|c| !c.is_empty())
        .collect();
    let projects: Option<Vec<String>> = match &actor {
        Actor::Operator => None,
        Actor::Agent(a) if a.reads_all() => None,
        Actor::Agent(a) => Some(vec![a.project.clone()]),
    };
    Ok(Json(
        state
            .db
            .read(move |c| board::active_cards(c, projects.as_deref(), &wanted))
            .await?,
    ))
}

async fn create_card(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<NewCard>,
) -> ApiResult<(StatusCode, Json<Card>)> {
    let card = state
        .db
        .write(actor.label(), move |t| {
            board::create_card(t, &actor, &name, body)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(card)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Card>> {
    let card = state.db.read(move |c| board::get_card(c, &id)).await?;
    policy::read_project(&actor, &card.project)?;
    Ok(Json(card))
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<CardPatch>,
) -> ApiResult<Json<Card>> {
    let card = state
        .db
        .write(actor.label(), move |t| {
            board::update_card(t, &actor, &id, patch)
        })
        .await?;
    Ok(Json(card))
}

async fn archive(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    state
        .db
        .write(actor.label(), move |t| board::archive_card(t, &actor, &id))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveBody {
    column: String,
    #[serde(default)]
    position: Option<i64>,
    #[serde(default)]
    comment: Option<String>,
}

async fn move_card(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(m): ApiJson<MoveBody>,
) -> ApiResult<Json<Card>> {
    let card = state
        .db
        .write(actor.label(), move |t| {
            board::move_card(t, &actor, &id, &m.column, m.position, m.comment.as_deref())
        })
        .await?;
    Ok(Json(card))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignBody {
    /// An address, `me`, or `null` to unassign.
    assignee: Option<String>,
}

async fn assign(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(a): ApiJson<AssignBody>,
) -> ApiResult<Json<Card>> {
    let card = state
        .db
        .write(actor.label(), move |t| {
            board::assign_card(t, &actor, &id, a.assignee.as_deref())
        })
        .await?;
    Ok(Json(card))
}

async fn claim(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Card>> {
    let card = state
        .db
        .write(actor.label(), move |t| board::claim_card(t, &actor, &id))
        .await?;
    Ok(Json(card))
}

async fn comments(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<board::Comment>>> {
    let card = state.db.read(move |c| board::get_card(c, &id)).await?;
    policy::read_project(&actor, &card.project)?;
    Ok(Json(card.comments))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentBody {
    body: String,
    #[serde(default)]
    reply_to: Option<String>,
}

async fn comment(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(c): ApiJson<CommentBody>,
) -> ApiResult<(StatusCode, Json<Card>)> {
    let card = state
        .db
        .write(actor.label(), move |t| {
            board::reply_comment(t, &actor, &id, &c.body, c.reply_to.as_deref())
        })
        .await?;
    Ok((StatusCode::CREATED, Json(card)))
}

async fn link(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(l): ApiJson<NewLink>,
) -> ApiResult<(StatusCode, Json<Card>)> {
    let card = state
        .db
        .write(actor.label(), move |t| board::add_link(t, &actor, &id, &l))
        .await?;
    Ok((StatusCode::CREATED, Json(card)))
}

#[derive(Debug, Default, Deserialize)]
struct SinceQuery {
    since: Option<String>,
}

async fn card_events(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<SinceQuery>,
) -> ApiResult<Response> {
    let rows = state
        .db
        .read(move |c| {
            let card = board::get_card(c, &id)?;
            policy::read_project(&actor, &card.project)?;
            let since = match page.cursor() {
                Some(cur) => events::resolve_since(c, Some(cur))?,
                None => events::resolve_since(c, q.since.as_deref())?,
            };
            let filter = EventFilter {
                card: Some(card.id),
                ..EventFilter::default()
            };
            let limit = page.limit();
            let rows = events::list(c, &filter, since, limit + 1)?;
            Ok(crate::store::Paged::from_rows(rows, limit, |e| {
                e.seq.to_string()
            }))
        })
        .await?;
    Ok(paged(rows))
}
