//! The grants of a global target to a project (docs/api.md §30.7).
//!
//! A project-owned monitor or schedule has no actor behind it: its rights
//! come from the project. It may use the targets of the project, and a
//! global target only when a sysadmin or the operator granted that
//! target to the project for that use (`monitors` or `schedules`). A
//! grant is explicit and visible: `GET /api/projects/{name}/target-grants`,
//! `terrarium project show`, and the Dashboard page show it. A monitor on
//! a `host` target runs a shell as the host agent's user, so one agent's
//! policy does not imply a grant.
//!
//! The rows go with the target and with the project (`ON DELETE
//! CASCADE`, migration 62).

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::{projects, targets};

/// What a grant lets the project-owned items of the project do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Project-owned monitors run their scripts on the target.
    Monitors,
    /// Project-owned schedules run their `exec` actions on the target.
    Schedules,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monitors => "monitors",
            Self::Schedules => "schedules",
        }
    }

    pub fn parse(s: &str) -> ApiResult<Self> {
        match s.trim() {
            "monitors" => Ok(Self::Monitors),
            "schedules" => Ok(Self::Schedules),
            other => Err(ApiError::invalid(format!(
                "bad grant use `{other}`: use monitors or schedules"
            ))
            .with_details(json!({"field": "for"}))),
        }
    }
}

/// One grant of a global target to a project.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetGrant {
    pub target: String,
    pub project: String,
    /// `monitors` or `schedules`.
    #[serde(rename = "for")]
    pub purpose: Purpose,
    /// `operator` or `agent:<ulid>`.
    pub granted_by: String,
    pub granted_at: String,
}

fn row(r: &Row<'_>) -> rusqlite::Result<TargetGrant> {
    let purpose: String = r.get(2)?;
    Ok(TargetGrant {
        target: r.get(0)?,
        project: r.get(1)?,
        purpose: if purpose == "schedules" {
            Purpose::Schedules
        } else {
            Purpose::Monitors
        },
        granted_by: r.get(3)?,
        granted_at: r.get(4)?,
    })
}

const COLS: &str = "target, project, purpose, granted_by, granted_at";

/// The grants of a project, by target, then use.
pub fn list(conn: &Connection, project: &str) -> ApiResult<Vec<TargetGrant>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM target_grants WHERE project = ?1 ORDER BY target, purpose"
    ))?;
    let rows = st
        .query_map([project], row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Is global target `target` granted to `project` for `purpose`?
pub fn has(conn: &Connection, target: &str, project: &str, purpose: Purpose) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM target_grants WHERE target = ?1 AND project = ?2 AND purpose = ?3",
            params![target, project, purpose.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn emit(t: &mut Tx<'_>, verb: &str, g: &TargetGrant) -> ApiResult<()> {
    t.emit(
        EventDraft::new(
            "target.grant",
            format!(
                "target {} {verb} project {} for {}",
                g.target,
                g.project,
                g.purpose.as_str()
            ),
            json!({"target": g.target, "project": g.project, "for": g.purpose.as_str(),
                   "change": verb}),
        )
        .project(g.project.clone()),
    )?;
    Ok(())
}

/// Grant global target `target` to `project` for `purpose`. The caller
/// checked the rights (sysadmin or the operator). A project target needs
/// no grant: `422`. A second grant of the same use answers the first.
pub fn grant(
    t: &mut Tx<'_>,
    target: &str,
    project: &str,
    purpose: Purpose,
    by: &str,
) -> ApiResult<TargetGrant> {
    projects::get(t, project)?;
    let row = targets::get(t, target)?;
    if let Some(p) = &row.def.project {
        return Err(ApiError::invalid(format!(
            "target `{target}` belongs to project `{p}`: a grant is for a global target \
             (the items of `{p}` use its own targets with no grant)"
        ))
        .with_details(json!({"field": "target"})));
    }
    if !has(t, target, project, purpose)? {
        t.execute(
            "INSERT INTO target_grants (target, project, purpose, granted_by, granted_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![target, project, purpose.as_str(), by, crate::db::now()],
        )?;
        let g = get(t, target, project, purpose)?;
        t.audit(
            "target.grant",
            target,
            Some(project),
            json!({"for": purpose.as_str()}),
        )?;
        emit(t, "granted to", &g)?;
        return Ok(g);
    }
    get(t, target, project, purpose)
}

fn get(conn: &Connection, target: &str, project: &str, purpose: Purpose) -> ApiResult<TargetGrant> {
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM target_grants WHERE target = ?1 AND project = ?2 AND purpose = ?3"
        ),
        params![target, project, purpose.as_str()],
        row,
    )
    .optional()?
    .ok_or_else(|| {
        ApiError::not_found(format!(
            "target `{target}` is not granted to project `{project}` for {}",
            purpose.as_str()
        ))
    })
}

/// Take back a grant. The next run of a project-owned item on the target
/// records the refusal.
pub fn revoke(
    t: &mut Tx<'_>,
    target: &str,
    project: &str,
    purpose: Purpose,
) -> ApiResult<TargetGrant> {
    let g = get(t, target, project, purpose)?;
    t.execute(
        "DELETE FROM target_grants WHERE target = ?1 AND project = ?2 AND purpose = ?3",
        params![target, project, purpose.as_str()],
    )?;
    t.audit(
        "target.ungrant",
        target,
        Some(project),
        json!({"for": purpose.as_str()}),
    )?;
    emit(t, "revoked from", &g)?;
    Ok(g)
}
