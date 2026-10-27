//! Agents, their roles, and their sessions.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::config::{AgentDef, Policy, RestartPolicy, check_system_prompt};
use terrarium_core::{Agent, AgentId, AgentStatus, HarnessId, HarnessSessionId, Role};

use crate::actor::AgentActor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::templates::TemplateRef;
use crate::store::{Page, Paged, check_name, json_col, projects};

/// An agent as the API shows it: the core [`Agent`] and bookkeeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentView {
    #[serde(flatten)]
    pub agent: Agent,
    /// `file` (declared in `agents/<name>.toml`), `api`, or
    /// `fleet:<name>` (owned by a fleet).
    pub source: String,
    /// The card the agent has in `in_progress`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_card: Option<String>,
    /// The template that the create copied (docs/api.md §21), if any.
    /// For display: the agent keeps no live link to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateRef>,
    /// The open waits of the agent (PLAN §12d). Absent when none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub waits: usize,
    /// What the agent waits on, in one short line. Absent when nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn parse<T: std::str::FromStr>(s: &str, what: &str) -> rusqlite::Result<T> {
    s.parse().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("bad {what} `{s}`").into(),
        )
    })
}

fn restart_from(s: &str) -> RestartPolicy {
    serde_json::from_value(json!(s)).unwrap_or_default()
}

fn status_from(s: &str) -> AgentStatus {
    serde_json::from_value(json!(s)).unwrap_or_default()
}

pub fn restart_str(r: RestartPolicy) -> &'static str {
    match r {
        RestartPolicy::Always => "always",
        RestartPolicy::OnFailure => "on-failure",
        RestartPolicy::Never => "never",
    }
}

fn base_row(r: &Row<'_>) -> rusqlite::Result<AgentView> {
    let id: String = r.get("id")?;
    let harness: String = r.get("harness")?;
    let policy: String = r.get("policy")?;
    let workspace: String = r.get("workspace")?;
    Ok(AgentView {
        agent: Agent {
            id: parse(&id, "agent id")?,
            name: r.get("name")?,
            project: r.get("project")?,
            harness: parse::<HarnessId>(&harness, "harness")?,
            subscription: r.get("subscription")?,
            model: r.get("model")?,
            workspace: PathBuf::from(workspace),
            harness_dir: None,
            prompt: r.get("prompt")?,
            system_prompt: r.get("system_prompt")?,
            roles: Vec::new(),
            policy: serde_json::from_str(&policy).unwrap_or_default(),
            restart: restart_from(&r.get::<_, String>("restart")?),
            status: status_from(&r.get::<_, String>("status")?),
            session: None,
            new_session_on_prompt_change: r.get("new_session_on_prompt_change")?,
        },
        source: r.get("source")?,
        current_card: None,
        template: r
            .get::<_, Option<String>>("template")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        waits: 0,
        waiting_on: None,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

/// Fill in the roles, the current session, and the current card.
fn complete(conn: &Connection, mut v: AgentView) -> ApiResult<AgentView> {
    let id = v.agent.id.to_string();
    v.agent.roles = roles(conn, &id)?;
    v.agent.session = conn
        .query_row(
            "SELECT harness_session_id FROM sessions
             WHERE agent_id = ?1 AND current = 1 AND harness_session_id IS NOT NULL",
            [&id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(HarnessSessionId);
    v.current_card = conn
        .query_row(
            "SELECT c.id FROM cards c JOIN board_columns k ON k.id = c.column_id
             WHERE c.assignee = ?1 AND k.name = 'in_progress' AND c.archived = 0
             ORDER BY c.updated_at DESC LIMIT 1",
            [&id],
            |r| r.get(0),
        )
        .optional()?;
    v.waits = crate::store::waits::open_count(conn, &id)?;
    if v.waits > 0 {
        v.waiting_on = crate::store::waits::waiting_on(conn, &id)?;
    }
    Ok(v)
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<AgentView> {
    let v = conn
        .query_row("SELECT * FROM agents WHERE id = ?1", [id], base_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no agent `{id}`")))?;
    complete(conn, v)
}

/// Find an agent by ULID, `project/name`, or a bare name. A bare name looks
/// in `context` first (the caller's project), then in all projects, and is
/// an error when two projects have it.
pub fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<AgentView> {
    if key.parse::<AgentId>().is_ok() {
        return get(conn, key);
    }
    let lookup = |project: &str, name: &str| -> ApiResult<Option<String>> {
        Ok(conn
            .query_row(
                "SELECT id FROM agents WHERE project = ?1 AND name = ?2",
                params![project, name],
                |r| r.get(0),
            )
            .optional()?)
    };
    let found = if let Some((project, name)) = key.split_once('/') {
        lookup(project, name)?
    } else {
        let mut id = match context {
            Some(p) => lookup(p, key)?,
            None => None,
        };
        if id.is_none() {
            let mut stmt = conn.prepare("SELECT id FROM agents WHERE name = ?1")?;
            let ids = stmt
                .query_map([key], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if ids.len() > 1 {
                return Err(ApiError::bad_request(format!(
                    "agent name `{key}` is in more than one project; use project/name or the ID"
                )));
            }
            id = ids.into_iter().next();
        }
        id
    };
    match found {
        Some(id) => get(conn, &id),
        None => Err(ApiError::not_found(format!("no agent `{key}`"))),
    }
}

/// The [`AgentActor`] for a request header value, if the agent exists.
pub fn actor_for(conn: &Connection, key: &str) -> ApiResult<Option<AgentActor>> {
    match find(conn, key, None) {
        Ok(v) => Ok(Some(AgentActor {
            id: v.agent.id.to_string(),
            name: v.agent.name,
            project: v.agent.project,
            roles: v.agent.roles,
            policy: v.agent.policy,
        })),
        Err(e) if e.code == crate::error::Code::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Agents ordered by ID, optionally of one project.
pub fn list(conn: &Connection, project: Option<&str>, page: &Page) -> ApiResult<Paged<AgentView>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(
        "SELECT * FROM agents WHERE id > ?1 AND (?2 IS NULL OR project = ?2)
         ORDER BY id LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![page.cursor().unwrap_or(""), project, (limit + 1) as i64],
            base_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let rows = rows
        .into_iter()
        .map(|v| complete(conn, v))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Paged::from_rows(rows, limit, |a| a.agent.id.to_string()))
}

/// The roles above `member`.
pub fn roles(conn: &Connection, id: &str) -> ApiResult<Vec<Role>> {
    let mut stmt = conn.prepare("SELECT role FROM roles WHERE agent_id = ?1 ORDER BY role")?;
    let rows = stmt
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .iter()
        .filter_map(|s| serde_json::from_value(json!(s)).ok())
        .collect())
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::Member => "member",
        Role::ProjectAdmin => "project_admin",
        Role::Sysadmin => "sysadmin",
    }
}

/// Replace the roles of an agent. `member` is implicit and is dropped.
pub fn set_roles(t: &mut Tx<'_>, id: &str, project: &str, wanted: &[Role]) -> ApiResult<Vec<Role>> {
    let before = roles(t, id)?;
    t.execute("DELETE FROM roles WHERE agent_id = ?1", [id])?;
    let at = now();
    for role in wanted.iter().filter(|r| **r != Role::Member) {
        let scope = (*role == Role::ProjectAdmin).then_some(project);
        t.execute(
            "INSERT OR IGNORE INTO roles (agent_id, role, project, granted_by, granted_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, role_str(*role), scope, t.actor, at],
        )?;
    }
    let after = roles(t, id)?;
    if before != after {
        t.audit(
            "agent.roles",
            id,
            Some(project),
            json!({"before": before, "after": after}),
        )?;
    }
    Ok(after)
}

fn check_def(def: &AgentDef) -> ApiResult<()> {
    check_name("agent", &def.name)?;
    if def.policy.mail_to.iter().any(|e| e.is_empty()) {
        return Err(ApiError::invalid("policy.mail_to has an empty entry"));
    }
    if let Some(bad) = def.policy.cross_project.iter().find(|e| {
        e.split_once('/')
            .is_none_or(|(p, n)| p.is_empty() || n.is_empty())
    }) {
        return Err(ApiError::invalid(format!(
            "policy.cross_project entry `{bad}` must be `project/agent` or `project/*`"
        )));
    }
    if let Some(text) = &def.system_prompt {
        check_system_prompt(text).map_err(ApiError::invalid)?;
    }
    Ok(())
}

/// The system prompt as the store keeps it: `None` for an empty text.
fn stored_system_prompt(text: Option<&String>) -> Option<&String> {
    text.filter(|t| !t.trim().is_empty())
}

/// Point every agent at its workspace under the work root,
/// `<work_dir>/<project>/<name>` (PLAN §11, "mode 1"). An agent made before
/// the work root has `<state>/agents/<ulid>/workspace`; its files move at
/// its next start (`crate::workspace::migrate`). Returns the agents that
/// changed.
pub fn set_workspaces(t: &mut Tx<'_>, work_dir: &Path) -> ApiResult<usize> {
    let rows: Vec<(String, String, String, String)> = {
        let mut stmt = t.prepare("SELECT id, project, name, workspace FROM agents")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut n = 0;
    for (id, project, name, ws) in rows {
        let want = crate::workspace::dir(work_dir, &project, &name);
        if std::path::Path::new(&ws) != want {
            t.execute(
                "UPDATE agents SET workspace = ?2 WHERE id = ?1",
                params![id, want.to_string_lossy()],
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Make an agent. The project must exist. Roles in `def` are written as is;
/// the caller checks that the actor may grant them.
pub fn create(
    t: &mut Tx<'_>,
    def: &AgentDef,
    work_dir: &Path,
    source: &str,
) -> ApiResult<AgentView> {
    create_from(t, def, work_dir, source, None)
}

/// [`create`], and note the template that `def` came from.
pub fn create_from(
    t: &mut Tx<'_>,
    def: &AgentDef,
    work_dir: &Path,
    source: &str,
    template: Option<&TemplateRef>,
) -> ApiResult<AgentView> {
    check_def(def)?;
    projects::get(t, &def.project)?;
    let exists: i64 = t.query_row(
        "SELECT count(*) FROM agents WHERE project = ?1 AND name = ?2",
        params![def.project, def.name],
        |r| r.get(0),
    )?;
    if exists > 0 {
        return Err(ApiError::conflict(format!(
            "project `{}` has an agent `{}` already",
            def.project, def.name
        )));
    }
    let mut agent = Agent::from_def(def, work_dir);
    agent.workspace = crate::workspace::dir(work_dir, &def.project, &def.name);
    agent.system_prompt = stored_system_prompt(agent.system_prompt.as_ref()).cloned();
    let id = agent.id.to_string();
    let at = now();
    t.execute(
        "INSERT INTO agents (id, name, project, harness, subscription, model, prompt, policy,
                             restart, status, workspace, source, created_at, updated_at,
                             new_session_on_prompt_change, template, system_prompt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'stopped', ?10, ?11, ?12, ?12, ?13, ?14,
                 ?15)",
        params![
            id,
            agent.name,
            agent.project,
            agent.harness.as_str(),
            agent.subscription,
            agent.model,
            agent.prompt,
            serde_json::to_string(&agent.policy)?,
            restart_str(agent.restart),
            agent.workspace.to_string_lossy(),
            source,
            at,
            agent.new_session_on_prompt_change,
            template.map(serde_json::to_string).transpose()?,
            agent.system_prompt
        ],
    )?;
    set_roles(t, &id, &def.project, &def.roles)?;
    t.audit(
        "agent.create",
        &id,
        Some(&def.project),
        json!({"name": def.name, "harness": def.harness, "source": source,
               "template": template}),
    )?;
    get(t, &id)
}

/// Note the template that a fleet entry of the agent names.
pub fn set_template(t: &Tx<'_>, id: &str, template: Option<&TemplateRef>) -> ApiResult<()> {
    t.execute(
        "UPDATE agents SET template = ?2 WHERE id = ?1",
        params![id, template.map(serde_json::to_string).transpose()?],
    )?;
    Ok(())
}

/// The body of `PATCH /api/agents/{id}`. A field that is absent stays as
/// it is. `subscription`, `model`, `prompt`, and `system_prompt` take
/// `null` to clear them; an empty `system_prompt` clears it too.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPatch {
    #[serde(default)]
    pub harness: Option<HarnessId>,
    #[serde(default, deserialize_with = "nullable")]
    pub subscription: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub model: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub prompt: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub system_prompt: Option<Option<String>>,
    #[serde(default)]
    pub policy: Option<Policy>,
    #[serde(default)]
    pub restart: Option<RestartPolicy>,
    #[serde(default)]
    pub new_session_on_prompt_change: Option<bool>,
}

/// A field that is present: `Some(None)` for `null`, `Some(Some(v))` for a
/// value. An absent field stays `None` through `#[serde(default)]`.
fn nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Write the definition fields of an agent.
pub fn write_def(t: &mut Tx<'_>, id: &str, p: &AgentPatch) -> ApiResult<()> {
    if let Some(policy) = &p.policy {
        check_def(&AgentDef {
            name: "x".into(),
            project: String::new(),
            harness: HarnessId::Claude,
            subscription: None,
            model: None,
            prompt: None,
            system_prompt: None,
            roles: Vec::new(),
            policy: policy.clone(),
            restart: RestartPolicy::default(),
            new_session_on_prompt_change: true,
        })?;
    }
    let system_prompt = p
        .system_prompt
        .as_ref()
        .map(|v| stored_system_prompt(v.as_ref()));
    if let Some(Some(text)) = system_prompt {
        check_system_prompt(text).map_err(ApiError::invalid)?;
    }
    // `?N IS NOT NULL` is the "field present" flag of each nullable column.
    t.execute(
        "UPDATE agents SET
            harness = coalesce(?2, harness),
            subscription = CASE WHEN ?9 THEN ?3 ELSE subscription END,
            model = CASE WHEN ?10 THEN ?4 ELSE model END,
            prompt = CASE WHEN ?11 THEN ?5 ELSE prompt END,
            policy = coalesce(?6, policy),
            restart = coalesce(?7, restart),
            new_session_on_prompt_change = coalesce(?12, new_session_on_prompt_change),
            system_prompt = CASE WHEN ?13 THEN ?14 ELSE system_prompt END,
            updated_at = ?8
         WHERE id = ?1",
        params![
            id,
            p.harness.map(HarnessId::as_str),
            p.subscription.clone().flatten(),
            p.model.clone().flatten(),
            p.prompt.clone().flatten(),
            p.policy.as_ref().map(serde_json::to_string).transpose()?,
            p.restart.map(restart_str),
            now(),
            p.subscription.is_some(),
            p.model.is_some(),
            p.prompt.is_some(),
            p.new_session_on_prompt_change,
            system_prompt.is_some(),
            system_prompt.flatten(),
        ],
    )?;
    Ok(())
}

pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let a = get(t, id)?;
    crate::store::waits::on_agent_deleted(t, id)?;
    t.execute("DELETE FROM agents WHERE id = ?1", [id])?;
    // The name stays for the cards, comments, and mail of the agent.
    t.execute(
        "INSERT OR REPLACE INTO agents_removed (id, project, name, removed_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, a.agent.project, a.agent.name, now()],
    )?;
    t.audit(
        "agent.delete",
        id,
        Some(&a.agent.project),
        json!({"name": a.agent.name}),
    )?;
    Ok(())
}

pub fn set_status(t: &mut Tx<'_>, id: &str, status: AgentStatus) -> ApiResult<AgentStatus> {
    let before: String = t
        .query_row("SELECT status FROM agents WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no agent `{id}`")))?;
    t.execute(
        "UPDATE agents SET status = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, status.as_str(), now()],
    )?;
    Ok(status_from(&before))
}

/// One harness conversation of an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    pub current: bool,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Why the session ended: `stopped`, `restarted`, `exit:<code>`, `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The number of turns that started in the session.
    pub turns: i64,
}

fn session_row(r: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get("id")?,
        agent: r.get("agent_id")?,
        harness_session_id: r.get("harness_session_id")?,
        current: r.get::<_, i64>("current")? != 0,
        started_at: r.get("started_at")?,
        ended_at: r.get("ended_at")?,
        outcome: r.get("end_reason")?,
        turns: r.get("turns")?,
    })
}

/// Sessions, newest first. The cursor is a session ID.
pub fn sessions(conn: &Connection, agent: &str, page: &Page) -> ApiResult<Paged<Session>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(
        "SELECT s.*, (SELECT count(*) FROM events e
                      WHERE e.session_id = s.id AND e.kind = 'turn_start') AS turns
         FROM sessions s
         WHERE s.agent_id = ?1 AND (?2 IS NULL OR s.id < ?2)
         ORDER BY s.id DESC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![agent, page.cursor(), (limit + 1) as i64],
            session_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |s| s.id.clone()))
}

/// One session of `agent`, by its session ID or its harness session ID
/// (the newest session with that harness session).
pub fn session(conn: &Connection, agent: &str, key: &str) -> ApiResult<Option<Session>> {
    Ok(conn
        .query_row(
            "SELECT s.*, (SELECT count(*) FROM events e
                          WHERE e.session_id = s.id AND e.kind = 'turn_start') AS turns
             FROM sessions s
             WHERE s.agent_id = ?1 AND (s.id = ?2 OR s.harness_session_id = ?2)
             ORDER BY s.id = ?2 DESC, s.id DESC LIMIT 1",
            params![agent, key],
            session_row,
        )
        .optional()?)
}

pub fn current_session(conn: &Connection, agent: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM sessions WHERE agent_id = ?1 AND current = 1",
            [agent],
            |r| r.get(0),
        )
        .optional()?)
}

/// The harness session of the last session, for resume.
pub fn last_harness_session(conn: &Connection, agent: &str) -> ApiResult<Option<HarnessSessionId>> {
    Ok(conn
        .query_row(
            "SELECT harness_session_id FROM sessions
             WHERE agent_id = ?1 AND harness_session_id IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            [agent],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(HarnessSessionId))
}

/// Is `name` a target of `project`? For the policy rule of
/// `policy::set_policy`: a project_admin adds only these to a policy.
pub fn project_target(conn: &Connection, project: &str, name: &str) -> bool {
    matches!(
        crate::store::targets::find(conn, name),
        Ok(Some(r)) if r.def.project.as_deref() == Some(project)
    )
}

/// Check a policy write by `actor` on an agent of `project` whose policy
/// is `old` (`None` for a new agent), against the targets in the store
/// (`docs/security-review.md` F13).
pub fn check_policy(
    conn: &Connection,
    actor: &crate::actor::Actor,
    project: &str,
    old: Option<&Policy>,
    new: &Policy,
) -> ApiResult<()> {
    crate::policy::set_policy(actor, project, old, new, &|n| {
        project_target(conn, project, n)
    })
}

/// Another agent than `except` with the harness session `harness` in any
/// of its sessions, if any (`docs/security-review.md` F12).
pub fn harness_session_owner(
    conn: &Connection,
    harness: &str,
    except: &str,
) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT agent_id FROM sessions
             WHERE harness_session_id = ?1 AND agent_id != ?2 LIMIT 1",
            [harness, except],
            |r| r.get(0),
        )
        .optional()?)
}

/// End the current session, if any.
pub fn end_session(t: &mut Tx<'_>, agent: &str, outcome: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE sessions SET current = 0, ended_at = ?2, end_reason = coalesce(end_reason, ?3)
         WHERE agent_id = ?1 AND current = 1",
        params![agent, now(), outcome],
    )?;
    Ok(())
}

/// End the current session and open a new one.
pub fn begin_session(t: &mut Tx<'_>, agent: &str, outcome_of_previous: &str) -> ApiResult<String> {
    end_session(t, agent, outcome_of_previous)?;
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO sessions (id, agent_id, current, started_at) VALUES (?1, ?2, 1, ?3)",
        params![id, agent, now()],
    )?;
    Ok(id)
}

pub fn set_harness_session(t: &mut Tx<'_>, agent: &str, harness: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE sessions SET harness_session_id = ?2 WHERE agent_id = ?1 AND current = 1",
        params![agent, harness],
    )?;
    Ok(())
}

/// The fields a file definition sets, for sync.
pub fn sync_def(t: &mut Tx<'_>, id: &str, def: &AgentDef) -> ApiResult<bool> {
    check_def(def)?;
    let before = get(t, id)?;
    let system_prompt = stored_system_prompt(def.system_prompt.as_ref());
    let changed = before.source != projects::SOURCE_FILE
        || before.agent.harness != def.harness
        || before.agent.subscription != def.subscription
        || before.agent.model != def.model
        || before.agent.prompt != def.prompt
        || before.agent.system_prompt.as_ref() != system_prompt
        || before.agent.policy != def.policy
        || before.agent.restart != def.restart;
    if changed {
        t.execute(
            "UPDATE agents SET harness = ?2, subscription = ?3, model = ?4, prompt = ?5,
                    policy = ?6, restart = ?7, source = 'file', updated_at = ?8,
                    new_session_on_prompt_change = ?9, system_prompt = ?10
             WHERE id = ?1",
            params![
                id,
                def.harness.as_str(),
                def.subscription,
                def.model,
                def.prompt,
                serde_json::to_string(&def.policy)?,
                restart_str(def.restart),
                now(),
                def.new_session_on_prompt_change,
                system_prompt
            ],
        )?;
    }
    let mut wanted: Vec<Role> = def
        .roles
        .iter()
        .copied()
        .filter(|r| *r != Role::Member)
        .collect();
    wanted.sort_by_key(|r| role_str(*r));
    wanted.dedup();
    let roles_changed = before.agent.roles != wanted;
    if roles_changed {
        set_roles(t, id, &def.project, &wanted)?;
    }
    Ok(changed || roles_changed)
}

/// Names, by agent ID, for rendering.
pub fn display_name(conn: &Connection, id: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT project || '/' || name FROM agents WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// The `project/name` of an agent, also of a deleted one (migration 12).
/// The readers of stored authors and assignees use it; `None` for a key
/// that never was an agent.
pub fn known_name(conn: &Connection, id: &str) -> ApiResult<Option<String>> {
    Ok(match display_name(conn, id)? {
        Some(n) => Some(n),
        None => removed(conn, id)?.map(|(p, n)| format!("{p}/{n}")),
    })
}

/// The `(project, name)` of a deleted agent.
pub fn removed(conn: &Connection, id: &str) -> ApiResult<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT project, name FROM agents_removed WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Find a deleted agent (migration 12) by ULID, `project/name`, or a bare
/// name, the same key forms as [`find`]. Returns `(id, project, name)`.
/// Only the read-only list filters use it, so that the operator can still
/// list what a removed agent did. A name that two tombstones share is an
/// error; the ULID picks one.
pub fn find_removed(
    conn: &Connection,
    key: &str,
    context: Option<&str>,
) -> ApiResult<Option<(String, String, String)>> {
    if key.parse::<AgentId>().is_ok() {
        return Ok(removed(conn, key)?.map(|(p, n)| (key.to_owned(), p, n)));
    }
    let query = |sql: &str, args: &[&str]| -> ApiResult<Vec<(String, String, String)>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    };
    const BY_NAME: &str =
        "SELECT id, project, name FROM agents_removed WHERE project = ?1 AND name = ?2";
    let rows = if let Some((project, name)) = key.split_once('/') {
        query(BY_NAME, &[project, name])?
    } else {
        let mut rows = match context {
            Some(p) => query(BY_NAME, &[p, key])?,
            None => Vec::new(),
        };
        if rows.is_empty() {
            rows = query(
                "SELECT id, project, name FROM agents_removed WHERE name = ?1",
                &[key],
            )?;
        }
        rows
    };
    if rows.len() > 1 {
        return Err(ApiError::bad_request(format!(
            "more than one removed agent was `{key}`; use the ID"
        )));
    }
    Ok(rows.into_iter().next())
}

/// The seq of the last runner event recorded for `agent` (migration 7).
pub fn runner_seq(conn: &Connection, agent: &str) -> ApiResult<u64> {
    let seq: i64 = conn
        .query_row(
            "SELECT runner_seq FROM agents WHERE id = ?1",
            [agent],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(u64::try_from(seq).unwrap_or(0))
}

/// Record a runner event seq. False: the seq is at or below the stored
/// one, so the event is a double (a replay after a reconnect).
pub fn advance_runner_seq(t: &mut Tx<'_>, agent: &str, seq: u64) -> ApiResult<bool> {
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    let n = t.execute(
        "UPDATE agents SET runner_seq = ?2 WHERE id = ?1 AND runner_seq < ?2",
        params![agent, seq],
    )?;
    Ok(n > 0)
}

/// Lower the stored seq to `seq` when it is higher: `events.jsonl` starts
/// again at `seq` (the file was removed or cut), and the next events must
/// not count as doubles.
pub fn clamp_runner_seq(t: &mut Tx<'_>, agent: &str, seq: u64) -> ApiResult<()> {
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    t.execute(
        "UPDATE agents SET runner_seq = ?2 WHERE id = ?1 AND runner_seq > ?2",
        params![agent, seq],
    )?;
    Ok(())
}

/// All agent IDs with their stored status.
pub fn all_statuses(conn: &Connection) -> ApiResult<Vec<(String, AgentStatus)>> {
    let mut stmt = conn.prepare("SELECT id, status FROM agents ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, s)| (id, status_from(&s)))
        .collect())
}

pub fn json_policy(text: &str) -> Policy {
    json_col(text)
}
