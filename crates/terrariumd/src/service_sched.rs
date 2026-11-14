//! Services C2 (TER-1265, design D3): the scheduling of service requests.
//!
//! A tick every [`TICK`] does, in order:
//!
//! 1. **Gone services.** An open request of a service that no longer
//!    exists ends as `failed` (C1 note N3).
//! 2. **The run limit.** A `running` request past its `run_until` (its
//!    start plus the service's `max_run_time`) ends as `timed_out`.
//! 3. **Grants.** A `waiting` request whose reservation is active goes to
//!    `running`. One whose reservation ended without a run ends: as
//!    `timed_out` when the reservation timed out in its queue, else as
//!    `failed`.
//! 4. **Release.** Each ended request, whoever ended it (the run of C3 and
//!    C4, the cancel route, the wait limit of `crate::service_sweep`, or
//!    the steps above), gets its queued reservation cancelled or its
//!    active one retracted, and its target lease revoked; then
//!    `released_at` is set (C1 note N1).
//! 5. **Starts.** The requests that `startable` gives, oldest first: a
//!    service without a resource claim goes to `running` at once. A claim
//!    is one queued reservation (`crate::reservations::grant` with
//!    `queue`), held by the target (N2), for the `max_run_time`, with the
//!    time left to `wait_until` as its queue timeout; the request is then
//!    `waiting`. A `409` (host offline, target running, the holder has a
//!    reservation) leaves the request `queued` until `retry_at`; any other
//!    refusal ends it as `failed`.
//!
//! A target lease needs an agent. [`lease_run`] leases the target to the
//! agent that runs a request (C4) until `run_until`; the release revokes
//! it. A script run (C3) has no agent and no lease: its reservation holds
//! the resources.
//!
//! The reservation and lease calls run as the operator: this is system
//! work, as the reservation and lease sweeps are.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;
use terrarium_core::resources::{Amounts, GpuClaim, GpuDevice};
use terrarium_core::service::{self as core, ServiceDef, State};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult, Code};
use crate::store::reservations::{self as res_store, State as ResState};
use crate::store::services::{self as store, Outcome, RequestRow};

/// How often the scheduler runs.
pub const TICK: Duration = Duration::from_secs(5);

/// How long a request waits after a reservation refusal of `409` before
/// the next try.
pub const RETRY: Duration = Duration::from_secs(60);

pub fn spawn(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            if let Err(e) = tick(&state, Utc::now()).await {
                warn!("service scheduler failed: {}", e.message);
            }
        }
    })
}

fn fmt(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn fmt_ms(ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(fmt)
}

fn add(at: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    at + chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::days(1))
}

/// What one [`tick`] did, for the tests and the log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Tick {
    /// Requests that went to `waiting` with a reservation.
    pub reserved: Vec<String>,
    /// Requests that went to `running`.
    pub started: Vec<String>,
    /// Requests that this tick ended (`failed` or `timed_out`).
    pub ended: Vec<String>,
    /// Requests whose reservation and lease were given back.
    pub released: Vec<String>,
    /// Requests left `queued` after a `409` refusal.
    pub deferred: Vec<String>,
}

/// One pass at a time: the background task and a test's call do not
/// grant the same request twice.
static TICK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One scheduler pass at `at`.
pub async fn tick(state: &Shared, at: DateTime<Utc>) -> ApiResult<Tick> {
    let _one = TICK_LOCK.lock().await;
    let mut out = Tick::default();
    fail_orphans(state, &mut out).await?;
    end_overruns(state, at, &mut out).await?;
    check_waiting(state, &mut out).await?;
    release_ended(state, &mut out).await?;
    start_queued(state, at, &mut out).await?;
    // A request that a step above ended (a refusal, a reservation that
    // ended) is released in the same pass.
    release_ended(state, &mut out).await?;
    Ok(out)
}

// ------------------------------------------------------------ the steps

fn open_ids(conn: &Connection, sql: &str, args: &[&str]) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let ids = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(ids)
}

/// End request `id` from the state it is in now, unless it has ended.
async fn end(state: &Shared, id: &str, to: State, why: String) -> ApiResult<bool> {
    let id = id.to_owned();
    state
        .db
        .write("system", move |t| {
            let cur = store::get_request(t, &id)?;
            if cur.state.is_final() {
                return Ok(false);
            }
            store::transition(
                t,
                &id,
                cur.state,
                to,
                Outcome {
                    error: Some(why),
                    ..Outcome::default()
                },
            )?;
            Ok(true)
        })
        .await
}

async fn fail_orphans(state: &Shared, out: &mut Tick) -> ApiResult<()> {
    let rows = state
        .db
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT r.id, r.service FROM service_requests r
                 WHERE r.state IN ('queued', 'waiting', 'running')
                   AND NOT EXISTS (SELECT 1 FROM services s WHERE s.name = r.service)
                 ORDER BY r.id",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    for (id, service) in rows {
        let why = format!("service `{service}` was removed");
        if end(state, &id, State::Failed, why).await? {
            info!(request = %id, %service, "service request failed: the service is gone");
            out.ended.push(id);
        }
    }
    Ok(())
}

async fn end_overruns(state: &Shared, at: DateTime<Utc>, out: &mut Tick) -> ApiResult<()> {
    let now = fmt(at);
    let ids = state
        .db
        .read(move |c| {
            open_ids(
                c,
                "SELECT id FROM service_requests
                 WHERE state = 'running' AND run_until IS NOT NULL AND run_until <= ?1
                 ORDER BY id",
                &[&now],
            )
        })
        .await?;
    for id in ids {
        let why = "the run passed the service's max run time".to_owned();
        if end(state, &id, State::TimedOut, why).await? {
            info!(request = %id, "service request timed out at its max run time");
            out.ended.push(id);
        }
    }
    Ok(())
}

async fn check_waiting(state: &Shared, out: &mut Tick) -> ApiResult<()> {
    let rows = state
        .db
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, reservation_id FROM service_requests
                 WHERE state = 'waiting' ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    for (id, reservation) in rows {
        let Some(rid) = reservation else {
            // No claim: a start that stopped between its two steps.
            if to_running(state, &id, None).await? {
                out.started.push(id);
            }
            continue;
        };
        let r = {
            let rid = rid.clone();
            state.db.read(move |c| res_store::get(c, &rid)).await
        };
        let r = match r {
            Ok(r) => r,
            Err(e) if e.code == Code::NotFound => {
                let why = format!("reservation `{rid}` is gone");
                if end(state, &id, State::Failed, why).await? {
                    out.ended.push(id);
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        match r.state {
            ResState::Active => {
                if to_running(state, &id, r.until).await? {
                    info!(request = %id, reservation = %rid, "service request runs");
                    out.started.push(id);
                }
            }
            ResState::Queued | ResState::Starting => {}
            ResState::TimedOut => {
                let why = "no resources within the wait limit".to_owned();
                if end(state, &id, State::TimedOut, why).await? {
                    out.ended.push(id);
                }
            }
            s => {
                let why = format!(
                    "reservation `{rid}` ended before the run ({})",
                    serde_json::to_value(s)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                );
                if end(state, &id, State::Failed, why).await? {
                    out.ended.push(id);
                }
            }
        }
    }
    Ok(())
}

/// `waiting -> running`, with `run_until`: the end of the reservation, or
/// now plus the `max_run_time` without one. `false`: another writer moved
/// the request first.
async fn to_running(state: &Shared, id: &str, until_ms: Option<i64>) -> ApiResult<bool> {
    let id = id.to_owned();
    state
        .db
        .write("system", move |t| {
            let cur = store::get_request(t, &id)?;
            if cur.state != State::Waiting {
                return Ok(false);
            }
            let svc = store::get(t, &cur.service)?;
            let run_until = until_ms
                .and_then(fmt_ms)
                .unwrap_or_else(|| fmt(add(Utc::now(), svc.def.max_run_time())));
            store::transition(t, &id, State::Waiting, State::Running, Outcome::default())?;
            t.execute(
                "UPDATE service_requests SET run_until = ?2 WHERE id = ?1",
                params![id, run_until],
            )?;
            Ok(true)
        })
        .await
}

async fn release_ended(state: &Shared, out: &mut Tick) -> ApiResult<()> {
    let rows = state
        .db
        .read(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT id FROM service_requests
                 WHERE released_at IS NULL AND state IN ({}) ORDER BY id",
                final_states()
            ))?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids.into_iter()
                .map(|id| store::get_request(c, &id))
                .collect::<ApiResult<Vec<_>>>()
        })
        .await?;
    for r in rows {
        let held = r.reservation_id.is_some() || r.lease_id.is_some();
        match release(state, &r).await {
            Ok(()) => {
                let id = r.id.clone();
                state
                    .db
                    .write("system", move |t| {
                        t.execute(
                            "UPDATE service_requests SET released_at = ?2 WHERE id = ?1",
                            params![id, crate::db::now()],
                        )?;
                        Ok(())
                    })
                    .await?;
                if held {
                    info!(request = %r.id, "service request released its reservation and lease");
                    out.released.push(r.id);
                }
            }
            // The next tick tries again (a busy host, a host offline).
            Err(e) => warn!(request = %r.id, "cannot release a service request: {}", e.message),
        }
    }
    Ok(())
}

fn final_states() -> String {
    State::ALL
        .iter()
        .filter(|s| s.is_final())
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Give back the reservation and the lease of ended request `r`.
async fn release(state: &Shared, r: &RequestRow) -> ApiResult<()> {
    if let Some(lid) = &r.lease_id {
        let lid = lid.clone();
        let lease = state
            .db
            .read(move |c| crate::store::leases::get(c, &lid))
            .await;
        match lease {
            Ok(l) if l.is_active() => {
                crate::leases::revoke(state, &Actor::Operator, &l.target, &l.agent).await?;
            }
            Ok(_) => {}
            Err(e) if e.code == Code::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if let Some(rid) = &r.reservation_id {
        let res = {
            let rid = rid.clone();
            state.db.read(move |c| res_store::get(c, &rid)).await
        };
        match res {
            Ok(x) if x.state == ResState::Queued => {
                crate::reservations::cancel(state, &Actor::Operator, rid).await?;
            }
            Ok(x) if matches!(x.state, ResState::Starting | ResState::Active) => {
                crate::reservations::retract(state, &Actor::Operator, rid).await?;
            }
            // Ending (the reservation sweep finishes it) or ended.
            Ok(_) => {}
            Err(e) if e.code == Code::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn start_queued(state: &Shared, at: DateTime<Utc>, out: &mut Tick) -> ApiResult<()> {
    let now = fmt(at);
    let plan = state
        .db
        .read(move |c| {
            let services = open_ids(
                c,
                "SELECT DISTINCT service FROM service_requests WHERE state = 'queued'
                 ORDER BY service",
                &[],
            )?;
            let mut plan = Vec::new();
            for name in services {
                let Some(svc) = store::find(c, &name)? else {
                    continue;
                };
                let mut ids = Vec::new();
                for id in store::startable(c, &name)? {
                    let r = store::get_request(c, &id)?;
                    // In order (Q3 A): a request that waits for its retry
                    // holds the ones behind it.
                    if retry_at(c, &id)?.is_some_and(|t| t > now) || r.wait_until <= now {
                        break;
                    }
                    ids.push(r);
                }
                plan.push((svc.def, ids));
            }
            Ok(plan)
        })
        .await?;
    for (def, rows) in plan {
        for r in rows {
            start_one(state, &def, &r, at, out).await?;
        }
    }
    Ok(())
}

fn retry_at(c: &Connection, id: &str) -> ApiResult<Option<String>> {
    Ok(c.query_row(
        "SELECT retry_at FROM service_requests WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?)
}

async fn start_one(
    state: &Shared,
    def: &ServiceDef,
    r: &RequestRow,
    at: DateTime<Utc>,
    out: &mut Tick,
) -> ApiResult<()> {
    if def.resources.is_empty() {
        let id = r.id.clone();
        let moved = state
            .db
            .write("system", move |t| {
                let cur = store::get_request(t, &id)?;
                if cur.state != State::Queued {
                    return Ok(false);
                }
                store::transition(t, &id, State::Queued, State::Waiting, Outcome::default())?;
                Ok(true)
            })
            .await?;
        if moved && to_running(state, &r.id, None).await? {
            out.started.push(r.id.clone());
        }
        return Ok(());
    }
    let body = match reservation_body(state, def, r, at).await {
        Ok(b) => b,
        Err(e) => return refused(state, r, e, at, out).await,
    };
    let host = body.0.clone();
    let granted = crate::reservations::grant(state, &Actor::Operator, &host, body.1).await;
    let res = match granted {
        Ok(res) => res,
        Err(e) => return refused(state, r, e, at, out).await,
    };
    // Keep the reservation on the request first: when another writer ended
    // the request meanwhile, the release gives it back.
    let (id, rid) = (r.id.clone(), res.id.clone());
    let moved = state
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE service_requests SET reservation_id = ?2, retry_at = NULL WHERE id = ?1",
                params![id, rid],
            )?;
            let cur = store::get_request(t, &id)?;
            if cur.state != State::Queued {
                return Ok(false);
            }
            store::transition(t, &id, State::Queued, State::Waiting, Outcome::default())?;
            Ok(true)
        })
        .await?;
    if !moved {
        return Ok(());
    }
    info!(request = %r.id, reservation = %res.id, state = ?res.state, "service request reserved");
    out.reserved.push(r.id.clone());
    if res.state == ResState::Active && to_running(state, &r.id, res.until).await? {
        out.started.push(r.id.clone());
    }
    Ok(())
}

/// A refused reservation: a `409` or `resource_busy` waits for
/// [`RETRY`]; anything else ends the request as `failed`.
async fn refused(
    state: &Shared,
    r: &RequestRow,
    e: ApiError,
    at: DateTime<Utc>,
    out: &mut Tick,
) -> ApiResult<()> {
    if matches!(e.code, Code::Conflict | Code::ResourceBusy) {
        debug!(request = %r.id, "service reservation refused for now: {}", e.message);
        let (id, next) = (r.id.clone(), fmt(add(at, RETRY)));
        state
            .db
            .write("system", move |t| {
                t.execute(
                    "UPDATE service_requests SET retry_at = ?2 WHERE id = ?1",
                    params![id, next],
                )?;
                Ok(())
            })
            .await?;
        out.deferred.push(r.id.clone());
        return Ok(());
    }
    let why = format!("the reservation was refused: {}", e.message);
    if end(state, &r.id, State::Failed, why).await? {
        info!(request = %r.id, "service request failed: {}", e.message);
        out.ended.push(r.id.clone());
    }
    Ok(())
}

/// The host and the reservation body of request `r`.
async fn reservation_body(
    state: &Shared,
    def: &ServiceDef,
    r: &RequestRow,
    at: DateTime<Utc>,
) -> ApiResult<(String, crate::reservations::NewBody)> {
    let wait_until = DateTime::parse_from_rfc3339(&r.wait_until)
        .map_err(|e| ApiError::internal(format!("bad wait_until: {e}")))?
        .with_timezone(&Utc);
    let left = (wait_until - at).num_seconds();
    if left <= 0 {
        return Err(ApiError::conflict("the wait limit is reached"));
    }
    let (target, res) = (def.target.clone(), def.resources.clone());
    let (host, gpus) = state
        .db
        .read(move |c| {
            let t = crate::store::targets::get(c, &target)?;
            let host = t.def.host.clone().ok_or_else(|| {
                ApiError::invalid(format!("target `{target}` has no host to reserve on"))
            })?;
            let gpus = pick_gpus(c, &host, &res)?;
            Ok((host, gpus))
        })
        .await?;
    let memory_bytes = match &def.resources.memory {
        Some(m) => {
            core::parse_size(m).ok_or_else(|| ApiError::invalid(format!("bad memory `{m}`")))?
        }
        None => 0,
    };
    let body = crate::reservations::NewBody {
        gpus,
        target: def.target.clone(),
        agent: None,
        amounts: Amounts {
            cpu_millis: def.resources.cpu_millis,
            memory_bytes,
            io: Default::default(),
        },
        until: fmt(add(at, def.max_run_time())),
        exclusive: false,
        reason: format!("service `{}` request {}", def.name, r.id),
        queue: true,
        queue_timeout: Some(format!("{left}s")),
    };
    Ok((host, body))
}

/// The GPUs of `host` for the claim: those that no open reservation
/// claims first, then the others (the reservation then queues for them).
/// Fewer GPUs known than claimed is `409`: the host may not have reported
/// them yet.
fn pick_gpus(
    c: &Connection,
    host: &str,
    res: &terrarium_core::service::Resources,
) -> ApiResult<Vec<GpuClaim>> {
    let n = usize::try_from(res.gpus).unwrap_or(usize::MAX);
    if n == 0 {
        return Ok(Vec::new());
    }
    let h = crate::store::hosts::get(c, host)?;
    let devices: Vec<GpuDevice> = serde_json::from_value(
        h.capabilities
            .get("gpus")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    )?;
    if devices.len() < n {
        return Err(ApiError::conflict(format!(
            "host `{host}` reports {} GPU(s), and the service claims {n}",
            devices.len()
        )));
    }
    let claimed: Vec<String> = res_store::list(c, host)?
        .into_iter()
        .filter(|x| {
            matches!(
                x.state,
                ResState::Queued | ResState::Starting | ResState::Active | ResState::Ending
            )
        })
        .flat_map(|x| x.gpus.into_iter().map(|g| g.uuid))
        .collect();
    let vram_bytes = if res.gpu_exclusive {
        0
    } else {
        res.gpu_vram
            .as_deref()
            .and_then(core::parse_size)
            .unwrap_or(0)
    };
    let (free, used): (Vec<_>, Vec<_>) = devices.iter().partition(|d| !claimed.contains(&d.uuid));
    Ok(free
        .into_iter()
        .chain(used)
        .take(n)
        .map(|d| GpuClaim {
            uuid: d.uuid.clone(),
            vram_bytes,
            exclusive: res.gpu_exclusive,
        })
        .collect())
}

// ------------------------------------------------------------ the lease

/// Lease the target of running request `id` to `agent` (the agent that
/// runs it, C4) until the request's `run_until`. The release revokes it.
pub async fn lease_run(
    state: &Shared,
    id: &str,
    agent: &str,
) -> ApiResult<crate::leases::LeaseOut> {
    let rid = id.to_owned();
    let (r, def) = state
        .db
        .read(move |c| {
            let r = store::get_request(c, &rid)?;
            let s = store::get(c, &r.service)?;
            Ok((r, s.def))
        })
        .await?;
    if r.state != State::Running {
        return Err(ApiError::conflict(format!(
            "service request `{id}` is {}, not running",
            r.state
        )));
    }
    if r.lease_id.is_some() {
        return Err(ApiError::conflict(format!(
            "service request `{id}` has a target lease already"
        )));
    }
    let lease = crate::leases::grant(
        state,
        &Actor::Operator,
        &def.target,
        crate::leases::GrantBody {
            agent: agent.to_owned(),
            until: r.run_until.clone(),
            exclusive: false,
            reason: Some(format!("service `{}` request {id}", def.name)),
        },
    )
    .await?;
    let (rid, lid) = (id.to_owned(), lease.lease.id.clone());
    state
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE service_requests SET lease_id = ?2 WHERE id = ?1",
                params![rid, lid],
            )?;
            Ok(())
        })
        .await?;
    Ok(lease)
}
