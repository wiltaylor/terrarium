//! Agents, their roles, and their sessions.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::config::{
    AgentDef, CompactMaxAge, IdleStop, Policy, RestartPolicy, check_system_prompt,
};
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
    /// The card the agent has in the `working` column of its board, if any.
    /// For an ephemeral agent (TER-75), else its bound card, else the
    /// newest open card that it is the assignee of, so a card agent shows
    /// its card while the card waits in another column.
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
    /// The harness version that the live agent runs (its version dir in
    /// the tools dir). It stays until the next start, also after an
    /// update. Absent when the agent does not run or the version is not
    /// known. The API fills it from the supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    /// The live agent runs with other skills than its `skills` list now
    /// picks from the library (PLAN §8.6): the list or a skill changed
    /// after the start. A restart applies them. Absent when false. The
    /// API fills it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skills_restart: bool,
    /// The skills that the agent gets at its next start (PLAN §8.6): the
    /// default-on skills of the library (when `default_skills`) and its
    /// `skills`, less `skills_off`, with the source of each (`default` or
    /// `explicit`). The API fills it.
    #[serde(default)]
    pub effective_skills: Vec<terrarium_core::skills::EffectiveSkill>,
    /// Nothing wakes the agent while it is paused (PLAN §18): its turns
    /// and mail wait. A pause stops a running agent.
    #[serde(default)]
    pub paused: bool,
    /// The reason of the last status change, for example `idle` for an
    /// idle stop or `paused`. Absent when not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// The turns, the mail, and the card and nudge pointers that wait for
    /// the agent ([`queued_count`]). Absent when 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub queued: usize,
    /// The harness of the agent has a compaction that the daemon can run
    /// and see the end of (PLAN §18, "Auto compact"). False: the daemon
    /// ignores the auto compact settings of the agent, and refuses a
    /// manual compaction.
    #[serde(default)]
    pub compact_supported: bool,
    /// The end of the last compaction that the daemon ran (RFC 3339).
    /// Absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compact_at: Option<String>,
    /// The result of the last compaction. Absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compact_ok: Option<bool>,
    /// An ephemeral agent (PLAN §18, "Ephemeral agents"): it does one
    /// task for its `owner`, and the finish removes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
    /// The owner of an ephemeral agent: an agent ULID, or `operator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The spawn of an ephemeral agent: the owner's name, the template,
    /// the task, and the deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<crate::store::spawns::SpawnInfo>,
    /// The ephemeral agents of this agent that run now. Absent when 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub spawned_running: usize,
    /// A compaction or a new session that waits (PLAN §18, "Compact and
    /// new session on request", `crate::reset`). Absent when none. It
    /// does not show the `then` text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_pending: Option<ResetPendingView>,
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
            subscription_pool: json_col(&r.get::<_, String>("subscription_pool")?),
            model: r.get("model")?,
            workspace: PathBuf::from(workspace),
            harness_dir: None,
            prompt: r.get("prompt")?,
            system_prompt: r.get("system_prompt")?,
            persona: r.get("persona")?,
            skills: json_col(&r.get::<_, String>("skills")?),
            skills_off: json_col(&r.get::<_, String>("skills_off")?),
            default_skills: r.get("default_skills")?,
            delivered_skills: Vec::new(),
            roles: Vec::new(),
            policy: serde_json::from_str(&policy).unwrap_or_default(),
            restart: restart_from(&r.get::<_, String>("restart")?),
            idle_stop: r
                .get::<_, Option<String>>("idle_stop")?
                .and_then(|s| s.parse::<IdleStop>().ok()),
            compact_on_idle: r.get("compact_on_idle")?,
            compact_max_age: r
                .get::<_, Option<String>>("compact_max_age")?
                .and_then(|s| s.parse::<CompactMaxAge>().ok()),
            card_reminder: r.get("card_reminder")?,
            status: status_from(&r.get::<_, String>("status")?),
            session: None,
            new_session_on_prompt_change: r.get("new_session_on_prompt_change")?,
            harness_settings: None,
        },
        source: r.get("source")?,
        current_card: None,
        template: r
            .get::<_, Option<String>>("template")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        waits: 0,
        waiting_on: None,
        harness_version: None,
        skills_restart: false,
        effective_skills: Vec::new(),
        paused: r.get("paused")?,
        status_reason: r.get("status_reason")?,
        queued: 0,
        compact_supported: parse::<HarnessId>(&harness, "harness")?
            .compact_command()
            .is_some(),
        last_compact_at: r.get("last_compact_at")?,
        last_compact_ok: r.get("last_compact_ok")?,
        owner: r.get("owner")?,
        ephemeral: r.get::<_, Option<String>>("owner")?.is_some(),
        spawn: None,
        spawned_running: 0,
        reset_pending: r
            .get::<_, Option<String>>("pending_reset")?
            .and_then(|s| serde_json::from_str::<PendingReset>(&s).ok())
            .map(|p| p.view()),
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
             WHERE c.assignee = ?1 AND k.role = 'working' AND c.archived = 0
             ORDER BY c.updated_at DESC LIMIT 1",
            [&id],
            |r| r.get(0),
        )
        .optional()?;
    v.waits = crate::store::waits::open_count(conn, &id)?;
    if v.waits > 0 {
        v.waiting_on = crate::store::waits::waiting_on(conn, &id)?;
    }
    v.queued = queued_count(conn, &id)?;
    if v.ephemeral {
        v.spawn = crate::store::spawns::info(conn, &id)?;
        if v.current_card.is_none() {
            v.current_card = ephemeral_card(conn, &id, v.spawn.as_ref())?;
        }
    }
    v.spawned_running = crate::store::spawns::running_count(conn, &id)?;
    Ok(v)
}

/// The card of ephemeral agent `id` outside the `working` column (TER-75):
/// its bound card, else the newest open card that it is the assignee of.
fn ephemeral_card(
    conn: &Connection,
    id: &str,
    spawn: Option<&crate::store::spawns::SpawnInfo>,
) -> ApiResult<Option<String>> {
    let bound = spawn
        .and_then(|s| s.bound.as_deref())
        .and_then(|b| b.strip_prefix("card:"));
    if let Some(card) = bound {
        return Ok(Some(card.to_owned()));
    }
    Ok(conn
        .query_row(
            "SELECT id FROM cards WHERE assignee = ?1 AND archived = 0
             ORDER BY updated_at DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// The work that waits for agent `id` (PLAN §18): its queued turns, its
/// `queued` mail, its parked card and nudge pointers, and a pending reset
/// with a `then` prompt (`crate::reset`). A lease line waits too, but it
/// does not count: it does not wake the agent.
pub fn queued_count(conn: &Connection, id: &str) -> ApiResult<usize> {
    let n: i64 = conn.query_row(
        "SELECT (SELECT count(*) FROM queued_turns WHERE agent = ?1)
              + (SELECT count(*) FROM messages
                 WHERE to_agent = ?1 AND to_kind = 'agent' AND state = 'queued')
              + (SELECT count(*) FROM pending_pointers
                 WHERE agent = ?1 AND kind IN ('card', 'lead'))
              + (SELECT count(*) FROM agents
                 WHERE id = ?1 AND json_extract(pending_reset, '$.then') IS NOT NULL)",
        [id],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The agents with work that waits for them ([`queued_count`] above 0).
pub fn with_queued_work(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT agent FROM queued_turns
         UNION SELECT to_agent FROM messages WHERE to_kind = 'agent' AND state = 'queued'
         UNION SELECT agent FROM pending_pointers WHERE kind IN ('card', 'lead')
         UNION SELECT id FROM agents WHERE json_extract(pending_reset, '$.then') IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The kinds of a reset (PLAN §18, "Compact and new session on
/// request").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    Compact,
    NewSession,
}

impl ResetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::NewSession => "new_session",
        }
    }
}

/// A compaction or a new session that waits for its agent
/// (`agents.pending_reset`, `crate::reset`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReset {
    pub kind: ResetKind,
    /// The prompt to send after the compaction or the new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<String>,
    /// A new session starts with the recap turn.
    #[serde(default = "yes")]
    pub recap: bool,
    /// Who asked, for the tag: `self`, `operator`, or `project/agent`.
    pub by: String,
    /// The actor label of the request (`operator`, `agent:<ulid>`).
    pub actor: String,
    /// The request waited for the end of the turn on purpose (`defer`, or
    /// a request of the agent itself).
    #[serde(default)]
    pub deferred: bool,
    pub requested_at: String,
}

fn yes() -> bool {
    true
}

/// What the agent view shows of a [`PendingReset`]: not the `then` text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetPendingView {
    pub kind: ResetKind,
    pub by: String,
    pub then_set: bool,
    pub recap: bool,
    pub deferred: bool,
    pub requested_at: String,
}

impl PendingReset {
    pub fn view(&self) -> ResetPendingView {
        ResetPendingView {
            kind: self.kind,
            by: self.by.clone(),
            then_set: self.then.is_some(),
            recap: self.recap,
            deferred: self.deferred,
            requested_at: self.requested_at.clone(),
        }
    }
}

/// The pending reset of agent `id`, if any.
pub fn pending_reset(conn: &Connection, id: &str) -> ApiResult<Option<PendingReset>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT pending_reset FROM agents WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
}

/// Set (`Some`) or clear (`None`) the pending reset of agent `id`.
/// Returns the one before.
pub fn set_pending_reset(
    t: &mut Tx<'_>,
    id: &str,
    reset: Option<&PendingReset>,
) -> ApiResult<Option<PendingReset>> {
    let before = pending_reset(t, id)?;
    let text = reset.map(|r| serde_json::to_string(r).unwrap_or_default());
    t.execute(
        "UPDATE agents SET pending_reset = ?2 WHERE id = ?1",
        params![id, text],
    )?;
    Ok(before)
}

/// The agents with a pending reset.
pub fn with_pending_reset(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM agents WHERE pending_reset IS NOT NULL")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One turn that waits for the wake of its agent (`queued_turns`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueuedTurn {
    pub id: i64,
    /// The turn as it will be typed, with its tag line.
    pub text: String,
    pub created_at: String,
}

/// Keep `text` as a turn for agent `id`, after the turns that wait already.
pub fn queue_turn(t: &mut Tx<'_>, id: &str, text: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO queued_turns (agent, text, created_at) VALUES (?1, ?2, ?3)",
        params![id, text, now()],
    )?;
    Ok(())
}

/// True when agent `id` has a turn that waits in the store.
pub fn has_queued_turns(conn: &Connection, id: &str) -> ApiResult<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM queued_turns WHERE agent = ?1)",
        [id],
        |r| r.get(0),
    )?)
}

/// The queued turns of agent `id`, oldest first.
pub fn queued_turns(conn: &Connection, id: &str) -> ApiResult<Vec<QueuedTurn>> {
    let mut stmt =
        conn.prepare("SELECT id, text, created_at FROM queued_turns WHERE agent = ?1 ORDER BY id")?;
    let rows = stmt
        .query_map([id], |r| {
            Ok(QueuedTurn {
                id: r.get(0)?,
                text: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// True when the queued turn `turn` of agent `id` still waits.
pub fn queued_turn_waits(conn: &Connection, id: &str, turn: i64) -> ApiResult<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM queued_turns WHERE agent = ?1 AND id = ?2)",
        params![id, turn],
        |r| r.get(0),
    )?)
}

/// Delete the queued turn `turn` of agent `id` (TER-54). `None`: it does
/// not wait (any more): it went to the supervisor, or it never was.
pub fn remove_queued_turn(t: &mut Tx<'_>, id: &str, turn: i64) -> ApiResult<Option<QueuedTurn>> {
    Ok(t.query_row(
        "DELETE FROM queued_turns WHERE agent = ?1 AND id = ?2
             RETURNING id, text, created_at",
        params![id, turn],
        |r| {
            Ok(QueuedTurn {
                id: r.get(0)?,
                text: r.get(1)?,
                created_at: r.get(2)?,
            })
        },
    )
    .optional()?)
}

/// Delete the queued turns of agent `id` up to `upto` (the ID of the
/// last turn that went to the supervisor).
pub fn forget_queued_turns(t: &mut Tx<'_>, id: &str, upto: i64) -> ApiResult<()> {
    t.execute(
        "DELETE FROM queued_turns WHERE agent = ?1 AND id <= ?2",
        params![id, upto],
    )?;
    Ok(())
}

/// Set or clear the `paused` flag. Returns the flag before.
pub fn set_paused(t: &mut Tx<'_>, id: &str, paused: bool) -> ApiResult<bool> {
    let before: bool = t
        .query_row("SELECT paused FROM agents WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no agent `{id}`")))?;
    t.execute(
        "UPDATE agents SET paused = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, paused, now()],
    )?;
    Ok(before)
}

/// Note a turn start of agent `id`. The event pump calls it for each
/// `turn_start` event (PLAN §18, "Auto compact").
pub fn note_turn(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE agents SET last_turn_at = ?2 WHERE id = ?1",
        params![id, now()],
    )?;
    Ok(())
}

/// Record the end of a compaction of agent `id`, also a failed one.
pub fn note_compaction(t: &mut Tx<'_>, id: &str, ok: bool) -> ApiResult<()> {
    t.execute(
        "UPDATE agents SET last_compact_at = ?2, last_compact_ok = ?3 WHERE id = ?1",
        params![id, now(), ok],
    )?;
    Ok(())
}

/// What the auto compact of an agent reads from the store
/// ([`compact_clock`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactClock {
    /// A turn started after the last compaction and after the start of
    /// the current session.
    pub turned: bool,
    /// Where the age of the session context counts from (RFC 3339): the
    /// end of the last compaction, else the start of the current session.
    /// A resume keeps the start of its session. `None`: no current
    /// session.
    pub since: Option<String>,
}

/// The auto compact clock of agent `id` (PLAN §18, "Auto compact").
pub fn compact_clock(conn: &Connection, id: &str) -> ApiResult<CompactClock> {
    let (turn, compact): (Option<String>, Option<String>) = conn.query_row(
        "SELECT last_turn_at, last_compact_at FROM agents WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let started: Option<String> = conn
        .query_row(
            "SELECT started_at FROM sessions WHERE agent_id = ?1 AND current = 1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(started) = started else {
        return Ok(CompactClock {
            turned: false,
            since: None,
        });
    };
    // RFC 3339 with milliseconds in UTC (`db::now`) sorts as text.
    let since = match compact {
        Some(c) if c > started => c,
        _ => started,
    };
    Ok(CompactClock {
        turned: turn.is_some_and(|t| t > since),
        since: Some(since),
    })
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
    list_with(conn, project, page, Ephemeral::None)
}

/// Which ephemeral agents (PLAN §18, "Ephemeral agents") a list shows.
/// [`list`] leaves them out: they are not peers, not in the address book,
/// and not in the project's counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ephemeral {
    /// None of them.
    #[default]
    None,
    /// The card agents (TER-75): those bound to a card, and those that are
    /// the assignee of a card that is not archived.
    Card,
    /// All of them.
    All,
}

/// [`list`], with the ephemeral agents that `ephemeral` names.
pub fn list_with(
    conn: &Connection,
    project: Option<&str>,
    page: &Page,
    ephemeral: Ephemeral,
) -> ApiResult<Paged<AgentView>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(
        "SELECT * FROM agents WHERE id > ?1 AND (?2 IS NULL OR project = ?2)
           AND (owner IS NULL OR ?4
                OR (?5 AND (EXISTS (SELECT 1 FROM spawns s WHERE s.id = agents.id
                                      AND s.bound_kind = 'card' AND s.finished_at IS NULL)
                            OR EXISTS (SELECT 1 FROM cards c WHERE c.assignee = agents.id
                                         AND c.archived = 0))))
         ORDER BY id LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![
                page.cursor().unwrap_or(""),
                project,
                (limit + 1) as i64,
                ephemeral == Ephemeral::All,
                ephemeral == Ephemeral::Card
            ],
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
        Role::Architect => "architect",
    }
}

/// Replace the roles of an agent. `member` is implicit and is dropped.
pub fn set_roles(t: &mut Tx<'_>, id: &str, project: &str, wanted: &[Role]) -> ApiResult<Vec<Role>> {
    // A sysadmin reads and writes everything: `architect` (read only) with
    // it means nothing, and would hide which one applies (TER-47).
    if wanted.contains(&Role::Architect) && wanted.contains(&Role::Sysadmin) {
        return Err(ApiError::invalid(
            "an agent has sysadmin or architect, not both: sysadmin already reads everything",
        ));
    }
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
    if let Some(p) = &def.persona {
        terrarium_core::config::check_persona(p.trim()).map_err(ApiError::invalid)?;
    }
    terrarium_core::skills::clean_list(&def.skills).map_err(ApiError::invalid)?;
    terrarium_core::skills::clean_off_list(&def.skills_off).map_err(ApiError::invalid)?;
    Ok(())
}

/// A skills list as the store keeps it (`terrarium_core::skills::clean_list`).
fn stored_skills(list: &[String]) -> ApiResult<Vec<String>> {
    terrarium_core::skills::clean_list(list).map_err(ApiError::invalid)
}

/// A `skills_off` list as the store keeps it
/// (`terrarium_core::skills::clean_off_list`).
fn stored_skills_off(list: &[String]) -> ApiResult<Vec<String>> {
    terrarium_core::skills::clean_off_list(list).map_err(ApiError::invalid)
}

/// The persona as the store keeps it: `None` for an empty name.
fn stored_persona(name: Option<&String>) -> Option<String> {
    name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty())
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
    check_plan_logins(
        t,
        def.harness,
        def.subscription.as_deref(),
        &def.subscription_pool,
    )?;
    // A new agent never takes a disabled subscription (PLAN §16).
    if let Some(n) = def.subscription.as_deref()
        && let Some(why) = crate::store::subscriptions::find(t, n)?
            .as_ref()
            .and_then(crate::store::subscriptions::disabled_refusal)
    {
        return Err(ApiError::invalid(why));
    }
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
    agent.subscription_pool = clean_pool(&agent.subscription_pool);
    agent.workspace = crate::workspace::dir(work_dir, &def.project, &def.name);
    agent.system_prompt = stored_system_prompt(agent.system_prompt.as_ref()).cloned();
    agent.persona = stored_persona(agent.persona.as_ref());
    agent.skills = stored_skills(&agent.skills)?;
    agent.skills_off = stored_skills_off(&agent.skills_off)?;
    let id = agent.id.to_string();
    let at = now();
    t.execute(
        "INSERT INTO agents (id, name, project, harness, subscription, model, prompt, policy,
                             restart, status, workspace, source, created_at, updated_at,
                             new_session_on_prompt_change, template, system_prompt,
                             subscription_pool, persona, skills, idle_stop,
                             compact_on_idle, compact_max_age, skills_off, default_skills,
                             card_reminder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'stopped', ?10, ?11, ?12, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
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
            agent.system_prompt,
            serde_json::to_string(&agent.subscription_pool)?,
            agent.persona,
            serde_json::to_string(&agent.skills)?,
            agent.idle_stop.map(IdleStop::as_text),
            agent.compact_on_idle,
            agent.compact_max_age.map(CompactMaxAge::as_text),
            serde_json::to_string(&agent.skills_off)?,
            agent.default_skills,
            agent.card_reminder
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
    // The tools of the template (design Q4, TER-35): granted to the new
    // agent, as the delegation of the sysadmin who set them.
    if let Some(tref) = template {
        let spec = crate::store::templates::get(t, &tref.id)?.spec;
        let tools = terrarium_core::agent_template::tools_of(&spec).map_err(ApiError::invalid)?;
        crate::store::tools::grant_from_template(t, &id, &def.project, tref, &tools)?;
    }
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
    /// The account pool (PLAN §16.3). `[]` clears it.
    #[serde(default)]
    pub subscription_pool: Option<Vec<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub model: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub prompt: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub system_prompt: Option<Option<String>>,
    /// `null` or an empty name: by role (PLAN §8.1).
    #[serde(default, deserialize_with = "nullable")]
    pub persona: Option<Option<String>>,
    #[serde(default)]
    pub policy: Option<Policy>,
    #[serde(default)]
    pub restart: Option<RestartPolicy>,
    /// The idle stop (PLAN §18): `off`, or a duration. `null` goes back
    /// to the global default.
    #[serde(default, deserialize_with = "nullable")]
    pub idle_stop: Option<Option<IdleStop>>,
    /// Compact at the idle stop (PLAN §18, "Auto compact"). `null` goes
    /// back to the global default.
    #[serde(default, deserialize_with = "nullable")]
    pub compact_on_idle: Option<Option<bool>>,
    /// The maximum age of the session context: `off`, or a duration.
    /// `null` goes back to the global default.
    #[serde(default, deserialize_with = "nullable")]
    pub compact_max_age: Option<Option<CompactMaxAge>>,
    /// Remind the agent of its cards in the working column when it goes
    /// idle (PLAN §18, "Card reminder"). `null` goes back to the global
    /// default.
    #[serde(default, deserialize_with = "nullable")]
    pub card_reminder: Option<Option<bool>>,
    #[serde(default)]
    pub new_session_on_prompt_change: Option<bool>,
    /// The skills (PLAN §8.6): names of the skill library, or `*`. `[]`
    /// clears them. The API checks that each name is in the library.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// The skills that the agent does not get (PLAN §8.6): default-on or
    /// explicit skill names. `[]` clears them.
    #[serde(default)]
    pub skills_off: Option<Vec<String>>,
    /// Give the agent the default-on skills of the library.
    #[serde(default)]
    pub default_skills: Option<bool>,
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
            subscription_pool: Vec::new(),
            model: None,
            prompt: None,
            system_prompt: None,
            persona: None,
            skills: Vec::new(),
            skills_off: Vec::new(),
            default_skills: true,
            roles: Vec::new(),
            policy: policy.clone(),
            restart: RestartPolicy::default(),
            idle_stop: None,
            compact_on_idle: None,
            compact_max_age: None,
            card_reminder: None,
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
    let persona = p.persona.as_ref().map(|v| stored_persona(v.as_ref()));
    if let Some(Some(name)) = &persona {
        terrarium_core::config::check_persona(name).map_err(ApiError::invalid)?;
    }
    let skills = p.skills.as_deref().map(stored_skills).transpose()?;
    let skills_off = p.skills_off.as_deref().map(stored_skills_off).transpose()?;
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
            subscription_pool = coalesce(?15, subscription_pool),
            persona = CASE WHEN ?16 THEN ?17 ELSE persona END,
            skills = coalesce(?18, skills),
            idle_stop = CASE WHEN ?19 THEN ?20 ELSE idle_stop END,
            compact_on_idle = CASE WHEN ?21 THEN ?22 ELSE compact_on_idle END,
            compact_max_age = CASE WHEN ?23 THEN ?24 ELSE compact_max_age END,
            skills_off = coalesce(?25, skills_off),
            default_skills = coalesce(?26, default_skills),
            card_reminder = CASE WHEN ?27 THEN ?28 ELSE card_reminder END,
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
            p.subscription_pool
                .as_ref()
                .map(|v| serde_json::to_string(&clean_pool(v)))
                .transpose()?,
            persona.is_some(),
            persona.flatten(),
            skills.as_ref().map(serde_json::to_string).transpose()?,
            p.idle_stop.is_some(),
            p.idle_stop.flatten().map(IdleStop::as_text),
            p.compact_on_idle.is_some(),
            p.compact_on_idle.flatten(),
            p.compact_max_age.is_some(),
            p.compact_max_age.flatten().map(CompactMaxAge::as_text),
            skills_off.as_ref().map(serde_json::to_string).transpose()?,
            p.default_skills,
            p.card_reminder.is_some(),
            p.card_reminder.flatten(),
        ],
    )?;
    if p.harness.is_some() || p.subscription.is_some() || p.subscription_pool.is_some() {
        // The rows as they are now; an error rolls the write back.
        let a = get(t, id)?.agent;
        check_plan_logins(
            t,
            a.harness,
            a.subscription.as_deref(),
            &a.subscription_pool,
        )?;
    }
    Ok(())
}

/// Check an account pool for an agent on `harness` whose current
/// subscription is `current` (PLAN §16.3), and give it as it is stored.
/// pi takes no plan login ([`check_plan_logins`]). Each member must
/// exist, the harness must be able to spend it, and all
/// members and the current subscription must have one provider. The
/// current subscription joins the pool at the front when it is missing.
/// An empty pool stays empty.
pub fn check_pool(
    conn: &Connection,
    harness: HarnessId,
    current: Option<&str>,
    pool: &[String],
) -> ApiResult<Vec<String>> {
    let mut pool = clean_pool(pool);
    check_plan_logins(conn, harness, current, &pool)?;
    if pool.is_empty() {
        return Ok(pool);
    }
    if let Some(c) = current
        && !pool.iter().any(|p| p == c)
    {
        pool.insert(0, c.to_owned());
    }
    let mut provider: Option<String> = None;
    for name in &pool {
        let Some(s) = crate::store::subscriptions::find(conn, name)? else {
            return Err(ApiError::invalid(format!(
                "`subscription_pool` names `{name}`, but there is no such subscription"
            )));
        };
        if !s.harnesses.contains(&harness) {
            return Err(ApiError::invalid(format!(
                "the {harness} harness cannot spend subscription `{name}` of the pool"
            )));
        }
        match &provider {
            Some(p) if *p != s.provider => {
                return Err(ApiError::invalid(format!(
                    "the pool mixes providers: `{name}` is {}, the others are {p}; a pool \
                     holds accounts of one provider",
                    s.provider
                )));
            }
            Some(_) => {}
            None => provider = Some(s.provider.clone()),
        }
    }
    Ok(pool)
}

/// pi uses API keys only (PLAN §16.3): a pi agent's subscription and each
/// member of its pool must not be a plan login. `422 invalid` names the
/// first plan. A name with no subscription passes here; the start says why.
pub fn check_plan_logins(
    conn: &Connection,
    harness: HarnessId,
    current: Option<&str>,
    pool: &[String],
) -> ApiResult<()> {
    // pi takes no plan login; a self-hosted server takes only the
    // harnesses of its API style (`plan_refusal` says both).
    for name in current.into_iter().chain(pool.iter().map(String::as_str)) {
        if let Some(why) = crate::store::subscriptions::find(conn, name)?
            .and_then(|s| crate::store::subscriptions::plan_refusal(harness, &s))
        {
            return Err(ApiError::invalid(why));
        }
    }
    Ok(())
}

/// The subscription `name` exists (issue #14): an agent or a template
/// that names a missing subscription fails at its first wake, so the API
/// refuses it at once. `422 invalid` with `details.field = "subscription"`
/// names the subscriptions of `harness`. `None` (the default account of
/// the harness) is no error.
pub fn check_subscription_exists(
    conn: &Connection,
    harness: HarnessId,
    name: Option<&str>,
) -> ApiResult<()> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(());
    };
    if crate::store::subscriptions::find(conn, name)?.is_some() {
        return Ok(());
    }
    Err(crate::store::schedules::field_error(
        "subscription",
        missing_subscription(conn, harness, name)?,
    ))
}

/// The text for a missing subscription `name` of a `harness` agent: the
/// subscriptions that the harness can spend, or the hint to leave it
/// empty.
pub fn missing_subscription(
    conn: &Connection,
    harness: HarnessId,
    name: &str,
) -> ApiResult<String> {
    let names: Vec<String> = crate::store::subscriptions::list(conn)?
        .into_iter()
        .filter(|s| s.harnesses.contains(&harness))
        .map(|s| s.name)
        .collect();
    let empty = if harness == HarnessId::Pi {
        ""
    } else {
        ", or leave it empty for the default account of the harness"
    };
    Ok(if names.is_empty() {
        format!(
            "there is no subscription `{name}`, and there is no {harness} subscription. \
             Define one (`terrarium admin subscription new`){empty}"
        )
    } else {
        format!(
            "there is no subscription `{name}`; the {harness} subscriptions are: {}. \
             Name one of them{empty}",
            names.join(", ")
        )
    })
}

/// A pool as it is stored: no empty names, no duplicates, in the given
/// order.
pub fn clean_pool(pool: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in pool.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if !out.iter().any(|o| o == n) {
            out.push(n.to_owned());
        }
    }
    out
}

/// The subscription that a file or fleet definition gives an agent whose
/// current subscription is `current`. With a pool that holds both, the
/// current one stays: the definition names the account that the agent
/// starts on, and a swap picks among the pool (PLAN §16.3).
pub fn synced_subscription(current: Option<&str>, def: &AgentDef) -> Option<String> {
    match current {
        Some(c)
            if def.subscription_pool.iter().any(|p| p == c)
                && def
                    .subscription
                    .as_deref()
                    .is_none_or(|d| def.subscription_pool.iter().any(|p| p == d)) =>
        {
            Some(c.to_owned())
        }
        _ => def.subscription.clone(),
    }
}

/// Set the current subscription of an agent (a swap, PLAN §16.3).
pub fn set_subscription(t: &mut Tx<'_>, id: &str, subscription: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE agents SET subscription = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, subscription, now()],
    )?;
    Ok(())
}

pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let a = get(t, id)?;
    super::reservations::guard_identity(t, "agent", id)?;
    crate::store::waits::on_agent_deleted(t, id)?;
    // The monitors and schedules that run as it: disabled, with a notice
    // to the project leads and the operator (`crate::owner`).
    crate::owner::on_agent_deleted(t, id, &format!("{}/{}", a.agent.project, a.agent.name))?;
    crate::store::questions::on_agent_deleted(t, id)?;
    // Its secret grants (PLAN §16b.3): a new agent with the same name
    // gets none of them.
    crate::secrets::grants::on_agent_deleted(t, id)?;
    // Its tool grants (`crate::store::tools`).
    crate::store::tools::on_agent_deleted(t, id)?;
    // The sessions stay for the card links (migration 37); the session
    // files go to the archive after the commit (`crate::session_archive`).
    crate::store::session_refs::on_agent_deleted(t, id, a.agent.harness.as_str())?;
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
    // The lists of the dashboard and of other clients drop the agent on
    // this event; without it, the agent stays with its last status.
    t.emit(
        crate::events::EventDraft::new(
            "agent_removed",
            format!("{}/{}: removed", a.agent.project, a.agent.name),
            json!({"agent_id": id, "project": a.agent.project, "name": a.agent.name}),
        )
        .agent(id, a.agent.name.clone())
        .project(a.agent.project.clone()),
    )?;
    Ok(())
}

/// Set the status and its reason. Returns the status before.
pub fn set_status(
    t: &mut Tx<'_>,
    id: &str,
    status: AgentStatus,
    reason: Option<&str>,
) -> ApiResult<AgentStatus> {
    let before: String = t
        .query_row("SELECT status FROM agents WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no agent `{id}`")))?;
    t.execute(
        "UPDATE agents SET status = ?2, status_reason = ?4, updated_at = ?3 WHERE id = ?1",
        params![id, status.as_str(), now(), reason],
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
    /// How many times the daemon resumed this harness session: each
    /// resume reuses the row (PLAN §18).
    pub resumes: i64,
    /// The start of the last resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<String>,
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
        resumes: r.get("resumes")?,
        resumed_at: r.get("resumed_at")?,
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

/// End the current session and open the session of a start that resumes
/// `resume` (PLAN §18): the agent's row of that harness session becomes
/// current again at once. Its end goes, `resumes` goes up by one, and
/// `resumed_at` is now. Answers the ID of the current row.
///
/// So a resume has no short row of its own. A short row went when the
/// harness reported its session, and a reader that had its ID (the
/// dashboard's Conversation tab) got `404` (issue #8). With no `resume`, or
/// no row of it, a new row opens, as [`begin_session`] does.
pub fn reopen_session(
    t: &mut Tx<'_>,
    agent: &str,
    outcome_of_previous: &str,
    resume: Option<&str>,
) -> ApiResult<String> {
    let row: Option<String> = match resume {
        Some(h) => t
            .query_row(
                "SELECT id FROM sessions WHERE agent_id = ?1 AND harness_session_id = ?2
                 ORDER BY id LIMIT 1",
                params![agent, h],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    };
    let Some(id) = row else {
        return begin_session(t, agent, outcome_of_previous);
    };
    end_session(t, agent, outcome_of_previous)?;
    t.execute(
        "UPDATE sessions SET current = 1, resumes = resumes + 1, resumed_at = ?2,
                ended_at = NULL, end_reason = NULL
         WHERE id = ?1",
        params![id, now()],
    )?;
    Ok(id)
}

/// End the current session and open the session of a start that resumes
/// the last harness session ([`last_harness_session`], then
/// [`reopen_session`]). Answers the harness session to resume, if any.
pub fn begin_resume(
    t: &mut Tx<'_>,
    agent: &str,
    outcome_of_previous: &str,
) -> ApiResult<Option<HarnessSessionId>> {
    let resume = last_harness_session(t, agent)?;
    reopen_session(
        t,
        agent,
        outcome_of_previous,
        resume.as_ref().map(|h| h.0.as_str()),
    )?;
    Ok(resume)
}

/// Record the harness session that the current session runs (PLAN §18).
///
/// - The same harness session again (a compact, a second report): no
///   change.
/// - A resume of a harness session that an older row of the agent has:
///   that row is the session. It becomes current again, `resumes` goes up
///   by one, `resumed_at` is now, and its end goes. The events of the new
///   row move to it, and the new row goes. So one harness session is one
///   row, however often the agent restarts.
/// - A new harness session on a row that has another one already (the
///   harness began a new conversation): that row ends (`new_session`),
///   and a new row holds the new one.
/// - Else the current row takes the harness session.
pub fn set_harness_session(t: &mut Tx<'_>, agent: &str, harness: &str) -> ApiResult<()> {
    let current: Option<(String, Option<String>)> = t
        .query_row(
            "SELECT id, harness_session_id FROM sessions WHERE agent_id = ?1 AND current = 1",
            [agent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((id, had)) = current else {
        return Ok(());
    };
    match had.as_deref() {
        Some(h) if h == harness => return Ok(()),
        Some(_) => {
            let id = begin_session(t, agent, "new_session")?;
            t.execute(
                "UPDATE sessions SET harness_session_id = ?2 WHERE id = ?1",
                params![id, harness],
            )?;
            return Ok(());
        }
        None => {}
    }
    let older: Option<String> = t
        .query_row(
            "SELECT id FROM sessions
             WHERE agent_id = ?1 AND harness_session_id = ?2 AND id != ?3
             ORDER BY id LIMIT 1",
            params![agent, harness, id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(old) = older else {
        t.execute(
            "UPDATE sessions SET harness_session_id = ?2 WHERE id = ?1",
            params![id, harness],
        )?;
        return Ok(());
    };
    t.execute(
        "UPDATE events SET session_id = ?2 WHERE session_id = ?1",
        params![id, old],
    )?;
    t.execute("DELETE FROM sessions WHERE id = ?1", [&id])?;
    t.execute(
        "UPDATE sessions SET current = 1, resumes = resumes + 1, resumed_at = ?2,
                ended_at = NULL, end_reason = NULL
         WHERE id = ?1",
        params![old, now()],
    )?;
    Ok(())
}

/// The fields a file definition sets, for sync.
pub fn sync_def(t: &mut Tx<'_>, id: &str, def: &AgentDef) -> ApiResult<bool> {
    check_def(def)?;
    let before = get(t, id)?;
    let system_prompt = stored_system_prompt(def.system_prompt.as_ref());
    let subscription = synced_subscription(before.agent.subscription.as_deref(), def);
    let pool = clean_pool(&def.subscription_pool);
    check_plan_logins(t, def.harness, subscription.as_deref(), &pool)?;
    let persona = stored_persona(def.persona.as_ref());
    let skills = stored_skills(&def.skills)?;
    let skills_off = stored_skills_off(&def.skills_off)?;
    let changed = before.source != projects::SOURCE_FILE
        || before.agent.harness != def.harness
        || before.agent.subscription != subscription
        || before.agent.subscription_pool != pool
        || before.agent.model != def.model
        || before.agent.prompt != def.prompt
        || before.agent.system_prompt.as_ref() != system_prompt
        || before.agent.persona != persona
        || before.agent.skills != skills
        || before.agent.skills_off != skills_off
        || before.agent.default_skills != def.default_skills
        || before.agent.policy != def.policy
        || before.agent.restart != def.restart
        || before.agent.idle_stop != def.idle_stop
        || before.agent.compact_on_idle != def.compact_on_idle
        || before.agent.compact_max_age != def.compact_max_age
        || before.agent.card_reminder != def.card_reminder;
    if changed {
        t.execute(
            "UPDATE agents SET harness = ?2, subscription = ?3, model = ?4, prompt = ?5,
                    policy = ?6, restart = ?7, source = 'file', updated_at = ?8,
                    new_session_on_prompt_change = ?9, system_prompt = ?10,
                    subscription_pool = ?11, persona = ?12, skills = ?13, idle_stop = ?14,
                    compact_on_idle = ?15, compact_max_age = ?16, skills_off = ?17,
                    default_skills = ?18, card_reminder = ?19
             WHERE id = ?1",
            params![
                id,
                def.harness.as_str(),
                subscription,
                def.model,
                def.prompt,
                serde_json::to_string(&def.policy)?,
                restart_str(def.restart),
                now(),
                def.new_session_on_prompt_change,
                system_prompt,
                serde_json::to_string(&pool)?,
                persona,
                serde_json::to_string(&skills)?,
                def.idle_stop.map(IdleStop::as_text),
                def.compact_on_idle,
                def.compact_max_age.map(CompactMaxAge::as_text),
                serde_json::to_string(&skills_off)?,
                def.default_skills,
                def.card_reminder
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events::EventHub;

    fn db() -> Db {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 't', 't');",
        )
        .unwrap();
        Db::new(conn, EventHub::default())
    }

    /// One start of the agent: a new row, then the harness reports `h`.
    /// A turn in it, as an event on the current row.
    async fn run(db: &Db, outcome: &'static str, h: &'static str) {
        db.write("system", move |t| {
            begin_session(t, "A1", outcome)?;
            set_harness_session(t, "A1", h)?;
            let s = current_session(t, "A1")?;
            t.execute(
                "INSERT INTO events (id, agent_id, session_id, kind, payload, at)
                 VALUES (?1, 'A1', ?2, 'turn_start', '{}', 't')",
                params![ulid::Ulid::new().to_string(), s],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    async fn rows(db: &Db) -> Vec<Session> {
        db.read(|c| sessions(c, "A1", &Page::default()))
            .await
            .unwrap()
            .items
    }

    /// A resume of the same harness session reuses its row (PLAN §18):
    /// two resumes give one row with `resumes` 2 and the turns of all
    /// three starts. A new harness session is a new row.
    #[tokio::test]
    async fn a_resume_reuses_the_row_of_its_harness_session() {
        let db = db();
        run(&db, "superseded", "H1").await;
        run(&db, "restarted", "H1").await;
        run(&db, "daemon_restart", "H1").await;
        let r = rows(&db).await;
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].harness_session_id.as_deref(), Some("H1"));
        assert_eq!((r[0].resumes, r[0].turns), (2, 3));
        assert!(r[0].current && r[0].ended_at.is_none() && r[0].outcome.is_none());
        assert!(r[0].resumed_at.is_some());
        // The same id again (a compact): no change.
        db.write("system", |t| set_harness_session(t, "A1", "H1"))
            .await
            .unwrap();
        assert_eq!(rows(&db).await[0].resumes, 2);

        // A new harness session on the next start: a new row.
        run(&db, "restarted", "H2").await;
        let r = rows(&db).await;
        assert_eq!(r.len(), 2, "{r:?}");
        let by = |r: &[Session], h: &str| {
            r.iter()
                .find(|s| s.harness_session_id.as_deref() == Some(h))
                .cloned()
                .unwrap()
        };
        let h2 = by(&r, "H2");
        assert_eq!((h2.resumes, h2.turns, h2.current), (0, 1, true));
        let h1 = by(&r, "H1");
        assert_eq!(h1.outcome.as_deref(), Some("restarted"));
        assert_eq!((h1.resumes, h1.turns, h1.current), (2, 3, false));

        // A resume of the older session: its row is current again.
        run(&db, "restarted", "H1").await;
        let r = rows(&db).await;
        assert_eq!(r.len(), 2, "{r:?}");
        let h1 = by(&r, "H1");
        assert_eq!((h1.resumes, h1.turns, h1.current), (3, 4, true));
        assert!(!by(&r, "H2").current);

        // The harness begins a new conversation in a running session: the
        // row ends, and a new row holds the new one.
        db.write("system", |t| set_harness_session(t, "A1", "H3"))
            .await
            .unwrap();
        let r = rows(&db).await;
        assert_eq!(r.len(), 3, "{r:?}");
        // Rows made in the same millisecond have no fixed order: find
        // them by their harness session, not by position.
        let h3 = r
            .iter()
            .find(|s| s.harness_session_id.as_deref() == Some("H3"));
        assert!(h3.unwrap().current, "{r:?}");
        assert_eq!(r.iter().filter(|s| s.current).count(), 1, "{r:?}");
        let h1 = r
            .iter()
            .find(|s| s.harness_session_id.as_deref() == Some("H1"));
        assert_eq!(h1.unwrap().outcome.as_deref(), Some("new_session"));
    }

    /// Issue #8: a start that resumes opens the row of its harness session
    /// at once. The ID that a reader got from the wake is still the
    /// current row after the harness reported the session.
    #[tokio::test]
    async fn a_resume_opens_its_row_before_the_harness_reports() {
        let db = db();
        run(&db, "superseded", "H1").await;
        let first = rows(&db).await[0].id.clone();
        db.write("system", |t| end_session(t, "A1", "idle"))
            .await
            .unwrap();

        let resume = db
            .write("system", |t| begin_resume(t, "A1", "superseded"))
            .await
            .unwrap();
        assert_eq!(resume.map(|h| h.0).as_deref(), Some("H1"));
        let r = rows(&db).await;
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].id, first);
        assert!(r[0].current && r[0].ended_at.is_none() && r[0].outcome.is_none());
        assert_eq!(r[0].resumes, 1);

        db.write("system", |t| set_harness_session(t, "A1", "H1"))
            .await
            .unwrap();
        let r = rows(&db).await;
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!((r[0].id.as_str(), r[0].current), (first.as_str(), true));
        assert_eq!(r[0].resumes, 1);

        // A relaunch of the restart policy names the session it resumes.
        db.write("system", |t| reopen_session(t, "A1", "crashed", Some("H1")))
            .await
            .unwrap();
        let r = rows(&db).await;
        assert_eq!((r.len(), r[0].resumes, r[0].current), (1, 2, true));

        // No harness session to resume: a new row.
        db.write("system", |t| reopen_session(t, "A1", "resume_failed", None))
            .await
            .unwrap();
        let r = rows(&db).await;
        assert_eq!(r.len(), 2, "{r:?}");
        let old = r.iter().find(|s| s.id == first).unwrap();
        assert_eq!(old.outcome.as_deref(), Some("resume_failed"));
        assert!(!old.current);
    }
}
