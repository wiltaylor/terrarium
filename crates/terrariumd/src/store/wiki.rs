//! The wiki (PLAN §15.1): Markdown pages with a revision history.
//!
//! - Every member creates pages and edits any page of its project.
//! - A full save names its base revision. A stale base is refused with the
//!   diff since the base. Section and append edits rarely conflict.
//! - Every revision has an author and a non-empty summary.
//! - Only admins move or delete pages. A deleted page keeps its history and
//!   can be restored.
//! - Every project has an `index` page from the first access.
//! - Wikilinks (`[[...]]`) and artifact embeds are indexed on save, so
//!   backlinks and artifact retention can use them.

use std::sync::OnceLock;

use regex::Regex;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::memory::{fts_query, name_of};
use crate::store::{Page, Paged, projects};

pub const INDEX: &str = "index";
pub const STATUSES: [&str; 3] = ["draft", "ready_for_review", "reviewed"];

// ---- shapes ----

/// A page with its current revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WikiPage {
    pub id: String,
    pub project: String,
    pub path: String,
    pub title: String,
    pub rev: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    pub body: String,
    /// The author of the current revision.
    pub author: String,
    pub author_name: String,
    pub summary: String,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    /// The links of the current revision: `{kind, target}`.
    pub links: Vec<WikiLink>,
}

/// A page in the tree or in search results, without its body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageSummary {
    pub id: String,
    pub project: String,
    pub path: String,
    pub title: String,
    pub rev: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    /// A piece of the body around the match, for search results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WikiLink {
    /// `page`, `card`, `memory`, `version`, or `artifact`.
    pub kind: String,
    /// `project/path`, a card ID, `project/MEM-n`, `project/<version>`, or
    /// `project/ART-n`.
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub page: String,
    pub rev: i64,
    pub author: String,
    pub author_name: String,
    pub summary: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub page: String,
    pub a: i64,
    pub b: i64,
    /// A unified diff from `a` to `b`.
    pub diff: String,
}

// ---- Markdown helpers ----

/// The fields of the front matter that the daemon reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrontMatter {
    pub title: Option<String>,
    pub status: Option<String>,
    pub reviewer: Option<String>,
}

/// Split `---\n...\n---\n` off the top of `body`. Returns the lines of the
/// front matter and the byte offset of the rest.
fn front_matter_block(body: &str) -> Option<(Vec<&str>, usize)> {
    let rest = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))?;
    let start = body.len() - rest.len();
    let mut offset = start;
    let mut lines = Vec::new();
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let l = line.trim_end_matches(['\n', '\r']);
        if l == "---" {
            return Some((lines, offset));
        }
        lines.push(l);
    }
    None
}

pub fn front_matter(body: &str) -> FrontMatter {
    let mut fm = FrontMatter::default();
    let Some((lines, _)) = front_matter_block(body) else {
        return fm;
    };
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches(['"', '\'']).trim();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "title" => fm.title = Some(v.to_owned()),
            "status" => fm.status = Some(v.to_owned()),
            "reviewer" => fm.reviewer = Some(v.to_owned()),
            _ => {}
        }
    }
    fm
}

/// Set `status` in the front matter, adding the block when there is none.
pub fn set_status(body: &str, status: &str) -> String {
    match front_matter_block(body) {
        Some((lines, end)) => {
            let mut out = String::from("---\n");
            let mut done = false;
            for l in lines {
                if l.split_once(':').is_some_and(|(k, _)| k.trim() == "status") {
                    out.push_str(&format!("status: {status}\n"));
                    done = true;
                } else {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            if !done {
                out.push_str(&format!("status: {status}\n"));
            }
            out.push_str("---\n");
            out.push_str(&body[end..]);
            out
        }
        None => format!("---\nstatus: {status}\n---\n{body}"),
    }
}

fn first_heading(body: &str) -> Option<String> {
    let start = front_matter_block(body).map_or(0, |(_, end)| end);
    body[start..]
        .lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_owned()))
        .filter(|t| !t.is_empty())
}

fn derive_title(path: &str, fm: &FrontMatter, body: &str) -> String {
    fm.title
        .clone()
        .or_else(|| first_heading(body))
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_owned())
}

struct Heading {
    line: usize,
    level: usize,
    text: String,
}

/// ATX headings outside fenced code blocks.
fn headings(lines: &[&str]) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut fence = false;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let level = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&level) && t[level..].starts_with([' ', '\t']) {
            let text = t[level..].trim().trim_end_matches('#').trim().to_owned();
            out.push(Heading {
                line: i,
                level,
                text,
            });
        }
    }
    out
}

fn heading_key(h: &str) -> String {
    h.trim().trim_start_matches('#').trim().to_lowercase()
}

/// The lines of the section under `heading`: from the line after the
/// heading to the next heading of the same or a higher level.
fn section_range(lines: &[&str], heading: &str) -> Option<(usize, usize)> {
    let want = heading_key(heading);
    let hs = headings(lines);
    let idx = hs.iter().position(|h| h.text.to_lowercase() == want)?;
    let h = &hs[idx];
    let end = hs[idx + 1..]
        .iter()
        .find(|n| n.level <= h.level)
        .map_or(lines.len(), |n| n.line);
    Some((h.line + 1, end))
}

fn section_text(body: &str, heading: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let (s, e) = section_range(&lines, heading)?;
    Some(lines[s..e].join("\n").trim().to_owned())
}

/// Replace the section under `heading` with `content`, or append a new
/// `## heading` section when there is none.
pub fn replace_section(body: &str, heading: &str, content: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let content = content.trim_matches('\n');
    match section_range(&lines, heading) {
        Some((s, e)) => {
            let mut out: Vec<String> = lines[..s].iter().map(|l| (*l).to_owned()).collect();
            out.push(String::new());
            out.push(content.to_owned());
            if e < lines.len() {
                out.push(String::new());
                out.extend(lines[e..].iter().map(|l| (*l).to_owned()));
            }
            let mut text = out.join("\n");
            text.push('\n');
            text
        }
        None => {
            let title = heading.trim().trim_start_matches('#').trim();
            format!("{}\n\n## {title}\n\n{content}\n", body.trim_end())
        }
    }
}

pub fn unified_diff(old: &str, new: &str, a: &str, b: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(a, b)
        .to_string()
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[\[([^\]\|\n]+)(?:\|[^\]\n]*)?\]\]").unwrap_or_else(|_| unreachable!())
    })
}

fn embed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:artifact:|```\s*artifact\s+)((?:[a-z0-9_-]+/)?ART-\d+)")
            .unwrap_or_else(|_| unreachable!())
    })
}

fn short_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:([a-z0-9][a-z0-9_-]*)/)?([A-Z][A-Z0-9]*)-(\d+)$")
            .unwrap_or_else(|_| unreachable!())
    })
}

/// The links in `body`, for a page of `project`.
pub fn extract_links(conn: &Connection, project: &str, body: &str) -> ApiResult<Vec<WikiLink>> {
    let mut out: Vec<WikiLink> = Vec::new();
    let mut push = |kind: &str, target: String| {
        if !out.iter().any(|l| l.kind == kind && l.target == target) {
            out.push(WikiLink {
                kind: kind.to_owned(),
                target,
            });
        }
    };
    for cap in link_re().captures_iter(body) {
        let raw = cap[1].trim();
        if raw.is_empty() {
            continue;
        }
        if let Some(rest) = raw.strip_prefix("project:") {
            push("page", rest.trim().to_owned());
            continue;
        }
        if let Some(m) = short_re().captures(raw) {
            let p = m.get(1).map_or(project, |p| p.as_str());
            let (prefix, n) = (&m[2], &m[3]);
            match prefix {
                "MEM" => push("memory", format!("{p}/MEM-{n}")),
                "ART" => push("artifact", format!("{p}/ART-{n}")),
                _ => push("card", format!("{prefix}-{n}")),
            }
            continue;
        }
        let is_version = (raw.starts_with('v')
            && raw[1..].starts_with(|c: char| c.is_ascii_digit()))
            || conn
                .query_row(
                    "SELECT 1 FROM versions WHERE project = ?1 AND name = ?2
                     AND NOT EXISTS (SELECT 1 FROM wiki_pages WHERE project = ?1 AND path = ?2
                                     AND deleted_at IS NULL)",
                    params![project, raw],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
        if is_version {
            push("version", format!("{project}/{raw}"));
        } else {
            push("page", format!("{project}/{raw}"));
        }
    }
    for cap in embed_re().captures_iter(body) {
        let raw = cap[1].to_owned();
        let target = match raw.split_once('/') {
            Some((p, s)) => format!("{p}/{}", s.to_ascii_uppercase()),
            None => format!("{project}/{}", raw.to_ascii_uppercase()),
        };
        push("artifact", target);
    }
    Ok(out)
}

/// Paths are slugs with `/`: lower-case letters, digits, `-`, `_`, `.`.
pub fn check_path(path: &str) -> ApiResult<String> {
    let p = path.trim().trim_matches('/');
    let ok = !p.is_empty()
        && p.len() <= 200
        && p.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
        });
    if ok {
        Ok(p.to_owned())
    } else {
        Err(ApiError::invalid(format!(
            "bad page path `{path}`: use segments of a-z, 0-9, `-`, `_`, `.`, joined by `/`"
        )))
    }
}

fn check_summary(summary: &str) -> ApiResult<String> {
    let s = summary.trim();
    if s.is_empty() {
        Err(ApiError::invalid(
            "every revision needs a non-empty summary",
        ))
    } else {
        Ok(s.to_owned())
    }
}

fn check_status(fm: &FrontMatter) -> ApiResult<()> {
    match &fm.status {
        Some(s) if !STATUSES.contains(&s.as_str()) => Err(ApiError::invalid(format!(
            "bad front matter status `{s}`: use {}",
            STATUSES.join(", ")
        ))),
        _ => Ok(()),
    }
}

// ---- rows ----

#[derive(Debug, Clone)]
pub struct PageRow {
    pub id: String,
    pub project: String,
    pub path: String,
    pub title: String,
    pub current_rev: i64,
    pub status: Option<String>,
    pub reviewer: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

fn page_row(r: &Row<'_>) -> rusqlite::Result<PageRow> {
    Ok(PageRow {
        id: r.get("id")?,
        project: r.get("project")?,
        path: r.get("path")?,
        title: r.get("title")?,
        current_rev: r.get("current_rev")?,
        status: r.get("status")?,
        reviewer: r.get("reviewer")?,
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        deleted_at: r.get("deleted_at")?,
    })
}

fn by_id(conn: &Connection, id: &str) -> ApiResult<Option<PageRow>> {
    Ok(conn
        .query_row("SELECT * FROM wiki_pages WHERE id = ?1", [id], page_row)
        .optional()?)
}

fn by_path(conn: &Connection, project: &str, path: &str) -> ApiResult<Option<PageRow>> {
    Ok(conn
        .query_row(
            "SELECT * FROM wiki_pages WHERE project = ?1 AND path = ?2 AND deleted_at IS NULL",
            params![project, path],
            page_row,
        )
        .optional()?)
}

/// Where a key points: `(project, path)` when it is not a page ID. Used to
/// make the `index` page on first access.
pub fn key_target(conn: &Connection, key: &str, context: Option<&str>) -> Option<(String, String)> {
    if is_ulid(key) {
        return None;
    }
    if let Some((p, rest)) = key.split_once('/')
        && projects::find(conn, p).ok().flatten().is_some()
    {
        return Some((p.to_owned(), rest.to_owned()));
    }
    context.map(|p| (p.to_owned(), key.to_owned()))
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26 && s.parse::<ulid::Ulid>().is_ok()
}

/// Find a page by ID, `project/path`, or a path in the caller's project.
/// An ID finds deleted pages too; a path finds only live pages.
pub fn find(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<PageRow> {
    let key = key.trim();
    let row = if is_ulid(key) {
        by_id(conn, key)?
    } else {
        let mut found = None;
        if let Some((p, rest)) = key.split_once('/')
            && projects::find(conn, p)?.is_some()
        {
            found = by_path(conn, p, rest)?;
        }
        if found.is_none()
            && let Some(p) = actor.project()
        {
            found = by_path(conn, p, key)?;
        }
        found
    };
    let row = row.ok_or_else(|| ApiError::not_found(format!("no wiki page `{key}`")))?;
    policy::read_project(actor, &row.project)?;
    Ok(row)
}

fn revision_row(conn: &Connection, page: &str, rev: i64) -> ApiResult<Revision> {
    let r = conn
        .query_row(
            "SELECT page, rev, author, summary, body, created_at FROM wiki_revisions
             WHERE page = ?1 AND rev = ?2",
            params![page, rev],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no revision {rev}")))?;
    Ok(Revision {
        page: r.0,
        rev: r.1,
        author_name: name_of(conn, &r.2)?,
        author: r.2,
        summary: r.3,
        body: Some(r.4),
        created_at: r.5,
    })
}

fn links_of(conn: &Connection, page: &str) -> ApiResult<Vec<WikiLink>> {
    let mut stmt =
        conn.prepare("SELECT kind, target FROM wiki_links WHERE page = ?1 ORDER BY kind, target")?;
    let rows = stmt
        .query_map([page], |r| {
            Ok(WikiLink {
                kind: r.get(0)?,
                target: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn render(conn: &Connection, row: PageRow) -> ApiResult<WikiPage> {
    let rev = revision_row(conn, &row.id, row.current_rev)?;
    Ok(WikiPage {
        links: links_of(conn, &row.id)?,
        id: row.id,
        project: row.project,
        path: row.path,
        title: row.title,
        rev: row.current_rev,
        status: row.status,
        reviewer: row.reviewer,
        body: rev.body.unwrap_or_default(),
        author: rev.author,
        author_name: rev.author_name,
        summary: rev.summary,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        deleted_at: row.deleted_at,
    })
}

fn summary_of(row: PageRow, snippet: Option<String>) -> PageSummary {
    PageSummary {
        id: row.id,
        project: row.project,
        path: row.path,
        title: row.title,
        rev: row.current_rev,
        status: row.status,
        reviewer: row.reviewer,
        updated_at: row.updated_at,
        deleted_at: row.deleted_at,
        snippet,
    }
}

pub fn get(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<WikiPage> {
    let row = find(conn, actor, key)?;
    render(conn, row)
}

// ---- writes ----

/// Index the current revision: links, full text, title, and front matter.
fn index(t: &Tx<'_>, page: &PageRow, body: &str) -> ApiResult<()> {
    t.execute("DELETE FROM wiki_links WHERE page = ?1", [&page.id])?;
    for l in extract_links(t, &page.project, body)? {
        t.execute(
            "INSERT OR IGNORE INTO wiki_links (page, kind, target) VALUES (?1, ?2, ?3)",
            params![page.id, l.kind, l.target],
        )?;
    }
    t.execute("DELETE FROM wiki_fts WHERE page = ?1", [&page.id])?;
    t.execute(
        "INSERT INTO wiki_fts (page, path, title, body) VALUES (?1, ?2, ?3, ?4)",
        params![page.id, page.path, page.title, body],
    )?;
    Ok(())
}

fn emit(t: &mut Tx<'_>, row: &PageRow, change: &str, extra: serde_json::Value) -> ApiResult<()> {
    let mut data = json!({
        "page_id": row.id,
        "path": row.path,
        "title": row.title,
        "rev": row.current_rev,
    });
    if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            d.insert(k.clone(), v.clone());
        }
    }
    t.emit(
        EventDraft::new(
            format!("wiki.{change}"),
            format!("wiki {} {change} (rev {})", row.path, row.current_rev),
            data,
        )
        .project(row.project.clone()),
    )?;
    Ok(())
}

/// Write a new revision of `row` with `body`.
fn write_revision(
    t: &mut Tx<'_>,
    author: &str,
    row: &PageRow,
    body: &str,
    summary: &str,
    title: Option<&str>,
) -> ApiResult<PageRow> {
    let (body, summary, title) = (t.redact(body), t.redact(summary), t.redact_opt(title));
    let (body, title) = (&*body, title.as_deref());
    let summary = check_summary(&summary)?;
    let fm = front_matter(body);
    check_status(&fm)?;
    let rev = row.current_rev + 1;
    let at = now();
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| derive_title(&row.path, &fm, body));
    t.execute(
        "INSERT INTO wiki_revisions (page, rev, author, summary, body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![row.id, rev, author, summary, body, at],
    )?;
    t.execute(
        "UPDATE wiki_pages SET current_rev = ?2, title = ?3, status = ?4, reviewer = ?5,
                               updated_at = ?6
         WHERE id = ?1",
        params![row.id, rev, title, fm.status, fm.reviewer, at],
    )?;
    let new = by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("page vanished"))?;
    index(t, &new, body)?;
    emit(
        t,
        &new,
        "saved",
        json!({"summary": summary, "author": author}),
    )?;
    Ok(new)
}

fn insert_page(
    t: &mut Tx<'_>,
    author: &str,
    project: &str,
    path: &str,
    body: &str,
    summary: &str,
    title: Option<&str>,
) -> ApiResult<PageRow> {
    let (body, summary, title) = (t.redact(body), t.redact(summary), t.redact_opt(title));
    let (body, title) = (&*body, title.as_deref());
    let summary = check_summary(&summary)?;
    let fm = front_matter(body);
    check_status(&fm)?;
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| derive_title(path, &fm, body));
    let id = ulid::Ulid::new().to_string();
    let at = now();
    t.execute(
        "INSERT INTO wiki_pages (id, project, path, title, current_rev, status, reviewer,
                                 created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?8)",
        params![id, project, path, title, fm.status, fm.reviewer, author, at],
    )?;
    t.execute(
        "INSERT INTO wiki_revisions (page, rev, author, summary, body, created_at)
         VALUES (?1, 1, ?2, ?3, ?4, ?5)",
        params![id, author, summary, body, at],
    )?;
    let row = by_id(t, &id)?.ok_or_else(|| ApiError::internal("page vanished"))?;
    index(t, &row, body)?;
    emit(t, &row, "created", json!({"author": author}))?;
    Ok(row)
}

/// Make the `index` page of `project` when it has none.
pub fn ensure_index(t: &mut Tx<'_>, project: &str) -> ApiResult<()> {
    projects::get(t, project)?;
    if by_path(t, project, INDEX)?.is_some() {
        return Ok(());
    }
    let body = format!(
        "# {project}\n\nThe wiki of project `{project}`. Keep this page as the table of contents.\n"
    );
    insert_page(
        t,
        "system",
        project,
        INDEX,
        &body,
        "Create the index page",
        None,
    )?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPage {
    pub path: String,
    pub body: String,
    pub summary: String,
    #[serde(default)]
    pub title: Option<String>,
}

pub fn create(t: &mut Tx<'_>, actor: &Actor, project: &str, p: NewPage) -> ApiResult<WikiPage> {
    projects::get(t, project)?;
    policy::read_project(actor, project)?;
    ensure_index(t, project)?;
    let path = check_path(&p.path)?;
    if by_path(t, project, &path)?.is_some() {
        return Err(ApiError::conflict(format!(
            "page `{path}` exists in project `{project}`; edit it with its base revision"
        )));
    }
    let row = insert_page(
        t,
        &actor.address(),
        project,
        &path,
        &p.body,
        &p.summary,
        p.title.as_deref(),
    )?;
    render(t, row)
}

fn live(row: &PageRow) -> ApiResult<()> {
    if row.deleted_at.is_some() {
        Err(ApiError::conflict(format!(
            "page `{}` is deleted; an admin restores it first",
            row.path
        )))
    } else {
        Ok(())
    }
}

fn stale(row: &PageRow, base: i64, diff: String, what: &str) -> ApiError {
    ApiError::new(
        Code::StaleBase,
        format!(
            "page `{}` is at rev {}, not your base rev {base}; {what}, then save with base {}",
            row.path, row.current_rev, row.current_rev
        ),
    )
    .with_details(json!({
        "base": base,
        "current": row.current_rev,
        "diff": diff,
    }))
}

fn body_at(conn: &Connection, page: &str, rev: i64) -> ApiResult<String> {
    Ok(revision_row(conn, page, rev)?.body.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Save {
    pub body: String,
    /// The revision the edit started from.
    pub base: i64,
    pub summary: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// Save a whole new body. A stale base is refused with the diff since it.
pub fn save(t: &mut Tx<'_>, actor: &Actor, key: &str, s: Save) -> ApiResult<WikiPage> {
    let row = find(t, actor, key)?;
    live(&row)?;
    if s.base != row.current_rev {
        if s.base < 1 || s.base > row.current_rev {
            return Err(ApiError::invalid(format!(
                "page `{}` has no rev {}; it is at rev {}",
                row.path, s.base, row.current_rev
            )));
        }
        let old = body_at(t, &row.id, s.base)?;
        let cur = body_at(t, &row.id, row.current_rev)?;
        let diff = unified_diff(
            &old,
            &cur,
            &format!("rev {}", s.base),
            &format!("rev {}", row.current_rev),
        );
        return Err(stale(
            &row,
            s.base,
            diff,
            "merge the changes since your base",
        ));
    }
    let new = write_revision(
        t,
        &actor.address(),
        &row,
        &s.body,
        &s.summary,
        s.title.as_deref(),
    )?;
    render(t, new)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionEdit {
    pub heading: String,
    pub body: String,
    /// Optional. With a base, the edit is refused only when this section
    /// changed since the base.
    #[serde(default)]
    pub base: Option<i64>,
    pub summary: String,
}

/// Replace one section by its heading, or add it at the end.
pub fn section(t: &mut Tx<'_>, actor: &Actor, key: &str, e: SectionEdit) -> ApiResult<WikiPage> {
    let row = find(t, actor, key)?;
    live(&row)?;
    if heading_key(&e.heading).is_empty() {
        return Err(ApiError::invalid("the section edit needs a heading"));
    }
    let cur = body_at(t, &row.id, row.current_rev)?;
    if let Some(base) = e.base
        && base != row.current_rev
    {
        if base < 1 || base > row.current_rev {
            return Err(ApiError::invalid(format!(
                "page `{}` has no rev {base}; it is at rev {}",
                row.path, row.current_rev
            )));
        }
        let old = body_at(t, &row.id, base)?;
        let then = section_text(&old, &e.heading).unwrap_or_default();
        let now_text = section_text(&cur, &e.heading).unwrap_or_default();
        if then != now_text {
            let diff = unified_diff(
                &then,
                &now_text,
                &format!("rev {base} section {}", e.heading),
                &format!("rev {} section {}", row.current_rev, e.heading),
            );
            return Err(stale(
                &row,
                base,
                diff,
                "someone changed this section since your base; merge it",
            ));
        }
    }
    let body = replace_section(&cur, &e.heading, &e.body);
    let new = write_revision(t, &actor.address(), &row, &body, &e.summary, None)?;
    render(t, new)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Append {
    pub body: String,
    pub summary: String,
}

/// Add text at the end of the page. It never conflicts.
pub fn append(t: &mut Tx<'_>, actor: &Actor, key: &str, a: Append) -> ApiResult<WikiPage> {
    let row = find(t, actor, key)?;
    live(&row)?;
    if a.body.trim().is_empty() {
        return Err(ApiError::invalid("nothing to append"));
    }
    let cur = body_at(t, &row.id, row.current_rev)?;
    let body = format!("{}\n\n{}\n", cur.trim_end(), a.body.trim_matches('\n'));
    let new = write_revision(t, &actor.address(), &row, &body, &a.summary, None)?;
    render(t, new)
}

/// Restore `rev` as a new revision. On a deleted page this also undeletes
/// it, which needs an admin.
pub fn restore(t: &mut Tx<'_>, actor: &Actor, key: &str, rev: i64) -> ApiResult<WikiPage> {
    let row = find(t, actor, key)?;
    let body = body_at(t, &row.id, rev)?;
    let mut row = row;
    if row.deleted_at.is_some() {
        policy::admin_wiki(actor, &row.project, "restoring a deleted page")?;
        if by_path(t, &row.project, &row.path)?.is_some() {
            return Err(ApiError::conflict(format!(
                "another page has the path `{}` now; move it first",
                row.path
            )));
        }
        t.execute(
            "UPDATE wiki_pages SET deleted_at = NULL WHERE id = ?1",
            [&row.id],
        )?;
        t.audit(
            "wiki.undelete",
            &format!("{}/{}", row.project, row.path),
            Some(&row.project),
            json!({"page_id": row.id, "rev": rev}),
        )?;
        row = by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("page vanished"))?;
    }
    let new = write_revision(
        t,
        &actor.address(),
        &row,
        &body,
        &format!("Restore rev {rev}"),
        None,
    )?;
    emit(t, &new, "restored", json!({"from_rev": rev}))?;
    render(t, new)
}

/// Delete a page. Its history stays.
pub fn delete(t: &mut Tx<'_>, actor: &Actor, key: &str) -> ApiResult<()> {
    let row = find(t, actor, key)?;
    policy::admin_wiki(actor, &row.project, "deleting a page")?;
    live(&row)?;
    t.execute(
        "UPDATE wiki_pages SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1",
        params![row.id, now()],
    )?;
    t.execute("DELETE FROM wiki_fts WHERE page = ?1", [&row.id])?;
    t.audit(
        "wiki.delete",
        &format!("{}/{}", row.project, row.path),
        Some(&row.project),
        json!({"page_id": row.id, "rev": row.current_rev}),
    )?;
    emit(t, &row, "deleted", json!({}))?;
    Ok(())
}

/// Move a page to a new path.
pub fn move_page(t: &mut Tx<'_>, actor: &Actor, key: &str, path: &str) -> ApiResult<WikiPage> {
    let row = find(t, actor, key)?;
    policy::admin_wiki(actor, &row.project, "moving a page")?;
    live(&row)?;
    let path = check_path(path)?;
    if path == row.path {
        return render(t, row);
    }
    if row.path == INDEX {
        return Err(ApiError::invalid("the index page stays at `index`"));
    }
    if by_path(t, &row.project, &path)?.is_some() {
        return Err(ApiError::conflict(format!("page `{path}` exists")));
    }
    t.execute(
        "UPDATE wiki_pages SET path = ?2, updated_at = ?3 WHERE id = ?1",
        params![row.id, path, now()],
    )?;
    let new = by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("page vanished"))?;
    let body = body_at(t, &new.id, new.current_rev)?;
    index(t, &new, &body)?;
    t.audit(
        "wiki.move",
        &format!("{}/{}", row.project, row.path),
        Some(&row.project),
        json!({"page_id": row.id, "from": row.path, "to": path}),
    )?;
    emit(t, &new, "moved", json!({"from": row.path}))?;
    render(t, new)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// `approve` (the page becomes `reviewed`) or `comment`.
    pub decision: String,
    #[serde(default)]
    pub comment: Option<String>,
}

/// Review a page and mail the author of its current revision.
pub fn review(t: &mut Tx<'_>, actor: &Actor, key: &str, r: Review) -> ApiResult<WikiPage> {
    let row = find(t, actor, key)?;
    live(&row)?;
    policy::review_page(actor, &row.project, row.reviewer.as_deref())?;
    let comment = r
        .comment
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let current = revision_row(t, &row.id, row.current_rev)?;
    let author = current.author.clone();
    let reviewer = Address::from_actor(actor).display();
    let (row, subject) = match r.decision.as_str() {
        "approve" => {
            let body = set_status(&current.body.unwrap_or_default(), "reviewed");
            let summary = match comment {
                Some(c) => format!("Reviewed by {reviewer}: {c}"),
                None => format!("Reviewed by {reviewer}"),
            };
            let new = write_revision(t, &actor.address(), &row, &body, &summary, None)?;
            (new, format!("Reviewed: {}", row.path))
        }
        "comment" => {
            if comment.is_none() {
                return Err(ApiError::new(
                    Code::CommentRequired,
                    "a review comment needs a comment",
                ));
            }
            (row.clone(), format!("Review comment: {}", row.path))
        }
        other => {
            return Err(ApiError::invalid(format!(
                "bad review decision `{other}`: use approve or comment"
            )));
        }
    };
    emit(
        t,
        &row,
        "reviewed",
        json!({"decision": r.decision, "comment": comment, "reviewer": actor.address()}),
    )?;
    if author != "system" && author != actor.address() {
        let to = mail::resolve(t, &author, Some(&row.project))?;
        let from = Address::Project(row.project.clone());
        let body = format!(
            "{reviewer} reviewed wiki page `{}` (rev {}): {}\n\n{}\n\nOpen: /projects/{}/wiki/{}",
            row.path,
            row.current_rev,
            if r.decision == "approve" {
                "approved"
            } else {
                "comment"
            },
            comment.unwrap_or("(no comment)"),
            row.project,
            row.path
        );
        mail::send(
            t,
            Outgoing {
                from: &from,
                to: &to,
                subject: &subject,
                body: &body,
                thread: None,
                in_reply_to: None,
            },
        )?;
    }
    render(t, row)
}

// ---- reads ----

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WikiFilter {
    pub q: Option<String>,
    /// List deleted pages instead.
    pub deleted: Option<bool>,
    /// Only pages with this front matter status.
    pub status: Option<String>,
}

/// The page tree by path (cursor: path), or search results best first
/// (cursor: an offset).
pub fn list(
    conn: &Connection,
    project: &str,
    f: &WikiFilter,
    page: &Page,
) -> ApiResult<Paged<PageSummary>> {
    projects::get(conn, project)?;
    let limit = page.limit();
    if let Some(q) = f.q.as_deref().and_then(|q| fts_query(q, "AND")) {
        let offset: i64 = page.cursor().and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut stmt = conn.prepare(
            "SELECT p.*, snippet(wiki_fts, 3, '**', '**', '…', 16) AS snip FROM wiki_fts
               JOIN wiki_pages p ON p.id = wiki_fts.page
             WHERE wiki_fts MATCH ?1 AND p.project = ?2 AND p.deleted_at IS NULL
             ORDER BY wiki_fts.rank LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt
            .query_map(params![q, project, (limit + 1) as i64, offset], |r| {
                Ok((page_row(r)?, r.get::<_, String>("snip")?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let more = rows.len() > limit;
        let items = rows
            .into_iter()
            .take(limit)
            .map(|(r, s)| summary_of(r, Some(s)))
            .collect();
        return Ok(Paged {
            items,
            next: more.then(|| (offset + limit as i64).to_string()),
        });
    }
    let after = page.cursor().unwrap_or("").to_owned();
    let deleted = f.deleted.unwrap_or(false);
    let mut stmt = conn.prepare(
        "SELECT * FROM wiki_pages WHERE project = ?1 AND path > ?2
           AND ((?3 AND deleted_at IS NOT NULL) OR (NOT ?3 AND deleted_at IS NULL))
           AND (?4 IS NULL OR status = ?4)
         ORDER BY path, id LIMIT ?5",
    )?;
    let rows = stmt
        .query_map(
            params![project, after, deleted, f.status, (limit + 1) as i64],
            page_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let items = rows.into_iter().map(|r| summary_of(r, None)).collect();
    Ok(Paged::from_rows(items, limit, |p: &PageSummary| {
        p.path.clone()
    }))
}

/// Revisions newest first, without bodies. Cursor: rev.
pub fn revisions(
    conn: &Connection,
    actor: &Actor,
    key: &str,
    page: &Page,
) -> ApiResult<Paged<Revision>> {
    let row = find(conn, actor, key)?;
    let limit = page.limit();
    let before: i64 = page
        .cursor()
        .and_then(|c| c.parse().ok())
        .unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT rev, author, summary, created_at FROM wiki_revisions
         WHERE page = ?1 AND rev < ?2 ORDER BY rev DESC LIMIT ?3",
    )?;
    let raw = stmt
        .query_map(params![row.id, before, (limit + 1) as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut items = Vec::new();
    for (rev, author, summary, created_at) in raw {
        items.push(Revision {
            page: row.id.clone(),
            rev,
            author_name: name_of(conn, &author)?,
            author,
            summary,
            created_at,
            body: None,
        });
    }
    Ok(Paged::from_rows(items, limit, |r| r.rev.to_string()))
}

pub fn revision(conn: &Connection, actor: &Actor, key: &str, rev: i64) -> ApiResult<Revision> {
    let row = find(conn, actor, key)?;
    revision_row(conn, &row.id, rev)
}

/// The diff from rev `a` (default: one before `b`) to rev `b` (default:
/// the current one).
pub fn diff(
    conn: &Connection,
    actor: &Actor,
    key: &str,
    a: Option<i64>,
    b: Option<i64>,
) -> ApiResult<Diff> {
    let row = find(conn, actor, key)?;
    let b = b.unwrap_or(row.current_rev);
    let a = a.unwrap_or((b - 1).max(1));
    let old = body_at(conn, &row.id, a)?;
    let new = body_at(conn, &row.id, b)?;
    Ok(Diff {
        diff: unified_diff(&old, &new, &format!("rev {a}"), &format!("rev {b}")),
        page: row.id,
        a,
        b,
    })
}

/// Live pages that link to this page.
pub fn backlinks(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<Vec<PageSummary>> {
    let row = find(conn, actor, key)?;
    let target = format!("{}/{}", row.project, row.path);
    let mut stmt = conn.prepare(
        "SELECT p.* FROM wiki_links l JOIN wiki_pages p ON p.id = l.page
         WHERE l.kind = 'page' AND l.target = ?1 AND p.deleted_at IS NULL AND p.id <> ?2
         ORDER BY p.project, p.path",
    )?;
    let rows = stmt
        .query_map(params![target, row.id], page_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|r| policy::read_project(actor, &r.project).is_ok())
        .map(|r| summary_of(r, None))
        .collect())
}

/// The live pages of `project` whose latest revision at `until` differs
/// from the one at `since`. Used by version compare.
pub fn changed_between(
    conn: &Connection,
    project: &str,
    since: &str,
    until: &str,
) -> ApiResult<Vec<PageChange>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.path,
                (SELECT max(rev) FROM wiki_revisions r WHERE r.page = p.id AND r.created_at <= ?2),
                (SELECT max(rev) FROM wiki_revisions r WHERE r.page = p.id AND r.created_at <= ?3)
         FROM wiki_pages p WHERE p.project = ?1 AND p.deleted_at IS NULL ORDER BY p.path",
    )?;
    let rows = stmt
        .query_map(params![project, since, until], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, path, from_rev, to_rev)| {
            let to_rev = to_rev?;
            (from_rev != Some(to_rev)).then_some(PageChange {
                id,
                path,
                from_rev,
                to_rev,
            })
        })
        .collect())
}

/// A page that changed between two times.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageChange {
    pub id: String,
    pub path: String,
    /// The revision at the earlier time; absent for a new page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_rev: Option<i64>,
    pub to_rev: i64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_status() {
        let b = "---\ntitle: Perf\nstatus: ready_for_review\nreviewer: lead\n---\n# H\n";
        let fm = front_matter(b);
        assert_eq!(fm.title.as_deref(), Some("Perf"));
        assert_eq!(fm.status.as_deref(), Some("ready_for_review"));
        assert_eq!(fm.reviewer.as_deref(), Some("lead"));
        let r = set_status(b, "reviewed");
        assert_eq!(front_matter(&r).status.as_deref(), Some("reviewed"));
        assert!(r.ends_with("# H\n"));
        let r = set_status("# X\n", "reviewed");
        assert_eq!(front_matter(&r).status.as_deref(), Some("reviewed"));
        assert_eq!(first_heading(&r).as_deref(), Some("X"));
    }

    #[test]
    fn sections() {
        let b = "# T\n\nintro\n\n## A\n\none\n\n### A.1\n\nsub\n\n## B\n\ntwo\n";
        let r = replace_section(b, "A", "new a");
        assert!(r.contains("## A\n\nnew a\n\n## B"), "{r}");
        assert!(!r.contains("sub"));
        let r = replace_section(b, "## b", "new b");
        assert!(r.ends_with("## B\n\nnew b\n"), "{r}");
        let r = replace_section(b, "C", "c");
        assert!(r.ends_with("## C\n\nc\n"), "{r}");
        assert_eq!(section_text(b, "B").as_deref(), Some("two"));
        // Headings inside code fences do not count.
        let f = "## A\n\n```\n## B\n```\n\n## C\nc\n";
        assert_eq!(section_text(f, "A").as_deref(), Some("```\n## B\n```"));
    }

    #[test]
    fn paths() {
        assert_eq!(
            check_path("reports/perf-week-3").unwrap(),
            "reports/perf-week-3"
        );
        assert!(check_path("Reports").is_err());
        assert!(check_path("a//b").is_err());
        assert!(check_path("a/../b").is_err());
        assert!(check_path("").is_err());
    }

    #[test]
    fn links() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let body = "See [[designs/runner-v2]], [[TER-42]], [[MEM-17|a fact]], [[v0.3.0]], \
                    [[project:other/reports/x]], ![d](artifact:ART-9), [r](artifact:art-10)\n\
                    ```artifact ART-11```\n";
        let links = extract_links(&conn, "p", body).unwrap();
        let got: Vec<(String, String)> = links.into_iter().map(|l| (l.kind, l.target)).collect();
        for want in [
            ("page", "p/designs/runner-v2"),
            ("card", "TER-42"),
            ("memory", "p/MEM-17"),
            ("version", "p/v0.3.0"),
            ("page", "other/reports/x"),
            ("artifact", "p/ART-9"),
            ("artifact", "p/ART-10"),
            ("artifact", "p/ART-11"),
        ] {
            assert!(
                got.contains(&(want.0.to_owned(), want.1.to_owned())),
                "{want:?} not in {got:?}"
            );
        }
    }
}
