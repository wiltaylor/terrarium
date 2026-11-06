//! Bound ephemeral agents (TER-74, wiki card-lifecycle §2 and §7 G3,
//! inbox-threads §3): an ephemeral agent whose life is tied to an object
//! instead of a lifetime.
//!
//! - **Bind.** A spawn with `bind: "<kind>:<id>"` ([`check`]) is bound to
//!   that object. Kinds: `card` (a card of the owner's project, not done
//!   and not archived). At most one open spawn per object. A bound agent
//!   has no lifetime timeout; [`BOUND_MAX_LIFETIME`] is only a cap, so a
//!   forgotten object cannot keep an agent for ever. It keeps its harness
//!   session over a prompt change (`new_session_on_prompt_change` off), so
//!   a harness that keeps the prompt of a session (Codex) resumes too.
//! - **Park** ([`park`]): while its object waits on others, the agent is
//!   parked: the wake refuses it (`crate::lifecycle::Woke::Parked`), its
//!   mail and turns wait, and the idle pass stops it as soon as it is at
//!   rest (never in a turn), with the status reason `parked`. Open waits,
//!   leases, and grants stay. A parked agent holds no place in the
//!   owner's `spawn.max_concurrent`.
//! - **Unpark** ([`unpark`]): the object came back. The park ends, the
//!   daemon queues the turn that says why, and wakes the agent. The wake
//!   is a start with resume of its last harness session (the same session
//!   row and workspace, `resumes` + 1), on every harness.
//! - **End** ([`end`]): the object is finished. The agent closes as with
//!   `agent done` (`crate::spawn::close`): the owner gets the result, the
//!   sessions go to the archive, the workspace to the trash.
//!
//! The card driver ([`spawn`], [`on_move`]) maps card events to these:
//!
//! | event | action |
//! |---|---|
//! | moved to `done` | end, `done` |
//! | a merge job passed (merged onto `next`) | end, `done` |
//! | archived | end, `cancelled` |
//! | moved to `validating`, `review`, `accepted` | park |
//! | moved to the `working` or `blocked` column while parked | unpark |
//! | moved from a wait column to another column (`ready` after a failure) | unpark |
//! | moved from another column to another column (`ready`, `backlog`) | park |
//!
//! The unpark turn is `[System:card <id>]` with the move, its comment,
//! and the top of the card body, where the job queue and the reviewer put
//! the failure block. Other owners (threads, TER-92) call [`park`],
//! [`unpark`], and [`end`] with their own kind.

use std::time::Duration;

use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::turn_tag::{self, SystemKind, TurnSource};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventDraft, StoredEvent};
use crate::store::board;
use crate::store::spawns::{self, Bound};

/// The kind of a binding to a card.
pub const KIND_CARD: &str = "card";
/// The cap of the life of a bound agent: 30 days, as the longest ETA.
pub const BOUND_MAX_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);
/// The columns where a card waits on others: its agent is parked there.
pub const WAIT_COLUMNS: [&str; 3] = ["validating", "review", "accepted"];
/// The most characters of the card body in an unpark turn.
const MAX_BODY: usize = 3000;

/// Check `text` (`<kind>:<id>`) as a binding for a spawn in `project`.
pub fn check(conn: &rusqlite::Connection, project: &str, text: &str) -> ApiResult<Bound> {
    let Some((kind, id)) = text.trim().split_once(':') else {
        return Err(ApiError::invalid(format!(
            "bind `{text}`: give `card:<id>`"
        )));
    };
    let b = match kind {
        KIND_CARD => {
            let card = board::get_card(conn, &board::normalise_id(id.trim()))?;
            if card.project != project {
                return Err(ApiError::invalid(format!(
                    "card {} is in project `{}`; a bound agent goes in `{project}`",
                    card.id, card.project
                )));
            }
            if card.archived || card.column == board::DONE {
                return Err(ApiError::conflict(format!(
                    "card {} is finished; a bound agent needs an open card",
                    card.id
                )));
            }
            Bound::new(KIND_CARD, &card.id)
        }
        _ => {
            return Err(ApiError::invalid(format!(
                "bind `{text}`: the kind `{kind}` is unknown; give `card:<id>`"
            )));
        }
    };
    if let Some(open) = spawns::open_bound(conn, &b)? {
        return Err(ApiError::conflict(format!(
            "{} has a bound agent already: `{}`",
            b.text(),
            open.name
        )));
    }
    Ok(b)
}

/// Park the ephemeral agent `id` with `reason`. False when it is parked
/// already or finished. The idle pass stops it when it is at rest.
pub async fn park(state: &Shared, id: &str, reason: &str) -> ApiResult<bool> {
    let (key, why) = (id.to_owned(), reason.to_owned());
    let parked = state
        .db
        .write("system", move |t| {
            if !spawns::set_parked(t, &key, &why)? {
                return Ok(false);
            }
            let sp = spawns::get(t, &key)?;
            t.emit(
                EventDraft::new(
                    "agent.parked",
                    format!("{}: parked ({why})", sp.name),
                    json!({"spawn_id": sp.id, "name": sp.name, "bound": sp.bound,
                           "reason": why}),
                )
                .agent(sp.id.clone(), sp.name.clone())
                .project(sp.project.clone()),
            )?;
            Ok(true)
        })
        .await?;
    if parked {
        info!(agent = %id, %reason, "parked a bound agent");
    }
    Ok(parked)
}

/// Unpark the ephemeral agent `id`: end the park, queue `turn` (if any),
/// and wake it with resume. False when it was not parked and no turn
/// came.
pub async fn unpark(
    state: &Shared,
    id: &str,
    turn: Option<String>,
    cause: &'static str,
) -> ApiResult<bool> {
    let key = id.to_owned();
    let changed = state
        .db
        .write("system", move |t| {
            let sp = spawns::get(t, &key)?;
            if !sp.running() {
                return Ok(false);
            }
            let was = spawns::clear_parked(t, &key)?;
            if let Some(turn) = &turn {
                crate::store::transcripts::record_turn(t, &key, turn)?;
                crate::store::agents::queue_turn(t, &key, turn)?;
            }
            if was {
                t.emit(
                    EventDraft::new(
                        "agent.unparked",
                        format!("{}: unparked ({cause})", sp.name),
                        json!({"spawn_id": sp.id, "name": sp.name, "bound": sp.bound,
                               "cause": cause}),
                    )
                    .agent(sp.id.clone(), sp.name.clone())
                    .project(sp.project.clone()),
                )?;
            }
            Ok(was || turn.is_some())
        })
        .await?;
    if changed && let Ok(a) = id.parse::<AgentId>() {
        info!(agent = %id, %cause, "unparked a bound agent");
        crate::lifecycle::wake_soon(state, a, cause);
    }
    Ok(changed)
}

/// The object `b` is finished: close its open agent with `outcome` and
/// `result`. False when it has none.
pub async fn end(
    state: &Shared,
    b: &Bound,
    outcome: &'static str,
    result: String,
) -> ApiResult<bool> {
    let key = b.clone();
    let Some(sp) = state.db.read(move |c| spawns::open_bound(c, &key)).await? else {
        return Ok(false);
    };
    info!(agent = %sp.id, bound = %b.text(), %outcome, "the object of a bound agent ended");
    let last = match sp.id.parse::<AgentId>() {
        Ok(a) => crate::spawn::last_message(state, a).await,
        Err(_) => None,
    };
    let result = match last.filter(|m| !m.trim().is_empty()) {
        Some(m) => format!("{result}.\n\nThe last message of the agent:\n{m}"),
        None => format!("{result}."),
    };
    crate::spawn::close(state, &sp.id, outcome, Some(result), true).await?;
    Ok(true)
}

/// What a card event does to the bound agent of the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Park(String),
    Unpark,
    End(&'static str, String),
}

/// The action of a move of a bound card from `from` to `to` (`to_role`:
/// the role of the new column), with the agent `parked` or not. The table
/// of the module docs.
pub fn on_move(from: &str, to: &str, to_role: Option<&str>, parked: bool) -> Option<Action> {
    if from == to {
        return None;
    }
    if to == board::DONE {
        return Some(Action::End(
            spawns::DONE,
            format!("the card moved {from} -> {to}"),
        ));
    }
    if WAIT_COLUMNS.contains(&to) {
        return Some(Action::Park(format!("card in {to}")));
    }
    if matches!(to_role, Some(board::WORKING | board::BLOCKED)) {
        return parked.then_some(Action::Unpark);
    }
    if WAIT_COLUMNS.contains(&from) {
        return Some(Action::Unpark);
    }
    Some(Action::Park(format!("card in {to}")))
}

/// The action of the event `ev` for a bound card, and the card ID.
fn action_of(ev: &StoredEvent, parked: bool) -> Option<Action> {
    let s = |k: &str| ev.data.get(k).and_then(Value::as_str);
    match ev.kind.as_str() {
        "card" => match s("change")? {
            "moved" => on_move(s("from")?, s("to")?, s("to_role"), parked),
            "archived" => Some(Action::End(
                spawns::CANCELLED,
                "the card was archived".into(),
            )),
            _ => None,
        },
        "job" if s("kind") == Some("merge") && s("state") == Some("passed") => Some(Action::End(
            spawns::DONE,
            format!(
                "the card was merged onto next (job {})",
                s("id").unwrap_or("?")
            ),
        )),
        _ => None,
    }
}

/// The card ID of a card or job event.
fn card_of(ev: &StoredEvent) -> Option<String> {
    match ev.kind.as_str() {
        "card" => ev.card.clone(),
        "job" => ev
            .data
            .get("card")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// The unpark turn of a card that came back.
fn return_turn(card: &board::Card, ev: &StoredEvent) -> String {
    let s = |k: &str| ev.data.get(k).and_then(Value::as_str).unwrap_or("?");
    let mut text = format!(
        "Your card {} came back: {}. You run again in your own session and workspace.",
        card.id, ev.summary
    );
    if let Some(c) = ev
        .data
        .get("comment")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    {
        text.push_str(&format!("\n\nThe comment of the move:\n{c}"));
    }
    let body = card.body.trim();
    if !body.is_empty() {
        let cut: String = body.chars().take(MAX_BODY).collect();
        let more = if cut.len() < body.len() {
            format!("\n[cut; `terrarium card show {}` has the rest]", card.id)
        } else {
            String::new()
        };
        text.push_str(&format!(
            "\n\nThe top of the card body (a failure block is here):\n{cut}{more}"
        ));
    }
    let next = if s("to_role") == board::WORKING {
        "Fix the failure, push, and send the card on as before."
    } else {
        "Claim the card again (`terrarium card claim <id>`) when the working column has room, \
         fix the failure, push, and send the card on as before."
    };
    text.push_str(&format!("\n\n{next}"));
    turn_tag::render(
        &TurnSource::System {
            kind: SystemKind::Card,
            detail: Some(card.id.clone()),
        },
        &text,
    )
}

/// Act on one event.
async fn handle(state: &Shared, ev: StoredEvent) {
    let Some(card_id) = card_of(&ev) else { return };
    let b = Bound::new(KIND_CARD, &card_id);
    let key = b.clone();
    let open = match state.db.read(move |c| spawns::open_bound(c, &key)).await {
        Ok(Some(sp)) => sp,
        Ok(None) => return,
        Err(e) => {
            warn!(card = %card_id, "cannot read the bound agent of a card: {e}");
            return;
        }
    };
    let Some(action) = action_of(&ev, open.parked_at.is_some()) else {
        return;
    };
    debug!(card = %card_id, agent = %open.id, ?action, "card event of a bound agent");
    let r = match action {
        Action::Park(reason) => park(state, &open.id, &reason).await.map(|_| ()),
        Action::Unpark => {
            let id = card_id.clone();
            let turn = match state.db.read(move |c| board::get_card(c, &id)).await {
                Ok(card) => Some(return_turn(&card, &ev)),
                Err(e) => {
                    warn!(card = %card_id, "cannot read the card for the unpark turn: {e}");
                    None
                }
            };
            unpark(state, &open.id, turn, "card returned")
                .await
                .map(|_| ())
        }
        Action::End(outcome, result) => {
            // A close stops the agent and can take a while: not in the
            // listener loop.
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = end(&state, &b, outcome, result).await {
                    warn!(bound = %b.text(), "cannot close the bound agent: {e}");
                }
            });
            Ok(())
        }
    };
    if let Err(e) = r {
        warn!(card = %card_id, "cannot apply a card event to its bound agent: {e}");
    }
}

/// The card driver: park, unpark, and end the agents bound to cards on
/// card and job events. One event at a time, in order.
pub fn spawn(state: &Shared) -> JoinHandle<()> {
    let mut rx = state.db.hub().subscribe();
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    warn!("the bound agent driver missed {n} events");
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            if card_of(&ev).is_some() {
                handle(&state, ev).await;
            }
        }
    })
}

/// For the spawn timer: the outcome and result of an open bound spawn
/// whose object is finished already (an event that the driver missed,
/// for example while the daemon was down). `None`: it goes on.
pub fn finished_object(
    conn: &rusqlite::Connection,
    sp: &spawns::Spawn,
) -> ApiResult<Option<(&'static str, String)>> {
    let Some(text) = &sp.bound else {
        return Ok(None);
    };
    let Some(card_id) = text.strip_prefix("card:") else {
        return Ok(None);
    };
    match board::get_card(conn, card_id) {
        Ok(c) if c.archived => Ok(Some((spawns::CANCELLED, "the card was archived".into()))),
        Ok(c) if c.column == board::DONE => Ok(Some((spawns::DONE, "the card is done".into()))),
        Ok(_) => Ok(None),
        Err(e) if e.code.status().is_client_error() => {
            Ok(Some((spawns::CANCELLED, "the card is gone".into())))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn park(to: &str) -> Option<Action> {
        Some(Action::Park(format!("card in {to}")))
    }

    #[test]
    fn moves_map_to_actions() {
        // Into a wait column: park, parked or not.
        for to in WAIT_COLUMNS {
            assert_eq!(on_move("in_progress", to, None, false), park(to));
        }
        assert_eq!(on_move("validating", "review", None, true), park("review"));
        // Done ends.
        assert!(matches!(
            on_move("accepted", "done", None, true),
            Some(Action::End("done", _))
        ));
        // A failure: back to ready from a wait column.
        assert_eq!(
            on_move("validating", "ready", None, true),
            Some(Action::Unpark)
        );
        assert_eq!(
            on_move("review", "backlog", None, true),
            Some(Action::Unpark)
        );
        // To the working or blocked column: unpark only a parked agent.
        assert_eq!(
            on_move("review", "in_progress", Some("working"), true),
            Some(Action::Unpark)
        );
        assert_eq!(
            on_move("ready", "in_progress", Some("working"), true),
            Some(Action::Unpark)
        );
        assert_eq!(
            on_move("ready", "in_progress", Some("working"), false),
            None
        );
        assert_eq!(
            on_move("in_progress", "blocked", Some("blocked"), false),
            None
        );
        // A lead takes the card back to ready or backlog: park.
        assert_eq!(on_move("in_progress", "ready", None, false), park("ready"));
        assert_eq!(on_move("ready", "backlog", None, true), park("backlog"));
        // No column change: nothing.
        assert_eq!(on_move("review", "review", None, true), None);
    }

    fn ev(kind: &str, card: Option<&str>, data: Value) -> StoredEvent {
        serde_json::from_value(json!({
            "seq": 1, "id": "01E", "at": "2026-11-06T00:00:00.000Z", "kind": kind,
            "summary": "TER-1 moved review -> ready by lead", "card": card, "data": data
        }))
        .unwrap()
    }

    #[test]
    fn events_map_to_actions() {
        let merged = ev(
            "job",
            None,
            json!({"kind": "merge", "state": "passed", "card": "TER-1", "id": "01J"}),
        );
        assert_eq!(card_of(&merged).as_deref(), Some("TER-1"));
        assert!(matches!(
            action_of(&merged, true),
            Some(Action::End("done", r)) if r.contains("merged onto next")
        ));
        let failed = ev(
            "job",
            None,
            json!({"kind": "merge", "state": "failed", "card": "TER-1"}),
        );
        assert_eq!(action_of(&failed, true), None);
        let validated = ev(
            "job",
            None,
            json!({"kind": "validate", "state": "passed", "card": "TER-1"}),
        );
        assert_eq!(action_of(&validated, true), None);
        let archived = ev("card", Some("TER-1"), json!({"change": "archived"}));
        assert!(matches!(
            action_of(&archived, false),
            Some(Action::End("cancelled", _))
        ));
        let moved = ev(
            "card",
            Some("TER-1"),
            json!({"change": "moved", "from": "review", "to": "ready", "to_role": null}),
        );
        assert_eq!(action_of(&moved, true), Some(Action::Unpark));
        let other = ev("mail", None, json!({}));
        assert_eq!(card_of(&other), None);
    }
}
