//! The kanban board (PLAN §13) and the rules the daemon enforces on it.
//!
//! - Every change is an event with an author.
//! - An agent may create cards, claim unassigned cards, move and edit its own
//!   cards, and comment on any card of its project. It may not move or
//!   reassign another agent's card. Project admins and the operator may.
//! - A card with unmet dependencies cannot enter `in_progress`.
//! - A column at its WIP limit refuses one more card.
//! - Moving a card to `blocked` needs a comment, and mails the operator and
//!   the card's creator.
//! - `done` is terminal for members. On `done`, every linked mail thread gets
//!   a notice, and dependent cards that are now free raise `unblocked`.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::{Page, Paged, agents, json_col, valid_name};

/// The columns of a new board, in order.
pub const DEFAULT_COLUMNS: [&str; 6] = [
    "backlog",
    "ready",
    "in_progress",
    "review",
    "blocked",
    "done",
];
/// Columns that the rules depend on. They cannot be renamed or removed.
pub const REQUIRED_COLUMNS: [&str; 4] = ["backlog", "in_progress", "blocked", "done"];

pub const BACKLOG: &str = "backlog";
pub const IN_PROGRESS: &str = "in_progress";
pub const BLOCKED: &str = "blocked";
pub const DONE: &str = "done";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub position: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wip_limit: Option<i64>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub project: String,
    pub column: String,
    pub position: i64,
    pub title: String,
    pub body: String,
    /// `operator` or an agent ULID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_name: Option<String>,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub depends_on: Vec<String>,
    /// The dependencies that are not done yet.
    pub blocked_by: Vec<String>,
    pub comments: Vec<Comment>,
    pub links: Vec<Link>,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
    pub updated_at: String,
    pub archived: bool,
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
    wip_limit: Option<i64>,
}

#[derive(Debug, Clone)]
struct CardRow {
    id: String,
    project: String,
    column_id: i64,
    column: String,
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
}

fn card_row(r: &Row<'_>) -> rusqlite::Result<CardRow> {
    Ok(CardRow {
        id: r.get("id")?,
        project: r.get("project")?,
        column_id: r.get("column_id")?,
        column: r.get("column_name")?,
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
    })
}

const CARD_SELECT: &str = "SELECT c.*, k.name AS column_name FROM cards c
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
    for (i, name) in DEFAULT_COLUMNS.iter().enumerate() {
        t.execute(
            "INSERT INTO board_columns (project, name, position) VALUES (?1, ?2, ?3)",
            params![project, name, i as i64],
        )?;
    }
    Ok(())
}

fn column_rows(conn: &Connection, project: &str) -> ApiResult<Vec<ColumnRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, wip_limit FROM board_columns WHERE project = ?1 ORDER BY position",
    )?;
    let rows = stmt
        .query_map([project], |r| {
            Ok(ColumnRow {
                id: r.get(0)?,
                name: r.get(1)?,
                wip_limit: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn column(conn: &Connection, project: &str, name: &str) -> ApiResult<ColumnRow> {
    let cols = column_rows(conn, project)?;
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    let names = names.join(", ");
    cols.into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| ApiError::invalid(format!("no column `{name}`; columns: {names}")))
}

fn active_count(conn: &Connection, column_id: i64) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM cards WHERE column_id = ?1 AND archived = 0",
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
pub fn find_open_by_label(
    conn: &Connection,
    project: &str,
    label: &str,
) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT c.id FROM cards c JOIN board_columns col ON col.id = c.column_id
             WHERE c.project = ?1 AND c.archived = 0 AND col.name <> ?3
               AND EXISTS (SELECT 1 FROM json_each(c.labels) WHERE value = ?2)
             ORDER BY c.number DESC LIMIT 1",
            params![project, label, DONE],
            |r| r.get(0),
        )
        .optional()?)
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
         WHERE d.card = ?1 AND k.name <> 'done' AND c.archived = 0
         ORDER BY d.depends_on",
    )?;
    let rows = stmt
        .query_map([id], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(rows)
}

fn render(conn: &Connection, row: CardRow) -> ApiResult<Card> {
    let mut stmt = conn.prepare(
        "SELECT id, author, body, created_at FROM card_comments WHERE card = ?1
         ORDER BY created_at, id",
    )?;
    let raw = stmt
        .query_map([&row.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut comments = Vec::new();
    for (id, author, body, created_at) in raw {
        comments.push(Comment {
            id,
            author_name: name_of(conn, &author)?,
            author,
            body,
            created_at,
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
    Ok(Card {
        depends_on: deps(conn, &row.id)?,
        blocked_by: unmet(conn, &row.id)?,
        created_by_name: name_of(conn, &row.created_by)?,
        id: row.id,
        project: row.project,
        column: row.column,
        position: row.position,
        title: row.title,
        body: row.body,
        assignee: row.assignee,
        assignee_name,
        labels: json_col(&row.labels),
        repo: row.repo,
        branch: row.branch,
        comments,
        links,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        archived: row.archived,
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
            name: c.name,
            position: i as i64,
            wip_limit: c.wip_limit,
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

/// Cards by number. The cursor is a card number.
pub fn list_cards(
    conn: &Connection,
    project: &str,
    f: &CardFilter,
    assignee: Option<&str>,
    page: &Page,
) -> ApiResult<Paged<Card>> {
    let limit = page.limit();
    let after: i64 = page.cursor().and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut stmt = conn.prepare(&format!(
        "{CARD_SELECT} WHERE c.project = ?1 AND c.number > ?2
           AND (?3 IS NULL OR k.name = ?3)
           AND (?4 IS NULL OR c.assignee = ?4)
           AND (?5 = 0 OR c.assignee IS NULL)
           AND (?6 IS NULL OR EXISTS (SELECT 1 FROM json_each(c.labels) WHERE value = ?6))
           AND c.archived = ?7
         ORDER BY c.number LIMIT ?8"
    ))?;
    let rows = stmt
        .query_map(
            params![
                project,
                after,
                f.column,
                assignee,
                f.unassigned as i64,
                f.label,
                f.archived as i64,
                (limit + 1) as i64
            ],
            |r| Ok((r.get::<_, i64>("number")?, card_row(r)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (n, row) in rows {
        out.push((n, render(conn, row)?));
    }
    let paged = Paged::from_rows(out, limit, |(n, _)| n.to_string());
    Ok(Paged {
        items: paged.items.into_iter().map(|(_, c)| c).collect(),
        next: paged.next,
    })
}

// ---- events and notices ----

fn card_event(t: &mut Tx<'_>, card: &CardRow, change: &str, mut extra: Value) -> ApiResult<()> {
    let actor_name = actor_display(t)?;
    let summary = match change {
        "moved" => format!(
            "{} moved {} -> {} by {actor_name}",
            card.id,
            extra["from"].as_str().unwrap_or("?"),
            extra["to"].as_str().unwrap_or("?")
        ),
        _ => format!("{} {change} by {actor_name}: {}", card.id, card.title),
    };
    if let Value::Object(map) = &mut extra {
        map.insert("change".into(), json!(change));
        map.insert("title".into(), json!(card.title));
        map.insert("column".into(), json!(card.column));
        map.insert("actor_name".into(), json!(actor_name));
    }
    let mut draft = EventDraft::new("card", summary, extra)
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

fn insert_comment(t: &mut Tx<'_>, card: &str, author: &str, body: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO card_comments (id, card, author, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![ulid::Ulid::new().to_string(), card, author, body, now()],
    )?;
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
    let by = name_of(t, &me)?;
    let subject = format!("{} blocked: {}", card.id, card.title);
    let body = format!(
        "Card {} \"{}\" moved to blocked by {by}.\n\nComment:\n{comment}\n\nRun `terrarium card show {}`.",
        card.id, card.title, card.id
    );
    let mut thread: Option<String> = None;
    for recipient in to.iter().filter(|a| a.key() != me) {
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

fn not_archived(card: &CardRow) -> ApiResult<()> {
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
    if to.name == IN_PROGRESS {
        let open = unmet(t, &card.id)?;
        if !open.is_empty() {
            return Err(ApiError::new(
                Code::DependenciesUnmet,
                format!(
                    "{} depends on {} that are not done",
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
    if to.name == BLOCKED && comment.is_none_or(|c| c.trim().is_empty()) {
        return Err(ApiError::new(
            Code::CommentRequired,
            format!(
                "moving {} to blocked needs a comment that says what is needed",
                card.id
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
    t.execute(
        "UPDATE cards SET column_id = ?2, position = ?3, updated_at = ?4 WHERE id = ?1",
        params![card.id, to.id, at, now()],
    )?;
    if let Some(c) = comment {
        insert_comment(t, &card.id, &actor.address(), c)?;
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
        json!({"from": card.column, "to": to.name, "comment": comment, "assignee": moved.assignee}),
    )?;
    if card.column == DONE {
        t.audit(
            "card.reopen",
            &card.id,
            Some(&card.project),
            json!({"to": to.name}),
        )?;
    }
    if to.name == BLOCKED {
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
    policy::read_project(actor, &card.project)?;
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
    policy::read_project(actor, &card.project)?;
    not_archived(&card)?;
    let to = who
        .map(|w| resolve_assignee(t, &card.project, w, actor))
        .transpose()?;
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
        "UPDATE cards SET assignee = ?2, updated_at = ?3 WHERE id = ?1",
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

/// Assign a card to the caller and move it to `in_progress`.
pub fn claim_card(t: &mut Tx<'_>, actor: &Actor, id: &str) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::read_project(actor, &card.project)?;
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
            "UPDATE cards SET assignee = ?2, updated_at = ?3 WHERE id = ?1",
            params![card.id, me, now()],
        )?;
        let row = find_row(t, &card.id)?;
        card_event(t, &row, "claimed", json!({"assignee": me}))?;
    }
    let row = find_row(t, &card.id)?;
    let to = column(t, &card.project, IN_PROGRESS)?;
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
    /// Needed when `column` is `blocked`.
    #[serde(default)]
    pub comment: Option<String>,
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

fn check_title(title: &str) -> ApiResult<()> {
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

pub fn create_card(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    mut new: NewCard,
) -> ApiResult<Card> {
    policy::read_project(actor, project)?;
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
            title.trim(),
            new.body.clone().unwrap_or_default(),
            serde_json::to_string(&new.labels)?,
            new.repo,
            new.branch,
            actor.address(),
            at
        ],
    )?;
    set_deps(t, &id, &new.depends_on)?;
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
    if let Some(col) = new.column.as_deref().filter(|c| *c != BACKLOG) {
        let to = column(t, project, col)?;
        if to.name == DONE && !policy::is_admin_of(actor, project) {
            return Err(ApiError::forbidden(
                "only a project admin or the operator makes a card in done",
            ));
        }
        check_move(t, actor, &row, &to, new.comment.as_deref())?;
        apply_move(t, actor, &row, &to, None, new.comment.as_deref())?;
    } else if let Some(c) = new.comment.as_deref().filter(|c| !c.trim().is_empty()) {
        insert_comment(t, &id, &actor.address(), c.trim())?;
    }
    if let Some(who) = &new.assignee {
        assign_card(t, actor, &id, Some(who))?;
    }
    get_card(t, &id)
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The body of `PATCH /api/cards/{id}`. Every field is optional. `column`
/// moves the card, `assignee` assigns it (`null` unassigns), `comment` goes
/// with the move or alone.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
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
}

pub fn update_card(t: &mut Tx<'_>, actor: &Actor, id: &str, p: CardPatch) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::read_project(actor, &card.project)?;
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
                p.title.as_deref().map(str::trim),
                p.body,
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
    get_card(t, &card.id)
}

/// Archive a card. It leaves the board; its history stays.
pub fn archive_card(t: &mut Tx<'_>, actor: &Actor, id: &str) -> ApiResult<()> {
    let card = find_row(t, id)?;
    policy::read_project(actor, &card.project)?;
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
    Ok(())
}

pub fn add_comment(t: &mut Tx<'_>, actor: &Actor, id: &str, body: &str) -> ApiResult<Card> {
    let card = find_row(t, id)?;
    policy::read_project(actor, &card.project)?;
    let body = body.trim();
    if body.is_empty() {
        return Err(ApiError::invalid("the comment is empty"));
    }
    insert_comment(t, &card.id, &actor.address(), body)?;
    touch(t, &card.id)?;
    card_event(t, &card, "commented", json!({"comment": body}))?;
    get_card(t, &card.id)
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
    policy::read_project(actor, &card.project)?;
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
    #[serde(default)]
    pub wip_limit: Option<i64>,
    /// Rename this existing column to `name`.
    #[serde(default)]
    pub rename_from: Option<String>,
    /// Ignored: the order of the list is the order of the board.
    #[serde(default)]
    pub position: Option<i64>,
    /// Ignored: read-only.
    #[serde(default)]
    pub count: Option<i64>,
}

/// Set the columns: the list is the whole board, in order. A column that is
/// not in the list is removed; it must have no cards.
pub fn set_columns(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    specs: &[ColumnSpec],
) -> ApiResult<Board> {
    policy::admin_project(actor, project, "editing columns and WIP limits")?;
    let current = column_rows(t, project)?;
    let exists = |n: &str| current.iter().any(|c| c.name == n);
    let mut seen = HashSet::new();
    for s in specs {
        if !valid_name(&s.name) {
            return Err(ApiError::invalid(format!("bad column name `{}`", s.name)));
        }
        if !seen.insert(s.name.clone()) {
            return Err(ApiError::invalid(format!(
                "column `{}` is twice in the list",
                s.name
            )));
        }
        if s.wip_limit.is_some_and(|w| w < 1) {
            return Err(ApiError::invalid(format!(
                "column `{}`: wip_limit must be 1 or more, or null",
                s.name
            )));
        }
        if let Some(from) = &s.rename_from {
            if !exists(from) {
                return Err(ApiError::invalid(format!("no column `{from}` to rename")));
            }
            if REQUIRED_COLUMNS.contains(&from.as_str()) {
                return Err(ApiError::invalid(format!(
                    "column `{from}` cannot be renamed"
                )));
            }
            if exists(&s.name) {
                return Err(ApiError::invalid(format!("column `{}` exists", s.name)));
            }
        }
    }
    for req in REQUIRED_COLUMNS {
        if !seen.contains(req) {
            return Err(ApiError::invalid(format!(
                "the board needs the column `{req}`"
            )));
        }
    }
    let kept: HashSet<&str> = specs
        .iter()
        .map(|s| s.rename_from.as_deref().unwrap_or(&s.name))
        .collect();
    let backlog = column(t, project, BACKLOG)?;
    for c in current.iter().filter(|c| !kept.contains(c.name.as_str())) {
        let n = active_count(t, c.id)?;
        if n > 0 {
            return Err(ApiError::conflict(format!(
                "column `{}` has {n} card(s); move them first",
                c.name
            )));
        }
        t.execute(
            "UPDATE cards SET column_id = ?2 WHERE column_id = ?1",
            params![c.id, backlog.id],
        )?;
        t.execute("DELETE FROM board_columns WHERE id = ?1", [c.id])?;
    }
    for (i, s) in specs.iter().enumerate() {
        let source = s.rename_from.as_deref().unwrap_or(&s.name);
        let updated = t.execute(
            "UPDATE board_columns SET name = ?3, position = ?4, wip_limit = ?5
             WHERE project = ?1 AND name = ?2",
            params![project, source, s.name, i as i64, s.wip_limit],
        )?;
        if updated == 0 {
            t.execute(
                "INSERT INTO board_columns (project, name, position, wip_limit)
                 VALUES (?1, ?2, ?3, ?4)",
                params![project, s.name, i as i64, s.wip_limit],
            )?;
        }
    }
    let after: Vec<Value> = specs
        .iter()
        .map(|s| json!({"name": s.name, "wip_limit": s.wip_limit}))
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
