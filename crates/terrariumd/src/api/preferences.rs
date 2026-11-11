//! Server-side operator preferences, shared by all browsers.
use axum::{Json, extract::State, routing::get};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::Shared;
use crate::{actor::Actor, error::ApiResult, policy, store::agents};

#[derive(Default, Deserialize, Serialize)]
pub struct Preferences {
    pub default_agent: Option<String>,
}

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new().route("/preferences", get(read).put(update), &[(axum::http::Method::GET, super::link_registry::Access::OperatorRead), (axum::http::Method::PUT, super::link_registry::Access::OperatorOnly)])
}

fn load(c: &rusqlite::Connection, owner: &str) -> ApiResult<Preferences> {
    let default_agent = c
        .query_row(
            "SELECT a.project || '/' || a.name FROM operator_preferences p
         JOIN agents a ON a.id = p.default_agent WHERE p.owner = ?1",
            [owner],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Preferences { default_agent })
}

async fn read(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Preferences>> {
    policy::operator_only(&actor, "operator preferences")?;
    let owner = actor.address();
    Ok(Json(state.db.read(move |c| load(c, &owner)).await?))
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Json(body): Json<Preferences>,
) -> ApiResult<Json<Preferences>> {
    policy::operator_only(&actor, "operator preferences")?;
    let owner = actor.address();
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| {
                let agent = body
                    .default_agent
                    .as_deref()
                    .map(|name| agents::find(t, name, None).map(|a| a.agent.id.to_string()))
                    .transpose()?;
                t.execute(
                    "INSERT INTO operator_preferences(owner, default_agent) VALUES (?1, ?2)
                   ON CONFLICT(owner) DO UPDATE SET default_agent = excluded.default_agent",
                    rusqlite::params![owner, agent],
                )?;
                t.audit(
                    "operator.preferences",
                    &owner,
                    None,
                    serde_json::json!({"default_agent": agent}),
                )?;
                load(t, &owner)
            })
            .await?,
    ))
}
