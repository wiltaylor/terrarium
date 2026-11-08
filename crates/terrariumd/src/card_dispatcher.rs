//! Opt-in ready-card dispatch. All spawn rights and caps belong to the
//! configured project admin, never to the operator. Selection is rechecked
//! in the transaction that creates the bound agent and claims its card.

use rusqlite::Connection;
use terrarium_core::Role;

use crate::actor::{Actor, AgentActor};
use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::spawn::{Internal, SpawnBody};
use crate::store::{agents, board, projects};

fn actor(agent: terrarium_core::Agent) -> Actor {
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
    let selected = state
        .db
        .read(move |c| {
            let Some(owner) = owner(c, &p)? else {
                return Ok(None);
            };
            Ok(candidate(c, &p)?.map(|(id, template)| (owner, id, template)))
        })
        .await?;
    let Some((who, id, template)) = selected else {
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
            return Err(ApiError::conflict("dispatcher selection changed"));
        }
        Ok(())
    });
    let card = id.clone();
    let claim: crate::spawn::AfterCreate = Box::new(move |t, agent| {
        let who = actor(agents::get(t, agent)?.agent);
        board::claim_card(t, &who, &card)?;
        Ok(())
    });
    let skills = state
        .config_repo
        .get()
        .ok()
        .map(|r| crate::skills::library_dir(&r.dir));
    crate::spawn::spawn_with(
        state,
        &who,
        who.address(),
        SpawnBody {
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
            targets: who
                .agent()
                .map(|a| a.policy.targets.clone())
                .unwrap_or_default(),
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
    .await?;
    Ok(true)
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
                    break;
                }
            }
        }
    }
}
