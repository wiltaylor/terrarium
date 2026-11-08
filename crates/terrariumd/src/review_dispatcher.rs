//! Opt-in review agents. Verdicts are accepted only from the current round's agent.
use rusqlite::{Connection, params};
use terrarium_core::Role;

use crate::actor::Actor;
use crate::api::Shared;
use crate::card_dispatcher::{actor, defer_project, project_limit};
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::spawn::{Internal, SpawnBody};
use crate::store::{agents, board, projects, spawns};

fn owner(c: &Connection, project: &str) -> ApiResult<Option<Actor>> {
    let Some(name) = projects::get(c, project)?.review_dispatcher else {
        return Ok(None);
    };
    let v = agents::find(c, &name, Some(project))?;
    if v.ephemeral || v.agent.project != project || !v.agent.roles.contains(&Role::ProjectAdmin) {
        return Err(ApiError::forbidden(
            "review dispatcher must be a project admin",
        ));
    }
    Ok((!v.paused).then(|| actor(v.agent)))
}

fn candidate(c: &Connection, project: &str) -> ApiResult<Option<String>> {
    let review = board::role_name(c, project, "review", "review")?;
    for card in board::get_board(c, project)?.cards {
        if card.column != review
            || card.kind != "card"
            || card.archived
            || board::is_held(c, &card.id)?
        {
            continue;
        }
        if spawns::open_bound(c, &spawns::Bound::new("review", &card.id))?.is_some() {
            continue;
        }
        let cooling: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM card_dispatch_retries WHERE card = ?1 AND retry_at > ?2)",
            params![card.id, chrono::Utc::now().timestamp()],
            |r| r.get(0),
        )?;
        let reviewed: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM card_reviews WHERE card = ?1 AND applied = 0)",
            [&card.id],
            |r| r.get(0),
        )?;
        if !cooling && !reviewed {
            return Ok(Some(card.id));
        }
    }
    Ok(None)
}

pub async fn dispatch(state: &Shared, project: &str) -> ApiResult<bool> {
    let p = project.to_owned();
    let who = match state.db.read(move |c| owner(c, &p)).await {
        Ok(Some(who)) => who,
        Ok(None) => return Ok(false),
        Err(e) => {
            defer_project(state, project, None, &e.message).await?;
            return Ok(false);
        }
    };
    let a = who.clone();
    if let Some(reason) = state.db.read(move |c| project_limit(c, &a)).await? {
        defer_project(state, project, Some(&who), &reason).await?;
        return Ok(false);
    }
    let p = project.to_owned();
    let Some(id) = state.db.read(move |c| candidate(c, &p)).await? else {
        return Ok(false);
    };
    let (p, expected, expected_owner) = (project.to_owned(), id.clone(), who.clone());
    let check: crate::spawn::DefCheck = Box::new(move |c, _| {
        if owner(c, &p)?.as_ref() != Some(&expected_owner) || candidate(c, &p)? != Some(expected) {
            return Err(ApiError::conflict("review selection changed")
                .with_details(serde_json::json!({"dispatcher_selection_changed":true})));
        }
        Ok(())
    });
    let card = id.clone();
    let attach: crate::spawn::AfterCreate = Box::new(move |t, agent| {
        t.execute(
            "INSERT INTO card_reviews(card, round, agent, applied) VALUES (?1, 1, ?2, 0)
            ON CONFLICT(card) DO UPDATE SET round = round + 1, agent = excluded.agent, applied = 0",
            params![card, agent],
        )?;
        t.execute("DELETE FROM card_dispatch_retries WHERE card = ?1", [&card])?;
        Ok(())
    });
    let skills = state
        .config_repo
        .get()
        .ok()
        .map(|r| crate::skills::library_dir(&r.dir));
    let result = crate::spawn::spawn_with(state, &who, who.address(), SpawnBody {
        template: format!("{project}/card-review"),
        task: format!("Review {id}. Read the card, repo AGENTS.md, branch diff and test evidence. Post a card comment whose first line is exactly 'accept' or 'changes', followed by findings. Do not move, merge or deploy. Then end your turn."),
        name: None, wait: false, timeout: None, project: None,
        targets: vec![], grants: vec![], bind: None,
    }, skills, None, Internal {
        bound: Some(spawns::Bound::new("review", &id)),
        check: Some(check), after_create: Some(attach), ..Internal::default()
    }).await;
    if let Err(e) = result {
        return crate::card_dispatcher::handle_spawn_error(state, project, id, who, e).await;
    }
    Ok(true)
}

/// Run within the comment transaction, so a verdict cannot race a card move.
pub fn verdict(t: &mut Tx<'_>, reviewer: &Actor, id: &str, body: &str) -> ApiResult<()> {
    let Some(a) = reviewer.agent() else {
        return Ok(());
    };
    let current: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM card_reviews WHERE card = ?1 AND agent = ?2 AND applied = 0)",
        params![id, a.id],
        |r| r.get(0),
    )?;
    if !current {
        return Ok(());
    }
    let card = board::get_card(t, id)?;
    let review = board::role_name(t, &card.project, "review", "review")?;
    if card.archived || card.column != review {
        return Ok(());
    }
    let Some(who) = owner(t, &card.project)? else {
        return Ok(());
    };
    let destination = match body.lines().next().map(str::trim) {
        Some("accept") => "merge_queue",
        Some("changes") => "ready",
        _ => return Ok(()),
    };
    // Mark this round consumed even when the decision is reserved for the lead.
    t.execute("UPDATE card_reviews SET applied = 1 WHERE card = ?1", [id])?;
    if card.labels.iter().any(|l| l == "review:lead") {
        return Ok(());
    }
    let column = board::role_name(t, &card.project, destination, destination)?;
    board::move_card(
        t,
        &who,
        id,
        &column,
        (destination == "ready").then_some(0),
        Some(body),
    )?;
    Ok(())
}

pub fn finished(c: &Connection, id: &str) -> ApiResult<Option<(&'static str, String)>> {
    let active: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM card_reviews r JOIN cards c ON c.id = r.card
         JOIN board_columns b ON b.id = c.column_id
         WHERE r.card = ?1 AND r.applied = 0 AND c.archived = 0 AND coalesce(b.role,b.name) = 'review')",
        [id], |r| r.get(0))?;
    Ok((!active).then(|| (spawns::DONE, "review round finished".into())))
}

pub async fn pass(state: &Shared) {
    let projects = state
        .db
        .read(|c| {
            let mut q = c.prepare(
                "SELECT name FROM projects WHERE review_dispatcher IS NOT NULL ORDER BY name",
            )?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await;
    let Ok(projects) = projects else { return };
    for project in projects {
        for _ in 0..16 {
            match dispatch(state, &project).await {
                Ok(false) => break,
                Ok(true) => {}
                Err(e) => tracing::debug!(%project, error = %e.message, "review dispatch deferred"),
            }
        }
    }
}
