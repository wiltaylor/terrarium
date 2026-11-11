//! Durable, per-card merge-conflict repairs. Spawn through the normal
//! template path under a project admin, without additional grants.
use rusqlite::{OptionalExtension, params};
use terrarium_core::Role;

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::spawn::{Internal, SpawnBody};
use crate::store::{agents, board, spawns};

#[derive(Clone)]
struct Request {
    job: String,
    card: String,
    project: String,
    template: String,
    owner: Option<String>,
    task: String,
}

async fn dispatch(state: &Shared, r: Request) -> ApiResult<()> {
    let request = r.clone();
    let live = state.db.write("system", move |t| {
        let r = request;
        // A card may have been archived or completed while this waited.
        let card = board::get_card(t, &r.card)?;
        let terminal: bool = t.query_row(
            "SELECT archived OR coalesce(k.role, k.name) IN ('accepted', 'done') FROM cards c JOIN board_columns k ON k.id = c.column_id WHERE c.id = ?1",
            [&r.card], |row| row.get(0),
        )?;
        if terminal || card.project != r.project {
            t.execute("UPDATE conflict_fixes SET done = 1 WHERE job = ?1 AND card = ?2", params![r.job, r.card])?;
            return Ok(Some(None));
        }
        if let Some(sp) = spawns::open_bound(t, &spawns::Bound::new(crate::bound::KIND_CARD, &r.card))? {
            let turn = terrarium_core::turn_tag::render(
                &terrarium_core::turn_tag::TurnSource::System {
                    kind: terrarium_core::turn_tag::SystemKind::Card,
                    detail: Some(r.card.clone()),
                }, &r.task,
            );
            crate::bound::unpark_tx(t, &sp.id, Some(&turn), "merge conflict")?;
            t.execute("UPDATE conflict_fixes SET done = 1 WHERE job = ?1 AND card = ?2", params![r.job, r.card])?;
            return Ok(Some(Some(sp.id)));
        }
        Ok(None)
    }).await?;
    if let Some(id) = live {
        if let Some(id) = id {
            crate::lifecycle::wake_soon(state, id.parse().map_err(|_| ApiError::invalid("bad bound agent id"))?, "merge conflict");
        }
        return Ok(());
    }
    let request = r.clone();
    let who = state.db.read(move |c| {
        let name = match request.owner {
            Some(name) => name,
            None => c.query_row(
                "SELECT a.name FROM agents a JOIN roles r ON r.agent_id = a.id WHERE a.project = ?1 AND r.project = ?1 AND r.role = 'project_admin' AND a.owner IS NULL ORDER BY a.name LIMIT 1",
                [&request.project], |row| row.get(0),
            ).optional()?.ok_or_else(|| ApiError::conflict("no project admin for conflict repair"))?,
        };
        let v = agents::find(c, &name, Some(&request.project))?;
        if v.ephemeral || v.paused || v.agent.project != request.project || !v.agent.roles.contains(&Role::ProjectAdmin) {
            return Err(ApiError::forbidden("conflict repair owner must be an unpaused project admin"));
        }
        Ok(crate::card_dispatcher::actor(v.agent))
    }).await?;
    let request = r.clone();
    let check: crate::spawn::DefCheck = Box::new(move |c, _| {
        let pending: bool = c.query_row("SELECT done = 0 FROM conflict_fixes WHERE job = ?1 AND card = ?2", params![request.job, request.card], |row| row.get(0))?;
        if !pending || spawns::open_bound(c, &spawns::Bound::new(crate::bound::KIND_CARD, &request.card))?.is_some() {
            return Err(ApiError::conflict("conflict repair already handled"));
        }
        Ok(())
    });
    let request = r.clone();
    let claim: crate::spawn::AfterCreate = Box::new(move |t, id| {
        let who = crate::card_dispatcher::actor(agents::get(t, id)?.agent);
        board::claim_card(t, &who, &request.card)?;
        t.execute("UPDATE conflict_fixes SET done = 1 WHERE job = ?1 AND card = ?2", params![request.job, request.card])?;
        Ok(())
    });
    let skills = state.config_repo.get().ok().map(|r| crate::skills::library_dir(&r.dir));
    crate::spawn::spawn_with(state, &who, who.address(), SpawnBody {
        like: None,
        like_skip_missing: false,
        template: r.template,
        task: r.task,
        name: None,
        wait: false,
        timeout: None,
        project: None,
        targets: Vec::new(),
        grants: Vec::new(),
        bind: Some(format!("card:{}", r.card)),
    }, skills, None, Internal { check: Some(check), after_create: Some(claim), ..Internal::default() }).await?;
    Ok(())
}

/// The request is committed with the rule step. A lost effect or daemon
/// restart cannot lose it. Atomic bound checks arbitrate with dispatchers.
pub async fn pass(state: &Shared) {
    let requests = state.db.read(|c| {
        let mut q = c.prepare("SELECT job, card, project, template, owner, task FROM conflict_fixes WHERE done = 0 AND retry_at <= unixepoch() ORDER BY rowid LIMIT 16")?;
        Ok(q.query_map([], |row| Ok(Request { job: row.get(0)?, card: row.get(1)?, project: row.get(2)?, template: row.get(3)?, owner: row.get(4)?, task: row.get(5)? }))?.collect::<Result<Vec<_>, _>>()?)
    }).await;
    let Ok(requests) = requests else { return };
    for r in requests {
        if let Err(error) = dispatch(state, r.clone()).await {
            tracing::warn!(card = %r.card, error = %error.message, "conflict repair deferred");
            let _ = state.db.write("system", move |t| {
                t.execute("UPDATE conflict_fixes SET retry_at = unixepoch() + 60 WHERE job = ?1 AND card = ?2", params![r.job, r.card])?;
                Ok(())
            }).await;
        }
    }
}
