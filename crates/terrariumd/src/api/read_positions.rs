//! Operator visit cursors. A visit returns the previous cursor and advances it
//! atomically, without an audit/event row for each page opening.
use axum::extract::{Path, State};
use axum::{Json, routing::post};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::Shared;
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::{policy, store};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new().route(
        "/read-positions/{kind}/{item}",
        post(visit),
        &[(
            axum::http::Method::POST,
            super::link_registry::Access::OperatorOnly,
        )],
    )
}

#[derive(Serialize)]
struct Visit {
    previous: Option<String>,
    read_at: String,
}

#[derive(Deserialize)]
struct Open {
    /// High-water timestamp from the rendered snapshot, not the browser clock.
    through: String,
}

async fn visit(
    State(state): State<Shared>,
    actor: Actor,
    Path((kind, item)): Path<(String, String)>,
    Json(body): Json<Open>,
) -> ApiResult<Json<Visit>> {
    policy::operator_only(&actor, "read positions")?;
    let through = chrono::DateTime::parse_from_rfc3339(&body.through)
        .map_err(|_| ApiError::bad_request("through must be an RFC3339 timestamp"))?;
    if through > chrono::Utc::now() {
        return Err(ApiError::bad_request("through must not be in the future"));
    }
    let read_at = through.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let owner = actor.address();
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| {
                // Resolve real items before writing: arbitrary IDs cannot grow the table.
                match kind.as_str() {
                    "thread" => {
                        store::threads::get_bare(t, &item)?;
                    }
                    "card" => {
                        store::board::project_of(t, &item)?;
                    }
                    "mail" if store::mail::thread_exists(t, &item)? => {}
                    "mail" => return Err(ApiError::not_found(format!("mail thread {item}"))),
                    _ => return Err(ApiError::bad_request("kind must be thread, mail or card")),
                }
                let previous = t
                    .query_row(
                        "SELECT read_at FROM read_positions WHERE owner=?1 AND kind=?2 AND item=?3",
                        params![owner, kind, item],
                        |r| r.get(0),
                    )
                    .optional()?;
                t.execute(
                    "INSERT INTO read_positions(owner,kind,item,read_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(owner,kind,item) DO UPDATE SET read_at=MAX(read_at,excluded.read_at)",
                    params![owner, kind, item, read_at],
                )?;
                let read_at = previous
                    .as_ref()
                    .map_or(read_at.clone(), |old: &String| old.max(&read_at).clone());
                Ok(Visit { previous, read_at })
            })
            .await?,
    ))
}
