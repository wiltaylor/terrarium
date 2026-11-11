//! `GET /api/agents/{id}/screen`: a plain-text snapshot of the agent's
//! harness screen (`docs/api.md` §6.5).
//!
//! Rights: the same as the PTY attach. The project_admin of the agent's
//! project, a sysadmin, or the operator reads it. The snapshot is
//! read-only: it takes no PTY attachment and types nothing.

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_runner::ScreenSnapshot;

use super::agents::{load, supervisor_error};
use super::{ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::ApiResult;
use crate::policy;

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new().route(
        "/agents/{id}/screen",
        get(screen),
        &[(axum::http::Method::GET, super::link_registry::Access::Link)],
    )
}

#[derive(Debug, Default, Deserialize)]
struct ScreenQuery {
    /// `true`: `text` is the escape sequences that draw the screen.
    #[serde(default)]
    ansi: Option<bool>,
}

/// The screen rows as text: no escape codes, no trailing blanks, and no
/// blank rows at the end.
pub fn plain_text(snap: &ScreenSnapshot) -> String {
    let end = snap
        .lines
        .iter()
        .rposition(|l| !l.trim_end().is_empty())
        .map_or(0, |i| i + 1);
    snap.lines[..end]
        .iter()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn screen(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<ScreenQuery>,
) -> ApiResult<Json<Value>> {
    let a = load(&state, &actor, id).await?;
    // A read: a sysadmin or an architect reads each screen (TER-47).
    if !actor.agent().is_some_and(|x| x.reads_all()) {
        policy::admin_project(&actor, &a.agent.project, "reading an agent screen")?;
    }
    let snap = state
        .supervisor
        .screen(a.agent.id)
        .await
        .map_err(supervisor_error)?;
    let status = state
        .supervisor
        .status(a.agent.id)
        .await
        .unwrap_or(a.agent.status);
    let text = if q.ansi.unwrap_or(false) {
        String::from_utf8_lossy(&snap.formatted).into_owned()
    } else {
        plain_text(&snap)
    };
    // The snapshot is redacted (PLAN §16b.6); the live PTY stream is not.
    let view = crate::secrets::redact::view_of(&actor);
    let text = state.db.redactor().get().redact_string_as(text, &view).0;
    Ok(Json(json!({
        "agent": format!("{}/{}", a.agent.project, a.agent.name),
        "agent_id": a.agent.id,
        "status": status,
        "rows": snap.rows,
        "cols": snap.cols,
        "text": text,
        "cursor": {"row": snap.cursor.0, "col": snap.cursor.1},
        "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_drops_the_blank_rows_at_the_end() {
        let snap = ScreenSnapshot::parse(5, 20, b"\x1b[1mone\x1b[0m  \r\n\r\nthree\r\n");
        assert_eq!(plain_text(&snap), "one\n\nthree");
        assert_eq!(plain_text(&ScreenSnapshot::parse(3, 10, b"")), "");
    }
}
