//! Opt-in review agents. Verdicts are accepted only from the current round's agent.
//!
//! Two callers share the selection and the spawn ([`Pick`], [`run`]): the
//! `review_dispatcher` project setting and the `review.start` rule action
//! (TER-1214, kanban rework C4b, D23). A project on `board_flow = columns`
//! has only the rule action: its setting is refused and ignored, and a
//! verdict there acts as the owner of the round's spawn.
use rusqlite::{Connection, OptionalExtension, params};
use terrarium_core::Role;
use terrarium_core::config::BoardFlow;

use crate::actor::Actor;
use crate::api::Shared;
use crate::card_dispatcher::{actor, defer_project, named_owner, project_limit};
use crate::db::Tx;
use crate::error::{ApiError, ApiResult, Code};
use crate::spawn::{Internal, SpawnBody};
use crate::store::{agents, board, mail, projects, spawns};

/// The template and the round limit of the `review_dispatcher` setting.
pub const DEFAULT_TEMPLATE: &str = "card-review";
pub const DEFAULT_MAX_ROUNDS: u32 = 3;

fn owner(c: &Connection, project: &str) -> ApiResult<Option<Actor>> {
    let p = projects::get(c, project)?;
    // The column rules start reviews on a columns board (TER-1214 D24).
    if p.board_flow == BoardFlow::Columns {
        return Ok(None);
    }
    let Some(name) = p.review_dispatcher else {
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

/// Page size of [`CANDIDATES`]: cards held by a `GitHub #<n>:` title stay
/// in review, so pages are walked by the `(position, id)` keyset until one
/// is not full, and a later card cannot be starved (TER-408, TER-412).
pub const CANDIDATE_PAGE: i64 = 16;

/// Bounded review-column lookup, one page after the keyset `(?4, ?5)`. Keep
/// the indexed query public for plan regression tests. Cards with both the
/// `github` and the `hold` label (every gate-held card, see
/// [`crate::store::github_gate::hold_labels`]) are dropped here. The SQL
/// `trim` and `lower` match a subset of what
/// [`crate::store::github_gate::is_held`] matches, never more, so the SQL
/// cannot drop a card that Rust would dispatch. The title rule stays in
/// Rust in [`candidate`].
pub const CANDIDATES: &str = "
SELECT c.id, c.position, c.title, c.labels FROM cards c INDEXED BY cards_column
WHERE c.column_id = (SELECT id FROM board_columns WHERE project = ?1 AND name = ?2)
  AND c.archived = 0 AND c.kind = 'card'
  AND NOT EXISTS (SELECT 1 FROM spawns s WHERE s.bound_kind = 'review'
      AND s.bound_id = c.id AND s.finished_at IS NULL)
  AND NOT EXISTS (SELECT 1 FROM card_dispatch_retries d WHERE d.card = c.id AND d.retry_at > ?3)
  AND NOT EXISTS (SELECT 1 FROM card_reviews r WHERE r.card = c.id
      AND (r.applied = 2 OR r.cap_notified = 1))
  AND NOT (EXISTS (SELECT 1 FROM json_each(c.labels) github_label
               WHERE trim(lower(github_label.value)) = 'github')
       AND EXISTS (SELECT 1 FROM json_each(c.labels) hold_label
               WHERE trim(lower(hold_label.value)) = 'hold'))
  AND (c.position, c.id) > (?4, ?5)
ORDER BY c.position, c.id LIMIT ?6";

/// One card of [`CANDIDATES`]: the same tests, for the card `?4` only (a
/// `review.start` on its run's card).
pub const ONE_CANDIDATE: &str = "
SELECT c.id, c.title, c.labels FROM cards c
WHERE c.id = ?4
  AND c.column_id = (SELECT id FROM board_columns WHERE project = ?1 AND name = ?2)
  AND c.archived = 0 AND c.kind = 'card'
  AND NOT EXISTS (SELECT 1 FROM spawns s WHERE s.bound_kind = 'review'
      AND s.bound_id = c.id AND s.finished_at IS NULL)
  AND NOT EXISTS (SELECT 1 FROM card_dispatch_retries d WHERE d.card = c.id AND d.retry_at > ?3)
  AND NOT EXISTS (SELECT 1 FROM card_reviews r WHERE r.card = c.id
      AND (r.applied = 2 OR r.cap_notified = 1))";

/// What one review dispatch chooses from and starts: the cards of the
/// review column `from` (only `card` when it is set), the template of the
/// agent, and the round limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub from: String,
    pub card: Option<String>,
    pub template: String,
    pub max_rounds: u32,
}

/// The `review_dispatcher` setting's pick: the review column, the
/// `card-review` template and 3 rounds.
fn setting_pick(c: &Connection, project: &str) -> ApiResult<Pick> {
    Ok(Pick {
        from: board::role_name(c, project, "review", "review")?,
        card: None,
        template: DEFAULT_TEMPLATE.into(),
        max_rounds: DEFAULT_MAX_ROUNDS,
    })
}

/// The first card of `pick` with no open review, no consumed verdict and
/// no cooldown, not held.
pub fn candidate(c: &Connection, project: &str, pick: &Pick) -> ApiResult<Option<String>> {
    let now = chrono::Utc::now().timestamp();
    if let Some(id) = &pick.card {
        let row = c
            .query_row(ONE_CANDIDATE, params![project, pick.from, now, id], |r| {
                Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .optional()?;
        return Ok(row
            .filter(|(title, labels)| {
                !crate::store::github_gate::is_held(
                    title,
                    &crate::store::json_col::<Vec<String>>(labels),
                )
            })
            .map(|_| id.clone()));
    }
    let review = &pick.from;
    let mut q = c.prepare(CANDIDATES)?;
    // `(position, id)` is a strict total order, so a tie at a page boundary
    // is not dropped. A full page of title-held cards continues with the
    // next page.
    let mut after: (i64, String) = (-1, String::new());
    loop {
        let mut rows: Vec<(String, i64, String, String)> = Vec::new();
        {
            let it = q.query_map(
                params![project, review, now, after.0, after.1, CANDIDATE_PAGE],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )?;
            for row in it {
                rows.push(row?);
            }
        }
        let full = rows.len() == CANDIDATE_PAGE as usize;
        for (id, position, title, labels) in rows {
            after = (position, id.clone());
            if !crate::store::github_gate::is_held(
                &title,
                &crate::store::json_col::<Vec<String>>(&labels),
            ) {
                return Ok(Some(id));
            }
        }
        // A page that is not full is the end of the review column.
        if !full {
            return Ok(None);
        }
    }
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

/// The review round of card `id` (0: none yet).
pub fn round(c: &Connection, id: &str) -> ApiResult<u32> {
    Ok(c.query_row(
        "SELECT coalesce((SELECT round FROM card_reviews WHERE card = ?1), 0)",
        [id],
        |r| r.get(0),
    )?)
}

/// At the round limit: mark card `id` capped and mail `who` once. `false`:
/// the card is under the limit.
pub fn cap(t: &mut Tx<'_>, who: &Actor, id: &str, max_rounds: u32) -> ApiResult<bool> {
    if round(t, id)? < max_rounds {
        return Ok(false);
    }
    let notified = t.execute(
        "UPDATE card_reviews SET cap_notified = 1 WHERE card = ?1 AND cap_notified = 0",
        [id],
    )?;
    if notified > 0 {
        tell_owner(
            t,
            who,
            id,
            &format!(
                "Review round limit ({max_rounds}) reached. Card remains in review for the lead."
            ),
        )?;
    }
    Ok(true)
}

/// Where the owner of a review dispatch comes from, for the recheck in the
/// spawn transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The `review_dispatcher` setting.
    Setting,
    /// The `owner` of a `review.start` rule action: an agent name.
    Named(String),
}

impl Owner {
    fn current(&self, c: &Connection, project: &str) -> ApiResult<Option<Actor>> {
        match self {
            Self::Setting => owner(c, project),
            Self::Named(name) => named_owner(c, project, name, "review.start owner"),
        }
    }
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
    let pick = state.db.read(move |c| setting_pick(c, &p)).await?;
    run(state, project, who, pick, Owner::Setting).await
}

/// Select one card of `pick` and start its review agent as `who`. At the
/// round limit the owner gets one mail instead. `Ok(true)`: an agent
/// started or a card was capped. The owner, the round and the selection
/// are rechecked in the spawn transaction. Public for tests.
pub async fn run(
    state: &Shared,
    project: &str,
    who: Actor,
    pick: Pick,
    source: Owner,
) -> ApiResult<bool> {
    let (p, pk) = (project.to_owned(), pick.clone());
    let Some(id) = state.db.read(move |c| candidate(c, &p, &pk)).await? else {
        return Ok(false);
    };
    let (card, a, max_rounds) = (id.clone(), who.clone(), pick.max_rounds);
    let capped = state
        .db
        .write("system", move |t| cap(t, &a, &card, max_rounds))
        .await?;
    if capped {
        return Ok(true);
    }
    // The rule action checked the owner's limits in its run (a no-op with
    // the reason); the spawn checks them again.
    if source == Owner::Setting {
        let a = who.clone();
        if let Some(reason) = state.db.read(move |c| project_limit(c, &a)).await? {
            defer_project(state, project, Some(&who), &reason).await?;
            return Ok(false);
        }
    }
    let (p, expected, expected_owner, pk) =
        (project.to_owned(), id.clone(), who.clone(), pick.clone());
    let check: crate::spawn::DefCheck = Box::new(move |c, _| {
        if round(c, &expected)? >= pk.max_rounds
            || source.current(c, &p)?.as_ref() != Some(&expected_owner)
            || candidate(c, &p, &pk)? != Some(expected)
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
            like: None,
            like_skip_missing: false,
            template: pick.template.clone(),
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
        return crate::card_dispatcher::handle_spawn_error(state, project, id, who, e, None).await;
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
    let first = body.lines().next().unwrap_or_default().trim();
    let destination = match first.strip_prefix("Review verdict: ").unwrap_or(first) {
        "accept" => "merge_queue",
        "changes" => "ready",
        _ => return Ok(()),
    };
    // An unusable owner (lost project admin, deleted) must not reject the
    // comment and lose the findings: keep the verdict, record why nothing
    // was applied, and leave the round open like a project without a
    // review dispatcher. Any other error (a database error) still fails.
    let who = match verdict_owner(t, &card.project, &a.id) {
        Ok(Some(who)) => who,
        Ok(None) => return Ok(()),
        Err(e) if matches!(e.code, Code::Forbidden | Code::NotFound) => {
            board::system_comment(
                t,
                id,
                &format!(
                    "Review verdict retained, but the review dispatcher owner is no longer \
                     valid: {}. Card remains in review for the lead.",
                    e.message
                ),
                None,
            )?;
            return Ok(());
        }
        Err(e) => return Err(e),
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

/// Who applies the verdict of the round of agent `agent`: on a columns
/// board the owner of the round's spawn (the `review.start` owner), else
/// the `review_dispatcher` setting.
fn verdict_owner(c: &Connection, project: &str, agent: &str) -> ApiResult<Option<Actor>> {
    if projects::board_flow(c, project)? != BoardFlow::Columns {
        return owner(c, project);
    }
    let spawn_owner: Option<String> = c
        .query_row("SELECT owner FROM spawns WHERE id = ?1", [agent], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(spawn_owner) = spawn_owner else {
        return Err(ApiError::not_found("the review round has no spawn"));
    };
    named_owner(c, project, &spawn_owner, "review.start owner")
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
