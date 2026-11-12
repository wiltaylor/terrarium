//! Bounded project pages and their snap-grid layouts.
use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{monitors, projects};
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::monitors::layout::{self, Block, BlockKind, BlockOptions, Layout, Size};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Widget {
    pub id: String,
    pub kind: WidgetKind,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Absent means content height for markdown/wiki, fixed height for monitors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_height: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiki: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    #[serde(default)]
    pub options: BlockOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Markdown,
    Wiki,
    Stat,
    Status,
    Chart,
    Text,
    Table,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageInput {
    pub title: String,
    pub position: u32,
    pub widgets: Vec<Widget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectPage {
    pub project: String,
    pub slug: String,
    #[serde(flatten)]
    pub content: PageInput,
    pub updated_at: String,
    pub updated_by: String,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectPage> {
    let widgets: String = r.get(4)?;
    let widgets = serde_json::from_str(&widgets).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(ProjectPage {
        project: r.get(0)?,
        slug: r.get(1)?,
        content: PageInput {
            title: r.get(2)?,
            position: r.get(3)?,
            widgets,
        },
        updated_at: r.get(5)?,
        updated_by: r.get(6)?,
    })
}

const COLS: &str = "project, slug, title, position, widgets, updated_at, updated_by";

pub fn list(c: &Connection, project: &str) -> ApiResult<Vec<ProjectPage>> {
    projects::get(c, project)?;
    let mut s = c.prepare(&format!(
        "SELECT {COLS} FROM project_pages WHERE project=?1 ORDER BY position, slug LIMIT 10"
    ))?;
    Ok(s.query_map([project], row)?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn get(c: &Connection, project: &str, slug: &str) -> ApiResult<ProjectPage> {
    projects::get(c, project)?;
    c.query_row(
        &format!("SELECT {COLS} FROM project_pages WHERE project=?1 AND slug=?2"),
        params![project, slug],
        row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found("page not found"))
}

fn identifier(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn check(c: &Connection, project: &str, slug: &str, input: &PageInput) -> ApiResult<()> {
    if !identifier(slug, 64)
        || slug.bytes().any(|b| b.is_ascii_uppercase())
        || !slug.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(ApiError::invalid("invalid page slug"));
    }
    if input.title.trim().is_empty() || input.title.chars().count() > 120 {
        return Err(ApiError::invalid("page title must be 1-120 characters"));
    }
    if input.widgets.len() > 100 {
        return Err(ApiError::invalid("at most 100 widgets"));
    }
    let mut ids = HashSet::new();
    let mut blocks = Vec::new();
    for (i, w) in input.widgets.iter().enumerate() {
        let bad = |message: &str| ApiError::invalid(format!("widgets[{i}]: {message}"));
        if !identifier(&w.id, 40) || !ids.insert(&w.id) {
            return Err(bad("invalid or duplicate id"));
        }
        if w.x >= 12
            || w.w == 0
            || w.w > 12
            || w.x + w.w > 12
            || w.y >= 10000
            || w.h == 0
            || w.h > 10000
            || w.y + w.h > 10000
        {
            return Err(bad("invalid grid rectangle"));
        }
        if input.widgets[..i]
            .iter()
            .any(|p| w.x < p.x + p.w && p.x < w.x + w.w && w.y < p.y + p.h && p.y < w.y + w.h)
        {
            return Err(bad("overlapping grid rectangles"));
        }
        if w.title.as_ref().is_some_and(|s| s.chars().count() > 120) {
            return Err(bad("title too long"));
        }
        if w.options.decimals.is_some_and(|d| d > 6) {
            return Err(bad("decimals must be 0-6"));
        }
        match w.kind {
            WidgetKind::Markdown => {
                if w.monitor.is_some()
                    || w.wiki.is_some()
                    || w.text.as_ref().is_none_or(|s| s.len() > 16384)
                {
                    return Err(bad(
                        "markdown requires text of at most 16384 bytes, and no other source",
                    ));
                }
            }
            WidgetKind::Wiki => {
                if w.monitor.is_some()
                    || w.text.is_some()
                    || w.wiki.as_ref().is_none_or(|s| {
                        super::wiki::check_path(s).is_err()
                    })
                {
                    return Err(bad(
                        "wiki requires a project-local wiki path and no other source",
                    ));
                }
            }
            kind => {
                if w.text.is_some() || w.wiki.is_some() || w.auto_height == Some(true) {
                    return Err(bad(
                        "monitor widgets require only a monitor and fixed height",
                    ));
                }
                let kind = match kind {
                    WidgetKind::Stat => BlockKind::Stat,
                    WidgetKind::Status => BlockKind::Status,
                    WidgetKind::Chart => BlockKind::Chart,
                    WidgetKind::Text => BlockKind::Text,
                    WidgetKind::Table => BlockKind::Table,
                    _ => unreachable!(),
                };
                blocks.push(Block {
                    id: w.id.clone(),
                    kind,
                    monitor: w.monitor.clone(),
                    size: Size::S,
                    title: w.title.clone(),
                    text: None,
                    options: w.options.clone(),
                });
            }
        }
    }
    // Resolve only referenced IDs via the primary key, never an unbounded monitor listing.
    let mut kinds = std::collections::HashMap::new();
    for b in &blocks {
        if let Some(id) = &b.monitor {
            let m = monitors::get(c, id).map_err(|e| {
                if e.code == crate::error::Code::NotFound {
                    ApiError::invalid("no such monitor in this project")
                } else {
                    e
                }
            })?;
            if m.project != project {
                return Err(ApiError::invalid("monitor belongs to another project"));
            }
            kinds.insert(id.clone(), m.def.kind);
        }
    }
    layout::check(&Layout { blocks }, &kinds)
        .map_err(|(f, m)| ApiError::invalid(format!("{f}: {m}")))
}

pub fn set(
    t: &mut Tx<'_>,
    project: &str,
    slug: &str,
    input: &PageInput,
    actor: &str,
) -> ApiResult<ProjectPage> {
    projects::get(t, project)?;
    check(t, project, slug, input)?;
    let exists: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_pages WHERE project=?1 AND slug=?2)",
        params![project, slug],
        |r| r.get(0),
    )?;
    if !exists && list(t, project)?.len() >= 10 {
        return Err(ApiError::invalid("at most 10 pages per project"));
    }
    let widgets = serde_json::to_string(&input.widgets)?;
    t.execute("INSERT INTO project_pages(project,slug,title,position,widgets,updated_at,updated_by) VALUES(?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(project,slug) DO UPDATE SET title=excluded.title,position=excluded.position,widgets=excluded.widgets,updated_at=excluded.updated_at,updated_by=excluded.updated_by",
        params![project, slug, input.title, input.position, widgets, now(), actor])?;
    changed(t, project, slug, false)?;
    get(t, project, slug)
}

pub fn remove(t: &mut Tx<'_>, project: &str, slug: &str) -> ApiResult<()> {
    get(t, project, slug)?;
    t.execute(
        "DELETE FROM project_pages WHERE project=?1 AND slug=?2",
        params![project, slug],
    )?;
    changed(t, project, slug, true)
}

fn changed(t: &mut Tx<'_>, project: &str, slug: &str, deleted: bool) -> ApiResult<()> {
    let data = json!({"project":project,"slug":slug,"deleted":deleted});
    t.audit(
        if deleted { "page.remove" } else { "page.set" },
        slug,
        Some(project),
        data.clone(),
    )?;
    t.emit(
        EventDraft::new(
            "page.updated",
            format!("page {project}/{slug} updated"),
            data,
        )
        .project(project.to_owned()),
    )?;
    Ok(())
}
