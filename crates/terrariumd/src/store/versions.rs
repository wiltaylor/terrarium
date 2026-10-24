//! Versions (PLAN §15.3): named release points that the user reviews.
//!
//! Statuses: `draft` -> `in_review` (submit) -> `approved` | `rejected`
//! (the operator decides) -> `released` (an admin, from `approved` only).
//! A rejected version can be edited and submitted again. A released
//! version is immutable.
//!
//! - Submit mails the operator with the dashboard link, on the version's
//!   review thread.
//! - A decision records a review, mails the submitter on that thread, and
//!   comments on every linked card.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::memory::name_of;
use crate::store::{Page, Paged, artifacts, board, projects, wiki};

pub const STATUSES: [&str; 5] = ["draft", "in_review", "approved", "rejected", "released"];

// ---- shapes ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionCard {
    pub id: String,
    pub title: String,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionArtifact {
    pub id: String,
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewView {
    pub id: String,
    pub author: String,
    pub author_name: String,
    /// `submitted`, `approved`, `rejected`, or `released`.
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub project: String,
    pub name: String,
    pub status: String,
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The mail thread of the review, once submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_by: Option<String>,
    /// The dashboard page of the version.
    pub url: String,
    pub cards: Vec<VersionCard>,
    pub artifacts: Vec<VersionArtifact>,
    pub reviews: Vec<ReviewView>,
}

#[derive(Debug, Clone)]
pub struct VersionRow {
    pub id: String,
    pub project: String,
    pub name: String,
    pub status: String,
    pub notes: String,
    pub repo: Option<String>,
    pub git_ref: Option<String>,
    pub thread_id: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub submitted_at: Option<String>,
    pub submitted_by: Option<String>,
    pub decided_at: Option<String>,
    pub decided_by: Option<String>,
    pub released_at: Option<String>,
    pub released_by: Option<String>,
}

fn version_row(r: &Row<'_>) -> rusqlite::Result<VersionRow> {
    Ok(VersionRow {
        id: r.get("id")?,
        project: r.get("project")?,
        name: r.get("name")?,
        status: r.get("status")?,
        notes: r.get("notes")?,
        repo: r.get("repo")?,
        git_ref: r.get("git_ref")?,
        thread_id: r.get("thread_id")?,
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        submitted_at: r.get("submitted_at")?,
        submitted_by: r.get("submitted_by")?,
        decided_at: r.get("decided_at")?,
        decided_by: r.get("decided_by")?,
        released_at: r.get("released_at")?,
        released_by: r.get("released_by")?,
    })
}

fn by_id(conn: &Connection, id: &str) -> ApiResult<Option<VersionRow>> {
    Ok(conn
        .query_row("SELECT * FROM versions WHERE id = ?1", [id], version_row)
        .optional()?)
}

fn by_name(conn: &Connection, project: &str, name: &str) -> ApiResult<Option<VersionRow>> {
    Ok(conn
        .query_row(
            "SELECT * FROM versions WHERE project = ?1 AND name = ?2",
            params![project, name],
            version_row,
        )
        .optional()?)
}

/// Find by ULID, `project/name`, or a name in `context`.
pub fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<VersionRow> {
    let key = key.trim();
    let mut found = None;
    if key.len() == 26 && key.parse::<ulid::Ulid>().is_ok() {
        found = by_id(conn, key)?;
    }
    if found.is_none()
        && let Some((p, name)) = key.split_once('/')
    {
        found = by_name(conn, p, name)?;
    }
    if found.is_none()
        && let Some(p) = context
    {
        found = by_name(conn, p, key)?;
    }
    found.ok_or_else(|| ApiError::not_found(format!("no version `{key}`")))
}

fn find_for(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<VersionRow> {
    let row = find(conn, key, actor.project())?;
    policy::read_project(actor, &row.project)?;
    Ok(row)
}

fn render(conn: &Connection, row: VersionRow) -> ApiResult<Version> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.title, k.name FROM version_cards vc
           JOIN cards c ON c.id = vc.card JOIN board_columns k ON k.id = c.column_id
         WHERE vc.version = ?1 ORDER BY c.number",
    )?;
    let cards = stmt
        .query_map([&row.id], |r| {
            Ok(VersionCard {
                id: r.get(0)?,
                title: r.get(1)?,
                column: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT a.num, a.id, a.name, a.kind, a.content_type, a.size FROM version_artifacts va
           JOIN artifacts a ON a.pk = va.artifact
         WHERE va.version = ?1 ORDER BY a.num",
    )?;
    let arts = stmt
        .query_map([&row.id], |r| {
            Ok(VersionArtifact {
                id: format!("{}-{}", artifacts::PREFIX, r.get::<_, i64>(0)?),
                uid: r.get(1)?,
                name: r.get(2)?,
                kind: r.get(3)?,
                content_type: r.get(4)?,
                size: r.get::<_, i64>(5)?.max(0) as u64,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT id, author, decision, comment, created_at FROM version_reviews
         WHERE version = ?1 ORDER BY created_at, id",
    )?;
    let raw = stmt
        .query_map([&row.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut reviews = Vec::new();
    for (id, author, decision, comment, created_at) in raw {
        reviews.push(ReviewView {
            id,
            author_name: name_of(conn, &author)?,
            author,
            decision,
            comment,
            created_at,
        });
    }
    Ok(Version {
        url: format!("/projects/{}/versions/{}", row.project, row.id),
        created_by_name: name_of(conn, &row.created_by)?,
        id: row.id,
        project: row.project,
        name: row.name,
        status: row.status,
        notes: row.notes,
        repo: row.repo,
        git_ref: row.git_ref,
        thread_id: row.thread_id,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        submitted_at: row.submitted_at,
        submitted_by: row.submitted_by,
        decided_at: row.decided_at,
        decided_by: row.decided_by,
        released_at: row.released_at,
        released_by: row.released_by,
        cards,
        artifacts: arts,
        reviews,
    })
}

pub fn get(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<Version> {
    let row = find_for(conn, actor, key)?;
    render(conn, row)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VersionFilter {
    pub status: Option<String>,
    pub name: Option<String>,
}

/// Versions of a project, newest first. Cursor: the version ID.
pub fn list(
    conn: &Connection,
    project: &str,
    f: &VersionFilter,
    page: &Page,
) -> ApiResult<Paged<Version>> {
    projects::get(conn, project)?;
    if let Some(s) = &f.status
        && !STATUSES.contains(&s.as_str())
    {
        return Err(ApiError::bad_request(format!(
            "bad status `{s}`: use {}",
            STATUSES.join(", ")
        )));
    }
    let limit = page.limit();
    let before = page.cursor().unwrap_or("~").to_owned();
    let mut stmt = conn.prepare(
        "SELECT * FROM versions WHERE project = ?1 AND id < ?2
           AND (?3 IS NULL OR status = ?3) AND (?4 IS NULL OR name = ?4)
         ORDER BY id DESC LIMIT ?5",
    )?;
    let rows = stmt
        .query_map(
            params![project, before, f.status, f.name, (limit + 1) as i64],
            version_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut items = Vec::new();
    for r in rows {
        items.push(render(conn, r)?);
    }
    Ok(Paged::from_rows(items, limit, |v| v.id.clone()))
}

// ---- writes ----

fn check_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    let mut chars = n.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && n.len() <= 63
        && chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(n.to_owned())
    } else {
        Err(ApiError::invalid(format!(
            "bad version name `{name}`: use 1-63 of A-Z, a-z, 0-9, `.`, `_`, `-`, starting with a letter or digit"
        )))
    }
}

fn immutable(row: &VersionRow) -> ApiResult<()> {
    if row.status == "released" {
        Err(ApiError::conflict(format!(
            "version `{}` is released; released versions are immutable",
            row.name
        )))
    } else {
        Ok(())
    }
}

/// Edits happen in `draft` and `rejected` only.
fn editable(row: &VersionRow) -> ApiResult<()> {
    immutable(row)?;
    match row.status.as_str() {
        "draft" | "rejected" => Ok(()),
        s => Err(ApiError::conflict(format!(
            "version `{}` is {s}; edit it in draft or after a rejection",
            row.name
        ))),
    }
}

fn emit(t: &mut Tx<'_>, row: &VersionRow, change: &str, extra: serde_json::Value) -> ApiResult<()> {
    let mut data = json!({
        "version_id": row.id,
        "name": row.name,
        "status": row.status,
    });
    if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            d.insert(k.clone(), v.clone());
        }
    }
    t.emit(
        EventDraft::new(
            format!("version.{change}"),
            format!("version {} {change}", row.name),
            data,
        )
        .project(row.project.clone()),
    )?;
    Ok(())
}

fn add_card(t: &Tx<'_>, row: &VersionRow, card: &str) -> ApiResult<()> {
    let c = board::get_card(t, card)?;
    if c.project != row.project {
        return Err(ApiError::invalid(format!(
            "card {} is in project `{}`, not `{}`",
            c.id, c.project, row.project
        )));
    }
    t.execute(
        "INSERT OR IGNORE INTO version_cards (version, card) VALUES (?1, ?2)",
        params![row.id, c.id],
    )?;
    Ok(())
}

fn add_artifact(t: &Tx<'_>, actor: &Actor, row: &VersionRow, key: &str) -> ApiResult<()> {
    let a = artifacts::find(t, key, Some(&row.project))?;
    if a.project != row.project {
        return Err(ApiError::invalid(
            "a version holds artifacts of its own project",
        ));
    }
    t.execute(
        "INSERT OR IGNORE INTO version_artifacts (version, artifact, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![row.id, artifacts::pk_of(&a), actor.address(), now()],
    )?;
    t.execute(
        "UPDATE artifacts SET warned_at = NULL WHERE pk = ?1",
        [artifacts::pk_of(&a)],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewVersion {
    pub name: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub cards: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

pub fn create(t: &mut Tx<'_>, actor: &Actor, project: &str, v: NewVersion) -> ApiResult<Version> {
    projects::get(t, project)?;
    policy::read_project(actor, project)?;
    let name = check_name(&v.name)?;
    if by_name(t, project, &name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "version `{name}` exists in project `{project}`"
        )));
    }
    let id = ulid::Ulid::new().to_string();
    let at = now();
    let blank = |s: Option<String>| s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    t.execute(
        "INSERT INTO versions (id, project, name, notes, repo, git_ref, created_by, created_at,
                               updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![
            id,
            project,
            name,
            v.notes,
            blank(v.repo),
            blank(v.git_ref),
            actor.address(),
            at
        ],
    )?;
    let row = by_id(t, &id)?.ok_or_else(|| ApiError::internal("version vanished"))?;
    for c in &v.cards {
        add_card(t, &row, c)?;
    }
    for a in &v.artifacts {
        add_artifact(t, actor, &row, a)?;
    }
    emit(t, &row, "created", json!({}))?;
    render(t, row)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionPatch {
    pub name: Option<String>,
    pub notes: Option<String>,
    /// An empty string clears it.
    pub repo: Option<String>,
    /// An empty string clears it.
    pub git_ref: Option<String>,
    /// Replace the linked cards.
    pub cards: Option<Vec<String>>,
    #[serde(default)]
    pub add_cards: Vec<String>,
    #[serde(default)]
    pub remove_cards: Vec<String>,
    /// Replace the attached artifacts.
    pub artifacts: Option<Vec<String>>,
    #[serde(default)]
    pub add_artifacts: Vec<String>,
    #[serde(default)]
    pub remove_artifacts: Vec<String>,
}

pub fn update(t: &mut Tx<'_>, actor: &Actor, key: &str, p: VersionPatch) -> ApiResult<Version> {
    let row = find_for(t, actor, key)?;
    editable(&row)?;
    policy::edit_version(actor, &row.project, &row.created_by, &row.name)?;
    let mut fields = Vec::new();
    if let Some(n) = &p.name {
        let n = check_name(n)?;
        if n != row.name {
            if by_name(t, &row.project, &n)?.is_some() {
                return Err(ApiError::conflict(format!("version `{n}` exists")));
            }
            t.execute(
                "UPDATE versions SET name = ?2 WHERE id = ?1",
                params![row.id, n],
            )?;
            fields.push("name");
        }
    }
    let opt = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_owned())
    };
    if let Some(n) = &p.notes {
        t.execute(
            "UPDATE versions SET notes = ?2 WHERE id = ?1",
            params![row.id, n],
        )?;
        fields.push("notes");
    }
    if let Some(r) = &p.repo {
        t.execute(
            "UPDATE versions SET repo = ?2 WHERE id = ?1",
            params![row.id, opt(r)],
        )?;
        fields.push("repo");
    }
    if let Some(g) = &p.git_ref {
        t.execute(
            "UPDATE versions SET git_ref = ?2 WHERE id = ?1",
            params![row.id, opt(g)],
        )?;
        fields.push("git_ref");
    }
    if let Some(cards) = &p.cards {
        t.execute("DELETE FROM version_cards WHERE version = ?1", [&row.id])?;
        for c in cards {
            add_card(t, &row, c)?;
        }
        fields.push("cards");
    }
    for c in &p.add_cards {
        add_card(t, &row, c)?;
    }
    for c in &p.remove_cards {
        t.execute(
            "DELETE FROM version_cards WHERE version = ?1 AND card = ?2",
            params![row.id, board::normalise_id(c)],
        )?;
    }
    if !p.add_cards.is_empty() || !p.remove_cards.is_empty() {
        fields.push("cards");
    }
    if let Some(arts) = &p.artifacts {
        t.execute(
            "DELETE FROM version_artifacts WHERE version = ?1",
            [&row.id],
        )?;
        for a in arts {
            add_artifact(t, actor, &row, a)?;
        }
        fields.push("artifacts");
    }
    for a in &p.add_artifacts {
        add_artifact(t, actor, &row, a)?;
    }
    for a in &p.remove_artifacts {
        let art = artifacts::find(t, a, Some(&row.project))?;
        t.execute(
            "DELETE FROM version_artifacts WHERE version = ?1 AND artifact = ?2",
            params![row.id, artifacts::pk_of(&art)],
        )?;
    }
    if !p.add_artifacts.is_empty() || !p.remove_artifacts.is_empty() {
        fields.push("artifacts");
    }
    fields.dedup();
    t.execute(
        "UPDATE versions SET updated_at = ?2 WHERE id = ?1",
        params![row.id, now()],
    )?;
    let new = by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("version vanished"))?;
    emit(t, &new, "updated", json!({"fields": fields}))?;
    render(t, new)
}

/// Attach artifact `pk` to the version `key` of `project`. Used by
/// `POST /api/artifacts/{id}/links` with kind `version`.
pub fn attach_artifact(
    t: &mut Tx<'_>,
    actor: &Actor,
    key: &str,
    project: &str,
    pk: i64,
) -> ApiResult<Version> {
    let row = find(t, key, Some(project))?;
    if row.project != project {
        return Err(ApiError::invalid(
            "an artifact attaches to a version of its project",
        ));
    }
    editable(&row)?;
    policy::edit_version(actor, &row.project, &row.created_by, &row.name)?;
    t.execute(
        "INSERT OR IGNORE INTO version_artifacts (version, artifact, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![row.id, pk, actor.address(), now()],
    )?;
    t.execute("UPDATE artifacts SET warned_at = NULL WHERE pk = ?1", [pk])?;
    t.execute(
        "UPDATE versions SET updated_at = ?2 WHERE id = ?1",
        params![row.id, now()],
    )?;
    emit(t, &row, "updated", json!({"fields": ["artifacts"]}))?;
    render(t, row)
}

fn review_row(
    t: &Tx<'_>,
    row: &VersionRow,
    actor: &Actor,
    decision: &str,
    comment: Option<&str>,
) -> ApiResult<()> {
    t.execute(
        "INSERT INTO version_reviews (id, version, author, decision, comment, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            ulid::Ulid::new().to_string(),
            row.id,
            actor.address(),
            decision,
            comment,
            now()
        ],
    )?;
    Ok(())
}

fn clean(comment: Option<&str>) -> Option<String> {
    comment
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
}

/// `draft` or `rejected` -> `in_review`, and mail the operator.
pub fn submit(
    t: &mut Tx<'_>,
    actor: &Actor,
    key: &str,
    comment: Option<&str>,
) -> ApiResult<Version> {
    let row = find_for(t, actor, key)?;
    editable(&row)?;
    policy::edit_version(actor, &row.project, &row.created_by, &row.name)?;
    let comment = clean(comment);
    let at = now();
    t.execute(
        "UPDATE versions SET status = 'in_review', submitted_at = ?2, submitted_by = ?3,
                             decided_at = NULL, decided_by = NULL, updated_at = ?2
         WHERE id = ?1",
        params![row.id, at, actor.address()],
    )?;
    review_row(t, &row, actor, "submitted", comment.as_deref())?;
    let v = render(
        t,
        by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("version vanished"))?,
    )?;
    let mut body = format!(
        "{} submitted version `{}` of project `{}` for review.\n\nOpen: {}\n",
        Address::from_actor(actor).display(),
        v.name,
        v.project,
        v.url
    );
    if let Some(c) = &comment {
        body.push_str(&format!("\n{c}\n"));
    }
    if !v.notes.trim().is_empty() {
        body.push_str(&format!("\nNotes:\n{}\n", v.notes.trim()));
    }
    if !v.cards.is_empty() {
        body.push_str("\nCards:\n");
        for c in &v.cards {
            body.push_str(&format!("- {} {} ({})\n", c.id, c.title, c.column));
        }
    }
    if !v.artifacts.is_empty() {
        body.push_str("\nArtifacts:\n");
        for a in &v.artifacts {
            body.push_str(&format!("- {} {} ({})\n", a.id, a.name, a.content_type));
        }
    }
    let from = Address::Project(v.project.clone());
    let msg = mail::send(
        t,
        Outgoing {
            from: &from,
            to: &Address::Operator,
            subject: &format!("Review version {} of {}", v.name, v.project),
            body: &body,
            thread: row.thread_id.as_deref(),
            in_reply_to: None,
        },
    )?;
    t.execute(
        "UPDATE versions SET thread_id = ?2 WHERE id = ?1",
        params![row.id, msg.thread_id],
    )?;
    let new = by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("version vanished"))?;
    emit(
        t,
        &new,
        "submitted",
        json!({"comment": comment, "thread_id": msg.thread_id}),
    )?;
    render(t, new)
}

/// The operator approves or rejects a version in review.
pub fn decide(
    t: &mut Tx<'_>,
    actor: &Actor,
    key: &str,
    approve: bool,
    comment: Option<&str>,
) -> ApiResult<Version> {
    let row = find_for(t, actor, key)?;
    immutable(&row)?;
    policy::decide_version(actor)?;
    if row.status != "in_review" {
        return Err(ApiError::conflict(format!(
            "version `{}` is {}; only a version in review is approved or rejected",
            row.name, row.status
        )));
    }
    let comment = clean(comment);
    if !approve && comment.is_none() {
        return Err(ApiError::new(
            Code::CommentRequired,
            "a rejection needs a comment that says what to fix",
        ));
    }
    let (status, word) = if approve {
        ("approved", "approved")
    } else {
        ("rejected", "rejected")
    };
    let at = now();
    t.execute(
        "UPDATE versions SET status = ?2, decided_at = ?3, decided_by = ?4, updated_at = ?3
         WHERE id = ?1",
        params![row.id, status, at, actor.address()],
    )?;
    review_row(t, &row, actor, status, comment.as_deref())?;
    t.audit(
        &format!("version.{}", if approve { "approve" } else { "reject" }),
        &format!("{}/{}", row.project, row.name),
        Some(&row.project),
        json!({"version_id": row.id, "comment": comment}),
    )?;
    let decider = Address::from_actor(actor).display();
    let note = match &comment {
        Some(c) => format!("Version {} {word} by {decider}: {c}", row.name),
        None => format!("Version {} {word} by {decider}.", row.name),
    };
    // Comment on every linked card.
    let mut stmt = t.prepare("SELECT card FROM version_cards WHERE version = ?1 ORDER BY card")?;
    let cards = stmt
        .query_map([&row.id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for c in &cards {
        board::add_comment(t, actor, c, &note)?;
    }
    // Mail the submitter on the review thread.
    if let Some(submitter) = &row.submitted_by
        && *submitter != actor.address()
    {
        let to = mail::resolve(t, submitter, Some(&row.project))?;
        let from = Address::Project(row.project.clone());
        let body = format!(
            "{note}\n\nOpen: /projects/{}/versions/{}\n",
            row.project, row.id
        );
        mail::send(
            t,
            Outgoing {
                from: &from,
                to: &to,
                subject: &format!("Version {} {word}", row.name),
                body: &body,
                thread: row.thread_id.as_deref(),
                in_reply_to: None,
            },
        )?;
    }
    let new = by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("version vanished"))?;
    emit(t, &new, word, json!({"comment": comment, "cards": cards}))?;
    render(t, new)
}

/// An admin releases an approved version. It is immutable after this.
pub fn release(
    t: &mut Tx<'_>,
    actor: &Actor,
    key: &str,
    comment: Option<&str>,
) -> ApiResult<Version> {
    let row = find_for(t, actor, key)?;
    immutable(&row)?;
    policy::release_version(actor, &row.project)?;
    if row.status != "approved" {
        return Err(ApiError::conflict(format!(
            "version `{}` is {}; only an approved version is released",
            row.name, row.status
        )));
    }
    let comment = clean(comment);
    let at = now();
    t.execute(
        "UPDATE versions SET status = 'released', released_at = ?2, released_by = ?3,
                             updated_at = ?2
         WHERE id = ?1",
        params![row.id, at, actor.address()],
    )?;
    review_row(t, &row, actor, "released", comment.as_deref())?;
    t.audit(
        "version.release",
        &format!("{}/{}", row.project, row.name),
        Some(&row.project),
        json!({"version_id": row.id, "git_ref": row.git_ref}),
    )?;
    let new = by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("version vanished"))?;
    emit(t, &new, "released", json!({"comment": comment}))?;
    render(t, new)
}

// ---- compare ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SetDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

pub use wiki::PageChange;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardsChange {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Cards of the project that moved to `done` between the two versions.
    pub landed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compare {
    pub version: String,
    pub to: String,
    /// The times compared: the other version's and this one's.
    pub since: String,
    pub until: String,
    pub cards: CardsChange,
    pub artifacts: SetDiff,
    pub pages: Vec<PageChange>,
}

/// The time a version stands for: released, else submitted, else now.
fn reference_time(row: &VersionRow) -> String {
    row.released_at
        .clone()
        .or_else(|| row.submitted_at.clone())
        .unwrap_or_else(now)
}

fn set_of(conn: &Connection, sql: &str, id: &str) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn diff_sets(now_set: &[String], then_set: &[String]) -> SetDiff {
    SetDiff {
        added: now_set
            .iter()
            .filter(|x| !then_set.contains(x))
            .cloned()
            .collect(),
        removed: then_set
            .iter()
            .filter(|x| !now_set.contains(x))
            .cloned()
            .collect(),
    }
}

/// Cards, pages, and artifacts that changed between `to` and `key`.
pub fn compare(conn: &Connection, actor: &Actor, key: &str, to: &str) -> ApiResult<Compare> {
    let this = find_for(conn, actor, key)?;
    let other = find(conn, to, Some(&this.project))?;
    if other.project != this.project {
        return Err(ApiError::invalid("compare versions of one project"));
    }
    let (mut since, mut until) = (reference_time(&other), reference_time(&this));
    if since > until {
        std::mem::swap(&mut since, &mut until);
    }
    let cards_sql = "SELECT card FROM version_cards WHERE version = ?1 ORDER BY card";
    let arts_sql = "SELECT 'ART-' || a.num FROM version_artifacts va
                    JOIN artifacts a ON a.pk = va.artifact WHERE va.version = ?1 ORDER BY a.num";
    let cards = diff_sets(
        &set_of(conn, cards_sql, &this.id)?,
        &set_of(conn, cards_sql, &other.id)?,
    );
    let arts = diff_sets(
        &set_of(conn, arts_sql, &this.id)?,
        &set_of(conn, arts_sql, &other.id)?,
    );
    let mut stmt = conn.prepare(
        "SELECT DISTINCT card FROM events
         WHERE kind = 'card' AND project = ?1 AND at > ?2 AND at <= ?3
           AND json_extract(payload, '$.change') = 'moved'
           AND json_extract(payload, '$.to') = 'done'
         ORDER BY card",
    )?;
    let landed = stmt
        .query_map(params![this.project, since, until], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let pages = wiki::changed_between(conn, &this.project, &since, &until)?;
    Ok(Compare {
        version: this.name,
        to: other.name,
        since,
        until,
        cards: CardsChange {
            added: cards.added,
            removed: cards.removed,
            landed,
        },
        artifacts: arts,
        pages,
    })
}
