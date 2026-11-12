//! Durable attribution and reporting, never physical cleanup authority.
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use super::waits::{self, Wait, WaitKind, WaitState};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status { Pending, Unknown, Unavailable }

#[derive(Debug, Clone, Serialize)]
pub struct Cleanup {
    pub operation_id: String,
    pub status: Status,
    pub reason: String,
    pub requested_at: String,
    pub updated_at: String,
    pub evidence_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<serde_json::Value>,
}

pub fn get(c: &Connection, w: &Wait) -> ApiResult<Option<Cleanup>> {
    let row = c.query_row(
        "SELECT operation_id, project, owner, session, target, status, reason, requested_at, updated_at, diagnostic FROM run_cleanup WHERE wait_id=?1",
        [&w.id], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?, r.get::<_,Option<String>>(3)?, r.get::<_,Option<String>>(4)?, r.get::<_,String>(5)?, r.get::<_,String>(6)?, r.get::<_,String>(7)?, r.get::<_,String>(8)?, r.get::<_,Option<String>>(9)?)),
    ).optional()?;
    let Some((id, project, owner, session, target, status, reason, requested_at, updated_at, diagnostic)) = row else { return Ok(None) };
    if project != w.project || owner != w.agent || session.as_deref() != w.session() || target.as_deref() != w.exec_target() {
        return Err(ApiError::conflict("cleanup attribution mismatch"));
    }
    let status = match status.as_str() {
        "pending" => Status::Pending,
        "unavailable" => Status::Unavailable,
        _ => Status::Unknown,
    };
    Ok(Some(Cleanup { operation_id: id, status, reason, requested_at, updated_at, evidence_kind: "none", diagnostic: diagnostic.and_then(|s| serde_json::from_str(&s).ok()) }))
}

/// The CAS and insert share the caller's write transaction. Losing the CAS
/// never creates authority for a later send, including legacy cancellations.
pub fn cancel(t: &mut Tx<'_>, w: &Wait, requester: &str, notify: bool) -> ApiResult<(Option<Cleanup>, bool)> {
    if w.kind != WaitKind::Process { return Err(ApiError::invalid("kill requires a process wait")); }
    let current = waits::get(t, &w.id)?;
    if current.project != w.project || current.agent != w.agent || current.session() != w.session() || current.exec_target() != w.exec_target() {
        return Err(ApiError::conflict("wait attribution changed"));
    }
    if let Some(op) = get(t, &current)? { return Ok((Some(op), false)); }
    if waits::finish(t, &w.id, WaitState::Cancelled, serde_json::json!({"outcome":"cancelled", "by":requester}), notify, 0)?.is_none() {
        return Ok((None, false));
    }
    let at = now();
    t.execute("INSERT INTO run_cleanup (operation_id,wait_id,project,owner,requester,session,target,schema_version,requested_at,updated_at,phase,status,reason) VALUES (?1,?2,?3,?4,?5,?6,?7,1,?8,?8,'queued','pending','intent_recorded')",
        params![ulid::Ulid::new().to_string(),w.id,w.project,w.agent,requester,w.session(),w.exec_target(),at])?;
    Ok((get(t, w)?, true))
}

pub fn dispatch(t: &mut Tx<'_>, id: &str) -> ApiResult<bool> {
    // Write uncertainty with the marker, so even a failed receipt write
    // cannot leave a dispatched operation claiming to be actively pending.
    Ok(t.execute("UPDATE run_cleanup SET phase='dispatch_started',status='unknown',reason='dispatch_started_effect_unknown',updated_at=?2 WHERE operation_id=?1 AND phase='queued' AND status='pending'", params![id,now()])? == 1)
}

pub fn finish(t: &mut Tx<'_>, id: &str, attempted: bool, reason: &str, diagnostic: Option<serde_json::Value>) -> ApiResult<()> {
    let phase = if attempted { "dispatch_started" } else { "queued" };
    let status = if attempted { "unknown" } else { "unavailable" };
    let expected = if attempted { "unknown" } else { "pending" };
    t.execute("UPDATE run_cleanup SET phase='finished',status=?2,reason=?3,updated_at=?4,diagnostic=?6 WHERE operation_id=?1 AND phase=?5 AND status=?7", params![id,status,reason,now(),phase,diagnostic.map(|v| v.to_string()),expected])?;
    Ok(())
}

/// Startup reconciliation is reporting only. Never replay control.
pub fn recover(t: &mut Tx<'_>) -> ApiResult<()> {
    t.execute("UPDATE run_cleanup SET status=CASE phase WHEN 'queued' THEN 'unavailable' ELSE 'unknown' END, reason='daemon_restart_no_replay',phase='finished',updated_at=?1 WHERE phase<>'finished'", [now()])?;
    Ok(())
}
