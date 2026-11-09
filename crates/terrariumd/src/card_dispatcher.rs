//! Opt-in ready-card dispatch. All spawn rights and caps belong to the
//! configured project admin, never to the operator. Selection is rechecked
//! in the transaction that creates the bound agent and claims its card.

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
    let Some(name) = projects::get(c, project)?.card_dispatcher else {
        return Ok(None);
    };
    let v = agents::find(c, &name, Some(project))?;
    if v.agent.project != project || !v.agent.roles.contains(&Role::ProjectAdmin) || v.ephemeral {
        return Err(ApiError::forbidden(
            "card dispatcher must be a project admin",
        ));
    }
    if v.paused {
        return Ok(None);
    }
    Ok(Some(actor(v.agent)))
}

fn template(card: &board::Card) -> Option<String> {
    // Only known worker types, not arbitrary labels used as template names.
    let types: Vec<_> = card
        .labels
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

fn candidate(c: &Connection, project: &str) -> ApiResult<Option<(String, String)>> {
    let b = board::get_board(c, project)?;
    let Some(working) = b
        .columns
        .iter()
        .find(|col| col.role.as_deref() == Some(board::WORKING))
    else {
        return Ok(None);
    };
    // Fail closed without an explicit limit: opt-in must not create an
    // unbounded number of agents on a board with no concurrency policy.
    if working.wip_limit.is_none_or(|limit| working.count >= limit) {
        return Ok(None);
    }
    let ready = board::role_name(c, project, "ready", "ready")?;
    for card in b.cards.iter().filter(|card| card.column == ready) {
        if card.kind != "card"
            || card.assignee.is_some()
            || card.agent.is_some()
            || !card.blocked_by.is_empty()
            || board::is_held(c, &card.id)?
        {
            continue;
        }
        let cooling: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM card_dispatch_retries WHERE card = ?1 AND retry_at > ?2)",
            params![card.id, chrono::Utc::now().timestamp()],
            |r| r.get(0),
        )?;
        if cooling {
            continue;
        }
        if let Some(template) = template(card) {
            return Ok(Some((card.id.clone(), template)));
        }
    }
    Ok(None)
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
    let selected = state.db.read(move |c| candidate(c, &p)).await?;
    let Some((id, template)) = selected else {
        return Ok(false);
    };
    let (p, expected, expected_owner, expected_template) = (
        project.to_owned(),
        id.clone(),
        who.clone(),
        template.clone(),
    );
    let check: crate::spawn::DefCheck = Box::new(move |c, _| {
        if owner(c, &p)?.as_ref() != Some(&expected_owner)
            || candidate(c, &p)? != Some((expected, expected_template))
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
        return handle_spawn_error(state, project, id, who, error).await;
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
pub async fn handle_spawn_error(
    state: &Shared,
    project: &str,
    id: String,
    who: Actor,
    error: ApiError,
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
    record_failure(state, id, who, error.message.clone()).await?;
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
async fn record_failure(state: &Shared, id: String, who: Actor, error: String) -> ApiResult<()> {
    state.db.write("system", move |t| {
        let card = board::get_card(t, &id)?;
        let ready = board::role_name(t, &card.project, "ready", "ready")?;
        let review = board::role_name(t, &card.project, "review", "review")?;
        let ineligible_column =
            card.column != review && (card.column != ready || card.agent.is_some());
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
