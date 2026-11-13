//! Opt-in ready-card dispatch. All spawn rights and caps belong to the
//! configured project admin, never to the operator. Selection is rechecked
//! in the transaction that creates the bound agent and claims its card.
//!
//! Two callers share the selection and the spawn ([`Pick`], [`run`]): the
//! `card_dispatcher` project setting (the fixed `type:` map, [`template`])
//! and the `card.assign` rule action with a `template` or `templates`
//! (TER-1115, kanban rework C4, D6). A project on `board_flow = columns`
//! has only the rule action: its setting is refused and ignored.

use rusqlite::{Connection, OptionalExtension, params};
use terrarium_core::Role;

use crate::actor::{Actor, AgentActor};
use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::spawn::{Internal, SpawnBody};
use crate::store::{agents, board, mail, projects, spawns};

pub(crate) fn actor(agent: terrarium_core::Agent) -> Actor {
    Actor::Agent(AgentActor {
        id: agent.id.to_string(),
        name: agent.name,
        project: agent.project,
        roles: agent.roles,
        policy: agent.policy,
    })
}

fn owner(c: &Connection, project: &str) -> ApiResult<Option<Actor>> {
    let p = projects::get(c, project)?;
    // The column rules assign on a columns board (TER-1115 D24).
    if p.board_flow == terrarium_core::config::BoardFlow::Columns {
        return Ok(None);
    }
    let Some(name) = p.card_dispatcher else {
        return Ok(None);
    };
    named_owner(c, project, &name, "card dispatcher")
}

/// The agent `name` as the owner of a dispatch in `project`: a project
/// admin of that project, not ephemeral. A paused owner dispatches nothing
/// (`None`). `what` names the owner in the refusal.
pub(crate) fn named_owner(
    c: &Connection,
    project: &str,
    name: &str,
    what: &str,
) -> ApiResult<Option<Actor>> {
    let v = agents::find(c, name, Some(project))?;
    if v.agent.project != project || !v.agent.roles.contains(&Role::ProjectAdmin) || v.ephemeral {
        return Err(ApiError::forbidden(format!(
            "{what} must be a project admin"
        )));
    }
    if v.paused {
        return Ok(None);
    }
    Ok(Some(actor(v.agent)))
}

/// How a dispatch picks the template of a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Templates {
    /// The fixed map of the `card_dispatcher` setting ([`template`]).
    TypeMap,
    /// One template for every eligible card (`card.assign.template`).
    One(String),
    /// `<label>=<template>` entries (`card.assign.templates`): the card
    /// must have exactly one of the labels.
    ByLabel(Vec<(String, String)>),
}

impl Templates {
    /// The template for a card with `labels`, if the card has one.
    pub fn pick(&self, labels: &[String]) -> Option<String> {
        match self {
            Self::TypeMap => template(labels),
            Self::One(t) => Some(t.clone()),
            Self::ByLabel(map) => {
                let mut hits = map.iter().filter(|(l, _)| labels.contains(l));
                match (hits.next(), hits.next()) {
                    (Some((_, t)), None) => Some(t.clone()),
                    _ => None,
                }
            }
        }
    }
}

/// What one dispatch chooses from: the cards of column `from` (only
/// `card` when it is set), the column `room` that must be under its WIP
/// limit (`None`: the working role), and the templates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub from: String,
    pub room: Option<String>,
    pub templates: Templates,
    pub card: Option<String>,
}

pub(crate) fn template(labels: &[String]) -> Option<String> {
    // Only known worker types, not arbitrary labels used as template names.
    let types: Vec<_> = labels
        .iter()
        .filter_map(|l| l.strip_prefix("type:"))
        .collect();
    match types.as_slice() {
        ["small"] => Some("card-small".into()),
        ["impl"] => Some("card-impl".into()),
        ["hard" | "impl-hard"] => Some("card-impl-hard".into()),
        _ => None,
    }
}

/// Bounded working-column WIP check: one indexed column lookup plus the
/// count of its active cards. Keep it public for plan regression tests.
pub const WORKING_WIP: &str = "
SELECT k.wip_limit,
       (SELECT count(*) FROM cards c WHERE c.column_id = k.id
         AND c.archived = 0 AND c.kind = 'card')
FROM board_columns k WHERE k.project = ?1 AND k.role = 'working'
ORDER BY k.position LIMIT 1";

/// Page size of [`CANDIDATES`]: held cards and cards with no usable type
/// label stay in ready, so pages are walked by the `(position, id)` keyset
/// until one is not full and a later dispatchable card cannot be starved
/// (TER-408).
pub const CANDIDATE_PAGE: i64 = 16;

/// Bounded ready-column lookup: cards with no assignee, no open bound
/// agent, no unmet dependency and no cooldown, in board order, one page
/// after the keyset `(?4, ?5)`. Keep the indexed query public for plan
/// regression tests.
pub const CANDIDATES: &str = "
SELECT c.id, c.position, c.title, c.labels FROM cards c INDEXED BY cards_dispatch_order
WHERE c.column_id = (SELECT id FROM board_columns WHERE project = ?1 AND name = ?2)
  AND c.archived = 0 AND c.kind = 'card' AND c.assignee IS NULL
  AND NOT EXISTS (SELECT 1 FROM spawns s WHERE s.bound_kind = 'card'
      AND s.bound_id = c.id AND s.finished_at IS NULL)
  AND NOT EXISTS (SELECT 1 FROM conflict_fixes f WHERE f.card = c.id AND f.done = 0)
  AND NOT EXISTS (SELECT 1 FROM card_dispatch_retries d WHERE d.card = c.id AND d.retry_at > ?3)
  AND NOT EXISTS (SELECT 1 FROM card_dependency_unmet e WHERE e.card = c.id)
  AND (c.position, c.id) > (?4, ?5)
ORDER BY c.position, c.id LIMIT ?6";

/// The WIP check of a named column, as [`WORKING_WIP`] for a rule's `room`.
pub const ROOM_WIP: &str = "
SELECT k.wip_limit,
       (SELECT count(*) FROM cards c WHERE c.column_id = k.id
         AND c.archived = 0 AND c.kind = 'card')
FROM board_columns k WHERE k.project = ?1 AND k.name = ?2";

/// One card of [`CANDIDATES`]: the same tests, for the card `?4` only (a
/// rule action on its run's card).
pub const ONE_CANDIDATE: &str = "
SELECT c.id, c.position, c.title, c.labels FROM cards c
WHERE c.id = ?4
  AND c.column_id = (SELECT id FROM board_columns WHERE project = ?1 AND name = ?2)
  AND c.archived = 0 AND c.kind = 'card' AND c.assignee IS NULL
  AND NOT EXISTS (SELECT 1 FROM spawns s WHERE s.bound_kind = 'card'
      AND s.bound_id = c.id AND s.finished_at IS NULL)
  AND NOT EXISTS (SELECT 1 FROM conflict_fixes f WHERE f.card = c.id AND f.done = 0)
  AND NOT EXISTS (SELECT 1 FROM card_dispatch_retries d WHERE d.card = c.id AND d.retry_at > ?3)
  AND NOT EXISTS (SELECT 1 FROM card_dependency_unmet e WHERE e.card = c.id)";

fn working_wip(r: &rusqlite::Row<'_>) -> rusqlite::Result<(Option<i64>, i64)> {
    Ok((r.get(0)?, r.get(1)?))
}

/// The first card of `pick` and its template, or why there is none now
/// (no room, no eligible card, no matching template).
pub fn select(
    c: &Connection,
    project: &str,
    pick: &Pick,
) -> ApiResult<Result<(String, String), &'static str>> {
    let room = match &pick.room {
        None => c.query_row(WORKING_WIP, [project], working_wip),
        Some(column) => c.query_row(ROOM_WIP, params![project, column], working_wip),
    }
    .optional()?;
    // Fail closed without an explicit limit: opt-in must not create an
    // unbounded number of agents on a board with no concurrency policy.
    if !matches!(room, Some((Some(limit), count)) if count < limit) {
        return Ok(Err("no room under the WIP limit"));
    }
    let now = chrono::Utc::now().timestamp();
    if let Some(id) = &pick.card {
        let row = c
            .query_row(
                ONE_CANDIDATE,
                params![project, pick.from, now, id],
                |r| Ok((r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
            )
            .optional()?;
        let Some((title, labels)) = row else {
            return Ok(Err("the card is not eligible"));
        };
        let labels = crate::store::json_col::<Vec<String>>(&labels);
        if crate::store::github_gate::is_held(&title, &labels) {
            return Ok(Err("the card is not eligible"));
        }
        return Ok(match pick.templates.pick(&labels) {
            Some(t) => Ok((id.clone(), t)),
            None => Err("no template matches the card"),
        });
    }
    let mut q = c.prepare(CANDIDATES)?;
    // Positions are neither dense nor unique (archived cards keep theirs,
    // group cards go in at 0 with no shift, create counts active cards
    // while a move counts all rows), so the keyset is the `(position, id)`
    // pair: a strict total order that cannot drop a tie at a page
    // boundary. Held cards and cards with no usable type label are skips
    // in Rust, so a full page of them continues with the next page
    // instead of starving an eligible card further down.
    let mut after: (i64, String) = (-1, String::new());
    loop {
        let mut rows: Vec<(String, i64, String, String)> = Vec::new();
        {
            let it = q.query_map(
                params![project, pick.from, now, after.0, after.1, CANDIDATE_PAGE],
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
            let labels = crate::store::json_col::<Vec<String>>(&labels);
            if crate::store::github_gate::is_held(&title, &labels) {
                continue;
            }
            if let Some(template) = pick.templates.pick(&labels) {
                return Ok(Ok((id, template)));
            }
        }
        // A page that is not full is the end of the column.
        if !full {
            return Ok(Err("no eligible card"));
        }
    }
}

/// The `card_dispatcher` setting's pick: the ready column, the working
/// role's room, the fixed map.
fn setting_pick(c: &Connection, project: &str) -> ApiResult<Pick> {
    Ok(Pick {
        from: board::role_name(c, project, "ready", "ready")?,
        room: None,
        templates: Templates::TypeMap,
        card: None,
    })
}

fn candidate(c: &Connection, project: &str, pick: &Pick) -> ApiResult<Option<(String, String)>> {
    Ok(select(c, project, pick)?.ok())
}

/// Try one card. The timer repeats after a successful claim to fill free WIP.
/// Public for integration tests with the fake supervisor.
pub async fn dispatch(state: &Shared, project: &str) -> ApiResult<bool> {
    let p = project.to_owned();
    let who = match state.db.read(move |c| owner(c, &p)).await {
        Ok(Some(who)) => who,
        Ok(None) => return Ok(false),
        Err(error) => {
            defer_project(state, project, None, &error.message).await?;
            return Ok(false);
        }
    };
    let owner_actor = who.clone();
    if let Some(reason) = state
        .db
        .read(move |c| project_limit(c, &owner_actor))
        .await?
    {
        defer_project(state, project, Some(&who), &reason).await?;
        return Ok(false);
    }
    let p = project.to_owned();
    let pick = state.db.read(move |c| setting_pick(c, &p)).await?;
    run(state, project, who, pick, Owner::Setting).await
}

/// Where the owner of a dispatch comes from, for the recheck in the spawn
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The `card_dispatcher` setting.
    Setting,
    /// The `owner` of a `card.assign` rule action: an agent name.
    Named(String),
}

impl Owner {
    fn current(&self, c: &Connection, project: &str) -> ApiResult<Option<Actor>> {
        match self {
            Self::Setting => owner(c, project),
            Self::Named(name) => named_owner(c, project, name, "card.assign owner"),
        }
    }
}

/// Select one card of `pick` and start its bound agent as `who`, which
/// claims the card. `Ok(true)`: an agent started. The owner and the
/// selection are rechecked in the spawn transaction. Public for tests.
pub async fn run(
    state: &Shared,
    project: &str,
    who: Actor,
    pick: Pick,
    source: Owner,
) -> ApiResult<bool> {
    let (p, pk) = (project.to_owned(), pick.clone());
    let selected = state.db.read(move |c| candidate(c, &p, &pk)).await?;
    let Some((id, template)) = selected else {
        return Ok(false);
    };
    // The setting keeps today's cooldown test (ready or review).
    let fail_from = (source != Owner::Setting).then(|| pick.from.clone());
    let (p, expected, expected_owner, expected_template, pk) = (
        project.to_owned(),
        id.clone(),
        who.clone(),
        template.clone(),
        pick.clone(),
    );
    let check: crate::spawn::DefCheck = Box::new(move |c, _| {
        if source.current(c, &p)?.as_ref() != Some(&expected_owner)
            || candidate(c, &p, &pk)? != Some((expected, expected_template))
        {
            return Err(ApiError::conflict("dispatcher selection changed")
                .with_details(serde_json::json!({"dispatcher_selection_changed": true})));
        }
        Ok(())
    });
    let card = id.clone();
    let claim: crate::spawn::AfterCreate = Box::new(move |t, agent| {
        let who = actor(agents::get(t, agent)?.agent);
        board::claim_card(t, &who, &card)?;
        t.execute("DELETE FROM card_dispatch_retries WHERE card = ?1", [&card])?;
        Ok(())
    });
    let skills = state
        .config_repo
        .get()
        .ok()
        .map(|r| crate::skills::library_dir(&r.dir));
    let result = crate::spawn::spawn_with(
        state,
        &who,
        who.address(),
        SpawnBody {
            like: None,
            like_skip_missing: false,
            template,
            task: format!(
                "Work on {id}. Read `terrarium card show {id}` and the repo AGENTS.md. \
            This card is already claimed for you. Set an ETA, work on its branch, \
            validate your pushed commit on a target and push the test evidence before review. \
            Follow delivery-process. Do not deploy or merge."
            ),
            name: Some(id.to_ascii_lowercase()),
            wait: false,
            timeout: None,
            project: None,
            // As with a manual template spawn: do not add owner targets.
            targets: Vec::new(),
            grants: Vec::new(),
            bind: Some(format!("card:{id}")),
        },
        skills,
        None,
        Internal {
            check: Some(check),
            after_create: Some(claim),
            ..Internal::default()
        },
    )
    .await;
    if let Err(error) = result {
        return handle_spawn_error(state, project, id, who, error, fail_from).await;
    }
    let p = project.to_owned();
    state
        .db
        .write("system", move |t| {
            t.execute(
                "DELETE FROM card_dispatch_deferrals WHERE project = ?1",
                [&p],
            )?;
            Ok(())
        })
        .await?;
    Ok(true)
}

/// Handle a refusal after selection, including a cap reached by a concurrent
/// manual spawn. Separate from preflight so both paths have regression tests.
///
/// `from`: the column that the card was picked from (`None`: the review
/// column of the review dispatcher, or the ready column).
pub async fn handle_spawn_error(
    state: &Shared,
    project: &str,
    id: String,
    who: Actor,
    error: ApiError,
    from: Option<String>,
) -> ApiResult<bool> {
    let details = error.details.as_ref();
    if details.is_some_and(|d| d["dispatcher_selection_changed"] == true) {
        return Ok(false);
    }
    if details.is_some_and(|d| d["spawn_cap"].is_string() || d["spawn_owner_right"].is_string()) {
        let reason = details
            .and_then(|d| d["spawn_cap"].as_str())
            .map(|cap| format!("Dispatcher deferred: spawn.{cap} reached"))
            .unwrap_or_else(|| error.message.clone());
        defer_project(state, project, Some(&who), &reason).await?;
        return Ok(false);
    }
    // Forbidden is not inherently owner-wide: template allowlists, targets,
    // and grants can reject only this card while later templates are usable.
    record_failure(state, id, who, error.message.clone(), from).await?;
    Err(error)
}

/// Check owner-wide limits before choosing a card. The spawn transaction
/// repeats the authoritative checks to cover concurrent manual spawns.
pub(crate) fn project_limit(c: &Connection, who: &Actor) -> ApiResult<Option<String>> {
    let Some(agent) = who.agent() else {
        return Ok(None);
    };
    let Some(policy) = &agent.policy.spawn else {
        return Ok(Some("Dispatcher owner has no spawn rights".into()));
    };
    if !policy.bound || policy.templates.is_empty() {
        return Ok(Some(
            "Dispatcher owner needs spawn.bound and allowed templates".into(),
        ));
    }
    if spawns::running_count(c, &agent.id)? >= policy.max_concurrent as usize {
        return Ok(Some(
            "Dispatcher deferred: spawn.max_concurrent reached".into(),
        ));
    }
    if let Some(limit) = policy.max_per_day {
        let since = (chrono::Utc::now() - chrono::Duration::hours(24))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if spawns::count_since(c, &agent.id, &since)? >= limit as usize {
            return Ok(Some(
                "Dispatcher deferred: spawn.max_per_day reached".into(),
            ));
        }
    }
    Ok(None)
}

/// Deduplicate project-wide notices, without penalizing any ready card.
/// Limits and configuration are rechecked once per timer pass, so a budget
/// reset or an owner repair resumes dispatch without a card cooldown.
pub(crate) async fn defer_project(
    state: &Shared,
    project: &str,
    who: Option<&Actor>,
    reason: &str,
) -> ApiResult<()> {
    let (project, reason) = (project.to_owned(), reason.to_owned());
    let to = who
        .map(mail::Address::from_actor)
        .unwrap_or_else(|| mail::Address::Project(project.clone()));
    state.db.write("system", move |t| {
        let previous: Option<String> = t.query_row(
            "SELECT reason FROM card_dispatch_deferrals WHERE project = ?1", [&project], |r| r.get(0),
        ).optional()?;
        if previous.as_deref() == Some(&reason) { return Ok(()) }
        t.execute("INSERT INTO card_dispatch_deferrals (project, reason) VALUES (?1, ?2)
            ON CONFLICT(project) DO UPDATE SET reason = excluded.reason", params![project, reason])?;
        mail::send(t, mail::Outgoing {
            from: &mail::Address::System, to: &to,
            subject: &format!("{project}: dispatcher deferred"),
            body: &format!("{reason}. No cards were marked failed. Dispatch resumes when the limit or configuration permits it."),
            thread: None, in_reply_to: None,
        })?;
        Ok(())
    }).await
}

/// Persistent cooldown and a single visible notice for a failing card.
/// A concurrent successful claim must not receive a failure notice.
async fn record_failure(
    state: &Shared,
    id: String,
    who: Actor,
    error: String,
    from: Option<String>,
) -> ApiResult<()> {
    state.db.write("system", move |t| {
        let card = board::get_card(t, &id)?;
        let review = board::role_name(t, &card.project, "review", "review")?;
        let ineligible_column = match &from {
            // A card dispatch: still in the column it was picked from.
            Some(from) if *from != review => card.column != *from || card.agent.is_some(),
            _ => {
                let ready = board::role_name(t, &card.project, "ready", "ready")?;
                card.column != review && (card.column != ready || card.agent.is_some())
            }
        };
        if card.archived || ineligible_column || card.assignee.is_some() {
            return Ok(());
        }
        let previous: Option<(u32, i64)> = t.query_row(
            "SELECT failures, retry_at FROM card_dispatch_retries WHERE card = ?1",
            [&id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let now = chrono::Utc::now().timestamp();
        // Concurrent failed attempts count once per retry interval.
        if previous.is_some_and(|(_, at)| at > now) {
            return Ok(());
        }
        let failures = previous.map_or(1, |(n, _)| n.saturating_add(1));
        let delay = (60_i64 << failures.saturating_sub(1).min(5)).min(1800);
        t.execute(
            "INSERT INTO card_dispatch_retries (card, failures, retry_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(card) DO UPDATE SET failures = excluded.failures, retry_at = excluded.retry_at",
            params![id, failures, now + delay],
        )?;
        if previous.is_none() {
            let message = format!("Dispatcher could not start {id}: {error}. Retrying after 1 minute, doubling up to 30 minutes. Other eligible ready cards can proceed.");
            board::system_comment(t, &id, &message, None)?;
            mail::send(t, mail::Outgoing {
                from: &mail::Address::System,
                to: &mail::Address::from_actor(&who),
                subject: &format!("{id}: dispatcher could not start agent"),
                body: &message,
                thread: None,
                in_reply_to: None,
            })?;
        }
        Ok(())
    }).await
}

/// Timer reconciliation also covers daemon restarts and missed card events.
pub async fn pass(state: &Shared) {
    let projects = state
        .db
        .read(|c| {
            let mut q = c.prepare(
                "SELECT name FROM projects WHERE card_dispatcher IS NOT NULL ORDER BY name",
            )?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await;
    let Ok(projects) = projects else { return };
    for project in projects {
        // Bound work per tick even on a board with a very large WIP limit.
        for _ in 0..16 {
            match dispatch(state, &project).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    tracing::debug!(%project, error = %e.message, "card dispatch deferred");
                    // This card is now cooling down. Try the next eligible one.
                }
            }
        }
    }
}
