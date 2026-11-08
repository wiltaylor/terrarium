//! Reservation enforcement, authorization, and the restart-safe sweep.

use crate::store::{
    agents, hosts,
    reservations::{self as store, Holder, Reservation, State},
    targets,
};
use crate::{
    actor::Actor,
    api::Shared,
    error::{ApiError, ApiResult},
    executor, policy,
};
use serde::Deserialize;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use terrarium_core::{
    config::{TargetDef, TargetKind},
    limits::Limits,
    resources::{Amounts, RuntimeLimits},
};
use terrarium_host::protocol::{DaemonFrame, ResourceApply, ResourceStop};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBody {
    pub target: String,
    /// Absent: reserve for the target. Present: reserve for this agent.
    pub agent: Option<String>,
    pub amounts: Amounts,
    pub until: String,
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub queue: bool,
    pub queue_timeout: Option<String>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn ready(state: &Shared, host: &str) -> ApiResult<()> {
    if state.executor.resource_pending(host) {
        return Err(ApiError::conflict(
            "host reservation reconciliation is pending; retry after enforcement is restored",
        ));
    }
    Ok(())
}

fn docker_defaults(state: &Shared, def: &TargetDef, limits: &mut RuntimeLimits) -> ApiResult<()> {
    let host = def
        .host
        .as_deref()
        .ok_or_else(|| ApiError::conflict("missing host capacity"))?;
    if !state.executor.hosts.is_online(host)
        || state
            .executor
            .hosts
            .latest_metrics(host)
            .is_some_and(|(_, stale)| stale)
    {
        return Err(ApiError::conflict(
            "host capacity report is stale; reservation capacity remains charged",
        ));
    }
    let capacity = state
        .executor
        .hosts
        .capabilities(host)
        .and_then(|c| c.capacity)
        .ok_or_else(|| {
            ApiError::conflict(
                "host capacity report is missing; reservation capacity remains charged",
            )
        })?;
    limits
        .docker_defaults(&capacity)
        .map_err(ApiError::conflict)
}

fn until(value: &str) -> ApiResult<i64> {
    let s = crate::leases::parse_until(Some(value))?
        .ok_or_else(|| ApiError::invalid("until is required"))?;
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|d| d.timestamp_millis())
        .map_err(|e| ApiError::invalid(e.to_string()))
}

fn budget(r: &Reservation) -> ApiResult<RuntimeLimits> {
    let cpu = u32::try_from(r.amounts.cpu_millis)
        .map_err(|_| ApiError::invalid("CPU amount is too large"))?;
    if cpu > 1_024_000 || !cpu.is_multiple_of(10) {
        return Err(ApiError::invalid(
            "CPU must be at most 1024 cores with at most two decimals",
        ));
    }
    if r.amounts.memory_bytes > 0 && r.amounts.memory_bytes < terrarium_core::limits::MIN_MEMORY {
        return Err(ApiError::invalid("memory reservation must be at least 16M"));
    }
    Ok(RuntimeLimits {
        limits: Limits {
            cpu_millis: (cpu > 0).then_some(cpu),
            memory_bytes: (r.amounts.memory_bytes > 0).then_some(r.amounts.memory_bytes),
            ..Default::default()
        },
        cpus: r.exclusive.then(|| r.cores.clone()),
        io: r.amounts.io.clone(),
    })
}

fn managed(r: &Reservation) -> bool {
    matches!(r.state, State::Starting | State::Active | State::Ending)
}

fn scope(r: &Reservation, target: &str) -> String {
    if r.exclusive {
        format!("_exclusive/{target}")
    } else {
        target.to_owned()
    }
}

fn shared_cpus(physical: &[u32], rows: &[Reservation]) -> ApiResult<Option<Vec<u32>>> {
    if !rows.iter().any(|r| managed(r) && r.exclusive) {
        return Ok(None);
    }
    let cpus: Vec<_> = physical
        .iter()
        .copied()
        .filter(|cpu| {
            !rows
                .iter()
                .any(|r| managed(r) && r.exclusive && r.cores.contains(cpu))
        })
        .collect();
    if cpus.is_empty() {
        return Err(ApiError::conflict(
            "no CPUs remain for shared managed workloads",
        ));
    }
    Ok(Some(cpus))
}

async fn row(state: &Shared, id: &str) -> ApiResult<Reservation> {
    let id = id.to_owned();
    state.db.read(move |c| store::get(c, &id)).await
}

async fn authorize(
    state: &Shared,
    actor: &Actor,
    r: &Reservation,
    holder_may_end: bool,
) -> ApiResult<()> {
    if holder_may_end
        && matches!(&r.holder, Holder::Agent(id) if actor.agent().is_some_and(|a| a.id == *id))
    {
        return Ok(());
    }
    let target = r.target.clone();
    let t = state.db.read(move |c| targets::get(c, &target)).await?;
    policy::lease_target(actor, &r.target, t.def.project.as_deref(), &r.project)
}

async fn host_targets(state: &Shared, host: &str) -> ApiResult<Vec<targets::TargetRow>> {
    let host = host.to_owned();
    state
        .db
        .read(move |c| {
            Ok(targets::list(c)?
                .into_iter()
                .filter(|r| r.def.host.as_deref() == Some(&host))
                .collect())
        })
        .await
}

/// Refuse unrepresentable live migrations instead of doubling a host-wide
/// agent budget across its old per-target slices.
async fn preflight(state: &Shared, r: &Reservation) -> ApiResult<()> {
    let rows = host_targets(state, &r.host).await?;
    let target = rows
        .iter()
        .find(|t| t.def.name == r.target)
        .ok_or_else(|| ApiError::invalid("target is not on the requested host"))?;
    if !matches!(target.def.kind, TargetKind::Host | TargetKind::Docker) {
        return Err(ApiError::invalid(
            "reservations need a host or managed Docker target",
        ));
    }
    let caps = state
        .executor
        .hosts
        .capabilities(&r.host)
        .ok_or_else(|| ApiError::conflict("reservation host is offline"))?;
    if !caps.reservations {
        return Err(ApiError::conflict(
            "host is missing the reservations capability; upgrade its host agent",
        ));
    }
    let limits = budget(r)?;
    let host = r.host.clone();
    let reservations = state.db.read(move |c| store::list(c, &host)).await?;
    if reservations.iter().any(|other| {
        other.id != r.id
            && managed(other)
            && matches!(
                (&r.holder, &other.holder),
                (Holder::Agent(_), Holder::Target(_)) | (Holder::Target(_), Holder::Agent(_))
            )
    }) {
        return Err(ApiError::conflict(
            "agent and target reservations cannot overlap on a host: their slice hierarchies differ",
        ));
    }
    if r.exclusive {
        RuntimeLimits {
            cpus: Some(vec![0]),
            ..Default::default()
        }
        .host_support(caps.limits.as_ref())
        .map_err(ApiError::conflict)?;
        let incompatible: Vec<_> = rows
            .iter()
            .filter(|t| {
                t.state == "running"
                    && (!matches!(t.def.kind, TargetKind::Host | TargetKind::Docker)
                        || (t.def.kind == TargetKind::Docker && t.def.image.is_none()))
            })
            .map(|t| t.def.name.clone())
            .collect();
        if !incompatible.is_empty() {
            return Err(
                ApiError::conflict("exclusive CPU cannot cover these running targets")
                    .with_details(json!({"targets":incompatible})),
            );
        }
        let names: Vec<_> = rows
            .iter()
            .filter(|t| t.def.kind == TargetKind::Host)
            .map(|t| t.def.name.clone())
            .collect();
        let sessions = state.executor.resource_sessions(&names, None);
        if !sessions.is_empty() {
            return Err(ApiError::conflict("end existing host sessions before an exclusive grant; live scopes cannot be reparented").with_details(json!({"sessions":sessions})));
        }
    }
    match target.def.kind {
        TargetKind::Docker => {
            if target.def.image.is_none() {
                return Err(ApiError::invalid(
                    "Docker reservations need a managed image target",
                ));
            }
            if matches!(r.holder, Holder::Agent(_)) {
                return Err(ApiError::invalid(
                    "a Docker container shares one budget: reserve for its target, not an individual agent",
                ));
            }
            limits
                .limits
                .docker_support(caps.limits.as_ref())
                .map_err(ApiError::conflict)?;
            if !limits.io.is_empty() && !target.def.ephemeral {
                return Err(ApiError::conflict(
                    "device IO reservations require an ephemeral Docker target: limits are fixed at create and cannot be cleared without recreating a permanent container",
                ));
            }
            if !limits.io.is_empty() && target.state == "running" {
                return Err(ApiError::conflict(
                    "device IO limits take effect at the next up; Docker cannot change them live",
                ));
            }
        }
        _ => limits
            .host_support(caps.limits.as_ref())
            .map_err(ApiError::conflict)?,
    }
    if let Holder::Agent(agent) = &r.holder {
        let names = rows.iter().map(|r| r.def.name.clone()).collect::<Vec<_>>();
        let sessions = state.executor.resource_sessions(&names, Some(agent));
        if !sessions.is_empty() {
            return Err(ApiError::conflict("finish existing host sessions before granting an agent reservation; live scopes cannot be reparented")
                .with_details(json!({"sessions":sessions,"host":r.host,"holder":agent})));
        }
    } else if target.def.kind == TargetKind::Host {
        let sessions = state
            .executor
            .resource_sessions(std::slice::from_ref(&r.target), None);
        if !sessions.is_empty() {
            return Err(ApiError::conflict("finish existing host sessions before granting a target reservation; unscoped processes cannot be reparented")
                .with_details(json!({"sessions":sessions,"host":r.host,"target":r.target})));
        }
    }
    Ok(())
}

pub async fn grant(
    state: &Shared,
    actor: &Actor,
    host: &str,
    body: NewBody,
) -> ApiResult<Reservation> {
    let _guard = state.executor.resource_lock.lock().await;
    ready(state, host)?;
    let target = body.target.clone();
    let key = body.agent.clone();
    let context = actor.project().map(str::to_owned);
    let (t, holder, project) = state
        .db
        .read(move |c| {
            let t = targets::get(c, &target)?;
            let (holder, project) = if let Some(key) = key {
                let a = agents::find(c, &key, t.def.project.as_deref().or(context.as_deref()))?;
                (Holder::Agent(a.agent.id.to_string()), a.agent.project)
            } else {
                (
                    Holder::Target(t.def.name.clone()),
                    t.def.project.clone().or(context).unwrap_or_default(),
                )
            };
            Ok((t, holder, project))
        })
        .await?;
    policy::lease_target(actor, &body.target, t.def.project.as_deref(), &project)?;
    if t.def.project.as_ref().is_some_and(|p| *p != project) {
        return Err(ApiError::invalid(
            "holder and target must belong to the same project",
        ));
    }
    if t.def.host.as_deref() != Some(host) {
        return Err(ApiError::invalid("target is not on the requested host"));
    }
    let at = now();
    let end = until(&body.until)?;
    let queue_ms = if body.queue {
        Some(
            i64::try_from(
                crate::settings::parse_duration(body.queue_timeout.as_deref().unwrap_or("1h"))
                    .map_err(ApiError::invalid)?
                    .as_millis(),
            )
            .map_err(|_| ApiError::invalid("queue timeout overflow"))?,
        )
    } else {
        None
    };
    let req = store::Request {
        host: host.to_owned(),
        project,
        holder,
        target: body.target,
        amounts: body.amounts,
        exclusive: body.exclusive,
        reason: body.reason,
        duration_ms: end - at,
        queue_ms,
    };
    // Store changes roll back if request validation fails. Enforcement never
    // marks the reservation active until the host acknowledges it.
    let r = state
        .db
        .write(actor.label(), move |tx| store::request(tx, req, at))
        .await?;
    if let Err(e) = preflight(state, &r).await {
        let id = r.id.clone();
        state
            .db
            .write("system", move |tx| {
                if r.state == State::Queued {
                    store::cancel(tx, &id)?;
                } else {
                    store::end(tx, &id, false)?;
                    store::finish_end(tx, &id)?;
                }
                Ok(())
            })
            .await?;
        return Err(e);
    }
    if r.state == State::Starting {
        start(state, &r).await
    } else {
        Ok(r)
    }
}

async fn apply(state: &Shared, r: &Reservation, reset: bool) -> ApiResult<()> {
    let target = r.target.clone();
    let t = state.db.read(move |c| targets::get(c, &target)).await?;
    let (scope, holder) = match &r.holder {
        Holder::Agent(id) => {
            let id = id.clone();
            let name = state
                .db
                .read(move |c| agents::display_name(c, &id))
                .await?
                .ok_or_else(|| ApiError::not_found("reservation agent was deleted"))?;
            (scope(r, "_reservation"), name)
        }
        Holder::Target(_) => (scope(r, &r.target), String::new()),
    };
    let mut limits = if reset {
        RuntimeLimits {
            limits: if matches!(r.holder, Holder::Target(_)) && t.def.kind == TargetKind::Docker {
                t.def
                    .limits
                    .as_ref()
                    .map(|l| l.resolve())
                    .transpose()
                    .map_err(ApiError::invalid)?
                    .unwrap_or_default()
            } else {
                Limits::default()
            },
            ..Default::default()
        }
    } else {
        budget(r)?
    };
    if t.def.kind == TargetKind::Docker && (reset || !r.exclusive) {
        let host = r.host.clone();
        let omitted = reset.then(|| r.id.clone());
        limits.cpus = state
            .db
            .read(move |c| {
                let rows: Vec<_> = store::list(c, &host)?
                    .into_iter()
                    .filter(|r| Some(&r.id) != omitted.as_ref())
                    .collect();
                if !rows.iter().any(|r| managed(r) && r.exclusive) {
                    return Ok(None);
                }
                shared_cpus(&store::capacity(c, &host)?.0.cpu_ids, &rows)
            })
            .await?;
    }
    if t.def.kind == TargetKind::Docker && t.state != "running" {
        return Ok(());
    }
    if t.def.kind == TargetKind::Docker && r.state == State::Active {
        limits.io.clear();
    }
    if t.def.kind == TargetKind::Docker {
        docker_defaults(state, &t.def, &mut limits)?;
    }
    let container = (t.def.kind == TargetKind::Docker).then(|| executor::container_name(&t.def));
    executor::verb_ok(state, &t.def, "reservation limits", move |session| {
        DaemonFrame::ResourceApply(ResourceApply {
            session,
            target: scope,
            holder,
            container,
            limits,
        })
    })
    .await?;
    Ok(())
}

async fn start(state: &Shared, r: &Reservation) -> ApiResult<Reservation> {
    let applied = async {
        if r.exclusive {
            isolate(state, &r.host, None).await?;
        }
        apply(state, r, false).await
    }
    .await;
    if let Err(e) = applied {
        let id = r.id.clone();
        state
            .db
            .write("system", move |tx| store::end(tx, &id, false))
            .await?;
        // Keep the capacity in ending if resetting a partially applied budget
        // fails. The sweep retries it after reconnect.
        if let Err(cleanup) = finish(state, r).await {
            tracing::warn!("reservation start cleanup: {cleanup}");
        }
        return Err(e);
    }
    let id = r.id.clone();
    state
        .db
        .write("system", move |tx| store::activate(tx, &id))
        .await
}

async fn finish(state: &Shared, r: &Reservation) -> ApiResult<Reservation> {
    let names: Vec<String> = match &r.holder {
        Holder::Agent(_) => host_targets(state, &r.host)
            .await?
            .into_iter()
            .map(|t| t.def.name)
            .collect(),
        Holder::Target(target) => vec![target.clone()],
    };
    let agent = match &r.holder {
        Holder::Agent(id) => Some(id.as_str()),
        Holder::Target(_) => None,
    };
    let unconfirmed = state.executor.end_resource_sessions(&names, agent).await;
    let target = r.target.clone();
    let t = state.db.read(move |c| targets::get(c, &target)).await?;
    if t.def.kind == TargetKind::Host {
        let (target, holder) = match &r.holder {
            Holder::Target(target) => (scope(r, target), String::new()),
            Holder::Agent(id) => {
                let id = id.clone();
                let holder = state
                    .db
                    .read(move |c| agents::display_name(c, &id))
                    .await?
                    .ok_or_else(|| ApiError::not_found("reservation agent was deleted"))?;
                (scope(r, "_reservation"), holder)
            }
        };
        executor::verb_ok(state, &t.def, "stop reservation slice", move |session| {
            DaemonFrame::ResourceStop(ResourceStop {
                session,
                target,
                holder,
            })
        })
        .await?;
    } else if !unconfirmed.is_empty() {
        let id = r.id.clone();
        let project = r.project.clone();
        state.db.write("system", move |tx| {
            let detail = json!({"reservation":id,"unconfirmed":unconfirmed});
            tx.audit("reserve.cleanup_unconfirmed", &id, Some(&project), detail.clone())?;
            tx.emit(crate::events::EventDraft::new("reservation_cleanup_warning",
                format!("reservation {id}: session cleanup could not be confirmed within the bounded wait"), detail).project(project))?;
            Ok(())
        }).await?;
    }
    let restored = apply(state, r, true).await;
    if let Err(error) = &restored {
        let id = r.id.clone();
        let project = r.project.clone();
        let reason = error.message.clone();
        state
            .db
            .write("system", move |tx| {
                let detail = json!({"reservation":id,"reason":reason,"capacity_charged":true});
                tx.audit(
                    "reserve.restore_pending",
                    &id,
                    Some(&project),
                    detail.clone(),
                )?;
                tx.emit(
                    crate::events::EventDraft::new(
                        "reservation_cleanup_warning",
                        format!(
                            "reservation {id}: limits restoration pending; capacity remains charged"
                        ),
                        detail,
                    )
                    .project(project),
                )?;
                Ok(())
            })
            .await?;
    }
    if t.def.kind == TargetKind::Docker && t.def.ephemeral && t.state == "running" {
        let container = executor::container_name(&t.def);
        executor::verb_ok(
            state,
            &t.def,
            "reservation cleanup fallback stop",
            move |session| {
                DaemonFrame::DockerStop(terrarium_host::protocol::DockerStop {
                    session,
                    container,
                    grace_secs: Some(10),
                    timeout_secs: Some(120),
                })
            },
        )
        .await?;
        let target = r.target.clone();
        let id = r.id.clone();
        let error = restored.err().map(|e| e.message);
        state
            .db
            .write("system", move |tx| {
                targets::set_state(tx, &target, "stopped", None)?;
                tx.audit(
                    "reserve.cleanup_stop",
                    &id,
                    None,
                    json!({"target":target,"restore_error":error}),
                )?;
                Ok(())
            })
            .await?;
    } else {
        restored?;
    }
    if r.exclusive {
        isolate(state, &r.host, Some(&r.id)).await?;
    }
    let id = r.id.clone();
    state
        .db
        .write("system", move |tx| store::finish_end(tx, &id))
        .await
}

pub async fn extend(
    state: &Shared,
    actor: &Actor,
    id: &str,
    value: &str,
) -> ApiResult<Reservation> {
    let _guard = state.executor.resource_lock.lock().await;
    let r = row(state, id).await?;
    authorize(state, actor, &r, false).await?;
    let end = until(value)?;
    let id = id.to_owned();
    state
        .db
        .write(actor.label(), move |tx| store::extend(tx, &id, end, now()))
        .await
}

pub async fn retract(state: &Shared, actor: &Actor, id: &str) -> ApiResult<Reservation> {
    let _guard = state.executor.resource_lock.lock().await;
    let r = row(state, id).await?;
    authorize(state, actor, &r, true).await?;
    let id = id.to_owned();
    let r = state
        .db
        .write(actor.label(), move |tx| store::end(tx, &id, false))
        .await?;
    finish(state, &r).await
}

pub async fn cancel(state: &Shared, actor: &Actor, id: &str) -> ApiResult<Reservation> {
    let _guard = state.executor.resource_lock.lock().await;
    let r = row(state, id).await?;
    authorize(state, actor, &r, true).await?;
    let id = id.to_owned();
    state
        .db
        .write(actor.label(), move |tx| store::cancel(tx, &id))
        .await
}

pub async fn sweep(state: &Shared) -> ApiResult<()> {
    for host in state.db.read(hosts::list).await? {
        let name = host.name.clone();
        let needs_work = state
            .db
            .read(move |c| {
                Ok(store::list(c, &name)?
                    .iter()
                    .any(|r| managed(r) || r.state == State::Queued))
            })
            .await?;
        if !needs_work && !state.executor.resource_pending(&host.name) {
            continue;
        }
        let _guard = state.executor.resource_lock.lock().await;
        if state.executor.resource_pending(&host.name)
            && let Err(error) = reconcile_host_locked(state, &host.name).await
        {
            tracing::warn!(host=%host.name, %error, "reservation reconciliation pending");
            continue;
        }
        let name = host.name.clone();
        let rows = state.db.read(move |c| store::list(c, &name)).await?;
        for r in rows {
            let result = if r.state == State::Ending {
                finish(state, &r).await.map(|_| ())
            } else if matches!(r.state, State::Active | State::Starting)
                && r.until.is_some_and(|u| u <= now())
            {
                let id = r.id.clone();
                let ended = state
                    .db
                    .write("system", move |tx| store::end(tx, &id, true))
                    .await?;
                finish(state, &ended).await.map(|_| ())
            } else if r.state == State::Starting {
                match preflight(state, &r).await {
                    Ok(()) => start(state, &r).await.map(|_| ()),
                    Err(error) => Err(error),
                }
            } else {
                Ok(())
            };
            if let Err(e) = result {
                tracing::warn!(reservation=%r.id, "reservation reconcile: {e}");
            }
        }
        let name = host.name.clone();
        state
            .db
            .write("system", move |tx| {
                store::warnings(tx, &name, now())?;
                Ok(())
            })
            .await?;
        let name = host.name.clone();
        if let Some(r) = state
            .db
            .write("system", move |tx| store::advance(tx, &name, now()))
            .await?
        {
            if let Err(e) = preflight(state, &r).await {
                // A queued request can become unenforceable before admission.
                let id = r.id.clone();
                state
                    .db
                    .write("system", move |tx| {
                        store::end(tx, &id, false)?;
                        store::finish_end(tx, &id)
                    })
                    .await?;
                tracing::warn!(reservation=%r.id, "queued reservation refused: {e}");
            } else if let Err(e) = start(state, &r).await {
                tracing::warn!(reservation=%r.id, "queued reservation start: {e}");
            }
        }
    }
    Ok(())
}

/// Reapply persisted enforcement before permitting new work after a host
/// reconnect. Failure leaves the admission gate closed and the sweep retries.
pub async fn reconcile_host(state: &Shared, host: &str) -> ApiResult<()> {
    let _guard = state.executor.resource_lock.lock().await;
    reconcile_host_locked(state, host).await
}

async fn reconcile_host_locked(state: &Shared, host: &str) -> ApiResult<()> {
    if !state.executor.hosts.is_online(host) {
        return Err(ApiError::conflict("host is offline"));
    }
    let name = host.to_owned();
    let rows = state.db.read(move |c| store::list(c, &name)).await?;
    if rows.iter().any(|r| managed(r) && r.exclusive) {
        isolate(state, host, None).await?;
    }
    for r in rows.iter().filter(|r| managed(r)) {
        if r.state == State::Ending {
            finish(state, r).await?;
        } else if r.until.is_some_and(|u| u <= now()) {
            let id = r.id.clone();
            let ended = state
                .db
                .write("system", move |tx| store::end(tx, &id, true))
                .await?;
            finish(state, &ended).await?;
        } else if r.state == State::Starting {
            preflight(state, r).await?;
            start(state, r).await?;
        } else {
            apply(state, r, false).await?;
        }
    }
    state.executor.set_resource_pending(host, false);
    Ok(())
}

pub fn spawn_timer(state: &Shared) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                if let Err(e) = sweep(&state).await {
                    tracing::warn!("reservation sweep: {e}");
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    })
}

/// Effective container reservation, used at creation. Fixed target limits
/// stay the default when there is no reservation.
pub fn docker_budget(state: &Shared, def: &TargetDef) -> ApiResult<Option<RuntimeLimits>> {
    let Some(host) = &def.host else {
        return Ok(None);
    };
    state.db.read_now(|c| {
        let rows = store::list(c, host)?;
        let shared = if rows.iter().any(|r| managed(r) && r.exclusive) {
            shared_cpus(&store::capacity(c, host)?.0.cpu_ids, &rows)?
        } else {
            None
        };
        for r in &rows {
            if r.holder == Holder::Target(def.name.clone()) && managed(r) {
                if r.state == State::Ending || r.until.is_some_and(|u| u <= now()) {
                    return Err(ApiError::conflict("reservation ended; cleanup is pending"));
                }
                let mut limits = budget(r)?;
                if !r.exclusive {
                    limits.cpus = shared;
                }
                return Ok(Some(limits));
            }
        }
        if let Some(cpus) = shared {
            return Ok(Some(RuntimeLimits {
                cpus: Some(cpus),
                limits: def
                    .limits
                    .as_ref()
                    .map(|l| l.resolve())
                    .transpose()
                    .map_err(ApiError::invalid)?
                    .unwrap_or_default(),
                ..Default::default()
            }));
        }
        Ok(None)
    })
}

/// Lifecycle admission shares the same host-wide exclusion boundary as exec.
pub fn lifecycle_admission(state: &Shared, def: &TargetDef) -> ApiResult<()> {
    let Some(host) = &def.host else {
        return Ok(());
    };
    ready(state, host)?;
    state.db.read_now(|c| {
        let rows = store::list(c, host)?;
        if rows.iter().any(|r| managed(r) && r.exclusive) && (!matches!(def.kind, TargetKind::Host | TargetKind::Docker)
            || (def.kind == TargetKind::Docker && def.image.is_none())) {
            return Err(ApiError::conflict("exclusive CPU reservations prohibit vmlab, SSH and unmanaged Docker paths on this host"));
        }
        if rows.iter().any(|r| r.holder == Holder::Target(def.name.clone()) && managed(r)
            && (r.state != State::Active || r.until.is_some_and(|u| u <= now()))) {
            return Err(ApiError::conflict("reservation enforcement or cleanup is pending"));
        }
        Ok(())
    })
}

/// A permanent stopped container does not run create-time flags again.
/// Apply supported defaults without replacing an active reservation budget.
pub async fn update_docker_defaults(state: &Shared, def: &TargetDef) -> ApiResult<()> {
    let caps = def
        .host
        .as_deref()
        .and_then(|h| state.executor.hosts.capabilities(h))
        .ok_or_else(|| ApiError::conflict("Docker host is offline; cannot update live limits"))?;
    if !caps.reservations {
        return Err(ApiError::conflict(
            "host is missing live resource update support",
        ));
    }
    let mut limits = docker_budget(state, def)?.unwrap_or(RuntimeLimits {
        limits: def
            .limits
            .as_ref()
            .map(|l| l.resolve())
            .transpose()
            .map_err(ApiError::invalid)?
            .unwrap_or_default(),
        ..Default::default()
    });
    limits
        .limits
        .docker_support(caps.limits.as_ref())
        .map_err(ApiError::conflict)?;
    // Active create-time IO is unchanged by a supported-field update.
    limits.io.clear();
    docker_defaults(state, def, &mut limits)?;
    let target = def.name.clone();
    let container = Some(executor::container_name(def));
    executor::verb_ok(state, def, "update Docker defaults", move |session| {
        DaemonFrame::ResourceApply(ResourceApply {
            session,
            target,
            holder: String::new(),
            container,
            limits,
        })
    })
    .await?;
    Ok(())
}

/// A permanent stopped container does not run create-time flags again.
/// Supported limits are always refreshed before start, including defaults
/// left behind by a reservation whose cleanup had to stop the container.
pub async fn before_docker_start(state: &Shared, def: &TargetDef) -> ApiResult<()> {
    let reservation = docker_budget(state, def)?;
    let supported = def
        .host
        .as_deref()
        .and_then(|h| state.executor.hosts.capabilities(h))
        .is_some_and(|c| c.reservations);
    if !supported {
        if reservation.is_some() {
            return Err(ApiError::conflict("host is missing reservation support"));
        }
        return Ok(());
    }
    let mut limits = reservation.unwrap_or(RuntimeLimits {
        limits: def
            .limits
            .as_ref()
            .map(|l| l.resolve())
            .transpose()
            .map_err(ApiError::invalid)?
            .unwrap_or_default(),
        ..Default::default()
    });
    if !limits.io.is_empty() {
        return Err(ApiError::conflict(
            "device IO limits need a newly created container at the next up; this permanent container already exists",
        ));
    }
    docker_defaults(state, def, &mut limits)?;
    let target = def.name.clone();
    let container = Some(executor::container_name(def));
    executor::verb_ok(
        state,
        def,
        "refresh Docker limits before start",
        move |session| {
            DaemonFrame::ResourceApply(ResourceApply {
                session,
                target,
                holder: String::new(),
                container,
                limits,
            })
        },
    )
    .await?;
    Ok(())
}

/// Future host commands share the host-wide agent slice or the target's
/// existing slice hierarchy. This read runs before sending the exec frame.
pub fn exec_budget(
    state: &Shared,
    def: &TargetDef,
    holder: Option<&str>,
) -> ApiResult<Option<terrarium_host::protocol::ExecLimits>> {
    let Some(host) = &def.host else {
        return Ok(None);
    };
    ready(state, host)?;
    state.db.read_now(|c| {
        let agent = holder.and_then(|h| agents::find(c, h, None).ok()).map(|a| a.agent.id.to_string());
        let rows = store::list(c, host)?;
        let exclusive = rows.iter().any(|r| managed(r) && r.exclusive);
        if exclusive && (!matches!(def.kind, TargetKind::Host | TargetKind::Docker)
            || (def.kind == TargetKind::Docker && def.image.is_none())) {
            return Err(ApiError::conflict("exclusive CPU reservations prohibit vmlab, SSH and unmanaged Docker execution on this host"));
        }
        for r in rows {
            let owns = match &r.holder { Holder::Agent(id) => agent.as_ref() == Some(id), Holder::Target(t) => *t == def.name };
            if !owns || !managed(&r) { continue; }
            if r.state != State::Active || r.until.is_some_and(|u| u <= now()) { return Err(ApiError::conflict("reservation is not active; enforcement or cleanup is pending")); }
            if def.kind != TargetKind::Host {
                if matches!(r.holder, Holder::Agent(_)) { return Err(ApiError::conflict("agent host reservation cannot cover a shared Docker container; reserve its target")); }
                return Ok(None);
            }
            let limits = budget(&r)?;
            return Ok(Some(terrarium_host::protocol::ExecLimits {
                target: scope(&r, if matches!(r.holder, Holder::Agent(_)) { "_reservation" } else { &def.name }),
                holder: holder.unwrap_or("operator").into(), limits: limits.limits, reservation: Some(limits),
            }));
        }
        if exclusive && def.kind == TargetKind::Host {
            // Even an otherwise unlimited command must enter terrarium.slice,
            // whose AllowedCPUs excludes every exclusive allocation.
            let limits = def.limits.as_ref().map(|l| l.resolve()).transpose().map_err(ApiError::invalid)?.unwrap_or_default();
            return Ok(Some(terrarium_host::protocol::ExecLimits { target: def.name.clone(), holder: holder.unwrap_or("operator").into(), limits, reservation: None }));
        }
        Ok(None)
    })
}

/// Restrict the shared systemd hierarchy and every existing managed Docker
/// container before an exclusive holder can start. Exclusive scopes are in a
/// sibling hierarchy, so they do not inherit the shared complement.
async fn isolate(state: &Shared, host: &str, omit: Option<&str>) -> ApiResult<()> {
    let name = host.to_owned();
    let (physical, rows) = state
        .db
        .read(move |c| Ok((store::capacity(c, &name)?.0, store::list(c, &name)?)))
        .await?;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| Some(r.id.as_str()) != omit)
        .collect();
    let cpus = shared_cpus(&physical.cpu_ids, &rows)?;
    let targets = host_targets(state, host).await?;
    if targets
        .iter()
        .any(|t| t.def.kind == TargetKind::Docker && t.state == "running" && t.def.image.is_none())
    {
        return Err(ApiError::conflict(
            "cannot change affinity of an unmanaged Docker container",
        ));
    }
    let representative = targets
        .first()
        .ok_or_else(|| ApiError::conflict("host has no target for isolation"))?;
    let limits = RuntimeLimits {
        cpus: cpus.clone(),
        ..Default::default()
    };
    executor::verb_ok(
        state,
        &representative.def,
        "shared CPU isolation",
        move |session| {
            DaemonFrame::ResourceApply(ResourceApply {
                session,
                target: String::new(),
                holder: String::new(),
                container: None,
                limits,
            })
        },
    )
    .await?;
    for t in targets
        .iter()
        .filter(|t| t.def.kind == TargetKind::Docker && t.state == "running")
    {
        let own = rows
            .iter()
            .find(|r| managed(r) && r.holder == Holder::Target(t.def.name.clone()));
        let mut limits = match own {
            Some(r) => budget(r)?,
            None => RuntimeLimits {
                limits: t
                    .def
                    .limits
                    .as_ref()
                    .map(|l| l.resolve())
                    .transpose()
                    .map_err(ApiError::invalid)?
                    .unwrap_or_default(),
                ..Default::default()
            },
        };
        if !own.is_some_and(|r| r.exclusive) {
            limits.cpus = cpus.clone();
        }
        docker_defaults(state, &t.def, &mut limits)?;
        // docker update leaves create-time device IO unchanged.
        limits.io.clear();
        let container = Some(executor::container_name(&t.def));
        let target = t.def.name.clone();
        executor::verb_ok(state, &t.def, "container CPU isolation", move |session| {
            DaemonFrame::ResourceApply(ResourceApply {
                session,
                target,
                holder: String::new(),
                container,
                limits,
            })
        })
        .await?;
    }
    Ok(())
}
