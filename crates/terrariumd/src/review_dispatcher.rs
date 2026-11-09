//! Opt-in review agents. Verdicts are accepted only from the current round's agent.
use rusqlite::{Connection, params};
use terrarium_core::Role;

use crate::actor::Actor;
use crate::api::Shared;
use crate::card_dispatcher::{actor, defer_project, project_limit};
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::spawn::{Internal, SpawnBody};
use crate::store::{agents, board, mail, projects, spawns};

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

/// Bounded review-column lookup. Keep the indexed query public for plan regression tests.
pub const CANDIDATES: &str = "
SELECT c.id, c.title, c.labels FROM cards c INDEXED BY cards_column
WHERE c.column_id = (SELECT id FROM board_columns WHERE project = ?1 AND name = ?2)
  AND c.archived = 0 AND c.kind = 'card'
  AND NOT EXISTS (SELECT 1 FROM spawns s WHERE s.bound_kind = 'review'
      AND s.bound_id = c.id AND s.finished_at IS NULL)
  AND NOT EXISTS (SELECT 1 FROM card_dispatch_retries d WHERE d.card = c.id AND d.retry_at > ?3)
  AND NOT EXISTS (SELECT 1 FROM card_reviews r WHERE r.card = c.id
      AND (r.applied = 2 OR r.cap_notified = 1))
ORDER BY c.position LIMIT 16";

fn candidate(c: &Connection, project: &str) -> ApiResult<Option<String>> {
    let review = board::role_name(c, project, "review", "review")?;
    let mut q = c.prepare(CANDIDATES)?;
    let rows = q.query_map(
        params![project, review, chrono::Utc::now().timestamp()],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    for row in rows {
        let (id, title, labels) = row?;
        if !crate::store::github_gate::is_held(
            &title,
            &crate::store::json_col::<Vec<String>>(&labels),
        ) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn tell_owner(t: &mut Tx<'_>, who: &Actor, id: &str, reason: &str) -> ApiResult<()> {
    board::system_comment(t, id, reason, None)?;
    mail::send(
        t,
        mail::Outgoing {
            from: &mail::Address::System,
            to: &mail::Address::from_actor(who),
            subject: &format!("{id}: review needs lead decision"),
            body: reason,
            thread: None,
            in_reply_to: None,
        },
    )?;
    Ok(())
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
    let p = project.to_owned();
    let Some(id) = state.db.read(move |c| candidate(c, &p)).await? else {
        return Ok(false);
    };
    let (card, a) = (id.clone(), who.clone());
    let capped = state
        .db
        .write("system", move |t| {
            let round: u32 = t.query_row(
                "SELECT coalesce((SELECT round FROM card_reviews WHERE card = ?1), 0)",
                [&card],
                |r| r.get(0),
            )?;
            if round < 3 {
                return Ok(false);
            }
            t.execute(
                "UPDATE card_reviews SET cap_notified = 1 WHERE card = ?1",
                [&card],
            )?;
            tell_owner(
                t,
                &a,
                &card,
                "Review round limit (3) reached. Card remains in review for the lead.",
            )?;
            Ok(true)
        })
        .await?;
    if capped {
        return Ok(true);
    }
    let a = who.clone();
    if let Some(reason) = state.db.read(move |c| project_limit(c, &a)).await? {
        defer_project(state, project, Some(&who), &reason).await?;
        return Ok(false);
    }
    let (p, expected, expected_owner) = (project.to_owned(), id.clone(), who.clone());
    let check: crate::spawn::DefCheck = Box::new(move |c, _| {
        let round: u32 = c.query_row(
            "SELECT coalesce((SELECT round FROM card_reviews WHERE card = ?1), 0)",
            [&expected],
            |r| r.get(0),
        )?;
        if round >= 3
            || owner(c, &p)?.as_ref() != Some(&expected_owner)
            || candidate(c, &p)? != Some(expected)
        {
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
    let task = format!(
        "Review {id}. Read the card, repo AGENTS.md, branch diff and test evidence. \
         Post a card comment whose first line is exactly 'Review verdict: accept' or \
         'Review verdict: changes', followed by findings. Do not move, merge or deploy. \
         Then end your turn."
    );
    let result = crate::spawn::spawn_with(
        state,
        &who,
        who.address(),
        SpawnBody {
            template: "card-review".into(),
            task,
            name: None,
            wait: false,
            timeout: None,
            project: None,
            targets: vec![],
            grants: vec![],
            bind: None,
        },
        skills,
        None,
        Internal {
            bound: Some(spawns::Bound::new("review", &id)),
            check: Some(check),
            after_create: Some(attach),
            ..Internal::default()
        },
    )
    .await;
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
        "SELECT EXISTS(SELECT 1 FROM card_reviews r JOIN spawns s ON s.id = r.agent
         WHERE r.card = ?1 AND r.agent = ?2 AND r.applied = 0 AND s.finished_at IS NULL)",
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
    let first = body.lines().next().unwrap_or_default().trim();
    let destination = match first.strip_prefix("Review verdict: ").unwrap_or(first) {
        "accept" => "merge_queue",
        "changes" => "ready",
        _ => return Ok(()),
    };
    // Mark this round consumed even when the decision is reserved for the lead.
    t.execute("UPDATE card_reviews SET applied = 2 WHERE card = ?1", [id])?;
    if card.labels.iter().any(|l| l == "review:lead") {
        return Ok(());
    }
    let moved = t.savepoint(|t| {
        let column = board::role_name(t, &card.project, destination, destination)?;
        board::move_card(
            t,
            &who,
            id,
            &column,
            (destination == "ready").then_some(0),
            Some(body),
        )
    });
    if let Err(e) = moved {
        tell_owner(
            t,
            &who,
            id,
            &format!(
                "Review verdict retained, but move to {destination} failed: {}. Card remains in review for the lead.",
                e.message
            ),
        )?;
    }
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
