//! Target leases (docs/api.md §17.7): a runtime grant of one target to one
//! agent, with an optional expiry, that a lead can take back.
//!
//! An agent may use a target when its `policy.targets` lists it, or when it
//! holds an active lease on it. A lease is active while it is not revoked
//! and its `expires_at` (if any) is later than now. The expiry timer
//! (`crate::leases`) marks a lease past its time as `expired`; before the
//! timer runs, the time compare alone makes it inactive.
//!
//! Times are stored as [`crate::db::now`] writes them (RFC 3339, UTC,
//! milliseconds), so a text compare orders them.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::store::agents;

/// A stored lease, with its agent name and its state now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeaseRow {
    pub id: String,
    pub target: String,
    /// The agent ULID.
    pub agent: String,
    /// `project/name` of the agent; `null` for a deleted agent.
    pub agent_name: Option<String>,
    /// The agent's project.
    pub project: String,
    /// `operator` or `agent:<ulid>`.
    pub granted_by: String,
    pub granted_at: String,
    /// `null`: until a revoke.
    pub expires_at: Option<String>,
    pub exclusive: bool,
    pub revoked_at: Option<String>,
    /// `operator`, `agent:<ulid>`, or `system` (the expiry timer).
    pub revoked_by: Option<String>,
    pub reason: Option<String>,
    /// `active`, `revoked`, or `expired`.
    pub state: &'static str,
}

impl LeaseRow {
    pub fn is_active(&self) -> bool {
        self.state == "active"
    }

    /// The holder for a message: `project/name`, else the ULID.
    pub fn holder(&self) -> &str {
        self.agent_name.as_deref().unwrap_or(&self.agent)
    }

    /// `until <time>` or `until revoked`.
    pub fn until(&self) -> String {
        match &self.expires_at {
            Some(t) => format!("until {t}"),
            None => "until revoked".into(),
        }
    }
}

const COLS: &str = "id, target, agent, project, granted_by, granted_at, expires_at, exclusive, \
                    revoked_at, revoked_by, end_reason, reason";

/// The SQL condition of an active lease; `?1` is now.
const ACTIVE: &str = "revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?1)";

fn row(r: &Row<'_>, at: &str) -> rusqlite::Result<LeaseRow> {
    let expires_at: Option<String> = r.get(6)?;
    let revoked_at: Option<String> = r.get(8)?;
    let end: Option<String> = r.get(10)?;
    let state = match (end.as_deref(), &revoked_at, &expires_at) {
        (Some("expired"), _, _) => "expired",
        (_, Some(_), _) => "revoked",
        (_, None, Some(e)) if e.as_str() <= at => "expired",
        _ => "active",
    };
    Ok(LeaseRow {
        id: r.get(0)?,
        target: r.get(1)?,
        agent: r.get(2)?,
        agent_name: None,
        project: r.get(3)?,
        granted_by: r.get(4)?,
        granted_at: r.get(5)?,
        expires_at,
        exclusive: r.get(7)?,
        revoked_at,
        revoked_by: r.get(9)?,
        reason: r.get(11)?,
        state,
    })
}

fn query(
    conn: &Connection,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> ApiResult<Vec<LeaseRow>> {
    let at = now();
    let sql = format!(
        "SELECT {COLS} FROM target_leases WHERE {filter} ORDER BY granted_at DESC, id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt
        .query_map(args, |r| row(r, &at))?
        .collect::<Result<Vec<_>, _>>()?;
    for l in &mut rows {
        l.agent_name = agents::display_name(conn, &l.agent)?;
    }
    Ok(rows)
}

/// The active leases on `target`. The leases of a deleted agent do not count.
pub fn active_on(conn: &Connection, target: &str) -> ApiResult<Vec<LeaseRow>> {
    let at = now();
    query(
        conn,
        &format!("target = ?2 AND {ACTIVE} AND agent IN (SELECT id FROM agents)"),
        &[&at, &target],
    )
}

/// The active leases of `agent`.
pub fn active_of(conn: &Connection, agent: &str) -> ApiResult<Vec<LeaseRow>> {
    let at = now();
    query(conn, &format!("agent = ?2 AND {ACTIVE}"), &[&at, &agent])
}

/// The leases on `target`: the active ones, or with `all` also the ended
/// ones (the last 100), newest first.
pub fn on_target(conn: &Connection, target: &str, all: bool) -> ApiResult<Vec<LeaseRow>> {
    if all {
        let mut v = query(conn, "target = ?1", &[&target])?;
        v.truncate(100);
        Ok(v)
    } else {
        active_on(conn, target)
    }
}

/// The leases of `agent`, as [`on_target`].
pub fn of_agent(conn: &Connection, agent: &str, all: bool) -> ApiResult<Vec<LeaseRow>> {
    if all {
        let mut v = query(conn, "agent = ?1", &[&agent])?;
        v.truncate(100);
        Ok(v)
    } else {
        active_of(conn, agent)
    }
}

/// The lease standing of an agent on a target, for the policy check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The agent holds an active lease.
    Active,
    /// The agent's last lease on the target ended (revoked or expired).
    Ended,
    /// The agent never had a lease on the target.
    None,
}

/// The [`Standing`] of `agent` on `target`.
pub fn standing(conn: &Connection, target: &str, agent: &str) -> ApiResult<Standing> {
    let at = now();
    let last = conn
        .query_row(
            &format!(
                "SELECT {COLS} FROM target_leases WHERE target = ?1 AND agent = ?2
                 ORDER BY granted_at DESC, id DESC LIMIT 1"
            ),
            params![target, agent],
            |r| row(r, &at),
        )
        .optional()?;
    if active_of(conn, agent)?.iter().any(|l| l.target == target) {
        return Ok(Standing::Active);
    }
    Ok(match last {
        Some(_) => Standing::Ended,
        None => Standing::None,
    })
}

/// The lease with `id`.
pub fn get(conn: &Connection, id: &str) -> ApiResult<LeaseRow> {
    query(conn, "id = ?1", &[&id])?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found(format!("no lease `{id}`")))
}

/// What a grant asks for.
#[derive(Debug, Clone)]
pub struct Grant {
    pub target: String,
    /// The target's project; `None` for a global target.
    pub target_project: Option<String>,
    pub agent: String,
    pub agent_project: String,
    pub expires_at: Option<String>,
    pub exclusive: bool,
    pub reason: Option<String>,
}

fn held(target: &str, other: &LeaseRow, msg: String) -> ApiError {
    ApiError::new(Code::LeaseHeld, msg).with_details(json!({
        "target": target,
        "holder": other.holder(),
        "holder_id": other.agent,
        "lease_id": other.id,
        "exclusive": other.exclusive,
        "expires_at": other.expires_at,
    }))
}

/// The `lease` event: it goes to the agent's stream, so the supervisor
/// types the pointer (PLAN §12). `change` is `granted`, `extended`, or
/// `ended`.
fn emit(t: &mut Tx<'_>, l: &LeaseRow, change: &str) -> ApiResult<()> {
    let summary = match change {
        "ended" => format!("lease on {} ended for {}", l.target, l.holder()),
        _ => format!("{} {change} target {} {}", l.holder(), l.target, l.until()),
    };
    let short = l
        .holder()
        .rsplit('/')
        .next()
        .unwrap_or(l.holder())
        .to_owned();
    let draft = EventDraft::new(
        "lease",
        summary,
        json!({"change": change, "target": l.target, "lease_id": l.id,
               "expires_at": l.expires_at, "exclusive": l.exclusive,
               "state": l.state}),
    )
    .project(l.project.clone())
    .agent(l.agent.clone(), short);
    t.emit(draft)?;
    Ok(())
}

/// Store a new lease, with its audit row (`target.grant`) and event. An
/// agent with an active lease on the target is `409 conflict`. A clash with
/// an exclusive lease of another agent is `409 lease_held`.
pub fn grant(t: &mut Tx<'_>, g: &Grant) -> ApiResult<LeaseRow> {
    if let Some(p) = &g.target_project
        && *p != g.agent_project
    {
        return Err(ApiError::invalid(format!(
            "target `{}` belongs to project `{p}`; lease it to an agent of `{p}`",
            g.target
        )));
    }
    let active = active_on(t, &g.target)?;
    if let Some(mine) = active.iter().find(|l| l.agent == g.agent) {
        return Err(ApiError::conflict(format!(
            "{} holds a lease on `{}` {} already; PATCH it to extend it, or revoke it first",
            mine.holder(),
            g.target,
            mine.until()
        )));
    }
    let others: Vec<&LeaseRow> = active.iter().filter(|l| l.agent != g.agent).collect();
    if let Some(x) = others.iter().find(|l| l.exclusive) {
        return Err(held(
            &g.target,
            x,
            format!(
                "target `{}` is held exclusively by {} {}",
                g.target,
                x.holder(),
                x.until()
            ),
        ));
    }
    if g.exclusive
        && let Some(x) = others.first()
    {
        return Err(held(
            &g.target,
            x,
            format!(
                "target `{}` is leased to {} {}; an exclusive lease needs no other holder",
                g.target,
                x.holder(),
                x.until()
            ),
        ));
    }
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO target_leases (id, target, agent, project, granted_by, granted_at,
                                    expires_at, exclusive, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            g.target,
            g.agent,
            g.agent_project,
            t.actor.clone(),
            now(),
            g.expires_at,
            g.exclusive,
            g.reason
        ],
    )?;
    let l = get(t, &id)?;
    t.audit(
        "target.grant",
        &g.target,
        Some(&l.project),
        json!({"lease": l.id, "agent": l.agent, "agent_name": l.agent_name,
               "expires_at": l.expires_at, "exclusive": l.exclusive, "reason": l.reason}),
    )?;
    emit(t, &l, "granted")?;
    Ok(l)
}

fn active_lease(t: &Tx<'_>, target: &str, agent: &str) -> ApiResult<LeaseRow> {
    active_of(t, agent)?
        .into_iter()
        .find(|l| l.target == target)
        .ok_or_else(|| {
            ApiError::not_found(format!("the agent holds no active lease on `{target}`"))
        })
}

/// Revoke the active lease of `agent` on `target` (`target.revoke`).
pub fn revoke(t: &mut Tx<'_>, target: &str, agent: &str) -> ApiResult<LeaseRow> {
    let l = active_lease(t, target, agent)?;
    t.execute(
        "UPDATE target_leases SET revoked_at = ?2, revoked_by = ?3, end_reason = 'revoked'
         WHERE id = ?1",
        params![l.id, now(), t.actor.clone()],
    )?;
    let l = get(t, &l.id)?;
    t.audit(
        "target.revoke",
        target,
        Some(&l.project),
        json!({"lease": l.id, "agent": l.agent, "agent_name": l.agent_name}),
    )?;
    emit(t, &l, "ended")?;
    Ok(l)
}

/// Set a new expiry on the active lease of `agent` on `target`
/// (`target.lease_extend`). `None`: until a revoke.
pub fn extend(
    t: &mut Tx<'_>,
    target: &str,
    agent: &str,
    expires_at: Option<String>,
) -> ApiResult<LeaseRow> {
    let l = active_lease(t, target, agent)?;
    t.execute(
        "UPDATE target_leases SET expires_at = ?2 WHERE id = ?1",
        params![l.id, expires_at],
    )?;
    let l = get(t, &l.id)?;
    t.audit(
        "target.lease_extend",
        target,
        Some(&l.project),
        json!({"lease": l.id, "agent": l.agent, "expires_at": l.expires_at}),
    )?;
    emit(t, &l, "extended")?;
    Ok(l)
}

/// Mark each lease past its expiry as `expired`, with the audit row
/// `target.lease_expired` and the `lease` event. Run it as `system`.
pub fn expire_due(t: &mut Tx<'_>) -> ApiResult<Vec<LeaseRow>> {
    let at = now();
    let due = query(
        t,
        "revoked_at IS NULL AND expires_at IS NOT NULL AND expires_at <= ?1",
        &[&at],
    )?;
    let mut out = Vec::new();
    for l in due {
        t.execute(
            "UPDATE target_leases SET revoked_at = ?2, revoked_by = 'system', end_reason = 'expired'
             WHERE id = ?1",
            params![l.id, at],
        )?;
        let l = get(t, &l.id)?;
        t.audit(
            "target.lease_expired",
            &l.target,
            Some(&l.project),
            json!({"lease": l.id, "agent": l.agent, "agent_name": l.agent_name,
                   "expires_at": l.expires_at}),
        )?;
        emit(t, &l, "ended")?;
        out.push(l);
    }
    Ok(out)
}

/// End every active lease on a target that is deleted.
pub fn end_all_on(t: &mut Tx<'_>, target: &str) -> ApiResult<Vec<LeaseRow>> {
    let at = now();
    let leases = query(t, &format!("target = ?2 AND {ACTIVE}"), &[&at, &target])?;
    let mut out = Vec::new();
    for l in leases {
        t.execute(
            "UPDATE target_leases SET revoked_at = ?2, revoked_by = ?3, end_reason = 'revoked'
             WHERE id = ?1",
            params![l.id, at, t.actor.clone()],
        )?;
        let l = get(t, &l.id)?;
        emit(t, &l, "ended")?;
        out.push(l);
    }
    Ok(out)
}

/// The next expiry of an active lease, for the timer.
pub fn next_expiry(conn: &Connection) -> ApiResult<Option<String>> {
    Ok(conn.query_row(
        "SELECT min(expires_at) FROM target_leases WHERE revoked_at IS NULL AND expires_at IS NOT NULL",
        [],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        c.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at, prefix)
                 VALUES ('p', 'x', 'x', 'P');",
        )
        .unwrap();
        for id in ["A1", "B2", "C3"] {
            c.execute(
                "INSERT INTO agents (id, name, project, harness, workspace, policy, restart, status,
                                     created_at, updated_at)
                 VALUES (?1, lower(?1), 'p', 'claude', '/w', '{}', 'never', 'stopped', 'x', 'x')",
                [id],
            )
            .unwrap();
        }
        c
    }

    fn tx<'c>(c: &'c mut Connection) -> Tx<'c> {
        Tx::for_test(c.transaction().unwrap(), "operator")
    }

    fn g(agent: &str, exclusive: bool, expires_at: Option<&str>) -> Grant {
        Grant {
            target: "vm".into(),
            target_project: Some("p".into()),
            agent: agent.into(),
            agent_project: "p".into(),
            expires_at: expires_at.map(str::to_owned),
            exclusive,
            reason: None,
        }
    }

    #[test]
    fn exclusive_leases_name_the_holder() {
        let mut c = db();
        let mut t = tx(&mut c);
        let a = grant(&mut t, &g("A1", true, None)).unwrap();
        assert!(a.is_active());
        assert_eq!(a.agent_name.as_deref(), Some("p/a1"));
        // Any lease on an exclusively held target is refused.
        let e = grant(&mut t, &g("B2", false, None)).unwrap_err();
        assert_eq!(e.code, Code::LeaseHeld);
        assert!(e.message.contains("p/a1"), "{}", e.message);
        assert!(e.message.contains("until revoked"), "{}", e.message);
        assert_eq!(e.details.unwrap()["holder"], "p/a1");
        // A second lease of the same agent is a conflict.
        let e = grant(&mut t, &g("A1", false, None)).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        revoke(&mut t, "vm", "A1").unwrap();
        // Non-exclusive leases coexist; an exclusive one needs no holder.
        grant(&mut t, &g("A1", false, None)).unwrap();
        grant(&mut t, &g("B2", false, None)).unwrap();
        let e = grant(&mut t, &g("C3", true, None)).unwrap_err();
        assert_eq!(e.code, Code::LeaseHeld);
        assert_eq!(active_on(&t, "vm").unwrap().len(), 2);
    }

    #[test]
    fn standing_follows_revoke_and_expiry() {
        let mut c = db();
        let mut t = tx(&mut c);
        assert_eq!(standing(&t, "vm", "A1").unwrap(), Standing::None);
        grant(&mut t, &g("A1", false, None)).unwrap();
        assert_eq!(standing(&t, "vm", "A1").unwrap(), Standing::Active);
        let r = revoke(&mut t, "vm", "A1").unwrap();
        assert_eq!(r.state, "revoked");
        assert_eq!(standing(&t, "vm", "A1").unwrap(), Standing::Ended);
        assert_eq!(revoke(&mut t, "vm", "A1").unwrap_err().code, Code::NotFound);
        // A lease past its time is inactive before the timer marks it.
        grant(&mut t, &g("B2", true, Some("2000-01-01T00:00:00.000Z"))).unwrap();
        assert_eq!(standing(&t, "vm", "B2").unwrap(), Standing::Ended);
        // It holds nothing: another agent gets an exclusive lease.
        grant(&mut t, &g("C3", true, None)).unwrap();
        let ended = expire_due(&mut t).unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].state, "expired");
        assert_eq!(ended[0].revoked_by.as_deref(), Some("system"));
        assert!(expire_due(&mut t).unwrap().is_empty());
        assert_eq!(on_target(&t, "vm", true).unwrap().len(), 3);
        assert_eq!(of_agent(&t, "C3", false).unwrap().len(), 1);
    }

    #[test]
    fn a_project_target_is_for_agents_of_its_project() {
        let mut c = db();
        let mut t = tx(&mut c);
        let mut x = g("A1", false, None);
        x.target_project = Some("q".into());
        assert_eq!(grant(&mut t, &x).unwrap_err().code, Code::Invalid);
        x.target_project = None;
        assert!(grant(&mut t, &x).is_ok());
    }
}
