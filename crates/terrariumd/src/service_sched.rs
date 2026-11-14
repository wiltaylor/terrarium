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
//! A reservation refusal that waiting does not change (a missing host
//! capability, a claim larger than the host, any refusal of the
//! reservation's preflight) ends the request as `failed`; [`precheck`]
//! finds the busy cases before a row is written. A reservation that no
//! request holds (the daemon stopped between the grant and its write) is
//! given back by [`release_orphans`].
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
    /// Reservations of the scheduler that no request held, given back.
    pub orphans: Vec<String>,
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
    release_orphans(state, &mut out).await?;
    Ok(out)
}

/// The open reservations that the scheduler made (its reason names a
/// request) and that no request holds: the daemon stopped between the
/// grant and the write of `reservation_id`. A grant and its write run in
/// one pass under [`TICK_LOCK`], so at this point no grant is half done.
async fn release_orphans(state: &Shared, out: &mut Tick) -> ApiResult<()> {
    let ids = state
        .db
        .read(|c| {
            open_ids(
                c,
                "SELECT id FROM reservations
                 WHERE state IN ('queued', 'starting', 'active')
                   AND json_extract(data, '$.granted_by') = 'operator'
                   AND json_extract(data, '$.holder.kind') = 'target'
                   AND json_extract(data, '$.reason') LIKE 'service `%` request %'
                   AND id NOT IN (SELECT reservation_id FROM service_requests
                                  WHERE reservation_id IS NOT NULL)
                 ORDER BY id",
                &[],
            )
        })
        .await?;
    for id in ids {
        let rid = id.clone();
        let r = state.db.read(move |c| res_store::get(c, &rid)).await?;
        let done = match r.state {
            ResState::Queued => crate::reservations::cancel(state, &Actor::Operator, &id).await,
            _ => crate::reservations::retract(state, &Actor::Operator, &id).await,
        };
        match done {
            Ok(_) => {
                info!(reservation = %id, "released a service reservation that no request holds");
                out.orphans.push(id);
            }
            Err(e) => {
                warn!(reservation = %id, "cannot release an orphan service reservation: {}", e.message)
            }
        }
    }
    Ok(())
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
    let (host, body) = match reservation_body(state, def, r, at).await {
        Ok(b) => b,
        Err(e) => return refused(state, r, Refusal::of(e), at, out).await,
    };
    if let Err(why) = precheck(state, def, &host).await {
        return refused(state, r, why, at, out).await;
    }
    let reason = body.reason.clone();
    let before = rows_with_reason(state, &host, &reason).await?;
    let granted = crate::reservations::grant(state, &Actor::Operator, &host, body).await;
    let res = match granted {
        Ok(res) => res,
        Err(e) => {
            // A refusal after the row was written comes from the
            // reservation's preflight. The pre-check above lets through
            // only what waiting does not change, so it is final: one row
            // and one round of mail per request at most.
            let made = rows_with_reason(state, &host, &reason).await? > before;
            let why = if made {
                Refusal::Final(e.message)
            } else {
                Refusal::of(e)
            };
            return refused(state, r, why, at, out).await;
        }
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

/// Why a reservation was not made.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    /// The resources or the host are busy for now: try again after
    /// [`RETRY`]. No reservation row was written.
    Busy(String),
    /// Waiting does not change it: the request fails.
    Final(String),
}

impl Refusal {
    /// A `409` or `resource_busy` before any row was written is busy;
    /// anything else is final.
    fn of(e: ApiError) -> Self {
        if matches!(e.code, Code::Conflict | Code::ResourceBusy) {
            Self::Busy(e.message)
        } else {
            Self::Final(e.message)
        }
    }
}

/// A refused reservation: [`Refusal::Busy`] waits for [`RETRY`];
/// [`Refusal::Final`] ends the request as `failed` with the reason.
async fn refused(
    state: &Shared,
    r: &RequestRow,
    why: Refusal,
    at: DateTime<Utc>,
    out: &mut Tick,
) -> ApiResult<()> {
    let message = match why {
        Refusal::Busy(m) => {
            debug!(request = %r.id, "service reservation refused for now: {m}");
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
        Refusal::Final(m) => m,
    };
    let why = format!("the reservation was refused: {message}");
    if end(state, &r.id, State::Failed, why).await? {
        info!(request = %r.id, "service request failed: {message}");
        out.ended.push(r.id.clone());
    }
    Ok(())
}

/// The reservation rows on `host` made for one request (by their reason).
async fn rows_with_reason(state: &Shared, host: &str, reason: &str) -> ApiResult<i64> {
    let (host, reason) = (host.to_owned(), reason.to_owned());
    state
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM reservations
                 WHERE host = ?1 AND json_extract(data, '$.reason') = ?2",
                params![host, reason],
                |r| r.get(0),
            )?)
        })
        .await
}

/// The reason of the reservation of request `id`. The orphan sweep reads
/// the request ID back from it.
fn reason_of(service: &str, id: &str) -> String {
    format!("service `{service}` request {id}")
}

/// The checks of `crate::reservations` (its `preflight`) that run after
/// the row is written, done before the grant: the ones that waiting does
/// not change fail the request; the busy ones wait without a row.
async fn precheck(state: &Shared, def: &ServiceDef, host: &str) -> Result<(), Refusal> {
    use terrarium_core::config::TargetKind;
    let Some(caps) = state.executor.hosts.capabilities(host) else {
        return Err(Refusal::Busy(format!("host `{host}` is offline")));
    };
    if !caps.reservations {
        return Err(Refusal::Final(format!(
            "host `{host}` is missing the reservations capability; upgrade its host agent"
        )));
    }
    let res = &def.resources;
    let n = usize::try_from(res.gpus).unwrap_or(usize::MAX);
    if n > 0 && !caps.gpu_reservations {
        return Err(Refusal::Final(format!(
            "host `{host}` lacks GPU reservation support"
        )));
    }
    if caps.gpus.len() < n {
        return Err(Refusal::Final(format!(
            "host `{host}` has {} GPU(s), and the service claims {n}",
            caps.gpus.len()
        )));
    }
    if !res.gpu_exclusive
        && let Some(v) = res.gpu_vram.as_deref().and_then(core::parse_size)
        && caps.gpus.iter().filter(|g| g.memory_bytes >= v).count() < n
    {
        return Err(Refusal::Final(format!(
            "host `{host}` has fewer than {n} GPU(s) with {v} bytes of memory"
        )));
    }
    let amounts = claim_amounts(def).map_err(|e| Refusal::Final(e.message))?;
    let (target, name, h) = (def.target.clone(), host.to_owned(), host.to_owned());
    let read = state
        .db
        .read(move |c| {
            let t = crate::store::targets::get(c, &target)?;
            let others: Vec<_> = crate::store::targets::list(c)?
                .into_iter()
                .filter(|o| o.def.host.as_deref() == Some(&h) && o.def.name != target)
                .collect();
            let budget = res_store::capacity(c, &h).map(|(_, b, _)| b);
            let agent_held = res_store::list(c, &h)?.iter().any(|x| {
                matches!(x.holder, res_store::Holder::Agent(_))
                    && matches!(
                        x.state,
                        ResState::Starting | ResState::Active | ResState::Ending
                    )
            });
            Ok((t, others, budget, agent_held))
        })
        .await
        .map_err(|e| Refusal::Final(e.message))?;
    let (target, others, budget, agent_held) = read;
    match budget {
        Ok(b) if b.remaining_after(&amounts).is_none() => {
            return Err(Refusal::Final(format!(
                "the claim is larger than the reservable capacity of host `{name}`"
            )));
        }
        Ok(_) => {}
        Err(e) => return Err(Refusal::Busy(e.message)),
    }
    let limits = terrarium_core::resources::RuntimeLimits {
        limits: terrarium_core::limits::Limits {
            cpu_millis: u32::try_from(amounts.cpu_millis).ok().filter(|c| *c > 0),
            memory_bytes: (amounts.memory_bytes > 0).then_some(amounts.memory_bytes),
            ..Default::default()
        },
        ..Default::default()
    };
    let support = if target.def.kind == TargetKind::Docker {
        limits.limits.docker_support(caps.limits.as_ref())
    } else {
        limits.host_support(caps.limits.as_ref())
    };
    if let Err(e) = support {
        return Err(Refusal::Final(format!(
            "host `{name}` cannot apply the claim: {e}"
        )));
    }
    // Busy: these change when other work on the host ends.
    if agent_held {
        return Err(Refusal::Busy(format!(
            "an agent holds a reservation on host `{name}`"
        )));
    }
    if n > 0 && target.def.kind == TargetKind::Docker && target.state == "running" {
        return Err(Refusal::Busy(format!(
            "target `{}` runs; a GPU reservation needs it stopped",
            target.def.name
        )));
    }
    if res.gpu_exclusive {
        let busy: Vec<String> = others
            .iter()
            .filter(|o| o.def.kind != TargetKind::Host && o.state == "running")
            .map(|o| o.def.name.clone())
            .collect();
        if !busy.is_empty() {
            return Err(Refusal::Busy(format!(
                "an exclusive GPU waits for these targets to stop: {}",
                busy.join(", ")
            )));
        }
    }
    if target.def.kind == TargetKind::Host
        && !state
            .executor
            .resource_sessions(std::slice::from_ref(&target.def.name), None)
            .is_empty()
    {
        return Err(Refusal::Busy(format!(
            "target `{}` has sessions that are not in a reservation",
            target.def.name
        )));
    }
    Ok(())
}

/// The CPU and memory of the claim.
fn claim_amounts(def: &ServiceDef) -> ApiResult<Amounts> {
    let memory_bytes = match &def.resources.memory {
        Some(m) => {
            core::parse_size(m).ok_or_else(|| ApiError::invalid(format!("bad memory `{m}`")))?
        }
        None => 0,
    };
    Ok(Amounts {
        cpu_millis: def.resources.cpu_millis,
        memory_bytes,
        io: Default::default(),
    })
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
    let body = crate::reservations::NewBody {
        gpus,
        target: def.target.clone(),
        agent: None,
        amounts: claim_amounts(def)?,
        until: fmt(add(at, def.max_run_time())),
        exclusive: false,
        reason: reason_of(&def.name, &r.id),
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
    if devices.is_empty() {
        return Err(ApiError::conflict(format!(
            "host `{host}` has not reported its GPUs"
        )));
    }
    if devices.len() < n {
        return Err(ApiError::invalid(format!(
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
            reason: Some(reason_of(&def.name, id)),
        },
    )
    .await?;
    // The check and the write in one transaction: a second call, or an end
    // of the request since the read above, gives the new lease back.
    let (rid, lid) = (id.to_owned(), lease.lease.id.clone());
    let kept = state
        .db
        .write("system", move |t| {
            let cur = store::get_request(t, &rid)?;
            if cur.state != State::Running || cur.lease_id.is_some() {
                return Ok(Err(format!(
                    "service request `{rid}` is {}{}",
                    cur.state,
                    if cur.lease_id.is_some() {
                        " and has a target lease already"
                    } else {
                        ", not running"
                    }
                )));
            }
            t.execute(
                "UPDATE service_requests SET lease_id = ?2 WHERE id = ?1",
                params![rid, lid],
            )?;
            Ok(Ok(()))
        })
        .await?;
    if let Err(why) = kept {
        crate::leases::revoke(
            state,
            &Actor::Operator,
            &lease.lease.target,
            &lease.lease.agent,
        )
        .await?;
        return Err(ApiError::conflict(why));
    }
    Ok(lease)
}
