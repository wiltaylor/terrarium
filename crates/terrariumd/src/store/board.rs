//! The kanban board (PLAN §13) and the rules the daemon enforces on it.
//!
//! - Every change is an event with an author.
//! - An agent may create cards, claim unassigned cards, move and edit its own
//!   cards, and comment on any card of its project. It may not move or
//!   reassign another agent's card. Project admins and the operator may.
//! - The columns are configurable ([`set_columns`]). `backlog` is always the
//!   first column and `done` always the last. Each other column can have a
//!   WIP limit and a role: exactly one column has the role `working`, and
//!   at most one has the role `blocked`. The rules below follow the roles,
//!   not the column names.
//! - A card with unmet dependencies cannot enter the `working` column.
//! - A column at its WIP limit refuses one more card.
//! - Moving a card to the `blocked` column needs a comment, and mails the
//!   operator and the card's creator.
//! - `done` is terminal for members. On `done`, every linked mail thread gets
//!   a notice, and dependent cards that are now free raise `unblocked`.
//! - A move of a card that an Inbox thread tracks can wake the thread's
//!   agent ([`crate::store::threads::on_card_moved`], TER-148).

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::session_refs::{self, SessionRef};
use crate::store::{Page, Paged, agents, github_gate, json_col, questions};

/// The columns of a new board, in order, with their roles.
pub const DEFAULT_COLUMNS: [(&str, Option<&str>); 9] = [
    ("backlog", None),
    ("ready", None),
    ("in_progress", Some(WORKING)),
    ("validating", Some(VALIDATING)),
    ("review", Some(REVIEW)),
    ("merge_queue", Some(MERGE_QUEUE)),
    ("accepted", Some(ACCEPTED)),
    ("blocked", Some(BLOCKED)),
    ("done", None),
];

/// The first column. New cards go here. It cannot be removed, renamed, or
/// moved, and takes no WIP limit and no role.
pub const BACKLOG: &str = "backlog";
/// The last column, as `backlog` is the first.
pub const DONE: &str = "done";
/// The role of the column that `card claim` moves a card to. Also the
/// alias of that column in a move or a filter.
pub const WORKING: &str = "working";
/// The role of the column where a move needs a comment and mails a notice.
/// Also the alias of that column in a move or a filter.
pub const BLOCKED: &str = "blocked";
/// The role of the column where a validate job holds a card. The daemon
/// finds it by this role, else by the name `validating`.
pub const VALIDATING: &str = "validating";
/// The role of the column where a validate pass goes. The daemon finds it
/// by this role, else by the name `review`.
pub const REVIEW: &str = "review";
/// The role of the column where cards wait for the batch merge. The daemon
/// moves no card there: the opt-in merge queue rules do (TER-172,
/// `rules_file::MERGE_QUEUE_RULES`).
pub const MERGE_QUEUE: &str = "merge_queue";
/// The role of the column where a merge pass goes. The daemon finds it by
/// this role, else by the name `accepted`.
pub const ACCEPTED: &str = "accepted";
/// The roles a column can have. At most one column has each role, except
/// that exactly one has `working`.
pub const ROLES: [&str; 6] = [WORKING, BLOCKED, VALIDATING, REVIEW, MERGE_QUEUE, ACCEPTED];
/// The roles that are also a name alias in a move or a filter. A column
/// with one of these names must have the role of that name.
const ALIAS_ROLES: [&str; 2] = [WORKING, BLOCKED];
/// The most characters of a column title.
pub const TITLE_MAX: usize = 40;

/// Is `name` one of the two fixed columns?
pub fn is_fixed(name: &str) -> bool {
    name == BACKLOG || name == DONE
}

/// The title of a column with no stored title: the name with spaces for
/// the underscores (`in_progress` -> `in progress`).
pub fn default_title(name: &str) -> String {
    name.replace('_', " ")
}

/// A new column name: a lower-case letter, then at most 31 lower-case
/// letters, digits, or underscores.
pub fn valid_column_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// The display label: the stored title, or [`default_title`].
    pub title: String,
    pub position: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wip_limit: Option<i64>,
    /// `working`, `blocked`, or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Cards in the column, archived cards not counted.
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    /// `operator`, `project:<name>`, or an agent ULID.
    pub author: String,
    pub author_name: String,
    pub body: String,
    pub created_at: String,
    /// The session of the agent that wrote the comment; `null` for the
    /// operator. It stays after the agent is deleted (`removed: true`).
    #[serde(default)]
    pub session: Option<SessionRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// `thread`, `branch`, `card`, or `url`.
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub created_by: String,
    pub created_at: String,
}

fn normal_card_kind() -> String {
    "card".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    #[serde(default = "normal_card_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<crate::store::groups::Group>,
    pub id: String,
    pub project: String,
    pub column: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_role: Option<String>,
    pub position: i64,
    pub title: String,
    pub body: String,
    /// `operator` or an agent ULID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_name: Option<String>,
    /// The card agent (TER-75): the ULID of the open ephemeral agent bound
    /// to the card (`agent spawn --card`). It stays while the card waits
    /// and the assignee is cleared. Absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub depends_on: Vec<String>,
    /// The dependencies that are neither accepted nor done yet.
    pub blocked_by: Vec<String>,
    pub comments: Vec<Comment>,
    pub links: Vec<Link>,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
    pub updated_at: String,
    pub archived: bool,
    /// When the work is expected to be done (RFC 3339, UTC). Every move
    /// to another column clears it (TER-155 D1). A job with a planned
    /// duration sets it when the job starts (D2). It stays when the card
    /// is archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_at: Option<String>,
    /// When the ETA was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_set_at: Option<String>,
    /// The address that set the ETA: `operator` or an agent ULID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_set_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_set_by_name: Option<String>,
    /// When the card last moved to `done`. A reopen clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<String>,
    /// The latest test run linked to the card (PLAN §15.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<crate::store::test_runs::CardTests>,
    /// The idle mark (TER-67, `crate::idle_alert`): the assignee rests with
    /// no wait since this time, past the project's idle alert, and the
    /// leads were told. It clears when the assignee works again or waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<String>,
    /// The open (queued or running) job of the card, with its queue place
    /// (TER-158, D4). Absent when the card has no open job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<crate::store::jobs::CardJob>,
    /// The stored status of the assignee agent (`working`, `idle`,
    /// `waiting`, `blocked`, `stopped`, ...). Absent for the operator, no
    /// assignee, or a removed agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub project: String,
    pub card_prefix: String,
    pub columns: Vec<Column>,
    /// Every card that is not archived, by column then position.
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone)]
struct ColumnRow {
    id: i64,
    name: String,
    title: Option<String>,
    wip_limit: Option<i64>,
    role: Option<String>,
}

impl ColumnRow {
    fn is(&self, role: &str) -> bool {
        self.role.as_deref() == Some(role)
    }
}

#[derive(Debug, Clone)]
struct CardRow {
    kind: String,
    id: String,
    project: String,
    column_id: i64,
    column: String,
    /// The role of the card's column.
    column_role: Option<String>,
    position: i64,
    title: String,
    body: String,
    assignee: Option<String>,
    labels: String,
    repo: Option<String>,
    branch: Option<String>,
    created_by: String,
    created_at: String,
    updated_at: String,
    archived: bool,
    eta_at: Option<String>,
    eta_set_at: Option<String>,
    eta_set_by: Option<String>,
    done_at: Option<String>,
    idle_since: Option<String>,
}

fn card_row(r: &Row<'_>) -> rusqlite::Result<CardRow> {
    Ok(CardRow {
        kind: r.get("kind")?,
        id: r.get("id")?,
        project: r.get("project")?,
        column_id: r.get("column_id")?,
        column: r.get("column_name")?,
        column_role: r.get("column_role")?,
        position: r.get("position")?,
        title: r.get("title")?,
        body: r.get("body")?,
        assignee: r.get("assignee")?,
        labels: r.get("labels")?,
        repo: r.get("repo")?,
        branch: r.get("branch")?,
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        archived: r.get::<_, i64>("archived")? != 0,
        eta_at: r.get("eta_at")?,
        eta_set_at: r.get("eta_set_at")?,
        eta_set_by: r.get("eta_set_by")?,
        done_at: r.get("done_at")?,
        idle_since: r.get("idle_since")?,
    })
}

const CARD_SELECT: &str = "SELECT c.*, k.name AS column_name, k.role AS column_role FROM cards c
                           JOIN board_columns k ON k.id = c.column_id";

/// A readable name for a stored address.
fn name_of(conn: &Connection, key: &str) -> ApiResult<String> {
    if key == "operator" || key.starts_with("project:") {
        return Ok(key.to_owned());
    }
    Ok(agents::known_name(conn, key)?.unwrap_or_else(|| key.to_owned()))
}

/// Make the board of a new project with the default columns.
pub fn create_board(t: &mut Tx<'_>, project: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO boards (project, created_at) VALUES (?1, ?2)",
        params![project, now()],
    )?;
    for (i, (name, role)) in DEFAULT_COLUMNS.iter().enumerate() {
        t.execute(
            "INSERT INTO board_columns (project, name, position, role) VALUES (?1, ?2, ?3, ?4)",
            params![project, name, i as i64, role],
        )?;
    }
    Ok(())
}

fn column_rows(conn: &Connection, project: &str) -> ApiResult<Vec<ColumnRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, title, wip_limit, role FROM board_columns WHERE project = ?1
         ORDER BY position",
    )?;
    let rows = stmt
        .query_map([project], |r| {
            Ok(ColumnRow {
                id: r.get(0)?,
                name: r.get(1)?,
                title: r.get(2)?,
                wip_limit: r.get(3)?,
                role: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The columns of the board of `project` as `(name, role)`, in order. The
/// rules file check (`crate::store::rules`) reads them.
pub fn column_roles(conn: &Connection, project: &str) -> ApiResult<Vec<(String, Option<String>)>> {
    Ok(column_rows(conn, project)?
        .into_iter()
        .map(|c| (c.name, c.role))
        .collect())
}

fn names_of(cols: &[ColumnRow]) -> String {
    cols.iter()
        .map(|c| match &c.role {
            Some(r) => format!("{} ({r})", c.name),
            None => c.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Find a column by its name, or by a role alias: `working` or `blocked`
/// names the column with that role. A column name wins over an alias.
fn column(conn: &Connection, project: &str, name: &str) -> ApiResult<ColumnRow> {
    let mut cols = column_rows(conn, project)?;
    let names = names_of(&cols);
    if let Some(i) = cols.iter().position(|c| c.name == name) {
        return Ok(cols.swap_remove(i));
    }
    if ROLES.contains(&name) {
        return cols.into_iter().find(|c| c.is(name)).ok_or_else(|| {
            ApiError::invalid(format!(
                "the board of `{project}` has no `{name}` column (no column has the role \
                 `{name}`); columns: {names}"
            ))
        });
    }
    Err(ApiError::invalid(format!(
        "no column `{name}`; columns: {names}"
    )))
}

/// The column with the role `role` on the board of `project`, never a
/// column that only has the name (422 when no column has the role).
fn role_column(conn: &Connection, project: &str, role: &str) -> ApiResult<ColumnRow> {
    let cols = column_rows(conn, project)?;
    let names = names_of(&cols);
    cols.into_iter().find(|c| c.is(role)).ok_or_else(|| {
        ApiError::invalid(format!(
            "the board of `{project}` has no column with the role `{role}`; columns: {names}"
        ))
    })
}

/// The name of the column that `name` means on the board of `project`: a
/// column name, or a role alias (`working`, `blocked`). 422 when there is
/// no such column.
pub fn resolve_column(conn: &Connection, project: &str, name: &str) -> ApiResult<String> {
    Ok(column(conn, project, name)?.name)
}

/// The role of the column `name` of `project`, if it has one.
pub fn role_of(conn: &Connection, project: &str, name: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT role FROM board_columns WHERE project = ?1 AND name = ?2",
            params![project, name],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// The name of the column that the daemon uses for `role` on the board of
/// `project`: the column with that role, else the column named `fallback`
/// (a board that gives no column the role). When neither exists, the
/// name is `fallback`, so the move that follows fails and says so.
pub fn role_name(
    conn: &Connection,
    project: &str,
    role: &str,
    fallback: &str,
) -> ApiResult<String> {
    let cols = column_rows(conn, project)?;
    Ok(cols
        .into_iter()
        .find(|c| c.is(role))
        .map_or_else(|| fallback.to_owned(), |c| c.name))
}

fn active_count(conn: &Connection, column_id: i64) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM cards WHERE column_id = ?1 AND archived = 0 AND kind = 'card'",
        [column_id],
        |r| r.get(0),
    )?)
}

/// Card IDs are upper case; accept any case.
pub fn normalise_id(id: &str) -> String {
    id.trim().to_ascii_uppercase()
}

fn find_row(conn: &Connection, id: &str) -> ApiResult<CardRow> {
    let id = normalise_id(id);
    conn.query_row(&format!("{CARD_SELECT} WHERE c.id = ?1"), [&id], card_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no card `{id}`")))
}

/// The cards of `agent` in the working column of their board (the column
/// with the role [`WORKING`]), not archived: `(id, eta_at)`, in card
/// order. For the card reminder (`crate::card_reminder`).
pub fn working_cards_of(
    conn: &Connection,
    agent: &str,
) -> ApiResult<Vec<(String, Option<String>)>> {
    let mut stmt = conn.prepare(&format!(
        "{CARD_SELECT} WHERE c.assignee = ?1 AND k.role = ?2 AND c.archived = 0
         ORDER BY c.project, c.number"
    ))?;
    let rows = stmt
        .query_map(params![agent, WORKING], card_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(|c| (c.id, c.eta_at)).collect())
}

/// Record that the daemon reminded the assignee of card `id` to update it
/// (PLAN §18, "Card reminder"): a `card` event with the change
/// `reminded`, so the card history shows it. It wakes no agent and types
/// no pointer.
pub fn note_reminder(t: &mut Tx<'_>, id: &str, eta_passed: bool) -> ApiResult<()> {
    let card = find_row(t, id)?;
    let summary = format!("{} reminder to update it sent to the assignee", card.id);
    card_event_as(
        t,
        &card,
        "card",
        "reminded",
        Some(summary),
        json!({"eta_passed": eta_passed}),
    )
}

/// The project of a card, for permission checks before a write.
pub fn project_of(conn: &Connection, id: &str) -> ApiResult<String> {
    Ok(find_row(conn, id)?.project)
}

/// The label that marks the card a trigger keeps open for `key` (a `card`
/// action, docs/api.md §19): `trigger:<key>`.
pub fn trigger_label(key: &str) -> String {
    format!("trigger:{key}")
}

/// The newest card of `project` with the label `label` that is not in
/// `done` and not archived.
/// The newest open card with `label`. Cards that the GitHub author gate
/// holds (TER-85) are skipped, so a schedule `card` action never finds,
/// moves or closes one.
pub fn find_open_by_label(
    conn: &Connection,
    project: &str,
    label: &str,
) -> ApiResult<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.title, c.labels FROM cards c JOIN board_columns col ON col.id = c.column_id
         WHERE c.project = ?1 AND c.archived = 0 AND col.name <> ?3
           AND EXISTS (SELECT 1 FROM json_each(c.labels) WHERE value = ?2)
         ORDER BY c.number DESC",
    )?;
    let rows = stmt.query_map(params![project, label, DONE], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, title, labels) = row?;
        if !github_gate::is_held(&title, &json_col::<Vec<String>>(&labels)) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Whether the GitHub author gate holds the card `id` (TER-85).
pub fn is_held(conn: &Connection, id: &str) -> ApiResult<bool> {
    Ok(card_is_held(&find_row(conn, id)?))
}

fn deps(conn: &Connection, id: &str) -> ApiResult<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT depends_on FROM card_deps WHERE card = ?1 ORDER BY depends_on")?;
    let rows = stmt
        .query_map([id], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(rows)
}

/// Dependencies that are neither done nor archived.
fn unmet(conn: &Connection, id: &str) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT d.depends_on FROM card_deps d
         JOIN cards c ON c.id = d.depends_on
         JOIN board_columns k ON k.id = c.column_id
         WHERE d.card = ?1 AND c.archived = 0
           AND coalesce(k.role, k.name) NOT IN ('accepted', 'done')
         ORDER BY d.depends_on",
    )?;
    let rows = stmt
        .query_map([id], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(rows)
}

fn render(conn: &Connection, row: CardRow) -> ApiResult<Card> {
    let mut stmt = conn.prepare(
        "SELECT id, author, body, created_at, session_id FROM card_comments WHERE card = ?1
         ORDER BY created_at, id",
    )?;
    let raw = stmt
        .query_map([&row.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut comments = Vec::new();
    for (id, author, body, created_at, session) in raw {
        let session = match session {
            Some(s) => session_refs::get(conn, &s)?.map(|r| r.session),
            None => None,
        };
        comments.push(Comment {
            id,
            author_name: name_of(conn, &author)?,
            author,
            body,
            created_at,
            session,
        });
    }
    let mut stmt = conn.prepare(
        "SELECT kind, ref, created_by, created_at FROM card_links WHERE card = ?1
         ORDER BY created_at, kind, ref",
    )?;
    let links = stmt
        .query_map([&row.id], |r| {
            Ok(Link {
                kind: r.get(0)?,
                reference: r.get(1)?,
                created_by: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let assignee_name = row
        .assignee
        .as_deref()
        .map(|a| name_of(conn, a))
        .transpose()?;
    let bound = crate::store::spawns::open_bound(
        conn,
        &crate::store::spawns::Bound::new(crate::bound::KIND_CARD, &row.id),
    )?;
    let eta_set_by_name = row
        .eta_set_by
        .as_deref()
        .map(|a| name_of(conn, a))
        .transpose()?;
    let assignee_status = match row.assignee.as_deref() {
        Some(a) if a != "operator" => conn
            .query_row("SELECT status FROM agents WHERE id = ?1", [a], |r| {
                r.get::<_, String>(0)
            })
            .optional()?,
        _ => None,
    };
    Ok(Card {
        kind: row.kind,
        group: crate::store::groups::for_card(conn, &row.id)?,
        tests: crate::store::test_runs::card_chip(conn, &row.id)?,
        job: crate::store::jobs::card_job(conn, &row.id)?,
        assignee_status,
        depends_on: deps(conn, &row.id)?,
        blocked_by: unmet(conn, &row.id)?,
        created_by_name: name_of(conn, &row.created_by)?,
        id: row.id,
        project: row.project,
        column: row.column,
        column_role: row.column_role,
        position: row.position,
        title: row.title,
        body: row.body,
        assignee: row.assignee,
        assignee_name,
        agent: bound.as_ref().map(|b| b.id.clone()),
        agent_name: bound.map(|b| b.name),
        labels: json_col(&row.labels),
        repo: row.repo,
        branch: row.branch,
        comments,
        links,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        archived: row.archived,
        eta_at: row.eta_at,
        eta_set_at: row.eta_set_at,
        eta_set_by: row.eta_set_by,
        eta_set_by_name,
        done_at: row.done_at,
        idle_since: row.idle_since,
    })
}

pub fn get_card(conn: &Connection, id: &str) -> ApiResult<Card> {
    let row = find_row(conn, id)?;
    render(conn, row)
}

pub fn get_board(conn: &Connection, project: &str) -> ApiResult<Board> {
    let prefix = crate::store::projects::get(conn, project)?.card_prefix;
    let mut columns = Vec::new();
    for (i, c) in column_rows(conn, project)?.into_iter().enumerate() {
        columns.push(Column {
            count: active_count(conn, c.id)?,
            title: c.title.unwrap_or_else(|| default_title(&c.name)),
            name: c.name,
            position: i as i64,
            wip_limit: c.wip_limit,
            role: c.role,
        });
    }
    let mut stmt = conn.prepare(&format!(
        "{CARD_SELECT} WHERE c.project = ?1 AND c.archived = 0 ORDER BY k.position, c.position"
    ))?;
    let rows = stmt
        .query_map([project], card_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let cards = rows
        .into_iter()
        .map(|r| render(conn, r))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Board {
        project: project.to_owned(),
        card_prefix: prefix,
        columns,
        cards,
    })
}

/// The board read model never loads descriptions, comment bodies, links or test details.
/// Done is a separate, bounded page. Counts always describe the whole board.
pub fn board_summary(
    conn: &Connection,
    project: &str,
    done: bool,
    page: &Page,
) -> ApiResult<Value> {
    let prefix = crate::store::projects::get(conn, project)?.card_prefix;
    let columns = column_rows(conn, project)?
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            Ok(Column {
                count: active_count(conn, c.id)?,
                title: c.title.unwrap_or_else(|| default_title(&c.name)),
                name: c.name,
                position: i as i64,
                wip_limit: c.wip_limit,
                role: c.role,
            })
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let offset = page
        .cursor()
        .map(|s| s.parse::<u32>())
        .transpose()
        .map_err(|_| ApiError::invalid("invalid board cursor"))?
        .unwrap_or(0);
    let limit = page.limit().min(100);
    // All correlated lookups use existing card/agent primary keys or the
    // card_comments(card, created_at), card_deps(card), spawns_bound_open indexes.
    let mut stmt = conn.prepare(
        "SELECT json_object(
          'id', c.id, 'kind', c.kind, 'project', c.project, 'column', k.name,
          'position', c.position, 'title', c.title, 'labels', json(c.labels),
          'assignee', c.assignee, 'assignee_name', coalesce(a.project || '/' || a.name, ar.project || '/' || ar.name, c.assignee),
          'assignee_status', a.status, 'agent', s.id,
          'agent_name', s.name, 'repo', c.repo, 'branch', c.branch,
          'created_by', c.created_by,
          'created_by_name', coalesce(cb.project || '/' || cb.name, cbr.project || '/' || cbr.name, c.created_by),
          'created_at', c.created_at, 'updated_at', c.updated_at,
          'archived', json('false'), 'eta_at', c.eta_at, 'eta_set_at', c.eta_set_at,
          'eta_set_by', c.eta_set_by,
          'eta_set_by_name', coalesce(eb.project || '/' || eb.name, ebr.project || '/' || ebr.name, c.eta_set_by),
          'done_at', c.done_at, 'idle_since', c.idle_since,
          'tests', (SELECT json_object('id', r.id, 'name', r.name, 'created_at', r.created_at,
            'status', CASE WHEN r.failed + r.errored > 0 THEN 'failed' WHEN r.total = 0 THEN 'empty' ELSE 'passed' END,
            'counts', json_object('passed', r.passed, 'failed', r.failed, 'errored', r.errored, 'skipped', r.skipped, 'total', r.total))
            FROM test_runs r WHERE r.pk = (SELECT max(run) FROM test_run_cards WHERE card = c.id)),
          'comment_count', (SELECT count(*) FROM card_comments WHERE card = c.id),
          'last_comment_at', (SELECT max(created_at) FROM card_comments WHERE card = c.id),
          'depends_on', (SELECT json_group_array(depends_on) FROM card_deps WHERE card = c.id),
          'blocked_by', (SELECT json_group_array(d.depends_on) FROM card_deps d
            JOIN cards dc ON dc.id = d.depends_on JOIN board_columns dk ON dk.id = dc.column_id
            WHERE d.card = c.id AND dc.archived = 0
              AND coalesce(dk.role, dk.name) NOT IN ('accepted', 'done')))
         FROM cards c JOIN board_columns k ON k.id = c.column_id
         LEFT JOIN agents a ON a.id = c.assignee
         LEFT JOIN agents_removed ar ON ar.id = c.assignee
         LEFT JOIN agents cb ON cb.id = c.created_by
         LEFT JOIN agents_removed cbr ON cbr.id = c.created_by
         LEFT JOIN agents eb ON eb.id = c.eta_set_by
         LEFT JOIN agents_removed ebr ON ebr.id = c.eta_set_by
         LEFT JOIN spawns s ON s.bound_kind = 'card' AND s.bound_id = c.id AND s.finished_at IS NULL
         WHERE c.project = ?1 AND c.archived = 0 AND (k.name = 'done') = ?2
         ORDER BY k.position, c.position, c.number LIMIT ?3 OFFSET ?4"
    )?;
    let raw = stmt
        .query_map(
            params![
                project,
                done,
                if done { (limit + 1) as i64 } else { -1 },
                if done { offset } else { 0 }
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cards: Vec<Value> = raw.iter().map(|s| json_col(s)).collect();
    // Preserve the card API's absent-when-unset optional metadata contract.
    for card in &mut cards {
        if let Some(fields) = card.as_object_mut() {
            fields.retain(|key, value| key == "last_comment_at" || !value.is_null());
        }
    }
    let next = if done && cards.len() > limit {
        cards.truncate(limit);
        Some((u64::from(offset) + limit as u64).to_string())
    } else {
        None
    };
    // Load group metadata once per group, rather than once per member.
    let visible_ids = json!(cards.iter().map(|c| &c["id"]).collect::<Vec<_>>()).to_string();
    let mut groups = conn.prepare(
        "SELECT card FROM card_groups WHERE project = ?1 AND
         (card IN (SELECT value FROM json_each(?2)) OR
          (closed_at IS NULL AND EXISTS (SELECT 1 FROM group_members m
           WHERE m.group_card = card_groups.card AND m.card IN (SELECT value FROM json_each(?2)))))",
    )?;
    let ids = groups
        .query_map(params![project, visible_ids], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let group = crate::store::groups::get(conn, &id)?;
        for card in &mut cards {
            if card["id"] == id
                || (group.closed_at.is_none() && group.members.iter().any(|m| card["id"] == *m))
            {
                card["group"] = json!(group);
            }
        }
    }
    let mut jobs = conn.prepare(
        "SELECT DISTINCT card FROM jobs WHERE project = ?1 AND state IN ('queued', 'running')",
    )?;
    let ids = jobs
        .query_map([project], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        if let Some(card) = cards.iter_mut().find(|c| c["id"] == id) {
            card["job"] = json!(crate::store::jobs::card_job(conn, &id)?);
        }
    }
    Ok(
        json!({"project": project, "card_prefix": prefix, "columns": columns, "cards": cards, "next_cursor": next}),
    )
}

/// Filters for `GET /api/projects/{name}/cards`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CardFilter {
    pub column: Option<String>,
    /// An address, or `me` for the calling agent.
    pub assignee: Option<String>,
    #[serde(default)]
    pub unassigned: bool,
    pub label: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

/// The place of a card in [`list_cards`]: column position, card position,
/// card number. The cursor is the place of the last card of the page, as
/// `<column>.<position>.<number>`.
fn list_cursor(cursor: Option<&str>) -> (i64, i64, i64) {
    let mut parts = cursor.unwrap_or_default().split('.').map(str::parse::<i64>);
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Ok(k)), Some(Ok(p)), Some(Ok(n)), None) => (k, p, n),
        // No cursor, or one that is not ours: start at the top.
        _ => (i64::MIN, i64::MIN, i64::MIN),
    }
}

/// Cards in board order (TER-73): by the place of their column on the
/// board, then by their place in the column (the top first), then by number.
pub fn list_cards(
    conn: &Connection,
    project: &str,
    f: &CardFilter,
    assignee: Option<&str>,
    page: &Page,
) -> ApiResult<Paged<Card>> {
    let limit = page.limit();
    let (after_k, after_p, after_n) = list_cursor(page.cursor());
    // A role alias (`working`, `blocked`) names the column with that role.
    // An unknown name matches no card, as before.
    let column_name = f.column.as_deref().map(|c| {
        column(conn, project, c)
            .map(|k| k.name)
            .unwrap_or_else(|_| c.to_owned())
    });
    let mut stmt = conn.prepare(
        "SELECT c.*, k.name AS column_name, k.role AS column_role,
                k.position AS column_position
           FROM cards c JOIN board_columns k ON k.id = c.column_id
          WHERE c.project = ?1 AND c.kind = 'card'
           AND (k.position, c.position, c.number) > (?2, ?9, ?10)
           AND (?3 IS NULL OR k.name = ?3)
           AND (?4 IS NULL OR c.assignee = ?4)
           AND (?5 = 0 OR c.assignee IS NULL)
           AND (?6 IS NULL OR EXISTS (SELECT 1 FROM json_each(c.labels) WHERE value = ?6))
           AND c.archived = ?7
         ORDER BY k.position, c.position, c.number LIMIT ?8",
    )?;
    let rows = stmt
        .query_map(
            params![
                project,
                after_k,
                column_name,
                assignee,
                f.unassigned as i64,
                f.label,
                f.archived as i64,
                (limit + 1) as i64,
                after_p,
                after_n
            ],
            |r| {
                let place = (
                    r.get::<_, i64>("column_position")?,
                    r.get::<_, i64>("position")?,
                    r.get::<_, i64>("number")?,
                );
                Ok((place, card_row(r)?))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (place, row) in rows {
        out.push((place, render(conn, row)?));
    }
    let paged = Paged::from_rows(out, limit, |((k, p, n), _)| format!("{k}.{p}.{n}"));
    Ok(Paged {
        items: paged.items.into_iter().map(|(_, c)| c).collect(),
        next: paged.next,
    })
}

/// A card of [`active_cards`]: the card, the kind of its column, and the
/// stored status of its assignee agent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActiveCard {
    #[serde(flatten)]
    pub card: Card,
    /// `working` (the column with that role), `research`, or another
    /// column that the query named.
    pub column_kind: String,
    /// A card in the blocked column (TER-51): when it moved there (the
    /// last move event; the card's `updated_at` without one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_since: Option<String>,
    /// A card in the blocked column: the comment of that move (what is
    /// needed), or the newest comment without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

/// When a card in the blocked column moved there, and the comment of that
/// move. The move event has both; without one (a card from before the
/// event log), `updated_at` and the newest comment.
fn blocked_info(conn: &Connection, card: &Card) -> ApiResult<(String, Option<String>)> {
    let ev: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT at, json_extract(payload, '$.comment') FROM events
              WHERE card = ?1 AND kind = 'card'
                AND json_extract(payload, '$.change') = 'moved'
                AND json_extract(payload, '$.to_role') = ?2
              ORDER BY seq DESC LIMIT 1",
            params![card.id, BLOCKED],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let newest = || card.comments.last().map(|c| c.body.clone());
    Ok(match ev {
        Some((at, Some(c))) if !c.trim().is_empty() => (at, Some(c)),
        Some((at, _)) => (at, newest()),
        None => (card.updated_at.clone(), newest()),
    })
}

/// When the card `id` last moved to another column (its newest `moved`
/// event), if it ever did.
pub fn last_moved_at(conn: &Connection, id: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT at FROM events
              WHERE card = ?1 AND kind = 'card'
                AND json_extract(payload, '$.change') = 'moved'
              ORDER BY seq DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// The most cards of [`active_cards`].
pub const ACTIVE_MAX: usize = 500;

/// The cards that are not archived in the columns `wanted`, in each
/// project of `projects` (`None`: every project), by project, then column,
/// then position (TER-43). Each wanted value is a role (`working`,
/// `blocked`) or a column name (`research`); a column matches when its
/// role or its name is one of them, so one call finds the working column
/// and the research column of each board.
pub fn active_cards(
    conn: &Connection,
    projects: Option<&[String]>,
    wanted: &[String],
) -> ApiResult<Vec<ActiveCard>> {
    if wanted.is_empty() || projects.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let wanted_json = serde_json::to_string(wanted)?;
    let projects_json = projects.map(serde_json::to_string).transpose()?;
    let mut stmt = conn.prepare(&format!(
        "{CARD_SELECT} WHERE c.archived = 0
           AND (k.role IN (SELECT value FROM json_each(?1))
                OR lower(k.name) IN (SELECT lower(value) FROM json_each(?1)))
           AND (?2 IS NULL OR c.project IN (SELECT value FROM json_each(?2)))
         ORDER BY c.project, k.position, c.position, c.number
         LIMIT {ACTIVE_MAX}"
    ))?;
    let rows = stmt
        .query_map(params![wanted_json, projects_json], card_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let kind = match row.column_role.as_deref() {
            Some(WORKING) => WORKING.to_owned(),
            Some(BLOCKED) => BLOCKED.to_owned(),
            _ => row.column.to_lowercase(),
        };
        // The card has the assignee's status (`Card::assignee_status`).
        let card = render(conn, row)?;
        let (blocked_since, blocked_reason) = if kind == BLOCKED {
            let (since, reason) = blocked_info(conn, &card)?;
            (Some(since), reason)
        } else {
            (None, None)
        };
        out.push(ActiveCard {
            card,
            column_kind: kind,
            blocked_since,
            blocked_reason,
        });
    }
    Ok(out)
}

// ---- events and notices ----

fn card_event(t: &mut Tx<'_>, card: &CardRow, change: &str, extra: Value) -> ApiResult<()> {
    card_event_as(t, card, "card", change, None, extra)
}

/// A card event of kind `kind`. `summary` replaces the default summary.
fn card_event_as(
    t: &mut Tx<'_>,
    card: &CardRow,
    kind: &str,
    change: &str,
    summary_override: Option<String>,
    mut extra: Value,
) -> ApiResult<()> {
    let actor_name = actor_display(t)?;
    let summary = match change {
        "moved" => format!(
            "{} moved {} -> {} by {actor_name}",
            card.id,
            extra["from"].as_str().unwrap_or("?"),
            extra["to"].as_str().unwrap_or("?")
        ),
        "eta" => match extra["eta_at"].as_str() {
            Some(at) => format!("{} ETA {at} by {actor_name}", card.id),
            None => format!("{} ETA cleared by {actor_name}", card.id),
        },
        _ => format!("{} {change} by {actor_name}: {}", card.id, card.title),
    };
    let summary = match summary_override {
        Some(s) => format!("{s} by {actor_name}"),
        None => summary,
    };
    if let Value::Object(map) = &mut extra {
        map.insert("change".into(), json!(change));
        map.insert("title".into(), json!(card.title));
        map.insert("column".into(), json!(card.column));
        map.insert("column_role".into(), json!(card.column_role));
        map.insert("actor_name".into(), json!(actor_name));
        // The session of an agent actor, for the card's session list.
        if let Some(sid) = session_refs::current_of(t, &t.actor)? {
            map.insert("actor_session".into(), json!(sid));
        }
    }
    let mut draft = EventDraft::new(kind, summary, extra)
        .project(card.project.clone())
        .card(card.id.clone());
    // The event belongs to the assignee's stream, so the runner can push it.
    let assignee = extra_assignee(change, &draft.data).or(card.assignee.clone());
    if let Some(a) = assignee.filter(|a| a != "operator")
        && let Some(name) = agents::display_name(t, &a)?
    {
        let short = name.rsplit('/').next().unwrap_or(&name).to_owned();
        draft = draft.agent(a, short);
    }
    t.emit(draft)?;
    Ok(())
}

fn extra_assignee(change: &str, data: &Value) -> Option<String> {
    match change {
        "assigned" | "claimed" => data["assignee"].as_str().map(str::to_owned),
        _ => None,
    }
}

/// `Tx::actor` (`operator`, `system`, or `agent:<ulid>`) in readable form.
fn actor_display(t: &Tx<'_>) -> ApiResult<String> {
    match t.actor.strip_prefix("agent:") {
        Some(id) => name_of(t, id),
        None => Ok(t.actor.clone()),
    }
}

/// The author that a comment or a blocked notice of `actor` shows in `t`.
/// The daemon writes some board changes with the rights of the operator:
/// the job queue, and a schedule that the project or the system owns. Their
/// transaction is labelled `system` or `schedule:<id>`, not `operator`, so
/// they show `system`. The rights are the operator's either way: this
/// changes the author only.
fn author_of(t: &Tx<'_>, actor: &Actor) -> String {
    match actor {
        Actor::Operator if t.actor != "operator" => "system".to_owned(),
        _ => actor.address(),
    }
}

/// Write a comment, redacted (PLAN §16b.6). Each comment comes here.
/// The comment of an agent links to the agent's current session (PLAN
/// §13), so the operator can read the work behind it.
fn insert_comment(t: &mut Tx<'_>, card: &str, author: &str, body: &str) -> ApiResult<()> {
    insert_reply(t, card, author, body, false)
}

fn insert_reply(
    t: &mut Tx<'_>,
    card: &str,
    author: &str,
    body: &str,
    human_reply: bool,
) -> ApiResult<()> {
    let session = session_refs::current_of(t, author)?;
    t.execute(
        "INSERT INTO card_comments (id, card, author, body, created_at, session_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            ulid::Ulid::new().to_string(),
            card,
            author,
            t.redact(body),
            now(),
            session
        ],
    )?;
    notify_comment(t, &find_row(t, card)?, author, body, human_reply)
}

fn notify_comment(
    t: &mut Tx<'_>,
    card: &CardRow,
    author: &str,
    body: &str,
    human_reply: bool,
) -> ApiResult<()> {
    let mut recipients = Vec::new();
    if author == "operator"
        && crate::store::projects::get(t, &card.project)?.human_comment_notifications
    {
        recipients = crate::question_guard::leads(t, &card.project, "")?;
        let full = get_card(t, &card.id)?;
        for id in [full.assignee, full.agent].into_iter().flatten() {
            if let Ok(address @ Address::Agent { .. }) = mail::resolve(t, &id, Some(&card.project))
            {
                recipients.push(address);
            }
        }
    } else if matches!(
        mail::resolve(t, author, Some(&card.project)),
        Ok(Address::Agent { .. })
    ) && (human_reply
        || body
            .split(|c: char| !c.is_alphanumeric() && c != '@' && c != '_')
            .any(|word| word.eq_ignore_ascii_case("@operator")))
    {
        recipients.push(Address::Operator);
    }
    let mut sent = HashSet::new();
    let subject = format!("{} comment: {}", card.id, card.title);
    let by = name_of(t, author)?;
    let message = format!(
        "{by} commented on {}: {}\n\n{body}\n\n[Open card](/p/{}/board?card={})",
        card.id, card.title, card.project, card.id
    );
    for to in recipients {
        if sent.insert(to.key()) {
            mail::send(
                t,
                Outgoing {
                    from: &Address::System,
                    to: &to,
                    subject: &subject,
                    body: &message,
                    thread: None,
                    in_reply_to: None,
                },
            )?;
        }
    }
    Ok(())
}

fn touch(t: &mut Tx<'_>, card: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE cards SET updated_at = ?2 WHERE id = ?1",
        params![card, now()],
    )?;
    Ok(())
}

/// The daemon mails the blocked notice (PLAN §13): the sender is `system`.
fn notify_blocked(t: &mut Tx<'_>, actor: &Actor, card: &CardRow, comment: &str) -> ApiResult<()> {
    let from = Address::System;
    let mut to = vec![Address::Operator];
    if card.created_by != "operator"
        && let Ok(Address::Agent { id, name, project }) =
            mail::resolve(t, &card.created_by, Some(&card.project))
    {
        to.push(Address::Agent { id, name, project });
    }
    let me = actor.address();
    let author = author_of(t, actor);
    let by = name_of(t, &author)?;
    let subject = format!("{} blocked: {}", card.id, card.title);
    let body = format!(
        "Card {} \"{}\" moved to {} (the blocked column) by {by}.\n\nComment:\n{comment}\n\nRun `terrarium card show {}`.",
        card.id, card.title, card.column, card.id
    );
    let mut thread: Option<String> = None;
    for recipient in to.iter().filter(|a| a.key() != me || author != me) {
        let m = mail::send(
            t,
            Outgoing {
                from: &from,
                to: recipient,
                subject: &subject,
                body: &body,
                thread: thread.as_deref(),
                in_reply_to: None,
            },
        )?;
        thread = Some(m.thread_id);
    }
    Ok(())
}

fn notify_done(t: &mut Tx<'_>, card: &CardRow, closing: &str) -> ApiResult<()> {
    let from = Address::Project(card.project.clone());
    let mut stmt =
        t.prepare("SELECT ref FROM card_links WHERE card = ?1 AND kind = 'thread' ORDER BY ref")?;
    let threads = stmt
        .query_map([&card.id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let subject = format!("Done: {} {}", card.id, card.title);
    let body = if closing.is_empty() {
        format!("Card {} \"{}\" is done.", card.id, card.title)
    } else {
        format!(
            "Card {} \"{}\" is done.\n\nClosing comment:\n{closing}",
            card.id, card.title
        )
    };
    for thread in &threads {
        for to in mail::participants(t, thread)?
            .iter()
            .filter(|p| p.key() != from.key())
        {
            mail::send(
                t,
                Outgoing {
                    from: &from,
                    to,
                    subject: &subject,
                    body: &body,
                    thread: Some(thread),
                    in_reply_to: None,
                },
            )?;
        }
        // Other cards that track the same thread show the update.
        let mut stmt = t.prepare(
            "SELECT card FROM card_links WHERE kind = 'thread' AND ref = ?1 AND card <> ?2",
        )?;
        let others = stmt
            .query_map(params![thread, card.id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for other in others {
            let note = format!(
                "{} in project {} is done: {}{}",
                card.id,
                card.project,
                card.title,
                if closing.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{closing}")
                }
            );
            insert_comment(t, &other, &from.key(), &note)?;
            touch(t, &other)?;
            let row = find_row(t, &other)?;
            card_event(
                t,
                &row,
                "thread_update",
                json!({"thread_id": thread, "done": card.id}),
            )?;
        }
    }
    // Dependent cards whose last open dependency this was.
    let mut stmt = t.prepare("SELECT card FROM card_deps WHERE depends_on = ?1 ORDER BY card")?;
    let dependents = stmt
        .query_map([&card.id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for d in dependents {
        if unmet(t, &d)?.is_empty() {
            let row = find_row(t, &d)?;
            card_event(t, &row, "unblocked", json!({"by": card.id}))?;
        }
    }
    Ok(())
}

// ---- permissions ----

fn owns(actor: &Actor, card: &CardRow) -> bool {
    let me = actor.address();
    match &card.assignee {
        Some(a) => *a == me,
        None => card.created_by == me,
    }
}

fn may_edit(actor: &Actor, card: &CardRow) -> ApiResult<()> {
    if card.kind == "group" {
        return Err(ApiError::invalid(
            "group cards are managed only by board rules",
        ));
    }
    if policy::is_admin_of(actor, &card.project) {
        return Ok(());
    }
    if card.column == DONE {
        return Err(ApiError::forbidden(format!(
            "{} is done; only a project admin or the operator changes a done card",
            card.id
        )));
    }
    if owns(actor, card) || card.created_by == actor.address() {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "{} is not your card; only its assignee, a project admin, or the operator may change it",
        card.id
    )))
}

/// Check a link from a test run of `project` to a card (PLAN §15.5): the
/// edit rights of the card. Returns the card ID.
pub fn check_test_link(
    conn: &Connection,
    actor: &Actor,
    id: &str,
    project: &str,
) -> ApiResult<String> {
    let card = find_row(conn, id)?;
    policy::write_project(actor, &card.project)?;
    if card.project != project {
        return Err(ApiError::invalid(format!(
            "a test run links to a card of its project; {} is in `{}`",
            card.id, card.project
        )));
    }
    not_archived(&card)?;
    may_edit(actor, &card)?;
    Ok(card.id)
}

/// What the card file routes (`crate::store::card_files`) need of a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRef {
    pub id: String,
    pub project: String,
    pub title: String,
    /// `operator` or an agent ULID.
    pub assignee: Option<String>,
    pub archived: bool,
}

fn card_ref_of(card: &CardRow) -> CardRef {
    CardRef {
        id: card.id.clone(),
        project: card.project.clone(),
        title: card.title.clone(),
        assignee: card.assignee.clone(),
        archived: card.archived,
    }
}

/// The card `id`, for a read of its files. The caller checks the read
/// right of the project.
pub fn card_ref(conn: &Connection, id: &str) -> ApiResult<CardRef> {
    Ok(card_ref_of(&find_row(conn, id)?))
}

/// Check that `actor` may change the files of card `id`: the rights of an
/// edit (PLAN §13). The card must not be archived.
pub fn check_file_edit(conn: &Connection, actor: &Actor, id: &str) -> ApiResult<CardRef> {
    let card = find_row(conn, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    may_edit(actor, &card)?;
    Ok(card_ref_of(&card))
}

/// Emit `card.<change>` for a file of card `id` (`file_added`,
/// `file_replaced`, `file_removed`), and touch the card. The event belongs
/// to the assignee's stream, as each card event does.
pub fn file_event(
    t: &mut Tx<'_>,
    id: &str,
    change: &str,
    summary: String,
    extra: Value,
) -> ApiResult<()> {
    let card = find_row(t, id)?;
    touch(t, &card.id)?;
    card_event_as(
        t,
        &card,
        &format!("card.{change}"),
        change,
        Some(summary),
        extra,
    )
}

/// Whether the GitHub author gate holds `card` (TER-85): a GitHub card
/// with the label `hold`.
fn card_is_held(card: &CardRow) -> bool {
    github_gate::is_held(&card.title, &json_col::<Vec<String>>(&card.labels))
}

fn not_archived(card: &CardRow) -> ApiResult<()> {
    if card.kind == "group" {
        return Err(ApiError::invalid(
            "group cards are managed only by board rules",
        ));
    }
    if card.archived {
        Err(ApiError::conflict(format!("{} is archived", card.id)))
    } else {
        Ok(())
    }
}

// ---- moves ----

fn check_move(
    t: &Tx<'_>,
    actor: &Actor,
    card: &CardRow,
    to: &ColumnRow,
    comment: Option<&str>,
) -> ApiResult<()> {
    not_archived(card)?;
    // The GitHub author gate (TER-85): only the operator moves a held
    // card, whatever the role of its column. The role of a column can
    // change (`set_columns`), so the lock does not depend on it.
    if to.id != card.column_id && !matches!(actor, Actor::Operator) && card_is_held(card) {
        return Err(ApiError::forbidden(format!(
            "{} is held by the GitHub author gate; only the operator moves it out of `{}`",
            card.id, card.column
        )));
    }
    let admin = policy::is_admin_of(actor, &card.project);
    if !admin && !owns(actor, card) {
        let whose = match &card.assignee {
            Some(a) => format!("assigned to {}", name_of(t, a)?),
            None => "not yours; claim it first".to_owned(),
        };
        return Err(ApiError::forbidden(format!(
            "{} is {whose}; only the assignee, a project admin, or the operator may move it",
            card.id
        )));
    }
    if card.column == DONE && to.name != DONE && !admin {
        return Err(ApiError::forbidden(format!(
            "{} is done; only a project admin or the operator reopens a card",
            card.id
        )));
    }
    if to.id == card.column_id {
        return Ok(());
    }
    if to.is(WORKING) {
        let open = unmet(t, &card.id)?;
        if !open.is_empty() {
            return Err(ApiError::new(
                Code::DependenciesUnmet,
                format!(
                    "{} depends on {} that are not accepted or done",
                    card.id,
                    open.join(", ")
                ),
            )
            .with_details(json!({"blocked_by": open})));
        }
    }
    if let Some(limit) = to.wip_limit
        && active_count(t, to.id)? >= limit
    {
        return Err(ApiError::new(
            Code::WipLimit,
            format!("column `{}` is at its WIP limit of {limit}", to.name),
        )
        .with_details(json!({"column": to.name, "wip_limit": limit})));
    }
    if to.is(BLOCKED) && comment.is_none_or(|c| c.trim().is_empty()) {
        return Err(ApiError::new(
            Code::CommentRequired,
            format!(
                "moving {} to `{}` (the blocked column) needs a comment that says what is needed",
                card.id, to.name
            ),
        ));
    }
    Ok(())
}

fn apply_move(
    t: &mut Tx<'_>,
    actor: &Actor,
    card: &CardRow,
    to: &ColumnRow,
    position: Option<i64>,
    comment: Option<&str>,
) -> ApiResult<()> {
    let comment = comment.map(str::trim).filter(|c| !c.is_empty());
    let same = to.id == card.column_id;
    // Take the card out of its column, then put it in at the new place.
    t.execute(
        "UPDATE cards SET position = position - 1
         WHERE column_id = ?1 AND position > ?2 AND id <> ?3",
        params![card.column_id, card.position, card.id],
    )?;
    let len: i64 = t.query_row(
        "SELECT count(*) FROM cards WHERE column_id = ?1 AND id <> ?2",
        params![to.id, card.id],
        |r| r.get(0),
    )?;
    let at = position.unwrap_or(len).clamp(0, len);
    t.execute(
        "UPDATE cards SET position = position + 1
         WHERE column_id = ?1 AND position >= ?2 AND id <> ?3",
        params![to.id, at, card.id],
    )?;
    // `done_at` is when the card moved to `done`; another column clears it.
    // A move to another column ends the idle mark (TER-67) and clears the
    // ETA (TER-155 D1): an ETA is for the work of one column. The `moved`
    // event keeps the old ETA.
    t.execute(
        "UPDATE cards SET column_id = ?2, position = ?3, updated_at = ?4,
            done_at = CASE WHEN ?5 THEN done_at WHEN ?6 THEN ?4 ELSE NULL END,
            idle_since = CASE WHEN ?5 THEN idle_since ELSE NULL END,
            eta_at = CASE WHEN ?5 THEN eta_at ELSE NULL END,
            eta_set_at = CASE WHEN ?5 THEN eta_set_at ELSE NULL END,
            eta_set_by = CASE WHEN ?5 THEN eta_set_by ELSE NULL END
         WHERE id = ?1",
        params![card.id, to.id, at, now(), same, to.name == DONE],
    )?;
    if let Some(c) = comment {
        let author = author_of(t, actor);
        insert_comment(t, &card.id, &author, c)?;
    }
    let moved = find_row(t, &card.id)?;
    if same {
        card_event(t, &moved, "reordered", json!({"position": at}))?;
        return Ok(());
    }
    card_event(
        t,
        &moved,
        "moved",
        json!({"from": card.column, "to": to.name, "from_role": card.column_role,
               "to_role": to.role, "comment": comment, "assignee": moved.assignee,
               "previous_eta_at": card.eta_at}),
    )?;
    // A thread that tracks this card wakes its agent on a move to blocked,
    // and when all its cards are in review or later (TER-148).
    crate::store::threads::on_card_moved(
        t,
        &card.id,
        &card.project,
        &card.column,
        &to.name,
        to.role.as_deref(),
    )?;
    if card.column == DONE {
        t.audit(
            "card.reopen",
            &card.id,
            Some(&card.project),
            json!({"to": to.name}),
        )?;
    }
    if to.is(BLOCKED) {
        notify_blocked(t, actor, &moved, comment.unwrap_or_default())?;
    }
    if to.name == DONE {
        let closing = match comment {
            Some(c) => c.to_owned(),
            None => t
                .query_row(
                    "SELECT body FROM card_comments WHERE card = ?1
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    [&card.id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_default(),
        };
        notify_done(t, &moved, &closing)?;
    }
    Ok(())
}

/// Move a card to a column, with the rules of §13.
pub fn move_card(
    t: &mut Tx<'_>,
    actor: &Actor,
    id: &str,
    column_name: &str,
    position: Option<i64>,
    comment: Option<&str>,
) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    let to = column(t, &card.project, column_name)?;
    check_move(t, actor, &card, &to, comment)?;
    apply_move(t, actor, &card, &to, position, comment)?;
    get_card(t, &card.id)
}

// ---- assignment ----

fn resolve_assignee(
    conn: &Connection,
    project: &str,
    who: &str,
    actor: &Actor,
) -> ApiResult<Address> {
    let who = if who == "me" {
        actor.address()
    } else {
        who.to_owned()
    };
    match mail::resolve(conn, &who, Some(project))? {
        Address::Project(_) => Err(ApiError::invalid(
            "a card cannot be assigned to a project inbox",
        )),
        Address::System => Err(ApiError::invalid("a card cannot be assigned to `system`")),
        Address::Agent {
            project: p, name, ..
        } if p != project => Err(ApiError::invalid(format!(
            "agent `{p}/{name}` is not in project `{project}`"
        ))),
        a => Ok(a),
    }
}

/// Assign a card, or unassign it with `None`.
pub fn assign_card(t: &mut Tx<'_>, actor: &Actor, id: &str, who: Option<&str>) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    let to = who
        .map(|w| resolve_assignee(t, &card.project, w, actor))
        .transpose()?;
    // The GitHub author gate (TER-85): only the operator assigns a held
    // card. Unassigning stays open: it gives no one the card.
    if to.is_some() && !matches!(actor, Actor::Operator) && card_is_held(&card) {
        return Err(ApiError::forbidden(format!(
            "{} is held by the GitHub author gate; only the operator assigns it",
            card.id
        )));
    }
    let me = actor.address();
    let admin = policy::is_admin_of(actor, &card.project);
    if !admin {
        if card.column == DONE {
            return Err(ApiError::forbidden(format!(
                "{} is done; only a project admin or the operator changes it",
                card.id
            )));
        }
        if let Some(current) = card.assignee.as_ref().filter(|a| **a != me) {
            return Err(ApiError::forbidden(format!(
                "{} is assigned to {}; only a project admin or the operator reassigns it",
                card.id,
                name_of(t, current)?
            )));
        }
    }
    let new = to.as_ref().map(Address::key);
    if new == card.assignee {
        return get_card(t, &card.id);
    }
    t.execute(
        "UPDATE cards SET assignee = ?2, idle_since = NULL, updated_at = ?3 WHERE id = ?1",
        params![card.id, new, now()],
    )?;
    if let Some(prev) = card.assignee.as_ref().filter(|a| **a != me) {
        t.audit(
            "card.reassign",
            &card.id,
            Some(&card.project),
            json!({"from": prev, "to": new}),
        )?;
    }
    let row = find_row(t, &card.id)?;
    card_event(
        t,
        &row,
        "assigned",
        json!({"assignee": new, "previous": card.assignee}),
    )?;
    get_card(t, &card.id)
}

/// Assign a card to the caller and move it to the `working` column.
pub fn claim_card(t: &mut Tx<'_>, actor: &Actor, id: &str) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    let me = actor.address();
    if let Some(current) = card.assignee.as_ref().filter(|a| **a != me) {
        return Err(ApiError::conflict(format!(
            "{} is assigned to {} already",
            card.id,
            name_of(t, current)?
        )));
    }
    if card.column == DONE {
        return Err(ApiError::conflict(format!("{} is done", card.id)));
    }
    if card.assignee.is_none() {
        t.execute(
            "UPDATE cards SET assignee = ?2, idle_since = NULL, updated_at = ?3 WHERE id = ?1",
            params![card.id, me, now()],
        )?;
        let row = find_row(t, &card.id)?;
        card_event(t, &row, "claimed", json!({"assignee": me}))?;
    }
    let row = find_row(t, &card.id)?;
    let to = column(t, &card.project, WORKING)?;
    check_move(t, actor, &row, &to, None)?;
    apply_move(t, actor, &row, &to, None, None)?;
    get_card(t, &card.id)
}

// ---- create, edit, archive ----

/// The body of `POST /api/projects/{name}/cards`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCard {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// A message ID: the card takes its subject, body, and thread.
    #[serde(default)]
    pub from_mail: Option<String>,
    /// Needed when `column` is the `blocked` column.
    #[serde(default)]
    pub comment: Option<String>,
    /// The ETA: an RFC 3339 time or a duration from now (`2h30m`).
    #[serde(default)]
    pub eta_at: Option<String>,
    /// Legacy caller assertion. Ignored by the author gate. Import an issue
    /// through the daemon's GitHub import endpoint for a verified author.
    #[serde(default)]
    pub github_author: Option<String>,
    /// An Inbox thread that tracks the new card (TER-146, TER-144 D2,
    /// [`crate::store::threads::link_card`]): the creator must read it.
    #[serde(default)]
    pub inbox_thread: Option<String>,
}

fn set_deps(t: &mut Tx<'_>, card: &str, wanted: &[String]) -> ApiResult<()> {
    let wanted: Vec<String> = wanted.iter().map(|d| normalise_id(d)).collect();
    for d in &wanted {
        if *d == card {
            return Err(ApiError::invalid(format!("{card} cannot depend on itself")));
        }
        find_row(t, d)?;
        // A cycle: `d` reaches `card` through existing dependencies.
        let mut seen = HashSet::new();
        let mut stack = vec![d.clone()];
        while let Some(next) = stack.pop() {
            if next == card {
                return Err(ApiError::invalid(format!(
                    "{card} -> {d} makes a dependency cycle"
                )));
            }
            if seen.insert(next.clone()) {
                stack.extend(deps(t, &next)?);
            }
        }
    }
    t.execute("DELETE FROM card_deps WHERE card = ?1", [card])?;
    for d in &wanted {
        t.execute(
            "INSERT OR IGNORE INTO card_deps (card, depends_on) VALUES (?1, ?2)",
            params![card, d],
        )?;
    }
    Ok(())
}

pub(crate) fn check_title(title: &str) -> ApiResult<()> {
    if title.trim().is_empty() {
        Err(ApiError::invalid("the card needs a title"))
    } else {
        Ok(())
    }
}

/// Can the actor read message `id`? Its recipient, its sender, an admin of
/// the project inbox it went to, or the operator.
pub fn may_read_message(actor: &Actor, m: &mail::Message) -> bool {
    match actor {
        Actor::Operator => true,
        Actor::Agent(a) => {
            m.to == a.id
                || m.from == a.id
                || m.to
                    .strip_prefix("project:")
                    .is_some_and(|p| a.is_admin_of(p))
        }
    }
}

pub fn create_card(t: &mut Tx<'_>, actor: &Actor, project: &str, new: NewCard) -> ApiResult<Card> {
    create_card_with_author(t, actor, project, new, None)
}

/// Only daemon-verified issue data may supply an author to the gate.
pub(crate) fn create_card_with_author(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    mut new: NewCard,
    verified_author: Option<&str>,
) -> ApiResult<Card> {
    policy::write_project(actor, project)?;
    let prefix = crate::store::projects::get(t, project)?.card_prefix;
    let mut thread = None;
    if let Some(mid) = &new.from_mail {
        let m = mail::get(t, mid)?;
        if !may_read_message(actor, &m) {
            return Err(ApiError::not_found(format!("no message `{mid}`")));
        }
        if new.title.is_none() {
            new.title = Some(if m.subject.is_empty() {
                format!("Mail from {}", m.from_name)
            } else {
                m.subject.clone()
            });
        }
        if new.body.is_none() {
            new.body = Some(format!("From {}:\n\n{}", m.from_name, m.body));
        }
        thread = Some(m.thread_id);
    }
    let title = new.title.clone().unwrap_or_default();
    check_title(&title)?;
    // The GitHub author gate (TER-85): an agent's GitHub card from an
    // author off the project's list is held.
    let held_for = match actor {
        Actor::Agent(a) => {
            let list = github_gate::authors(t, project)?;
            ((!list.is_empty() || verified_author.is_some())
                && github_gate::is_github(&title, &new.labels)
                && !github_gate::allowed(&list, verified_author))
            .then(|| a.clone())
        }
        Actor::Operator => None,
    };
    if held_for.is_some() {
        github_gate::hold_labels(&mut new.labels);
    }
    let backlog = column(t, project, BACKLOG)?;
    t.execute(
        "UPDATE projects SET card_seq = card_seq + 1 WHERE name = ?1",
        [project],
    )?;
    let number: i64 = t.query_row(
        "SELECT card_seq FROM projects WHERE name = ?1",
        [project],
        |r| r.get(0),
    )?;
    let id = format!("{prefix}-{number}");
    let position = active_count(t, backlog.id)?;
    let at = now();
    t.execute(
        "INSERT INTO cards (id, project, number, column_id, position, title, body, labels, repo,
                            branch, created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            id,
            project,
            number,
            backlog.id,
            position,
            t.redact(title.trim()),
            t.redact(new.body.as_deref().unwrap_or_default()),
            serde_json::to_string(&new.labels)?,
            new.repo,
            new.branch,
            actor.address(),
            at
        ],
    )?;
    set_deps(t, &id, &new.depends_on)?;
    if let Some(th) = new.inbox_thread.as_deref().filter(|s| !s.trim().is_empty()) {
        crate::store::threads::link_card(t, actor, th, &id)?;
    }
    if let Some(thread) = &thread {
        t.execute(
            "INSERT INTO card_links (card, kind, ref, created_by, created_at)
             VALUES (?1, 'thread', ?2, ?3, ?4)",
            params![id, thread, actor.address(), at],
        )?;
    }
    let row = find_row(t, &id)?;
    card_event(
        t,
        &row,
        "created",
        json!({"from_mail": new.from_mail, "thread_id": thread}),
    )?;
    if let Some(agent) = &held_for {
        hold(t, actor, agent, &row, verified_author)?;
        if let Some(c) = new.comment.as_deref().filter(|c| !c.trim().is_empty()) {
            let author = author_of(t, actor);
            insert_comment(t, &id, &author, c.trim())?;
        }
        return get_card(t, &id);
    }
    // Include the requested assignee in delivery of the initial comment.
    if let Some(who) = &new.assignee {
        assign_card(t, actor, &id, Some(who))?;
    }
    // A role alias (`working`, `blocked`) names the column with that role.
    let to = new
        .column
        .as_deref()
        .map(|c| column(t, project, c))
        .transpose()?
        .filter(|k| k.name != BACKLOG);
    if let Some(to) = to {
        if to.name == DONE && !policy::is_admin_of(actor, project) {
            return Err(ApiError::forbidden(
                "only a project admin or the operator makes a card in done",
            ));
        }
        check_move(t, actor, &row, &to, new.comment.as_deref())?;
        apply_move(t, actor, &row, &to, None, new.comment.as_deref())?;
    } else if let Some(c) = new.comment.as_deref().filter(|c| !c.trim().is_empty()) {
        let author = author_of(t, actor);
        insert_comment(t, &id, &author, c.trim())?;
    }
    if let Some(eta) = new.eta_at.as_deref() {
        set_eta(t, actor, &id, Some(eta))?;
    }
    get_card(t, &id)
}

/// Hold a new GitHub card (TER-85, [`github_gate`]): move it to the
/// blocked column, whatever column it asked for, and open the hold
/// question to the operator for the creating agent. The card's
/// assignee, column, and ETA of the request do not apply.
fn hold(
    t: &mut Tx<'_>,
    actor: &Actor,
    agent: &crate::actor::AgentActor,
    row: &CardRow,
    author: Option<&str>,
) -> ApiResult<()> {
    let blocked = role_column(t, &row.project, BLOCKED)?;
    let comment = github_gate::hold_comment(&row.project, author);
    apply_move(t, actor, row, &blocked, None, Some(&comment))?;
    let context = format!(
        "Title: {}\nDaemon-verified GitHub author: {}",
        row.title,
        author
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("(none)")
    );
    questions::ask(
        t,
        questions::NewQuestion {
            agent: agent.id.clone(),
            kind: questions::QuestionKind::Question,
            text: github_gate::question_text(&row.id, &row.project, author),
            options: vec![
                github_gate::RELEASE.to_owned(),
                github_gate::KEEP.to_owned(),
            ],
            context: Some(context),
            // The question lives in the agent's project; link the card
            // when it is there too.
            card: (agent.project == row.project).then(|| row.id.clone()),
            hold_card: Some(row.id.clone()),
            ..questions::NewQuestion::default()
        },
    )?;
    Ok(())
}

/// The operator released a held card with the answer of its hold
/// question (TER-85): take `hold` off, and move the card from the
/// blocked column to the backlog. A card that is gone or archived stays
/// as it is.
pub(crate) fn release_hold(t: &mut Tx<'_>, id: &str, question: &str) -> ApiResult<()> {
    let Ok(card) = find_row(t, id) else {
        return Ok(());
    };
    if card.archived {
        return Ok(());
    }
    let held = card_is_held(&card);
    let labels: Vec<String> = json_col(&card.labels);
    let kept = github_gate::without_hold(&labels);
    if kept != labels {
        t.execute(
            "UPDATE cards SET labels = ?2, updated_at = ?3 WHERE id = ?1",
            params![card.id, serde_json::to_string(&kept)?, now()],
        )?;
        let row = find_row(t, &card.id)?;
        card_event(t, &row, "edited", json!({"fields": ["labels"]}))?;
    }
    let row = find_row(t, &card.id)?;
    // The lock does not depend on the column role, so neither does the
    // release: a held card goes to the backlog from any column.
    if held && row.column != BACKLOG {
        let backlog = column(t, &row.project, BACKLOG)?;
        let comment = format!(
            "Released by the operator (the answer to hold question {question}): the card is work."
        );
        apply_move(t, &Actor::Operator, &row, &backlog, None, Some(&comment))?;
    }
    Ok(())
}

/// A field that tells `null` from absent: absent is `None`, `null` is
/// `Some(None)`.
pub(crate) fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The body of `PATCH /api/cards/{id}`. Every field is optional. `column`
/// moves the card, `assignee` assigns it (`null` unassigns), `comment` goes
/// with the move or alone, `eta_at` sets the ETA (`null` clears it).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Labels to add to the card's labels (TER-36). Not with `labels`.
    #[serde(default)]
    pub add_labels: Vec<String>,
    /// Labels to take away from the card's labels. Not with `labels`.
    #[serde(default)]
    pub remove_labels: Vec<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub repo: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub branch: Option<Option<String>>,
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
    #[serde(default, deserialize_with = "double_option")]
    pub assignee: Option<Option<String>>,
    #[serde(default)]
    pub comment: Option<String>,
    /// An RFC 3339 time or a duration from now; `null` clears the ETA.
    #[serde(default, deserialize_with = "double_option")]
    pub eta_at: Option<Option<String>>,
}

pub fn update_card(t: &mut Tx<'_>, actor: &Actor, id: &str, p: CardPatch) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    let mut p = p;
    // `add_labels` and `remove_labels` change the current labels in this
    // write, so two changes at once do not lose each other's labels.
    if !p.add_labels.is_empty() || !p.remove_labels.is_empty() {
        if p.labels.is_some() {
            return Err(ApiError::invalid(
                "give `labels` (replace them all) or `add_labels`/`remove_labels`, not both",
            ));
        }
        let mut labels: Vec<String> = json_col(&card.labels);
        labels.retain(|l| !p.remove_labels.contains(l));
        for l in std::mem::take(&mut p.add_labels) {
            if !labels.contains(&l) {
                labels.push(l);
            }
        }
        p.remove_labels.clear();
        p.labels = Some(labels);
    }
    let edits = p.title.is_some()
        || p.body.is_some()
        || p.labels.is_some()
        || p.repo.is_some()
        || p.branch.is_some()
        || p.depends_on.is_some();
    if edits {
        not_archived(&card)?;
        may_edit(actor, &card)?;
        if let Some(title) = &p.title {
            check_title(title)?;
        }
        if !matches!(actor, Actor::Operator) {
            gate_edit(t, &card, p.title.as_deref(), p.labels.as_deref())?;
        }
        t.execute(
            "UPDATE cards SET
                title = coalesce(?2, title),
                body = coalesce(?3, body),
                labels = coalesce(?4, labels),
                repo = CASE WHEN ?5 THEN ?6 ELSE repo END,
                branch = CASE WHEN ?7 THEN ?8 ELSE branch END,
                updated_at = ?9
             WHERE id = ?1",
            params![
                card.id,
                t.redact_opt(p.title.as_deref().map(str::trim)),
                t.redact_opt(p.body.as_deref()),
                p.labels.as_ref().map(serde_json::to_string).transpose()?,
                p.repo.is_some(),
                p.repo.clone().flatten(),
                p.branch.is_some(),
                p.branch.clone().flatten(),
                now()
            ],
        )?;
        if let Some(d) = &p.depends_on {
            set_deps(t, &card.id, d)?;
        }
        let fields: Vec<&str> = [
            p.title.as_ref().map(|_| "title"),
            p.body.as_ref().map(|_| "body"),
            p.labels.as_ref().map(|_| "labels"),
            p.repo.as_ref().map(|_| "repo"),
            p.branch.as_ref().map(|_| "branch"),
            p.depends_on.as_ref().map(|_| "depends_on"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let row = find_row(t, &card.id)?;
        card_event(t, &row, "edited", json!({ "fields": fields }))?;
    }
    let me = actor.address();
    let assign_first = matches!(&p.assignee, Some(Some(w)) if *w == "me" || *w == me);
    if assign_first && let Some(who) = &p.assignee {
        assign_card(t, actor, &card.id, who.as_deref())?;
    }
    let current = find_row(t, &card.id)?;
    // A client may send back the column it read; that alone is no move.
    let moves = p.position.is_some() || p.column.as_ref().is_some_and(|c| *c != current.column);
    if moves {
        let col = p.column.clone().unwrap_or_else(|| current.column.clone());
        move_card(t, actor, &card.id, &col, p.position, p.comment.as_deref())?;
    } else if let Some(c) = &p.comment {
        add_comment(t, actor, &card.id, c)?;
    }
    if !assign_first && let Some(who) = &p.assignee {
        assign_card(t, actor, &card.id, who.as_deref())?;
    }
    // Last, so that an assignment in the same patch gives the right to it.
    // A duration for a card with an open job is the job's planned
    // duration (TER-155 D2): the ETA starts when the job runs.
    if let Some(eta) = &p.eta_at {
        let planned = eta
            .as_deref()
            .filter(|v| chrono::DateTime::parse_from_rfc3339(v.trim()).is_err())
            .and_then(|v| terrarium_core::eta::parse_duration(v).ok());
        match (planned, crate::store::jobs::open_of_card(t, &card.id)?) {
            (Some(d), Some(job)) => plan_job(t, actor, &card.id, &job, d.num_seconds())?,
            _ => {
                set_eta(t, actor, &card.id, eta.as_deref())?;
            }
        }
    }
    get_card(t, &card.id)
}

/// An ETA duration for a card with an open job: the job's planned
/// duration (TER-155 D2). The rights are those of [`set_eta`].
fn plan_job(
    t: &mut Tx<'_>,
    actor: &Actor,
    id: &str,
    job: &crate::store::jobs::Job,
    secs: i64,
) -> ApiResult<()> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    may_edit(actor, &card)?;
    let previous = job.planned_secs;
    crate::store::jobs::set_planned(t, job, secs, &actor.label())?;
    if !owns(actor, &card) {
        t.audit(
            "card.eta",
            &card.id,
            Some(&card.project),
            json!({"job": job.id, "planned_secs": secs, "previous_planned_secs": previous}),
        )?;
    }
    Ok(())
}

/// The GitHub author gate (TER-85) on an agent's edit: a held card stays
/// held, and a card that is no GitHub card does not become one while the
/// project has an allow list (GitHub work starts with `card new`).
fn gate_edit(
    t: &Tx<'_>,
    card: &CardRow,
    title: Option<&str>,
    labels: Option<&[String]>,
) -> ApiResult<()> {
    let have: Vec<String> = json_col(&card.labels);
    let new_title = title.unwrap_or(&card.title);
    let new_labels = labels.unwrap_or(&have);
    if github_gate::is_held(&card.title, &have) && !github_gate::is_held(new_title, new_labels) {
        return Err(ApiError::forbidden(format!(
            "{} is held by the GitHub author gate; only the operator takes off `hold` or \
             `github`",
            card.id
        )));
    }
    if !github_gate::is_github(&card.title, &have)
        && github_gate::is_github(new_title, new_labels)
        && !github_gate::authors(t, &card.project)?.is_empty()
    {
        return Err(ApiError::forbidden(format!(
            "{} is no GitHub card; make GitHub work with `card new --github-author <login>`, \
             so the GitHub author gate sees it",
            card.id
        )));
    }
    Ok(())
}

/// Set the ETA of a card, or clear it with `None` (PLAN §13). The value is
/// an RFC 3339 time or a duration from now (`terrarium_core::eta`): in
/// the future and at most 30 days away. The rights are those of an edit.
/// A done card takes no new ETA (the move to `done` cleared it).
/// A change to the ETA of a card that is not the actor's is audited, as a
/// reassignment is.
pub fn set_eta(t: &mut Tx<'_>, actor: &Actor, id: &str, eta: Option<&str>) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    may_edit(actor, &card)?;
    let at = match eta {
        Some(v) => {
            if card.column == DONE {
                return Err(ApiError::conflict(format!(
                    "{} is done; a done card takes no new ETA",
                    card.id
                )));
            }
            let at =
                terrarium_core::eta::resolve(v, chrono::Utc::now()).map_err(ApiError::invalid)?;
            Some(crate::time_param::stored(at))
        }
        None if card.eta_at.is_none() => return get_card(t, &card.id),
        None => None,
    };
    let me = actor.address();
    let stamp = now();
    let (set_at, set_by) = match at {
        Some(_) => (Some(stamp.as_str()), Some(me.as_str())),
        None => (None, None),
    };
    t.execute(
        "UPDATE cards SET eta_at = ?2, eta_set_at = ?3, eta_set_by = ?4, updated_at = ?5
         WHERE id = ?1",
        params![card.id, at, set_at, set_by, stamp],
    )?;
    if !owns(actor, &card) {
        t.audit(
            "card.eta",
            &card.id,
            Some(&card.project),
            json!({"eta_at": at, "previous": card.eta_at}),
        )?;
    }
    let row = find_row(t, &card.id)?;
    card_event(
        t,
        &row,
        "eta",
        json!({"eta_at": at, "previous": card.eta_at}),
    )?;
    get_card(t, &card.id)
}

/// Set or clear the ETA of a card for its job (TER-155 D2), as the
/// daemon: `at` is the job start plus the planned duration, or `None`
/// while the job waits in the queue. `by` is the actor label of the
/// actor that queued the job; it becomes `eta_set_by`. A card that is
/// gone, archived, or done is left alone. No write when nothing changes.
pub fn set_job_eta(t: &mut Tx<'_>, id: &str, at: Option<&str>, by: &str) -> ApiResult<()> {
    let card = match find_row(t, id) {
        Ok(c) => c,
        Err(e) if e.code == Code::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if card.archived || card.column == DONE || card.eta_at.as_deref() == at {
        return Ok(());
    }
    let stamp = now();
    let by = by.strip_prefix("agent:").unwrap_or(by);
    let (set_at, set_by) = match at {
        Some(_) => (Some(stamp.as_str()), Some(by)),
        None => (None, None),
    };
    t.execute(
        "UPDATE cards SET eta_at = ?2, eta_set_at = ?3, eta_set_by = ?4, updated_at = ?5
         WHERE id = ?1",
        params![card.id, at, set_at, set_by, stamp],
    )?;
    let row = find_row(t, &card.id)?;
    card_event(
        t,
        &row,
        "eta",
        json!({"eta_at": at, "previous": card.eta_at, "job": true}),
    )
}

/// Archive a card. It leaves the board; its history stays.
pub fn archive_card(t: &mut Tx<'_>, actor: &Actor, id: &str) -> ApiResult<()> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    if card.kind == "group" {
        return Err(ApiError::invalid(
            "group cards are closed only by board rules",
        ));
    }
    if card.archived {
        return Ok(());
    }
    let admin = policy::is_admin_of(actor, &card.project);
    let me = actor.address();
    let mine = card.created_by == me && card.assignee.as_ref().is_none_or(|a| *a == me);
    if !admin && !mine {
        return Err(ApiError::forbidden(format!(
            "only the creator of an unassigned card, a project admin, or the operator archives {}",
            card.id
        )));
    }
    t.execute(
        "UPDATE cards SET archived = 1, updated_at = ?2 WHERE id = ?1",
        params![card.id, now()],
    )?;
    if admin && !mine {
        t.audit("card.archive", &card.id, Some(&card.project), json!({}))?;
    }
    card_event(t, &card, "archived", json!({}))?;
    crate::store::threads::on_card_archived(t, &card.id)?;
    Ok(())
}

pub fn add_comment(t: &mut Tx<'_>, actor: &Actor, id: &str, body: &str) -> ApiResult<Card> {
    reply_comment(t, actor, id, body, None)
}

/// An explicit reply must refer to a comment on this card. Replies to a
/// human, and standalone @operator mentions, also reach the operator's Inbox.
pub fn reply_comment(
    t: &mut Tx<'_>,
    actor: &Actor,
    id: &str,
    body: &str,
    reply_to: Option<&str>,
) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    if card.kind == "group" {
        return Err(ApiError::invalid(
            "group cards are managed only by board rules",
        ));
    }
    let body = body.trim();
    if body.is_empty() {
        return Err(ApiError::invalid("the comment is empty"));
    }
    let human_reply = if let Some(reply) = reply_to {
        let author: Option<String> = t
            .query_row(
                "SELECT author FROM card_comments WHERE id = ?1 AND card = ?2",
                params![reply, card.id],
                |r| r.get(0),
            )
            .optional()?;
        author.ok_or_else(|| ApiError::invalid("reply_to must name a comment on this card"))?
            == "operator"
    } else {
        false
    };
    let author = author_of(t, actor);
    insert_reply(t, &card.id, &author, body, human_reply)?;
    touch(t, &card.id)?;
    card_event(t, &card, "commented", json!({"comment": body}))?;
    get_card(t, &card.id)
}

/// The project of card `id`, for a rights check of a caller that holds
/// no [`Actor`] yet (the handoff cards of a thread, TER-95).
pub fn card_project(conn: &Connection, id: &str) -> ApiResult<String> {
    Ok(find_row(conn, id)?.project)
}

/// A comment of `system` on card `id` that links to `session` (the
/// archived session of a thread agent, TER-95), so that the card's
/// sessions list it. No rights check here: the caller checks that the
/// reader of the handoff may read the card. The card ID.
pub fn system_comment(
    t: &mut Tx<'_>,
    id: &str,
    body: &str,
    session: Option<&str>,
) -> ApiResult<String> {
    let card = find_row(t, id)?;
    t.execute(
        "INSERT INTO card_comments (id, card, author, body, created_at, session_id)
         VALUES (?1, ?2, 'system', ?3, ?4, ?5)",
        params![
            ulid::Ulid::new().to_string(),
            card.id,
            t.redact(body),
            now(),
            session
        ],
    )?;
    touch(t, &card.id)?;
    card_event(t, &card, "commented", json!({"comment": body}))?;
    Ok(card.id)
}

/// The body of `POST /api/cards/{id}/links`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewLink {
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

pub fn add_link(t: &mut Tx<'_>, actor: &Actor, id: &str, link: &NewLink) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::write_project(actor, &card.project)?;
    not_archived(&card)?;
    may_edit(actor, &card)?;
    let reference = link.reference.trim();
    if reference.is_empty() {
        return Err(ApiError::invalid("the link needs a ref"));
    }
    let reference = match link.kind.as_str() {
        "thread" => {
            if !mail::thread_exists(t, reference)? {
                return Err(ApiError::not_found(format!("no mail thread `{reference}`")));
            }
            reference.to_owned()
        }
        "card" => find_row(t, reference)?.id,
        "branch" | "url" => reference.to_owned(),
        other => {
            return Err(ApiError::invalid(format!(
                "bad link kind `{other}`: use thread, branch, card, or url"
            )));
        }
    };
    t.execute(
        "INSERT OR IGNORE INTO card_links (card, kind, ref, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![card.id, link.kind, reference, actor.address(), now()],
    )?;
    touch(t, &card.id)?;
    card_event(
        t,
        &card,
        "linked",
        json!({"kind": link.kind, "ref": reference}),
    )?;
    get_card(t, &card.id)
}

// ---- columns ----

/// One column in `PATCH /api/projects/{name}/board`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnSpec {
    pub name: String,
    /// The display label. Absent: the column keeps its title (a new column:
    /// the default). Null, empty, or equal to [`default_title`]: the
    /// default.
    #[serde(default, deserialize_with = "double_option")]
    pub title: Option<Option<String>>,
    /// Absent or null: no limit.
    #[serde(default)]
    pub wip_limit: Option<i64>,
    /// `working`, `blocked`, or null for none. Absent: the column keeps its
    /// role (a new column: none).
    #[serde(default, deserialize_with = "double_option")]
    pub role: Option<Option<String>>,
    /// Rename this existing column to `name`. Its cards stay in it.
    #[serde(default)]
    pub rename_from: Option<String>,
    /// Ignored: the order of the list is the order of the board.
    #[serde(default)]
    pub position: Option<i64>,
    /// Ignored: read-only.
    #[serde(default)]
    pub count: Option<i64>,
}

/// A checked [`ColumnSpec`].
struct Wanted {
    name: String,
    title: Option<String>,
    wip_limit: Option<i64>,
    role: Option<String>,
    /// The existing column it keeps (its ID), or `None` for a new column.
    keeps: Option<i64>,
    rename_from: Option<String>,
}

fn check_column_title(name: &str, title: Option<&str>) -> ApiResult<Option<String>> {
    let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    if t.chars().count() > TITLE_MAX {
        return Err(ApiError::invalid(format!(
            "column `{name}`: the title has more than {TITLE_MAX} characters"
        )));
    }
    if t.chars().any(char::is_control) {
        return Err(ApiError::invalid(format!(
            "column `{name}`: the title has a control character"
        )));
    }
    Ok((t != default_title(name)).then(|| t.to_owned()))
}

/// Check the whole list against the rules of [`set_columns`].
fn check_columns(current: &[ColumnRow], specs: &[ColumnSpec]) -> ApiResult<Vec<Wanted>> {
    let by_name: HashMap<&str, &ColumnRow> = current.iter().map(|c| (c.name.as_str(), c)).collect();
    if specs.first().map(|s| s.name.as_str()) != Some(BACKLOG)
        || specs.first().is_some_and(|s| s.rename_from.is_some())
    {
        return Err(ApiError::invalid(
            "`backlog` must be the first column; it cannot be removed, renamed, or moved",
        ));
    }
    if specs.last().map(|s| s.name.as_str()) != Some(DONE)
        || specs.last().is_some_and(|s| s.rename_from.is_some())
    {
        return Err(ApiError::invalid(
            "`done` must be the last column; it cannot be removed, renamed, or moved",
        ));
    }
    let mut names = HashSet::new();
    let mut sources = HashSet::new();
    let mut out = Vec::new();
    for s in specs {
        let name = s.name.as_str();
        if !names.insert(name) {
            return Err(ApiError::invalid(format!(
                "column `{name}` is twice in the list"
            )));
        }
        let source = s.rename_from.as_deref().unwrap_or(name);
        if !sources.insert(source) {
            return Err(ApiError::invalid(format!(
                "column `{source}` is twice in the list"
            )));
        }
        let old: Option<&ColumnRow> = match &s.rename_from {
            Some(from) => {
                let Some(c) = by_name.get(from.as_str()) else {
                    return Err(ApiError::invalid(format!("no column `{from}` to rename")));
                };
                if is_fixed(from) {
                    return Err(ApiError::invalid(format!(
                        "column `{from}` cannot be renamed"
                    )));
                }
                if from != name && by_name.contains_key(name) {
                    return Err(ApiError::invalid(format!(
                        "column `{name}` exists; rename it first, or choose another name"
                    )));
                }
                Some(*c)
            }
            None => by_name.get(name).copied(),
        };
        let keeps = old.map(|c| c.id);
        // A new name follows the strict rule; a name that the board has
        // already stays valid.
        let is_new_name =
            s.rename_from.as_deref().is_some_and(|f| f != name) || !by_name.contains_key(name);
        if is_new_name && !valid_column_name(name) {
            return Err(ApiError::invalid(format!(
                "bad column name `{name}`: a lower-case letter, then at most 31 lower-case \
                 letters, digits, or underscores"
            )));
        }
        if is_new_name && is_fixed(name) {
            return Err(ApiError::invalid(format!(
                "`{name}` is a fixed column; another column cannot take its name"
            )));
        }
        if let Some(w) = s.wip_limit {
            if is_fixed(name) {
                return Err(ApiError::invalid(format!(
                    "column `{name}` takes no WIP limit"
                )));
            }
            if w < 1 {
                return Err(ApiError::invalid(format!(
                    "column `{name}`: wip_limit must be 1 or more, or null"
                )));
            }
        }
        let role = match &s.role {
            Some(r) => r.as_deref(),
            None => old.and_then(|c| c.role.as_deref()),
        };
        let role = role.map(str::trim).filter(|r| !r.is_empty());
        if let Some(r) = role {
            if !ROLES.contains(&r) {
                return Err(ApiError::invalid(format!(
                    "column `{name}`: bad role `{r}`; use one of {}, or null",
                    ROLES.join(", ")
                )));
            }
            if is_fixed(name) {
                return Err(ApiError::invalid(format!("column `{name}` takes no role")));
            }
        }
        // The aliases `working` and `blocked` must name one column only.
        if ALIAS_ROLES.contains(&name) && role != Some(name) {
            return Err(ApiError::invalid(format!(
                "the name `{name}` is kept for the column with the role `{name}`; \
                 give this column the role `{name}`, or another name"
            )));
        }
        out.push(Wanted {
            title: match &s.title {
                Some(t) => check_column_title(name, t.as_deref())?,
                // A kept title that is now the default of a new name goes.
                None => check_column_title(name, old.and_then(|c| c.title.as_deref()))?,
            },
            name: name.to_owned(),
            wip_limit: s.wip_limit,
            role: role.map(str::to_owned),
            keeps,
            rename_from: s.rename_from.clone(),
        });
    }
    for r in ROLES {
        let n = out.iter().filter(|w| w.role.as_deref() == Some(r)).count();
        if r == WORKING && n != 1 {
            return Err(ApiError::invalid(format!(
                "exactly one column must have the role `working` (`terrarium card claim` moves \
                 a card there); this list has {n}"
            )));
        }
        if r != WORKING && n > 1 {
            return Err(ApiError::invalid(format!(
                "at most one column can have the role `{r}`; this list has {n}"
            )));
        }
    }
    Ok(out)
}

/// Set the columns: the list is the whole board, in order (PLAN §13).
///
/// - `backlog` is first and `done` is last. They stay, keep their names,
///   and take no WIP limit and no role.
/// - Exactly one column has the role `working`; at most one has each other
///   role.
/// - A column that is not in the list is removed; it must have no active
///   cards (409, with the cards).
/// - `rename_from` renames a column; its cards stay in it.
/// - A WIP limit lower than the count of the column is allowed: the column
///   refuses more cards until it is under the limit.
pub fn set_columns(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    specs: &[ColumnSpec],
) -> ApiResult<Board> {
    policy::admin_project(actor, project, "editing the board columns")?;
    let current = column_rows(t, project)?;
    let wanted = check_columns(&current, specs)?;
    let kept: HashSet<i64> = wanted.iter().filter_map(|w| w.keeps).collect();
    let backlog = column(t, project, BACKLOG)?;
    for c in current.iter().filter(|c| !kept.contains(&c.id)) {
        let mut stmt = t.prepare(
            "SELECT id FROM cards WHERE column_id = ?1 AND archived = 0 ORDER BY number",
        )?;
        let cards = stmt
            .query_map([c.id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        if !cards.is_empty() {
            return Err(ApiError::conflict(format!(
                "column `{}` has {} card(s): {}; move them first",
                c.name,
                cards.len(),
                cards.join(", ")
            ))
            .with_details(json!({"column": c.name, "cards": cards})));
        }
        // Archived cards of a removed column go to the backlog.
        t.execute(
            "UPDATE cards SET column_id = ?2 WHERE column_id = ?1",
            params![c.id, backlog.id],
        )?;
        t.execute("DELETE FROM board_columns WHERE id = ?1", [c.id])?;
    }
    // Free the names and the roles first, so that the UNIQUE indexes do
    // not see a half-done change (two columns that swap a role).
    t.execute(
        "UPDATE board_columns SET name = '#' || id, role = NULL WHERE project = ?1",
        [project],
    )?;
    for (i, w) in wanted.iter().enumerate() {
        match w.keeps {
            Some(id) => {
                t.execute(
                    "UPDATE board_columns SET name = ?2, title = ?3, position = ?4,
                        wip_limit = ?5, role = ?6
                     WHERE id = ?1",
                    params![id, w.name, w.title, i as i64, w.wip_limit, w.role],
                )?;
            }
            None => {
                t.execute(
                    "INSERT INTO board_columns (project, name, title, position, wip_limit, role)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![project, w.name, w.title, i as i64, w.wip_limit, w.role],
                )?;
            }
        }
    }
    let after: Vec<Value> = wanted
        .iter()
        .map(|w| {
            json!({"name": w.name, "title": w.title, "wip_limit": w.wip_limit, "role": w.role,
                   "rename_from": w.rename_from})
        })
        .collect();
    t.audit(
        "board.columns",
        project,
        Some(project),
        json!({"columns": after}),
    )?;
    t.emit(
        EventDraft::new(
            "board",
            format!("{project}: columns changed"),
            json!({"columns": after}),
        )
        .project(project),
    )?;
    get_board(t, project)
}
