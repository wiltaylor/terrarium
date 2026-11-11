//! `/api/projects/{name}/board`, `/api/projects/{name}/cards`, `/api/cards/...`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use super::longpoll::{self, WaitQuery};
use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::ApiResult;
use crate::events::{self, EventFilter};
use crate::policy;
use crate::store::Page;
use crate::store::accepted_heads;
use crate::store::board::{self, Board, Card, CardFilter, CardPatch, ColumnSpec, NewCard, NewLink};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/projects/{name}/board",
            get(show_board).patch(patch_board),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (
                    axum::http::Method::PATCH,
                    super::link_registry::Access::Link,
                ),
            ],
        )
        .route(
            "/projects/{name}/cards",
            get(list_cards).post(create_card),
            &[
                (
                    axum::http::Method::GET,
                    super::link_registry::Access::Conditional,
                ),
                (
                    axum::http::Method::POST,
                    super::link_registry::Access::Conditional,
                ),
            ],
        )
        .route(
            "/projects/{name}/github-issues/import",
            post(import_issue),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/cards",
            get(active_cards),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/cards/{id}",
            get(show).patch(update).delete(archive),
            &[
                (
                    axum::http::Method::GET,
                    super::link_registry::Access::Conditional,
                ),
                (
                    axum::http::Method::PATCH,
                    super::link_registry::Access::Conditional,
                ),
                (
                    axum::http::Method::DELETE,
                    super::link_registry::Access::Conditional,
                ),
            ],
        )
        .route(
            "/cards/{id}/move",
            post(move_card),
            &[(
                axum::http::Method::POST,
                super::link_registry::Access::Conditional,
            )],
        )
        .route(
            "/cards/{id}/assign",
            post(assign),
            &[(
                axum::http::Method::POST,
                super::link_registry::Access::Conditional,
            )],
        )
        .route(
            "/cards/{id}/claim",
            post(claim),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/cards/{id}/comments",
            get(comments).post(comment),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/cards/{id}/links",
            post(link),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/cards/{id}/events",
            get(card_events),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
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
    let (who, project) = (actor.clone(), name.clone());
    state
        .db
        .read(move |c| crate::store::threads::read_linked_card(c, &who, &project, None))
        .await?;
    let rows = longpoll::hold(&state, wait, &["card"], || {
        let (actor, name, page, f) = (actor.clone(), name.clone(), page.clone(), f.clone());
        state.db.read(move |c| {
            crate::store::threads::read_linked_card(c, &actor, &name, None)?;
            crate::store::projects::get(c, &name)?;
            let assignee = match f.assignee.as_deref() {
                None => None,
                Some("me") => Some(actor.address()),
                Some(a) => Some(crate::store::mail::resolve_known(c, a, Some(&name))?.key()),
            };
            let restricted = crate::store::threads::read_card_project(c, &actor, &name, None).is_err();
            let agent = match &actor {
                Actor::Agent(a) if restricted => Some(a.id.as_str()),
                _ => None,
            };
            board::list_cards_for_request(c, &name, &f, assignee.as_deref(), &page, agent)
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
) -> ApiResult<Json<serde_json::Value>> {
    let who = actor.clone();
    let card = state
        .db
        .read(move |c| {
            let card = board::get_card(c, &id)?;
            crate::store::threads::read_linked_card(c, &who, &card.project, Some(&card.id))?;
            Ok(card)
        })
        .await?;
    let id = card.id.clone();
    let mut questions = state
        .db
        .read(move |c| {
            crate::store::questions::list(
                c,
                &crate::store::questions::Filter {
                    card: Some(id),
                    ..Default::default()
                },
            )
        })
        .await?;
    questions.retain(|q| policy::read_questions(&actor, &q.agent, &q.project).is_ok());
    let mut result = serde_json::json!(card);
    result["questions"] = serde_json::json!(questions);
    Ok(Json(result))
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
    let to = m.column.clone();
    let head = head_before(&state, &actor, &id, move |_, cols| goes_to_queue(cols, &to)).await;
    let card = state
        .db
        .write(actor.label(), move |t| {
            let card =
                board::move_card(t, &actor, &id, &m.column, m.position, m.comment.as_deref())?;
            record_before(t, &card, head.as_ref())?;
            board::get_card(t, &card.id)
        })
        .await?;
    accepted_soon(&state, &card);
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
    // An accept verdict of a review agent moves the card into the merge
    // queue in the write (`review_dispatcher::verdict`).
    let first = c.body.trim().lines().next().unwrap_or_default().trim();
    let accept = first.strip_prefix("Review verdict: ").unwrap_or(first) == "accept";
    let head = if accept {
        head_before(&state, &actor, &id, |card, cols| {
            cols.iter()
                .any(|(name, role)| *name == card.column && role.as_deref() == Some(board::REVIEW))
                || card.column == board::REVIEW
        })
        .await
    } else {
        None
    };
    let card = state
        .db
        .write(actor.label(), move |t| {
            let card = board::reply_comment(t, &actor, &id, &c.body, c.reply_to.as_deref())?;
            record_before(t, &card, head.as_ref())?;
            board::get_card(t, &card.id)
        })
        .await?;
    // A review verdict in the comment can move the card into the merge
    // queue (`review_dispatcher::verdict`).
    accepted_soon(&state, &card);
    Ok((StatusCode::CREATED, Json(card)))
}

/// The branch and its head on the remote of card `id`, read before a write
/// that can move the card into the merge queue (TER-553): `goes` says if
/// it can, from the card and the board columns. The write then records the
/// head as the reviewed commit, so the batch rule of the move sees it.
/// `None` when the card cannot go there, or the head cannot be read: the
/// background read records it after the move.
async fn head_before<F>(
    state: &Shared,
    actor: &Actor,
    id: &str,
    goes: F,
) -> Option<(String, String)>
where
    F: FnOnce(&Card, &[(String, Option<String>)]) -> bool + Send + 'static,
{
    let (a, id) = (actor.clone(), id.to_owned());
    let place = state
        .db
        .read(move |c| {
            let card = board::get_card(c, &board::normalise_id(&id))?;
            policy::write_project(&a, &card.project)?;
            let cols = board::column_roles(c, &card.project)?;
            if accepted_heads::is_queue(&card.column, card.column_role.as_deref())
                || !goes(&card, &cols)
            {
                return Ok(None);
            }
            let repo = crate::api::jobs::repo_of(c, &card, None)?;
            let branch = crate::api::jobs::branch_of(&card, None)?;
            Ok(Some((card.project, repo, branch)))
        })
        .await
        .ok()
        .flatten()?;
    let (project, repo, branch) = place;
    match crate::jobs::remote_head(state, &project, &repo, &branch).await {
        Ok(Some(h)) if crate::jobs::check_commit(&h).is_ok() => Some((branch, h)),
        _ => None,
    }
}

/// Is `to`, as a move gives it, the merge queue of the board `cols`? The
/// column named `to`, else the column with the role `to`, as
/// `board::move_card` resolves it (TER-580).
fn goes_to_queue(cols: &[(String, Option<String>)], to: &str) -> bool {
    let by_name = cols.iter().find(|(name, _)| name == to);
    let col = by_name.or_else(|| {
        board::ROLES
            .contains(&to)
            .then(|| cols.iter().find(|(_, role)| role.as_deref() == Some(to)))
            .flatten()
    });
    col.is_some_and(|(name, role)| accepted_heads::is_queue(name, role.as_deref()))
}

/// Record the head read by [`head_before`], when the write moved the card
/// into the merge queue and the card branch is still the one read.
fn record_before(
    t: &mut crate::db::Tx<'_>,
    card: &Card,
    head: Option<&(String, String)>,
) -> ApiResult<()> {
    let Some((branch, commit)) = head else {
        return Ok(());
    };
    if !accepted_heads::is_queue(&card.column, card.column_role.as_deref()) {
        return Ok(());
    }
    if crate::api::jobs::branch_of(card, None).ok().as_ref() == Some(branch) {
        accepted_heads::record_open(t, &card.id, branch, commit)?;
    }
    Ok(())
}

/// A card that is in the merge queue now: record its reviewed commit, the
/// head of its branch, right after the move (TER-553).
fn accepted_soon(state: &Shared, card: &Card) {
    if crate::store::accepted_heads::is_queue(&card.column, card.column_role.as_deref()) {
        crate::jobs::record_accepted_soon(state);
    }
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
