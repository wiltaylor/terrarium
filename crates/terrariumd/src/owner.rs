//! The owner of a monitor or a schedule (issue #18, docs/api.md §30.7).
//!
//! - `actor` (the default): the runs act as `run_as`, the actor that made
//!   the item or last changed what it runs. The daemon checks that
//!   actor's rights again at each run.
//! - `project`: no actor is behind the item; its rights come from the
//!   project. It uses the targets of the project, and a global target that
//!   a sysadmin or the operator granted to the project for that use
//!   ([`crate::store::target_grants`]), checked at each run. A schedule's
//!   other actions stay inside the project, as a project_admin's do.
//! - `system`: the rights of the operator. Only a sysadmin or the
//!   operator makes, changes, or takes over a system-owned item.
//!
//! A project-owned item needs project_admin of the project, sysadmin, or
//! the operator for each write ([`may_manage`]); the store audits each
//! write. The runs show the item as the actor: each monitor result has
//! `monitor:<project>/<name>` (a status change writes a `monitor.status`
//! audit row; the runs write no `target.exec` row), and the `target.exec`
//! row of an owned schedule's exec has `schedule:<id>`.
//!
//! When an agent is deleted, [`on_agent_deleted`] disables each enabled
//! actor-owned monitor and schedule that runs as it, with a reason, and
//! mails the project leads and the operator (the operator's Inbox). A
//! project admin takes such an item over with an edit: `--owner actor`
//! (it then runs as the caller) or `--owner project`.

use std::collections::BTreeMap;

use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::actor::Actor;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::schedules::Reach;
use crate::store::target_grants::{self, Purpose};
use crate::store::targets::{self, TargetRow};

/// Who owns a monitor or a schedule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// The runs act as `run_as`.
    #[default]
    Actor,
    /// The rights come from the project.
    Project,
    /// The rights of the operator.
    System,
}

impl Owner {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Actor => "actor",
            Self::Project => "project",
            Self::System => "system",
        }
    }

    /// The stored form. An unknown value reads as `actor` (the CHECK of
    /// migration 62 keeps it out).
    pub fn from_db(s: &str) -> Self {
        match s {
            "project" => Self::Project,
            "system" => Self::System,
            _ => Self::Actor,
        }
    }

    /// Owned by the project or the system: no actor is behind it.
    pub fn is_owned(self) -> bool {
        self != Self::Actor
    }
}

/// Create, change, delete, run, or take over an item of `owner` in
/// `project` (`None`: global). `actor`: no rule past the item's own.
/// `project`: project_admin of the project, sysadmin, or the operator; a
/// global item cannot be project-owned. `system`: sysadmin or the
/// operator.
pub fn may_manage(actor: &Actor, owner: Owner, project: Option<&str>, what: &str) -> ApiResult<()> {
    match (owner, project) {
        (Owner::Actor, _) => Ok(()),
        (Owner::Project, None) => Err(ApiError::invalid(
            "a global item cannot be project-owned: use --owner system or --owner actor",
        )
        .with_details(json!({"field": "owner"}))),
        (Owner::Project, Some(p)) => {
            policy::admin_project(actor, p, &format!("{what} (owner project)"))
        }
        (Owner::System, _) => policy::sysadmin(actor, &format!("{what} (owner system)")),
    }
}

/// The target that a project- or system-owned item of `project` uses for
/// `purpose`. A target of another project: `403`. A target of the
/// project: yes. A global target: a system-owned item yes; a
/// project-owned one only with a grant ([`target_grants`]).
pub fn owned_target(
    conn: &Connection,
    owner: Owner,
    project: Option<&str>,
    target: &str,
    purpose: Purpose,
) -> ApiResult<TargetRow> {
    let row = targets::get(conn, target)?;
    match (&row.def.project, project) {
        (Some(q), Some(p)) if q == p => return Ok(row),
        (Some(q), _) => {
            return Err(ApiError::forbidden(format!(
                "target `{target}` belongs to project `{q}`; an item uses global targets and those of its own project"
            )));
        }
        (None, _) => {}
    }
    match (owner, project) {
        (Owner::System | Owner::Actor, _) => Ok(row),
        (Owner::Project, None) => Err(ApiError::invalid("a global item cannot be project-owned")),
        (Owner::Project, Some(p)) => {
            if target_grants::has(conn, target, p, purpose)? {
                Ok(row)
            } else {
                Err(ApiError::forbidden(format!(
                    "the global target `{target}` is not granted to project `{p}` for {use_}; \
                     a sysadmin or the operator grants it: terrarium project grant {target} \
                     --for {use_} --project {p}",
                    use_ = purpose.as_str()
                )))
            }
        }
    }
}

/// What the action of a project- or system-owned schedule of `project`
/// may touch. `system`: anything. `project`: what a project_admin's
/// schedule may ([`policy::inside_project`]), and a global target of an
/// `exec` only with a grant for schedules.
pub fn schedule_reach(
    conn: &Connection,
    owner: Owner,
    project: Option<&str>,
    reach: &Reach,
) -> ApiResult<()> {
    match (owner, project) {
        (Owner::Actor | Owner::System, _) => Ok(()),
        (Owner::Project, None) => Err(ApiError::invalid(
            "a global schedule cannot be project-owned",
        )),
        (Owner::Project, Some(p)) => {
            policy::inside_project(p, reach)?;
            if let Reach::Target {
                name,
                project: None,
            } = reach
            {
                owned_target(conn, owner, project, name, Purpose::Schedules)?;
            }
            Ok(())
        }
    }
}

/// May a project- or system-owned item of `project` use secret `name`?
/// `system`: any secret. `project`: a secret of the project, or a global
/// secret granted to the project (`terrarium secret grant <name>
/// project:<p>`). No secret of the name is `422 invalid` on `field`.
pub fn secret_use(
    conn: &Connection,
    owner: Owner,
    project: Option<&str>,
    name: &str,
    field: &str,
) -> ApiResult<()> {
    use crate::secrets::{self, GLOBAL, grants};
    let project = project.filter(|p| *p != GLOBAL);
    let own = match project {
        Some(p) => secrets::get(conn, p, name)?,
        None => None,
    };
    let global = secrets::get(conn, GLOBAL, name)?;
    if own.is_none() && global.is_none() {
        return Err(crate::store::schedules::field_error(
            field,
            format!("no secret `{name}`"),
        ));
    }
    match (owner, project) {
        (Owner::Actor | Owner::System, _) => Ok(()),
        (Owner::Project, None) => Err(ApiError::invalid("a global item cannot be project-owned")),
        (Owner::Project, Some(_)) if own.is_some() => Ok(()),
        (Owner::Project, Some(p)) => {
            let granted = grants::find(conn, "", p, name)?.is_some_and(|e| e.scope == GLOBAL);
            if granted {
                Ok(())
            } else {
                Err(ApiError::forbidden(format!(
                    "`{name}` is a global secret with no grant to project `{p}`: a project-owned \
                     item uses it only with a `use` grant (terrarium secret grant {name} project:{p})"
                )))
            }
        }
    }
}

/// The actor label of the runs of a monitor.
pub fn monitor_label(project: &str, name: &str) -> String {
    format!("monitor:{project}/{name}")
}

/// One item that [`on_agent_deleted`] disabled.
struct Orphan {
    kind: &'static str,
    key: String,
    project: Option<String>,
}

/// The reason on an item whose `run_as` agent `who` was deleted.
pub fn orphan_reason(who: &str) -> String {
    format!(
        "its run_as agent {who} was deleted; a project admin takes it over with an edit \
         (--owner actor runs it as the caller, --owner project gives it to the project), \
         then enables it"
    )
}

/// Agent `id` (`project/name` is `who`) is being deleted: disable each
/// enabled actor-owned monitor and schedule that runs as it, with a
/// reason, and mail the leads of each item's project and the operator.
/// The caller runs it in the delete's transaction, before the row goes.
pub fn on_agent_deleted(t: &mut Tx<'_>, id: &str, who: &str) -> ApiResult<()> {
    let label = format!("agent:{id}");
    let reason = orphan_reason(who);
    let mut gone: Vec<Orphan> = Vec::new();
    let monitors: Vec<String> = {
        let mut st = t.prepare(
            "SELECT id FROM monitors WHERE owner = 'actor' AND run_as = ?1 AND enabled = 1
             ORDER BY project, name",
        )?;
        st.query_map([&label], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for m in monitors {
        let m = crate::store::monitors::disable(t, &m, &reason)?;
        gone.push(Orphan {
            kind: "monitor",
            key: m.key(),
            project: Some(m.project),
        });
    }
    let schedules: Vec<String> = {
        let mut st = t.prepare(
            "SELECT id FROM schedules WHERE owner = 'actor' AND run_as = ?1 AND enabled = 1
             ORDER BY scope, name",
        )?;
        st.query_map([&label], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for s in schedules {
        let s = crate::store::schedules::set_enabled(t, &s, false, Some(&reason), Utc::now())?;
        gone.push(Orphan {
            kind: "schedule",
            key: s.key(),
            project: s.project.clone(),
        });
    }
    if gone.is_empty() {
        return Ok(());
    }
    notify(t, id, who, &gone)
}

/// The leads of `project` (agents with `project_admin`) other than
/// `except`.
fn leads(conn: &Connection, project: &str, except: &str) -> ApiResult<Vec<Address>> {
    let mut st = conn.prepare(
        "SELECT a.id, a.name FROM agents a JOIN roles r ON r.agent_id = a.id
         WHERE a.project = ?1 AND r.role = 'project_admin' AND a.id <> ?2
         ORDER BY a.name",
    )?;
    let rows = st
        .query_map(params![project, except], |r| {
            Ok(Address::Agent {
                id: r.get(0)?,
                name: r.get(1)?,
                project: project.to_owned(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn notify(t: &mut Tx<'_>, id: &str, who: &str, gone: &[Orphan]) -> ApiResult<()> {
    let lines = |items: &[&Orphan]| {
        items
            .iter()
            .map(|o| format!("- {} {}", o.kind, o.key))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let subject = format!("{who} was deleted: its monitors and schedules are disabled");
    let body_of = |items: &[&Orphan]| {
        format!(
            "Agent `{who}` was deleted. These items ran as it, so the daemon disabled them:\n\n{}\n\n\
             Take one over with an edit, then enable it:\n\
             - `terrarium monitor edit <project>/<name> --owner project --enable` (the project \
             owns it; its rights come from the project), or `--owner actor` (it runs as you);\n\
             - `terrarium schedule set <project>/<name> --owner project --enabled`.",
            lines(items)
        )
    };
    // The operator's Inbox: every item.
    let all: Vec<&Orphan> = gone.iter().collect();
    let first = mail::send(
        t,
        Outgoing {
            from: &Address::System,
            to: &Address::Operator,
            subject: &subject,
            body: &body_of(&all),
            thread: None,
            in_reply_to: None,
        },
    )?;
    // The leads of each project: the items of that project.
    let mut by_project: BTreeMap<&str, Vec<&Orphan>> = BTreeMap::new();
    for o in gone {
        if let Some(p) = o.project.as_deref() {
            by_project.entry(p).or_default().push(o);
        }
    }
    for (p, items) in by_project {
        for lead in leads(t, p, id)? {
            mail::send(
                t,
                Outgoing {
                    from: &Address::System,
                    to: &lead,
                    subject: &subject,
                    body: &body_of(&items),
                    thread: Some(&first.thread_id),
                    in_reply_to: None,
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::actor::AgentActor;
    use terrarium_core::Role;

    fn agent(project: &str, roles: Vec<Role>) -> Actor {
        Actor::Agent(AgentActor {
            id: "01AGENT".into(),
            name: "a".into(),
            project: project.into(),
            roles,
            policy: Default::default(),
        })
    }

    #[test]
    fn the_rights_matrix_of_a_write() {
        let worker = agent("p", vec![]);
        let lead = agent("p", vec![Role::ProjectAdmin]);
        let other_lead = agent("q", vec![Role::ProjectAdmin]);
        let sys = agent("q", vec![Role::Sysadmin]);
        let op = Actor::Operator;
        // Actor-owned: the item's own rules decide.
        assert!(may_manage(&worker, Owner::Actor, Some("p"), "x").is_ok());
        // Project-owned: project_admin of the project, sysadmin, the operator.
        assert!(may_manage(&worker, Owner::Project, Some("p"), "x").is_err());
        assert!(may_manage(&other_lead, Owner::Project, Some("p"), "x").is_err());
        assert!(may_manage(&lead, Owner::Project, Some("p"), "x").is_ok());
        assert!(may_manage(&sys, Owner::Project, Some("p"), "x").is_ok());
        assert!(may_manage(&op, Owner::Project, Some("p"), "x").is_ok());
        assert!(may_manage(&op, Owner::Project, None, "x").is_err());
        // System-owned: sysadmin or the operator.
        assert!(may_manage(&lead, Owner::System, Some("p"), "x").is_err());
        assert!(may_manage(&worker, Owner::System, Some("p"), "x").is_err());
        assert!(may_manage(&sys, Owner::System, Some("p"), "x").is_ok());
        assert!(may_manage(&op, Owner::System, None, "x").is_ok());
    }

    #[test]
    fn owners_round_trip() {
        for o in [Owner::Actor, Owner::Project, Owner::System] {
            assert_eq!(Owner::from_db(o.as_str()), o);
            let v = serde_json::to_value(o).unwrap();
            assert_eq!(v, json!(o.as_str()));
        }
        assert_eq!(Owner::default(), Owner::Actor);
        assert!(!Owner::Actor.is_owned());
        assert!(Owner::Project.is_owned());
    }
}
