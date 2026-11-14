//! Services C1 (TER-1264, design D1-D3): the definitions and the request
//! queue. No scheduling and no run here: C2 takes requests from
//! [`startable`] and moves them with [`transition`]; C3 and C4 run them;
//! C5 mails the result.
//!
//! - **Definitions.** `services/<name>.toml` files of the config repo
//!   (source `file`, [`sync_files`]) and `POST /api/services` (source
//!   `api`), as tool proxies. [`validate`] adds to `ServiceDef::check` the
//!   checks that need the store: the project, the target and its scope,
//!   and the exclusive GPU rule (MEM-399: only on a docker target).
//! - **Requests.** `service_requests`, one row per call, in arrival order
//!   (the ULID). The states and their edges are `terrarium_core::service::State`.
//! - **Payloads.** Files in `<state>/services/payloads/<id>/`
//!   ([`payload_dir`]). The caps are checked by [`check_payload_room`];
//!   [`expired_payloads`] lists those past the retention.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;
use terrarium_core::config::TargetKind;
use terrarium_core::service::{self as core, ServiceDef, State};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::store::projects::{SOURCE_API, SOURCE_FILE};
use crate::store::sync::Changes;

/// A stored service.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServiceRow {
    #[serde(flatten)]
    pub def: ServiceDef,
    /// `global` or `project`.
    pub scope: &'static str,
    /// `file` or `api`.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

const COLS: &str = "name, def, source, created_at, updated_at";

fn row(r: &Row<'_>) -> rusqlite::Result<ServiceRow> {
    let name: String = r.get(0)?;
    let def: String = r.get(1)?;
    let mut def: ServiceDef = serde_json::from_str(&def).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    def.name = name;
    Ok(ServiceRow {
        scope: if def.project.is_some() {
            "project"
        } else {
            "global"
        },
        def,
        source: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

/// The checks that need the store.
pub fn validate(conn: &Connection, def: &ServiceDef) -> ApiResult<()> {
    def.check().map_err(ApiError::invalid)?;
    if let Some(p) = &def.project {
        crate::store::projects::get(conn, p)?;
    }
    let target = crate::store::targets::find(conn, &def.target)?.ok_or_else(|| {
        ApiError::invalid(format!(
            "service `{}`: no target `{}`",
            def.name, def.target
        ))
    })?;
    crate::store::jobs::check_not_release_target(
        conn,
        &def.target,
        &format!("service `{}` on the target", def.name),
    )?;
    if let Some(p) = &def.project
        && target.def.project.as_deref() != Some(p.as_str())
    {
        return Err(ApiError::invalid(format!(
            "project service `{}` names target `{}`, which is not a target of project `{p}`",
            def.name, def.target
        )));
    }
    // MEM-399: an exclusive GPU is enforced on a docker target. A host
    // target is refused here even when its host has the native helper
    // (TER-345): the host may be offline when the service is defined.
    if def.resources.gpu_exclusive && target.def.kind != TargetKind::Docker {
        return Err(ApiError::invalid(format!(
            "service `{}` claims an exclusive GPU, and target `{}` is not a docker target: \
             an exclusive GPU is enforced only on a docker target",
            def.name, def.target
        )));
    }
    Ok(())
}

fn def_json(def: &ServiceDef) -> ApiResult<String> {
    let mut v = serde_json::to_value(def)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("name");
    }
    Ok(serde_json::to_string(&v)?)
}

pub fn list(conn: &Connection) -> ApiResult<Vec<ServiceRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM services ORDER BY name"))?;
    let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn find(conn: &Connection, name: &str) -> ApiResult<Option<ServiceRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM services WHERE name = ?1"),
            [name],
            row,
        )
        .optional()?)
}

pub fn get(conn: &Connection, name: &str) -> ApiResult<ServiceRow> {
    find(conn, name)?.ok_or_else(|| ApiError::not_found(format!("no service `{name}`")))
}

fn insert(t: &mut Tx<'_>, def: &ServiceDef, source: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO services (name, project, def, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![def.name, def.project, def_json(def)?, source, now()],
    )?;
    Ok(())
}

/// `POST /api/services`.
pub fn create(t: &mut Tx<'_>, def: &ServiceDef) -> ApiResult<ServiceRow> {
    validate(t, def)?;
    if find(t, &def.name)?.is_some() {
        return Err(ApiError::conflict(format!("service `{}` exists", def.name)));
    }
    insert(t, def, SOURCE_API)?;
    get(t, &def.name)
}

fn refuse_file(r: &ServiceRow) -> ApiResult<()> {
    if r.source == SOURCE_FILE {
        return Err(ApiError::new(
            Code::FileManaged,
            format!(
                "service `{}` is declared in services/{}.toml; edit the file, then POST /api/reload",
                r.def.name, r.def.name
            ),
        ));
    }
    Ok(())
}

/// `PATCH /api/services/{name}`: a JSON merge patch. The name and the
/// project do not change.
pub fn update(t: &mut Tx<'_>, name: &str, patch: &Value) -> ApiResult<ServiceRow> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    let Some(p) = patch.as_object() else {
        return Err(ApiError::bad_request("the body must be a JSON object"));
    };
    let mut v = serde_json::to_value(&current.def)?;
    let Some(o) = v.as_object_mut() else {
        return Err(ApiError::internal("a service is not an object"));
    };
    for (k, val) in p {
        if k == "name" && val.as_str() != Some(name) {
            return Err(ApiError::invalid("a service cannot be renamed"));
        }
        if k == "project" && val.as_str() != current.def.project.as_deref() {
            return Err(ApiError::invalid(
                "the project of a service does not change: delete it and define it again",
            ));
        }
        if val.is_null() {
            o.remove(k);
        } else {
            o.insert(k.clone(), val.clone());
        }
    }
    let mut def: ServiceDef = serde_json::from_value(v)
        .map_err(|e| ApiError::bad_request(format!("bad service field: {e}")))?;
    def.name = name.to_owned();
    validate(t, &def)?;
    t.execute(
        "UPDATE services SET def = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, def_json(&def)?, now()],
    )?;
    get(t, name)
}

/// The requests of `service` that have not ended.
pub fn open_requests(conn: &Connection, service: &str) -> ApiResult<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM service_requests
         WHERE service = ?1 AND state IN ('queued', 'waiting', 'running')",
        [service],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// `DELETE /api/services/{name}`. Refused while a request is open: cancel
/// them first. The ended requests stay until the retention sweep.
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<ServiceRow> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    let open = open_requests(t, name)?;
    if open > 0 {
        return Err(ApiError::conflict(format!(
            "service `{name}` has {open} open request(s): cancel them first"
        )));
    }
    t.execute("DELETE FROM services WHERE name = ?1", [name])?;
    Ok(current)
}

/// The `services/<name>.toml` files: add, change, and remove the `file`
/// rows, as `store::tools::sync_files`. A file that fails a check is left
/// out with an error line.
pub fn sync_files(t: &mut Tx<'_>, defs: &[ServiceDef]) -> ApiResult<(Changes, Vec<String>)> {
    let mut changes = Changes::default();
    let mut errors = Vec::new();
    let mut kept = Vec::new();
    for def in defs {
        if let Err(e) = validate(t, def) {
            errors.push(format!("services/{}.toml: {}", def.name, e.message));
            continue;
        }
        kept.push(def.name.clone());
        match find(t, &def.name)? {
            None => {
                insert(t, def, SOURCE_FILE)?;
                changes.added.push(def.name.clone());
            }
            Some(r) if r.def != *def || r.source != SOURCE_FILE => {
                t.execute(
                    "UPDATE services SET project = ?2, def = ?3, source = 'file', updated_at = ?4
                     WHERE name = ?1",
                    params![def.name, def.project, def_json(def)?, now()],
                )?;
                changes.updated.push(def.name.clone());
            }
            Some(_) => {}
        }
    }
    let gone: Vec<String> = list(t)?
        .into_iter()
        .filter(|r| r.source == SOURCE_FILE && !kept.contains(&r.def.name))
        .map(|r| r.def.name)
        .collect();
    for name in gone {
        t.execute("DELETE FROM services WHERE name = ?1", [&name])?;
        changes.removed.push(name);
    }
    Ok((changes, errors))
}

/// The project was removed: its services go. Their requests stay as
/// history until the retention sweep.
pub fn on_project_deleted(conn: &Connection, name: &str) -> ApiResult<usize> {
    Ok(conn.execute("DELETE FROM services WHERE project = ?1", [name])?)
}

// ---------------------------------------------------------------- callers

/// Who calls: the label kept on the request and the rights checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    Operator,
    Agent {
        id: String,
        name: String,
        project: String,
    },
}

impl Caller {
    /// `operator` or `agent:<project>/<name>`.
    pub fn label(&self) -> String {
        match self {
            Self::Operator => "operator".into(),
            Self::Agent { name, project, .. } => format!("agent:{project}/{name}"),
        }
    }

    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Operator => None,
            Self::Agent { id, .. } => Some(id),
        }
    }
}

/// Is `caller` in the service's caller list? An empty list is the
/// service's own project; a global service with an empty list has no
/// callers but its admins (the API checks those).
pub fn in_callers(def: &ServiceDef, caller: &Caller) -> bool {
    let Caller::Agent { name, project, .. } = caller else {
        return false;
    };
    if def.callers.is_empty() {
        return def.project.as_deref() == Some(project.as_str());
    }
    let me = format!("agent:{project}/{name}");
    let mine = format!("project:{project}");
    def.callers.iter().any(|c| *c == me || *c == mine)
}

// ---------------------------------------------------------------- requests

/// One request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestRow {
    pub id: String,
    pub service: String,
    pub project: Option<String>,
    pub caller: String,
    pub caller_agent: Option<String>,
    pub state: State,
    pub request: Value,
    pub payload_size: u64,
    pub payload_sha256: Option<String>,
    pub response: Option<Value>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    pub wait_until: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub payload_deleted_at: Option<String>,
    /// The place in the service's queue (1 is next), for a queued request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<u64>,
}

const REQ_COLS: &str = "id, service, project, caller, caller_agent, state, request, payload_size, \
     payload_sha256, response, exit_code, error, wait_until, created_at, started_at, ended_at, \
     payload_deleted_at";

fn json_col(s: Option<String>, i: usize) -> rusqlite::Result<Option<Value>> {
    s.map(|s| {
        serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

fn req_row(r: &Row<'_>) -> rusqlite::Result<RequestRow> {
    let state: String = r.get(5)?;
    let state = State::parse(&state).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            format!("bad state `{state}`").into(),
        )
    })?;
    let size: i64 = r.get(7)?;
    Ok(RequestRow {
        id: r.get(0)?,
        service: r.get(1)?,
        project: r.get(2)?,
        caller: r.get(3)?,
        caller_agent: r.get(4)?,
        state,
        request: json_col(Some(r.get(6)?), 6)?.unwrap_or(Value::Null),
        payload_size: u64::try_from(size).unwrap_or(0),
        payload_sha256: r.get(8)?,
        response: json_col(r.get(9)?, 9)?,
        exit_code: r.get(10)?,
        error: r.get(11)?,
        wait_until: r.get(12)?,
        created_at: r.get(13)?,
        started_at: r.get(14)?,
        ended_at: r.get(15)?,
        payload_deleted_at: r.get(16)?,
        position: None,
    })
}

fn with_position(conn: &Connection, mut r: RequestRow) -> ApiResult<RequestRow> {
    if r.state == State::Queued {
        let ahead: i64 = conn.query_row(
            "SELECT COUNT(*) FROM service_requests
             WHERE service = ?1 AND state = 'queued' AND id < ?2",
            params![r.service, r.id],
            |x| x.get(0),
        )?;
        r.position = Some(u64::try_from(ahead).unwrap_or(0) + 1);
    }
    Ok(r)
}

pub fn find_request(conn: &Connection, id: &str) -> ApiResult<Option<RequestRow>> {
    let r = conn
        .query_row(
            &format!("SELECT {REQ_COLS} FROM service_requests WHERE id = ?1"),
            [id],
            req_row,
        )
        .optional()?;
    r.map(|r| with_position(conn, r)).transpose()
}

pub fn get_request(conn: &Connection, id: &str) -> ApiResult<RequestRow> {
    find_request(conn, id)?.ok_or_else(|| ApiError::not_found(format!("no service request `{id}`")))
}

/// A filter of [`list_requests`].
#[derive(Debug, Clone, Default)]
pub struct RequestFilter {
    pub service: Option<String>,
    /// Only the requests of this agent.
    pub caller_agent: Option<String>,
    /// Only the requests of the operator.
    pub operator_only: bool,
    pub state: Option<State>,
    pub limit: usize,
}

/// The requests, newest first.
pub fn list_requests(conn: &Connection, f: &RequestFilter) -> ApiResult<Vec<RequestRow>> {
    let mut sql = format!("SELECT {REQ_COLS} FROM service_requests WHERE 1 = 1");
    let mut args: Vec<String> = Vec::new();
    if let Some(s) = &f.service {
        args.push(s.clone());
        sql.push_str(&format!(" AND service = ?{}", args.len()));
    }
    if let Some(a) = &f.caller_agent {
        args.push(a.clone());
        sql.push_str(&format!(" AND caller_agent = ?{}", args.len()));
    }
    if f.operator_only {
        sql.push_str(" AND caller_agent IS NULL");
    }
    if let Some(s) = f.state {
        args.push(s.as_str().to_owned());
        sql.push_str(&format!(" AND state = ?{}", args.len()));
    }
    let limit = if f.limit == 0 { 100 } else { f.limit.min(1000) };
    sql.push_str(&format!(" ORDER BY id DESC LIMIT {limit}"));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), req_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(|r| with_position(conn, r)).collect()
}

/// A new request. The caller has passed the rights and the payload checks.
pub struct NewRequest<'a> {
    pub id: &'a str,
    pub service: &'a ServiceRow,
    pub caller: &'a Caller,
    pub request: &'a Value,
    pub payload_size: u64,
    pub payload_sha256: Option<&'a str>,
}

/// Queue a request. `wait_until` is now plus the service's wait limit.
pub fn insert_request(t: &mut Tx<'_>, n: &NewRequest<'_>) -> ApiResult<RequestRow> {
    let at = chrono::Utc::now();
    let wait = chrono::Duration::from_std(n.service.def.wait_limit())
        .map_err(|e| ApiError::internal(format!("bad wait limit: {e}")))?;
    let fmt = |d: chrono::DateTime<chrono::Utc>| {
        d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    t.execute(
        "INSERT INTO service_requests
           (id, service, project, caller, caller_agent, state, request, payload_size,
            payload_sha256, wait_until, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9, ?10)",
        params![
            n.id,
            n.service.def.name,
            n.service.def.project,
            n.caller.label(),
            n.caller.agent_id(),
            serde_json::to_string(n.request)?,
            i64::try_from(n.payload_size).unwrap_or(i64::MAX),
            n.payload_sha256,
            fmt(at + wait),
            fmt(at),
        ],
    )?;
    get_request(t, n.id)
}

/// What a [`transition`] sets besides the state.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub response: Option<Value>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
}

/// Move request `id` from `from` to `to`. A request that is not in `from`
/// (another writer moved it) is `409`, and so is an edge that
/// `State::can_go` refuses. `running` sets `started_at`; a final state
/// sets `ended_at` and the outcome.
pub fn transition(
    t: &mut Tx<'_>,
    id: &str,
    from: State,
    to: State,
    out: Outcome,
) -> ApiResult<RequestRow> {
    if !from.can_go(to) {
        return Err(ApiError::conflict(format!(
            "a service request cannot go from {from} to {to}"
        )));
    }
    let at = now();
    let n = t.execute(
        "UPDATE service_requests SET state = ?3,
           started_at = CASE WHEN ?3 = 'running' THEN ?4 ELSE started_at END,
           ended_at = CASE WHEN ?5 THEN ?4 ELSE ended_at END,
           response = COALESCE(?6, response),
           exit_code = COALESCE(?7, exit_code),
           error = COALESCE(?8, error)
         WHERE id = ?1 AND state = ?2",
        params![
            id,
            from.as_str(),
            to.as_str(),
            at,
            to.is_final(),
            out.response.map(|v| v.to_string()),
            out.exit_code,
            out.error,
        ],
    )?;
    if n == 0 {
        let cur = get_request(t, id)?;
        return Err(ApiError::conflict(format!(
            "service request `{id}` is {}, not {from}",
            cur.state
        )));
    }
    get_request(t, id)
}

/// Cancel request `id` from whatever open state it is in.
pub fn cancel(t: &mut Tx<'_>, id: &str, why: &str) -> ApiResult<RequestRow> {
    let cur = get_request(t, id)?;
    if cur.state.is_final() {
        return Err(ApiError::conflict(format!(
            "service request `{id}` has ended ({})",
            cur.state
        )));
    }
    transition(
        t,
        id,
        cur.state,
        State::Cancelled,
        Outcome {
            error: Some(why.to_owned()),
            ..Outcome::default()
        },
    )
}

/// The queued requests of `service` that may start now, oldest first:
/// as many as the concurrency limit leaves after the requests that wait
/// for resources or run (design Q3 A: the rest wait in order).
pub fn startable(conn: &Connection, service: &str) -> ApiResult<Vec<String>> {
    let s = get(conn, service)?;
    let busy: i64 = conn.query_row(
        "SELECT COUNT(*) FROM service_requests
         WHERE service = ?1 AND state IN ('waiting', 'running')",
        [service],
        |r| r.get(0),
    )?;
    let free = i64::from(s.def.concurrency()) - busy;
    if free <= 0 {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id FROM service_requests WHERE service = ?1 AND state = 'queued'
         ORDER BY id LIMIT ?2",
    )?;
    let ids = stmt
        .query_map(params![service, free], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(ids)
}

/// The requests that have not started by their `wait_until` (`now` in
/// the [`now`] format), moved to `timed_out` (design D3, Q5 A).
pub fn time_out_waits(t: &mut Tx<'_>, at: &str) -> ApiResult<Vec<RequestRow>> {
    let mut stmt = t.prepare(
        "SELECT id, state FROM service_requests
         WHERE state IN ('queued', 'waiting') AND wait_until <= ?1 ORDER BY id",
    )?;
    let due = stmt
        .query_map([at], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut out = Vec::new();
    for (id, state) in due {
        let Some(from) = State::parse(&state) else {
            continue;
        };
        out.push(transition(
            t,
            &id,
            from,
            State::TimedOut,
            Outcome {
                error: Some("the request did not start within the wait limit".into()),
                ..Outcome::default()
            },
        )?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- payloads

/// The payload directory of request `id`.
pub fn payload_dir(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("services").join("payloads").join(id)
}

/// The request payload file of request `id`.
pub fn payload_file(state_dir: &Path, id: &str) -> PathBuf {
    payload_dir(state_dir, id).join("request")
}

/// The payload bytes kept for `service`, or for every service.
pub fn retained_bytes(conn: &Connection, service: Option<&str>) -> ApiResult<u64> {
    let n: i64 = conn.query_row(
        "SELECT COALESCE(SUM(payload_size), 0) FROM service_requests
         WHERE payload_deleted_at IS NULL AND (?1 IS NULL OR service = ?1)",
        [service],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// The free space of a filesystem: `(available, total)` bytes.
pub type FreeSpace = (u64, u64);

/// May `size` more payload bytes of `service` be stored? The per-payload
/// and the per-service limits of the service, the global cap, and the
/// free-space floor of the state filesystem (O2). `free`: `None` when the
/// free space is not known (the check is then skipped and logged by the
/// caller). The error is `413` for a payload over its limit, `503` for a
/// full store.
pub fn check_payload_room(
    conn: &Connection,
    service: &ServiceRow,
    size: u64,
    free: Option<FreeSpace>,
) -> ApiResult<()> {
    let max = service.def.max_payload_bytes();
    if size > max {
        return Err(ApiError::new(
            Code::TooLarge,
            format!(
                "the payload has {size} bytes; service `{}` takes at most {max}",
                service.def.name
            ),
        ));
    }
    if size == 0 {
        return Ok(());
    }
    let kept = retained_bytes(conn, Some(&service.def.name))?;
    let cap = service.def.max_retained_bytes();
    if kept.saturating_add(size) > cap {
        return Err(full(format!(
            "service `{}` keeps {kept} payload bytes of its {cap}: wait for requests to end or for the 7-day retention",
            service.def.name
        )));
    }
    let all = retained_bytes(conn, None)?;
    if all.saturating_add(size) > core::GLOBAL_PAYLOAD_CAP {
        return Err(full(format!(
            "the payload store is at its global cap of {} bytes",
            core::GLOBAL_PAYLOAD_CAP
        )));
    }
    if let Some((avail, total)) = free
        && total > 0
        && avail.saturating_sub(size).saturating_mul(100) < total.saturating_mul(core::MIN_FREE_PERCENT)
    {
        return Err(full(format!(
            "the state filesystem would have under {}% free: no new payloads",
            core::MIN_FREE_PERCENT
        )));
    }
    Ok(())
}

fn full(msg: String) -> ApiError {
    ApiError::new(Code::Unavailable, msg)
}

/// The requests whose payload is past the retention (created before
/// `cutoff`) and not yet deleted. Only ended requests: a payload of an
/// open request stays.
pub fn expired_payloads(conn: &Connection, cutoff: &str) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM service_requests
         WHERE payload_deleted_at IS NULL AND created_at < ?1
           AND state IN ('done', 'failed', 'cancelled', 'timed_out')
         ORDER BY id",
    )?;
    let ids = stmt
        .query_map([cutoff], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(ids)
}

/// The payload files of `id` are gone.
pub fn mark_payload_deleted(conn: &Connection, id: &str) -> ApiResult<()> {
    conn.execute(
        "UPDATE service_requests SET payload_deleted_at = ?2 WHERE id = ?1",
        params![id, now()],
    )?;
    Ok(())
}
