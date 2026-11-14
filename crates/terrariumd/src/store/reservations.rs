//! Durable resource admission and host FIFO. All changes run inside the
//! database write transaction. `starting` consumes capacity until enforcement
//! succeeds, and `ending` consumes it until sessions and limits are cleaned up.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::resources::{Amounts, HostCapacity, IoAmounts};

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Starting,
    Active,
    Ending,
    Expired,
    Retracted,
    Cancelled,
    TimedOut,
}

impl State {
    fn holds_capacity(self) -> bool {
        matches!(self, Self::Starting | Self::Active | Self::Ending)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Ending => "ending",
            Self::Expired => "expired",
            Self::Retracted => "retracted",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Holder {
    Agent(String),
    Target(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    #[serde(default)]
    pub gpus: Vec<terrarium_core::resources::GpuClaim>,
    #[serde(default)]
    pub gpu_warned: bool,
    #[serde(default)]
    pub gpu_below_since: Option<i64>,
    #[serde(default)]
    pub gpu_last_warning: Option<i64>,
    pub id: String,
    pub host: String,
    pub project: String,
    pub holder: Holder,
    /// The project target through which the holder uses this host.
    pub target: String,
    pub amounts: Amounts,
    pub exclusive: bool,
    pub cores: Vec<u32>,
    pub reason: String,
    pub granted_by: String,
    pub created_at: i64,
    pub start: Option<i64>,
    pub until: Option<i64>,
    pub duration_ms: i64,
    pub queue_until: Option<i64>,
    pub state: State,
    pub end_state: Option<State>,
    pub warned_15: bool,
    pub warned_5: bool,
}

/// A refused reservation, for a caller that tries again later (services,
/// TER-1265): `Busy` clears when other work on the host ends or the host
/// comes back; `Final` does not change by waiting.
#[derive(Debug, Clone)]
pub enum Refusal {
    Busy(ApiError),
    Final(ApiError),
}

impl Refusal {
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Busy(_))
    }

    pub fn into_error(self) -> ApiError {
        match self {
            Self::Busy(e) | Self::Final(e) => e,
        }
    }
}

/// An error of the store or of a check that waiting does not change.
impl From<ApiError> for Refusal {
    fn from(e: ApiError) -> Self {
        Self::Final(e)
    }
}

/// Already-authorized request. The API resolves agent/target IDs and checks
/// lease permissions before calling the store.
pub struct Request {
    pub gpus: Vec<terrarium_core::resources::GpuClaim>,
    pub host: String,
    pub project: String,
    pub holder: Holder,
    pub target: String,
    pub amounts: Amounts,
    pub exclusive: bool,
    pub reason: String,
    pub duration_ms: i64,
    pub queue_ms: Option<i64>,
}

pub fn list(conn: &Connection, host: &str) -> ApiResult<Vec<Reservation>> {
    let mut stmt = conn.prepare("SELECT data FROM reservations WHERE host=?1 ORDER BY seq")?;
    let rows = stmt
        .query_map([host], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|s| {
            serde_json::from_str(&s)
                .map_err(|e| ApiError::internal(format!("invalid stored reservation: {e}")))
        })
        .collect()
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Reservation> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM reservations WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    serde_json::from_str(
        &data.ok_or_else(|| ApiError::not_found(format!("no reservation `{id}`")))?,
    )
    .map_err(|e| ApiError::internal(format!("invalid stored reservation: {e}")))
}

/// Keep the identity needed to restore limits until cleanup has completed.
pub fn guard_identity(conn: &Connection, kind: &str, id: &str) -> ApiResult<()> {
    let mut stmt = conn.prepare(
        "SELECT id FROM reservations WHERE state IN ('queued','starting','active','ending')
         AND ((?1='target' AND json_extract(data,'$.target')=?2)
           OR (?1='agent' AND json_extract(data,'$.holder.kind')='agent'
               AND json_extract(data,'$.holder.id')=?2)) ORDER BY seq",
    )?;
    let ids = stmt
        .query_map(params![kind, id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(());
    }
    Err(ApiError::conflict("cancel or retract reservations and wait for cleanup before changing their target or holder")
        .with_details(json!({"reservations":ids})))
}

pub fn capacity(conn: &Connection, host: &str) -> ApiResult<(HostCapacity, Amounts, u8)> {
    let h = super::hosts::get(conn, host)?;
    let physical: HostCapacity =
        serde_json::from_value(h.capabilities.get("capacity").cloned().unwrap_or_default())
            .map_err(|_| ApiError::conflict(format!("host `{host}` has not reported capacity")))?;
    let config: Option<(u8, String)> = conn
        .query_row(
            "SELECT percentage,io FROM resource_budgets WHERE host=?1",
            [host],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (percent, io) = config.unwrap_or((0, "{}".into()));
    let mut amounts = physical.amounts();
    amounts.io = serde_json::from_str(&io).map_err(|e| ApiError::internal(e.to_string()))?;
    let budget = amounts.reservable(percent).map_err(ApiError::invalid)?;
    Ok((physical, budget, percent))
}

pub fn configure(
    t: &mut Tx<'_>,
    host: &str,
    percentage: u8,
    io: BTreeMap<String, IoAmounts>,
) -> ApiResult<()> {
    super::hosts::get(t, host)?;
    let amounts = Amounts {
        io: io.clone(),
        ..Default::default()
    };
    amounts.reservable(percentage).map_err(ApiError::invalid)?;
    t.execute(
        "INSERT INTO resource_budgets(host,percentage,io) VALUES (?1,?2,?3)
        ON CONFLICT(host) DO UPDATE SET percentage=excluded.percentage,io=excluded.io",
        params![host, percentage, serde_json::to_string(&io)?],
    )?;
    // Changing a budget may prevent new admissions but never kills existing
    // reservations implicitly. They remain charged until explicitly ended.
    t.audit(
        "reserve.capacity",
        host,
        None,
        json!({"percentage":percentage,"io":io}),
    )?;
    Ok(())
}

fn save(t: &mut Tx<'_>, r: &Reservation, change: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE reservations SET state=?2,data=?3 WHERE id=?1",
        params![r.id, r.state.name(), serde_json::to_string(r)?],
    )?;
    t.audit(
        &format!("reserve.{change}"),
        &r.id,
        Some(&r.project),
        json!(r),
    )?;
    let mut event = EventDraft::new(
        "reservation",
        format!("reservation {} {change} on {}", r.id, r.host),
        json!({"change":change,"reservation":r}),
    )
    .project(r.project.clone());
    if let Holder::Agent(id) = &r.holder {
        let name = super::agents::display_name(t, id)?.unwrap_or_else(|| id.clone());
        event = event.agent(id.clone(), name);
    }
    t.emit(event)?;
    super::waits::check_reservation_waits(t)?;
    if !matches!(change, "starting" | "ending") {
        notify(t, r, change)?;
    }
    Ok(())
}

fn notify(t: &mut Tx<'_>, r: &Reservation, change: &str) -> ApiResult<()> {
    use super::mail::{self, Address, Outgoing};
    let mut ids = Vec::new();
    match &r.holder {
        Holder::Agent(id) => ids.push(id.clone()),
        Holder::Target(target) => {
            let mut stmt = t.prepare("SELECT id, policy FROM agents")?;
            let agents = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (id, policy) in agents {
                let p: terrarium_core::config::Policy = serde_json::from_str(&policy)?;
                if p.targets.contains(target) {
                    ids.push(id);
                }
            }
            for lease in super::leases::active_on(t, target)? {
                ids.push(lease.agent);
            }
        }
    }
    if change.starts_with("gpu_") {
        let mut stmt = t.prepare("SELECT a.id FROM agents a JOIN roles r ON r.agent_id=a.id WHERE a.project=?1 AND r.role='project_admin'")?;
        ids.extend(
            stmt.query_map([&r.project], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    ids.sort();
    ids.dedup();
    let end = r
        .until
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| "not started".into());
    let line = if change == "gpu_overage" {
        format!(
            "Reservation {} on {}: measured holder compute VRAM exceeds a cooperative shared GPU share. No process was killed. Reduce GPU memory use or ask the lead to change the reservation.",
            r.id, r.host
        )
    } else if change == "warning" {
        let minutes = if r.warned_5 { 5 } else { 15 };
        format!(
            "Reservation {} on {} ends within {minutes} minutes, at {end}. Ask your lead to run `terrarium reserve extend {} --until <duration>` before it ends. Sessions end with reservation_ended at expiry.",
            r.id, r.host, r.id
        )
    } else {
        format!(
            "Reservation {} on {} {change}. State: {}. Until: {end}. Run `terrarium reserve ls --host {}`.",
            r.id,
            r.host,
            r.state.name(),
            r.host
        )
    };
    let subject = format!("Reservation {}: {change}", r.id);
    for id in ids {
        let Ok(a) = super::agents::get(t, &id) else {
            continue;
        };
        let to = Address::Agent {
            id: id.clone(),
            name: a.agent.name.clone(),
            project: a.agent.project.clone(),
        };
        mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: &to,
                subject: &subject,
                body: &line,
                thread: None,
                in_reply_to: None,
            },
        )?;
        t.emit(
            EventDraft::new(
                "reservation_pointer",
                line.clone(),
                json!({"reservation_id":r.id,"line":line}),
            )
            .agent(id, a.agent.name)
            .project(a.agent.project),
        )?;
    }
    Ok(())
}

fn allocation(
    conn: &Connection,
    request: &Reservation,
    existing: &[Reservation],
) -> ApiResult<Vec<u32>> {
    let (physical, mut remaining, _) = capacity(conn, &request.host)?;
    let held: Vec<_> = existing
        .iter()
        .filter(|r| r.id != request.id && r.state.holds_capacity())
        .collect();
    let conflict = || {
        ApiError::conflict(format!("reservable capacity on `{}` is exhausted", request.host))
        .with_details(json!({"host":request.host,"holders":held.iter().map(|r| json!({"holder":r.holder,"until":r.until,"reservation":r.id})).collect::<Vec<_>>()}))
    };
    let host = super::hosts::get(conn, &request.host)?;
    let devices: Vec<terrarium_core::resources::GpuDevice> = serde_json::from_value(
        host.capabilities
            .get("gpus")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )?;
    let mut seen = std::collections::BTreeSet::new();
    for claim in &request.gpus {
        terrarium_core::resources::validate_gpu_id(&claim.uuid).map_err(ApiError::invalid)?;
        if !seen.insert(&claim.uuid) {
            return Err(ApiError::invalid("duplicate GPU claim"));
        }
        let device = devices
            .iter()
            .find(|d| d.uuid == claim.uuid)
            .ok_or_else(|| ApiError::conflict("GPU is absent from host inventory"))?;
        if !claim.exclusive && (claim.vram_bytes == 0 || claim.vram_bytes > device.memory_bytes) {
            return Err(ApiError::invalid(
                "shared GPU needs a positive VRAM share no larger than device capacity",
            ));
        }
        if capacity(conn, &request.host)?.2 == 0 {
            return Err(conflict());
        }
        let mut available = device.memory_bytes;
        for other in held
            .iter()
            .flat_map(|r| &r.gpus)
            .filter(|g| g.uuid == claim.uuid)
        {
            if claim.exclusive || other.exclusive {
                return Err(conflict());
            }
            available = available
                .checked_sub(other.vram_bytes)
                .ok_or_else(&conflict)?;
        }
        if !claim.exclusive && claim.vram_bytes > available {
            return Err(conflict());
        }
    }
    for r in &held {
        remaining = remaining
            .remaining_after(&r.amounts)
            .ok_or_else(&conflict)?;
    }
    remaining
        .remaining_after(&request.amounts)
        .ok_or_else(&conflict)?;
    if !request.exclusive {
        return Ok(Vec::new());
    }
    if request.amounts.cpu_millis == 0 || !request.amounts.cpu_millis.is_multiple_of(1000) {
        return Err(ApiError::invalid(
            "exclusive CPU reservations need a positive whole number of cores",
        ));
    }
    let needed = usize::try_from(request.amounts.cpu_millis / 1000).map_err(|_| conflict())?;
    let mut available = physical.cpu_ids;
    available.sort_unstable();
    available.dedup();
    if needed >= available.len() {
        return Err(ApiError::invalid(
            "exclusive reservations must leave a CPU for shared managed workloads",
        ));
    }
    available.retain(|cpu| !held.iter().any(|r| r.cores.contains(cpu)));
    if available.len() <= needed {
        return Err(conflict());
    }
    available.truncate(needed);
    Ok(available)
}

pub fn request(t: &mut Tx<'_>, req: Request, at: i64) -> ApiResult<Reservation> {
    request_deferred(t, req, at, false)
}

/// The reservation that [`request_deferred`] would store for `req`, with
/// the checks that come before its admission: the amounts, the total
/// budget of the host, and one reservation per holder and host (busy).
/// It writes nothing.
pub fn candidate(
    conn: &Connection,
    req: Request,
    at: i64,
    granted_by: &str,
) -> Result<Reservation, Refusal> {
    // TER-623: a release target is used by the release job only.
    crate::store::jobs::check_not_release_target(
        conn,
        &req.target,
        "reserving through the target",
    )?;
    req.amounts.validate().map_err(ApiError::invalid)?;
    if (req.amounts.is_empty() && req.gpus.is_empty())
        || req.duration_ms <= 0
        || req.queue_ms.is_some_and(|q| q <= 0)
    {
        return Err(ApiError::invalid("give nonzero amounts and positive duration/queue timeout").into());
    }
    let until = at
        .checked_add(req.duration_ms)
        .ok_or_else(|| ApiError::invalid("duration overflow"))?;
    let queue_until = req
        .queue_ms
        .map(|q| {
            at.checked_add(q)
                .ok_or_else(|| ApiError::invalid("queue timeout overflow"))
        })
        .transpose()?;
    let mut r = Reservation {
        gpus: req.gpus,
        gpu_warned: false,
        gpu_below_since: None,
        gpu_last_warning: None,
        id: ulid::Ulid::new().to_string(),
        host: req.host,
        project: req.project,
        holder: req.holder,
        target: req.target,
        amounts: req.amounts,
        exclusive: req.exclusive,
        cores: vec![],
        reason: req.reason,
        granted_by: granted_by.to_owned(),
        created_at: at,
        start: Some(at),
        until: Some(until),
        duration_ms: req.duration_ms,
        queue_until,
        state: State::Starting,
        end_state: None,
        warned_15: false,
        warned_5: false,
    };
    // Even queued requests must fit the total budget. Impossible requests
    // cannot occupy the head and starve the entire FIFO.
    allocation(conn, &r, &[])?;
    let existing = list(conn, &r.host)?;
    if existing
        .iter()
        .any(|x| x.holder == r.holder && (x.state.holds_capacity() || x.state == State::Queued))
    {
        return Err(Refusal::Busy(ApiError::conflict(
            "holder already has a reservation on this host; extend or retract it",
        )));
    }
    Ok(r)
}

pub fn request_deferred(
    t: &mut Tx<'_>,
    req: Request,
    at: i64,
    deferred: bool,
) -> ApiResult<Reservation> {
    let actor = t.actor.clone();
    let mut r = candidate(t, req, at, &actor).map_err(Refusal::into_error)?;
    let queue_until = r.queue_until;
    let existing = list(t, &r.host)?;
    let admission = if deferred {
        Err(ApiError::resource_busy(
            "host lifecycle verbs are in progress",
        ))
    } else if existing.iter().any(|x| x.state == State::Queued) {
        Err(ApiError::conflict(
            "host reservation FIFO has waiting holders",
        ))
    } else {
        allocation(t, &r, &existing)
    };
    match admission {
        Ok(cores) => r.cores = cores,
        Err(_) if queue_until.is_some() => {
            r.state = State::Queued;
            r.start = None;
            r.until = None;
        }
        Err(e) => return Err(e),
    }
    t.execute(
        "INSERT INTO reservations(id,host,project,state,data) VALUES (?1,?2,?3,?4,?5)",
        params![
            r.id,
            r.host,
            r.project,
            r.state.name(),
            serde_json::to_string(&r)?
        ],
    )?;
    save(
        t,
        &r,
        if r.state == State::Queued {
            "queued"
        } else {
            "starting"
        },
    )?;
    Ok(r)
}

/// Enforced starts become visible as active only after the host acknowledges.
pub fn activate(t: &mut Tx<'_>, id: &str) -> ApiResult<Reservation> {
    let mut r = get(t, id)?;
    if r.state != State::Starting {
        return Err(ApiError::conflict("reservation is not starting"));
    }
    r.state = State::Active;
    save(t, &r, "granted")?;
    Ok(r)
}

pub fn extend(t: &mut Tx<'_>, id: &str, until: i64, at: i64) -> ApiResult<Reservation> {
    let mut r = get(t, id)?;
    if r.state != State::Active || r.until.is_none_or(|u| u <= at) {
        return Err(ApiError::conflict(
            "only an active, unexpired reservation can be extended",
        ));
    }
    if until <= r.until.unwrap_or(at) {
        return Err(ApiError::invalid(
            "new until must be later than the current until",
        ));
    }
    r.until = Some(until);
    r.warned_15 = false;
    r.warned_5 = false;
    save(t, &r, "extended")?;
    Ok(r)
}

pub fn end(t: &mut Tx<'_>, id: &str, expired: bool) -> ApiResult<Reservation> {
    let mut r = get(t, id)?;
    if !matches!(r.state, State::Starting | State::Active) {
        return Err(ApiError::conflict("reservation is not active or starting"));
    }
    r.state = State::Ending;
    r.end_state = Some(if expired {
        State::Expired
    } else {
        State::Retracted
    });
    save(t, &r, "ending")?;
    Ok(r)
}

/// Release capacity only after cleanup, so a slow host cannot overlap budgets.
pub fn finish_end(t: &mut Tx<'_>, id: &str) -> ApiResult<Reservation> {
    let mut r = get(t, id)?;
    if r.state != State::Ending {
        return Err(ApiError::conflict("reservation is not ending"));
    }
    r.state = r
        .end_state
        .ok_or_else(|| ApiError::internal("missing reservation end state"))?;
    save(t, &r, "ended")?;
    Ok(r)
}

pub fn cancel(t: &mut Tx<'_>, id: &str) -> ApiResult<Reservation> {
    let mut r = get(t, id)?;
    if r.state != State::Queued {
        return Err(ApiError::conflict(
            "only a queued reservation can be cancelled",
        ));
    }
    r.state = State::Cancelled;
    save(t, &r, "cancelled")?;
    Ok(r)
}

/// Expire queue entries, then admit at most the head. No backfilling past a
/// request that cannot fit. The caller enforces and activates the returned row.
pub fn advance(t: &mut Tx<'_>, host: &str, at: i64) -> ApiResult<Option<Reservation>> {
    let mut rows = list(t, host)?;
    for r in &mut rows {
        if r.state == State::Queued && r.queue_until.is_some_and(|u| u <= at) {
            r.state = State::TimedOut;
            save(t, r, "timed_out")?;
        }
    }
    let Some(head) = rows.iter().find(|r| r.state == State::Queued) else {
        return Ok(None);
    };
    let mut r = head.clone();
    let cores = match allocation(t, &r, &rows) {
        Ok(c) => c,
        Err(e) if e.code == crate::error::Code::Conflict => return Ok(None),
        Err(e) => return Err(e),
    };
    r.cores = cores;
    r.state = State::Starting;
    r.start = Some(at);
    r.until = Some(
        at.checked_add(r.duration_ms)
            .ok_or_else(|| ApiError::invalid("duration overflow"))?,
    );
    save(t, &r, "starting")?;
    Ok(Some(r))
}

/// Persist warning marks in the same transaction as the caller's pointer and
/// mail. A late sweep sends only the most urgent warning, not both at once.
pub fn warnings(t: &mut Tx<'_>, host: &str, at: i64) -> ApiResult<Vec<(Reservation, u8)>> {
    let mut due = Vec::new();
    for mut r in list(t, host)? {
        if r.state != State::Active {
            continue;
        }
        let Some(left) = r.until.and_then(|u| u.checked_sub(at)) else {
            continue;
        };
        if left <= 0 {
            continue;
        }
        let minutes = if left <= 300_000 && !r.warned_5 {
            r.warned_5 = true;
            r.warned_15 = true;
            5
        } else if left <= 900_000 && !r.warned_15 {
            r.warned_15 = true;
            15
        } else {
            continue;
        };
        save(t, &r, "warning")?;
        due.push((r, minutes));
    }
    Ok(due)
}

/// Warning only: a shared allocation is cooperative, never a kill threshold.
pub fn gpu_usage(t: &mut Tx<'_>, id: &str, over: bool, at: i64) -> ApiResult<()> {
    let mut r = get(t, id)?;
    if r.state != State::Active {
        return Ok(());
    }
    let mut warn = false;
    if over {
        r.gpu_below_since = None;
        if !r.gpu_warned
            && r.gpu_last_warning
                .is_none_or(|last| at.saturating_sub(last) >= 3_600_000)
        {
            r.gpu_warned = true;
            r.gpu_last_warning = Some(at);
            warn = true;
        }
    } else if r.gpu_warned {
        let since = *r.gpu_below_since.get_or_insert(at);
        if at.saturating_sub(since) >= 600_000 {
            r.gpu_warned = false;
        }
    }
    if warn {
        save(t, &r, "gpu_overage")
    } else {
        t.execute(
            "UPDATE reservations SET data=?2 WHERE id=?1",
            params![r.id, serde_json::to_string(&r)?],
        )?;
        Ok(())
    }
}
