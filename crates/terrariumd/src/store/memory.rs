//! The memory graph (PLAN §14): one graph per project.
//!
//! - Everyone in the project reads everything.
//! - The author (or an admin) edits a memory. Anyone adds tags and links.
//! - Agents never delete; they supersede. Admins and the operator delete.
//! - Search hides superseded memories unless `all` is set.
//! - `contradicts` and `supersedes` links come first in `show`.
//!
//! IDs are project-local: `MEM-17`. In the API a key is `MEM-17` (the
//! caller's project) or `project/MEM-17`.

use std::collections::{BTreeSet, HashSet, VecDeque};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::policy;
use crate::store::{Page, Paged, agents, board, json_col, mail, projects};

pub const PREFIX: &str = "MEM";

/// The fixed link vocabulary of PLAN §5.
pub const RELATIONS: [&str; 7] = [
    "relates_to",
    "supports",
    "contradicts",
    "supersedes",
    "derived_from",
    "part_of",
    "example_of",
];

/// Source kinds a memory may cite.
pub const SOURCE_KINDS: [&str; 5] = ["card", "thread", "path", "commit", "url"];

// ---- shared helpers (also used by the wiki, artifact, and version stores) ----

/// The readable form of a stored author: `operator`, `project:<name>`, or
/// `project/agent`.
pub fn name_of(conn: &Connection, key: &str) -> ApiResult<String> {
    if key == "operator" || key == "system" || key.starts_with("project:") {
        return Ok(key.to_owned());
    }
    Ok(agents::display_name(conn, key)?.unwrap_or_else(|| key.to_owned()))
}

/// The next project-local number for `kind` (`memory`, `artifact`).
pub fn next_number(t: &Tx<'_>, project: &str, kind: &str) -> ApiResult<i64> {
    t.execute(
        "INSERT INTO object_seq (project, kind, last) VALUES (?1, ?2, 1)
         ON CONFLICT (project, kind) DO UPDATE SET last = last + 1",
        params![project, kind],
    )?;
    Ok(t.query_row(
        "SELECT last FROM object_seq WHERE project = ?1 AND kind = ?2",
        params![project, kind],
        |r| r.get(0),
    )?)
}

/// Parse `PREFIX-17` or `project/PREFIX-17` into a project and a number.
///
/// Without a project, an agent's own project is used. For the operator the
/// number must be unique across projects in `table`.
pub fn resolve_short(
    conn: &Connection,
    key: &str,
    prefix: &str,
    table: &str,
    context: Option<&str>,
) -> ApiResult<(String, i64)> {
    let key = key.trim();
    let (project, short) = match key.split_once('/') {
        Some((p, s)) => (Some(p.to_owned()), s),
        None => (None, key),
    };
    let upper = short.to_ascii_uppercase();
    let num = upper
        .strip_prefix(prefix)
        .and_then(|s| s.strip_prefix('-'))
        .and_then(|n| n.parse::<i64>().ok())
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "bad ID `{key}`: use {prefix}-<number> or project/{prefix}-<number>"
            ))
        })?;
    if let Some(p) = project.or_else(|| context.map(str::to_owned)) {
        return Ok((p, num));
    }
    let mut stmt = conn.prepare(&format!("SELECT project FROM {table} WHERE num = ?1"))?;
    let found = stmt
        .query_map([num], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    match found.len() {
        0 => Err(ApiError::not_found(format!("no {prefix}-{num}"))),
        1 => Ok((found.into_iter().next().unwrap_or_default(), num)),
        _ => Err(ApiError::bad_request(format!(
            "{prefix}-{num} is in more than one project; use project/{prefix}-{num}"
        ))),
    }
}

/// Turn free text into an FTS5 query: each word quoted, joined with `op`
/// (`AND` or `OR`). `None` when there is no word.
pub fn fts_query(text: &str, op: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\""))
        .collect();
    (!words.is_empty()).then(|| words.join(&format!(" {op} ")))
}

// ---- shapes ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkView {
    pub from: String,
    pub to: String,
    pub relation: String,
    /// `out` when this memory is `from`, `in` when it is `to`.
    pub direction: String,
    /// The memory at the other end.
    pub other: String,
    pub other_title: String,
    pub other_superseded: bool,
    pub author: String,
    pub author_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// `MEM-17`.
    pub id: String,
    /// `project/MEM-17`, unique across projects.
    pub key: String,
    pub project: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub author: String,
    pub author_name: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// The memories this one superseded.
    pub supersedes: Vec<String>,
    pub sources: Vec<Source>,
    /// `contradicts` and `supersedes` first, then the rest by time.
    pub links: Vec<LinkView>,
}

/// One search hit for duplicate checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub id: String,
    pub title: String,
    pub superseded: bool,
}

/// `POST /api/memories` answers the memory and the top hits for its title.
#[derive(Debug, Clone, Serialize)]
pub struct Created {
    #[serde(flatten)]
    pub memory: Memory,
    pub similar: Vec<Hit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub relation: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub root: String,
    pub depth: usize,
    pub nodes: Vec<Memory>,
    pub edges: Vec<Edge>,
}

// ---- rows ----

#[derive(Debug, Clone)]
struct MemRow {
    pk: i64,
    project: String,
    num: i64,
    title: String,
    body: String,
    tags: String,
    author: String,
    created_at: String,
    updated_at: String,
    superseded_by: Option<i64>,
}

fn mem_row(r: &Row<'_>) -> rusqlite::Result<MemRow> {
    Ok(MemRow {
        pk: r.get("pk")?,
        project: r.get("project")?,
        num: r.get("num")?,
        title: r.get("title")?,
        body: r.get("body")?,
        tags: r.get("tags")?,
        author: r.get("author")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        superseded_by: r.get("superseded_by")?,
    })
}

fn short(num: i64) -> String {
    format!("{PREFIX}-{num}")
}

fn by_pk(conn: &Connection, pk: i64) -> ApiResult<MemRow> {
    conn.query_row("SELECT * FROM memories WHERE pk = ?1", [pk], mem_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found("no such memory"))
}

fn num_of(conn: &Connection, pk: i64) -> ApiResult<i64> {
    Ok(by_pk(conn, pk)?.num)
}

/// Find a memory by key. `context` is the caller's project for bare IDs.
fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<MemRow> {
    let (project, num) = resolve_short(conn, key, PREFIX, "memories", context)?;
    conn.query_row(
        "SELECT * FROM memories WHERE project = ?1 AND num = ?2",
        params![project, num],
        mem_row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("no memory {}", short(num))))
}

/// Find a memory the actor may read.
fn find_for(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<MemRow> {
    let row = find(conn, key, actor.project())?;
    policy::read_project(actor, &row.project)?;
    Ok(row)
}

fn relation_rank(rel: &str) -> u8 {
    match rel {
        "contradicts" => 0,
        "supersedes" => 1,
        _ => 2,
    }
}

fn render(conn: &Connection, row: MemRow) -> ApiResult<Memory> {
    let mut stmt =
        conn.prepare("SELECT kind, ref FROM memory_sources WHERE memory = ?1 ORDER BY kind, ref")?;
    let sources = stmt
        .query_map([row.pk], |r| {
            Ok(Source {
                kind: r.get(0)?,
                reference: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT l.from_id, l.to_id, l.relation, l.author, l.note, l.created_at,
                o.num, o.title, o.superseded_by IS NOT NULL, 'out'
           FROM memory_links l JOIN memories o ON o.pk = l.to_id WHERE l.from_id = ?1
         UNION ALL
         SELECT l.from_id, l.to_id, l.relation, l.author, l.note, l.created_at,
                o.num, o.title, o.superseded_by IS NOT NULL, 'in'
           FROM memory_links l JOIN memories o ON o.pk = l.from_id WHERE l.to_id = ?1",
    )?;
    let raw = stmt
        .query_map([row.pk], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, bool>(8)?,
                r.get::<_, String>(9)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut links = Vec::new();
    for (_, _, relation, author, note, created_at, onum, otitle, osup, dir) in raw {
        let (from_num, to_num) = if dir == "out" {
            (row.num, onum)
        } else {
            (onum, row.num)
        };
        links.push(LinkView {
            from: short(from_num),
            to: short(to_num),
            relation,
            direction: dir,
            other: short(onum),
            other_title: otitle,
            other_superseded: osup,
            author_name: name_of(conn, &author)?,
            author,
            note,
            created_at,
        });
    }
    links.sort_by(|a, b| {
        relation_rank(&a.relation)
            .cmp(&relation_rank(&b.relation))
            .then_with(|| a.created_at.cmp(&b.created_at))
    });
    let mut stmt =
        conn.prepare("SELECT num FROM memories WHERE superseded_by = ?1 ORDER BY num")?;
    let supersedes = stmt
        .query_map([row.pk], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(short)
        .collect();
    let superseded_by = row
        .superseded_by
        .map(|pk| num_of(conn, pk).map(short))
        .transpose()?;
    Ok(Memory {
        id: short(row.num),
        key: format!("{}/{}", row.project, short(row.num)),
        author_name: name_of(conn, &row.author)?,
        project: row.project,
        title: row.title,
        body: row.body,
        tags: json_col(&row.tags),
        author: row.author,
        created_at: row.created_at,
        updated_at: row.updated_at,
        superseded_by,
        supersedes,
        sources,
        links,
    })
}

pub fn get(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<Memory> {
    let row = find_for(conn, actor, key)?;
    render(conn, row)
}

/// The project of a memory, for checks before a write.
pub fn project_of(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<String> {
    Ok(find_for(conn, actor, key)?.project)
}

// ---- list and search ----

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemoryFilter {
    pub q: Option<String>,
    pub tag: Option<String>,
    /// An address, or `me`.
    pub author: Option<String>,
    /// Include superseded memories.
    #[serde(default)]
    pub all: Option<bool>,
}

/// List (newest first, cursor: number) or search (best first, cursor: an
/// offset). `author` is the stored author key.
pub fn list(
    conn: &Connection,
    project: &str,
    f: &MemoryFilter,
    author: Option<&str>,
    page: &Page,
) -> ApiResult<Paged<Memory>> {
    projects::get(conn, project)?;
    let limit = page.limit();
    let all = f.all.unwrap_or(false);
    let query = f.q.as_deref().and_then(|q| fts_query(q, "AND"));
    let rows: Vec<MemRow> = if let Some(q) = &query {
        let offset: i64 = page.cursor().and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut stmt = conn.prepare(
            "SELECT m.* FROM memories_fts JOIN memories m ON m.pk = memories_fts.rowid
             WHERE memories_fts MATCH ?1 AND m.project = ?2
               AND (?3 OR m.superseded_by IS NULL)
               AND (?4 IS NULL OR m.author = ?4)
               AND (?5 IS NULL OR EXISTS (SELECT 1 FROM json_each(m.tags) WHERE value = ?5))
             ORDER BY memories_fts.rank LIMIT ?6 OFFSET ?7",
        )?;
        let rows = stmt
            .query_map(
                params![q, project, all, author, f.tag, (limit + 1) as i64, offset],
                mem_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let more = rows.len() > limit;
        let mut out = Vec::new();
        for r in rows.into_iter().take(limit) {
            out.push(render(conn, r)?);
        }
        return Ok(Paged {
            items: out,
            next: more.then(|| (offset + limit as i64).to_string()),
        });
    } else {
        let before: i64 = page
            .cursor()
            .and_then(|c| c.parse().ok())
            .unwrap_or(i64::MAX);
        let mut stmt = conn.prepare(
            "SELECT * FROM memories m WHERE m.project = ?1 AND m.num < ?2
               AND (?3 OR m.superseded_by IS NULL)
               AND (?4 IS NULL OR m.author = ?4)
               AND (?5 IS NULL OR EXISTS (SELECT 1 FROM json_each(m.tags) WHERE value = ?5))
             ORDER BY m.num DESC LIMIT ?6",
        )?;
        stmt.query_map(
            params![project, before, all, author, f.tag, (limit + 1) as i64],
            mem_row,
        )?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut items = Vec::new();
    for r in rows {
        items.push(render(conn, r)?);
    }
    Ok(Paged::from_rows(items, limit, |m| {
        m.id.trim_start_matches("MEM-").to_owned()
    }))
}

/// The top hits for `text`, any word matching, superseded included.
pub fn similar(
    conn: &Connection,
    project: &str,
    text: &str,
    exclude: Option<i64>,
    n: usize,
) -> ApiResult<Vec<Hit>> {
    let Some(q) = fts_query(text, "OR") else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT m.num, m.title, m.superseded_by IS NOT NULL FROM memories_fts
           JOIN memories m ON m.pk = memories_fts.rowid
         WHERE memories_fts MATCH ?1 AND m.project = ?2 AND m.pk IS NOT ?3
         ORDER BY memories_fts.rank LIMIT ?4",
    )?;
    let hits = stmt
        .query_map(params![q, project, exclude, n as i64], |r| {
            Ok(Hit {
                id: short(r.get(0)?),
                title: r.get(1)?,
                superseded: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(hits)
}

// ---- writes ----

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewLinkIn {
    pub to: String,
    pub relation: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewMemory {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Links from the new memory to existing ones.
    #[serde(default)]
    pub links: Vec<NewLinkIn>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    /// Replace the tags. Author or admin.
    pub tags: Option<Vec<String>>,
    /// Add tags. Anyone in the project.
    #[serde(default)]
    pub add_tags: Vec<String>,
    /// Remove tags. Author or admin.
    #[serde(default)]
    pub remove_tags: Vec<String>,
    /// Replace the sources. Author or admin.
    pub sources: Option<Vec<Source>>,
}

fn clean_tags(tags: &[String]) -> ApiResult<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for t in tags {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        if t.len() > 64 || t.chars().any(char::is_whitespace) {
            return Err(ApiError::invalid(format!(
                "bad tag `{t}`: up to 64 characters, no spaces"
            )));
        }
        out.insert(t.to_owned());
    }
    Ok(out)
}

fn check_title(title: &str) -> ApiResult<String> {
    let t = title.trim();
    if t.is_empty() {
        return Err(ApiError::invalid("the memory needs a title"));
    }
    if t.chars().count() > 300 {
        return Err(ApiError::invalid("the title is longer than 300 characters"));
    }
    Ok(t.to_owned())
}

fn clean_source(conn: &Connection, project: &str, s: &Source) -> ApiResult<Source> {
    let reference = s.reference.trim();
    if reference.is_empty() {
        return Err(ApiError::invalid("a source needs a ref"));
    }
    let reference = match s.kind.as_str() {
        "card" => {
            let card = board::get_card(conn, reference)?;
            if card.project != project {
                return Err(ApiError::invalid(format!(
                    "card {} is in project `{}`, not `{project}`",
                    card.id, card.project
                )));
            }
            card.id
        }
        "thread" => {
            if !mail::thread_exists(conn, reference)? {
                return Err(ApiError::not_found(format!("no mail thread `{reference}`")));
            }
            reference.to_owned()
        }
        "path" | "commit" | "url" => reference.to_owned(),
        other => {
            return Err(ApiError::invalid(format!(
                "bad source kind `{other}`: use {}",
                SOURCE_KINDS.join(", ")
            )));
        }
    };
    Ok(Source {
        kind: s.kind.clone(),
        reference,
    })
}

fn set_sources(t: &Tx<'_>, pk: i64, project: &str, sources: &[Source]) -> ApiResult<()> {
    let mut clean = Vec::new();
    for s in sources {
        clean.push(clean_source(t, project, s)?);
    }
    t.execute("DELETE FROM memory_sources WHERE memory = ?1", [pk])?;
    for s in clean {
        t.execute(
            "INSERT OR IGNORE INTO memory_sources (memory, kind, ref) VALUES (?1, ?2, ?3)",
            params![pk, s.kind, s.reference],
        )?;
    }
    Ok(())
}

fn emit(t: &mut Tx<'_>, row: &MemRow, change: &str, extra: serde_json::Value) -> ApiResult<()> {
    let id = short(row.num);
    let mut data = json!({
        "memory_id": id,
        "key": format!("{}/{id}", row.project),
        "title": row.title,
    });
    if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            d.insert(k.clone(), v.clone());
        }
    }
    t.emit(
        EventDraft::new(
            format!("memory.{change}"),
            format!("{id} {change}: {}", row.title),
            data,
        )
        .project(row.project.clone()),
    )?;
    Ok(())
}

pub fn create(t: &mut Tx<'_>, actor: &Actor, project: &str, m: NewMemory) -> ApiResult<Created> {
    projects::get(t, project)?;
    policy::read_project(actor, project)?;
    let title = check_title(&m.title)?;
    let tags = clean_tags(&m.tags)?;
    let num = next_number(t, project, "memory")?;
    let at = now();
    t.execute(
        "INSERT INTO memories (project, num, title, body, tags, author, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![
            project,
            num,
            title,
            m.body,
            serde_json::to_string(&tags)?,
            actor.address(),
            at
        ],
    )?;
    let pk = t.last_insert_rowid();
    set_sources(t, pk, project, &m.sources)?;
    let row = by_pk(t, pk)?;
    emit(t, &row, "created", json!({}))?;
    for l in &m.links {
        add_link_rows(t, actor, &row, &l.to, &l.relation, l.note.as_deref())?;
    }
    let similar = similar(t, project, &title, Some(pk), 5)?;
    let memory = render(t, by_pk(t, pk)?)?;
    Ok(Created { memory, similar })
}

pub fn update(t: &mut Tx<'_>, actor: &Actor, key: &str, p: MemoryPatch) -> ApiResult<Memory> {
    let row = find_for(t, actor, key)?;
    let id = short(row.num);
    let full_edit = p.title.is_some()
        || p.body.is_some()
        || p.tags.is_some()
        || !p.remove_tags.is_empty()
        || p.sources.is_some();
    if full_edit {
        policy::edit_memory(actor, &row.project, &row.author, &id)?;
    }
    let mut fields = Vec::new();
    let title = match &p.title {
        Some(t) => {
            fields.push("title");
            check_title(t)?
        }
        None => row.title.clone(),
    };
    let body = match &p.body {
        Some(b) => {
            fields.push("body");
            b.clone()
        }
        None => row.body.clone(),
    };
    let mut tags: BTreeSet<String> = match &p.tags {
        Some(t) => clean_tags(t)?,
        None => json_col::<Vec<String>>(&row.tags).into_iter().collect(),
    };
    tags.extend(clean_tags(&p.add_tags)?);
    for r in &p.remove_tags {
        tags.remove(r.trim());
    }
    if p.tags.is_some() || !p.add_tags.is_empty() || !p.remove_tags.is_empty() {
        fields.push("tags");
    }
    if let Some(s) = &p.sources {
        set_sources(t, row.pk, &row.project, s)?;
        fields.push("sources");
    }
    t.execute(
        "UPDATE memories SET title = ?2, body = ?3, tags = ?4, updated_at = ?5 WHERE pk = ?1",
        params![row.pk, title, body, serde_json::to_string(&tags)?, now()],
    )?;
    if full_edit && actor.address() != row.author {
        t.audit(
            "memory.edit",
            &format!("{}/{id}", row.project),
            Some(&row.project),
            json!({"fields": fields}),
        )?;
    }
    let new = by_pk(t, row.pk)?;
    emit(t, &new, "updated", json!({"fields": fields}))?;
    render(t, new)
}

pub fn delete(t: &mut Tx<'_>, actor: &Actor, key: &str) -> ApiResult<()> {
    let row = find_for(t, actor, key)?;
    policy::delete_memory(actor, &row.project)?;
    t.execute("DELETE FROM memories WHERE pk = ?1", [row.pk])?;
    let id = short(row.num);
    t.audit(
        "memory.delete",
        &format!("{}/{id}", row.project),
        Some(&row.project),
        json!({"title": row.title}),
    )?;
    emit(t, &row, "deleted", json!({}))?;
    Ok(())
}

fn check_relation(rel: &str) -> ApiResult<()> {
    if RELATIONS.contains(&rel) {
        Ok(())
    } else {
        Err(ApiError::invalid(format!(
            "bad relation `{rel}`: use one of {}",
            RELATIONS.join(", ")
        )))
    }
}

/// Mark `old` as superseded by `new` and add the `supersedes` edge.
fn apply_supersede(t: &mut Tx<'_>, actor: &Actor, old: &MemRow, new: &MemRow) -> ApiResult<()> {
    if old.pk == new.pk {
        return Err(ApiError::invalid("a memory cannot supersede itself"));
    }
    if new.superseded_by == Some(old.pk) {
        return Err(ApiError::invalid(format!(
            "{} is superseded by {}; it cannot supersede it back",
            short(new.num),
            short(old.num)
        )));
    }
    if let Some(by) = old.superseded_by
        && by != new.pk
        && !policy::is_admin_of(actor, &old.project)
    {
        return Err(ApiError::conflict(format!(
            "{} is already superseded by {}",
            short(old.num),
            short(num_of(t, by)?)
        )));
    }
    t.execute(
        "UPDATE memories SET superseded_by = ?2, updated_at = ?3 WHERE pk = ?1",
        params![old.pk, new.pk, now()],
    )?;
    t.execute(
        "INSERT OR IGNORE INTO memory_links (from_id, to_id, relation, author, created_at)
         VALUES (?1, ?2, 'supersedes', ?3, ?4)",
        params![new.pk, old.pk, actor.address(), now()],
    )?;
    emit(t, old, "superseded", json!({"by": short(new.num)}))?;
    Ok(())
}

fn add_link_rows(
    t: &mut Tx<'_>,
    actor: &Actor,
    from: &MemRow,
    to: &str,
    relation: &str,
    note: Option<&str>,
) -> ApiResult<()> {
    check_relation(relation)?;
    let target = find(t, to, Some(&from.project))?;
    if target.project != from.project {
        return Err(ApiError::invalid(
            "links stay inside one project's memory graph",
        ));
    }
    if target.pk == from.pk {
        return Err(ApiError::invalid("a memory cannot link to itself"));
    }
    if relation == "supersedes" {
        return apply_supersede(t, actor, &target, from);
    }
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    t.execute(
        "INSERT INTO memory_links (from_id, to_id, relation, author, note, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (from_id, to_id, relation) DO UPDATE SET note = coalesce(excluded.note, note)",
        params![from.pk, target.pk, relation, actor.address(), note, now()],
    )?;
    emit(
        t,
        from,
        "linked",
        json!({"to": short(target.num), "relation": relation, "note": note}),
    )?;
    Ok(())
}

pub fn add_link(t: &mut Tx<'_>, actor: &Actor, key: &str, link: &NewLinkIn) -> ApiResult<Memory> {
    let from = find_for(t, actor, key)?;
    add_link_rows(
        t,
        actor,
        &from,
        &link.to,
        &link.relation,
        link.note.as_deref(),
    )?;
    let row = by_pk(t, from.pk)?;
    render(t, row)
}

pub fn remove_link(
    t: &mut Tx<'_>,
    actor: &Actor,
    key: &str,
    to: &str,
    relation: &str,
) -> ApiResult<Memory> {
    let from = find_for(t, actor, key)?;
    check_relation(relation)?;
    let target = find(t, to, Some(&from.project))?;
    let author: String = t
        .query_row(
            "SELECT author FROM memory_links WHERE from_id = ?1 AND to_id = ?2 AND relation = ?3",
            params![from.pk, target.pk, relation],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no `{relation}` link from {} to {}",
                short(from.num),
                short(target.num)
            ))
        })?;
    policy::unlink_memory(actor, &from.project, &author)?;
    t.execute(
        "DELETE FROM memory_links WHERE from_id = ?1 AND to_id = ?2 AND relation = ?3",
        params![from.pk, target.pk, relation],
    )?;
    if relation == "supersedes" && target.superseded_by == Some(from.pk) {
        t.execute(
            "UPDATE memories SET superseded_by = NULL, updated_at = ?2 WHERE pk = ?1",
            params![target.pk, now()],
        )?;
    }
    emit(
        t,
        &from,
        "unlinked",
        json!({"to": short(target.num), "relation": relation}),
    )?;
    render(t, by_pk(t, from.pk)?)
}

/// `key` is superseded by `by`.
pub fn supersede(t: &mut Tx<'_>, actor: &Actor, key: &str, by: &str) -> ApiResult<Memory> {
    let old = find_for(t, actor, key)?;
    let new = find(t, by, Some(&old.project))?;
    if new.project != old.project {
        return Err(ApiError::invalid(
            "a memory is superseded by a memory of the same project",
        ));
    }
    apply_supersede(t, actor, &old, &new)?;
    render(t, by_pk(t, old.pk)?)
}

/// The memory and its neighbours to `depth`, over links in both directions.
pub fn graph(conn: &Connection, actor: &Actor, key: &str, depth: usize) -> ApiResult<Graph> {
    let root = find_for(conn, actor, key)?;
    let depth = depth.min(5);
    let mut seen: HashSet<i64> = HashSet::from([root.pk]);
    let mut order = vec![root.pk];
    let mut queue = VecDeque::from([(root.pk, 0usize)]);
    let mut edges = Vec::new();
    let mut edge_seen = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT from_id, to_id, relation, author, note, created_at FROM memory_links
         WHERE from_id = ?1 OR to_id = ?1 ORDER BY created_at",
    )?;
    while let Some((pk, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        let rows = stmt
            .query_map([pk], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (from, to, relation, author, note, created_at) in rows {
            let other = if from == pk { to } else { from };
            if edge_seen.insert((from, to, relation.clone())) {
                edges.push((from, to, relation, author, note, created_at));
            }
            if seen.insert(other) {
                order.push(other);
                queue.push_back((other, d + 1));
            }
        }
    }
    // Keep only edges between nodes in the result.
    let mut out_edges = Vec::new();
    for (from, to, relation, author, note, created_at) in edges {
        if seen.contains(&from) && seen.contains(&to) {
            out_edges.push(Edge {
                from: short(num_of(conn, from)?),
                to: short(num_of(conn, to)?),
                relation,
                author,
                note,
                created_at,
            });
        }
    }
    let mut nodes = Vec::new();
    for pk in order {
        nodes.push(render(conn, by_pk(conn, pk)?)?);
    }
    Ok(Graph {
        root: short(root.num),
        depth,
        nodes,
        edges: out_edges,
    })
}
