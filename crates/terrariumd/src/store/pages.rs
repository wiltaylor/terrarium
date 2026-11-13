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

mod v2;
pub use v2::*;

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
    pub options: WidgetOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frame { Grid, FullWidth, Viewport }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WidgetOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<layout::Range>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparkline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
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
    Actions,
    MonitorSummary,
    JobQueue,
    EtaAccuracy,
    Board,
    Tests,
    Schedules,
    Rules,
    Agents,
    Repos,
    WikiIndex,
    Memory,
    Db,
    Pipelines,
    Calendar,
    Mail,
    Events,
}

impl WidgetKind {
    pub fn views(self) -> &'static [&'static str] {
        match self {
            Self::Board => &["lanes", "done", "columns"],
            Self::Tests => &["runs"],
            Self::Schedules => &["schedules", "triggers", "event_sources"],
            Self::Rules => &["flow", "lint", "table", "last_runs"],
            Self::Agents => &["agents", "card_agents"],
            Self::Repos => &["list", "files"],
            Self::WikiIndex => &["browser"],
            Self::Memory => &["list", "graph"],
            Self::Db => &["types", "records"],
            Self::Pipelines => &["state", "runs"],
            Self::Calendar => &["month", "week", "agenda"],
            Self::Mail => &["messages"],
            Self::Events => &["events"],
            _ => &[],
        }
    }
    pub fn is_panel(self) -> bool {
        matches!(self, Self::Board | Self::Tests | Self::Schedules | Self::Rules
            | Self::Agents | Self::Repos | Self::WikiIndex | Self::Memory | Self::Db
            | Self::Pipelines | Self::Calendar | Self::Mail | Self::Events)
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin: Option<Builtin>,
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
        builtin: r.get::<_, Option<String>>(7)?.map(|s| serde_json::from_value(json!(s)))
            .transpose().map_err(|e| rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e)))?,
    })
}

const COLS: &str = "project, slug, title, position, widgets, updated_at, updated_by, builtin";

pub fn dashboard_slug(c: &Connection, project: &str) -> ApiResult<String> {
    Ok(c.query_row(
        "SELECT slug FROM project_dashboard_pages WHERE project=?1",
        [project],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or_else(|| "dashboard".into()))
}

pub fn list(c: &Connection, project: &str) -> ApiResult<Vec<ProjectPage>> {
    projects::get(c, project)?;
    let mut s = c.prepare(&format!(
        "SELECT {COLS} FROM project_pages WHERE project=?1 ORDER BY position, slug LIMIT 31"
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

fn check(c: &Connection, project: &str, slug: &str, input: &PageInput, enabled: bool) -> ApiResult<()> {
    if !identifier(slug, 64)
        || slug.bytes().any(|b| b.is_ascii_uppercase())
        || !slug.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(ApiError::invalid("invalid page slug"));
    }
    if input.title.trim().is_empty() || input.title.chars().count() > 120 {
        return Err(ApiError::invalid("page title must be 1-120 characters"));
    }
    if input.widgets.len() > 104
        || input
            .widgets
            .iter()
            .filter(|w| {
                !matches!(
                    w.kind,
                    WidgetKind::Actions
                        | WidgetKind::MonitorSummary
                        | WidgetKind::JobQueue
                        | WidgetKind::EtaAccuracy
                )
            })
            .count()
            > 100
    {
        return Err(ApiError::invalid(
            "at most 100 content widgets and 104 widgets in total",
        ));
    }
    let mut ids = HashSet::new();
    let mut blocks = Vec::new();
    for (i, w) in input.widgets.iter().enumerate() {
        let bad = |message: &str| ApiError::invalid(format!("widgets[{i}]: {message}"));
        if !enabled && (w.kind.is_panel() || w.frame.is_some() || w.options.search.is_some() || w.options.view.is_some()) {
            return Err(bad("project pages v2 is disabled"));
        }
        if matches!(w.frame, Some(Frame::FullWidth | Frame::Viewport)) && (w.x != 0 || w.w != 12) {
            return Err(bad("full-width frames require x=0,w=12"));
        }
        if w.kind.is_panel() {
            if w.options.range.is_some() || w.options.sparkline.is_some() || w.options.decimals.is_some()
                || w.options.search.as_ref().is_some_and(|s| s.len() > 256)
                || w.options.view.as_deref().is_some_and(|v| !w.kind.views().contains(&v)) {
                return Err(bad("panels accept only bounded search and a view belonging to their kind"));
            }
        } else if w.options.search.is_some() || w.options.view.is_some() {
            return Err(bad("search/view are only valid for panels"));
        }
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
            kind if kind.is_panel() => {
                if w.monitor.is_some() || w.wiki.is_some() || w.text.is_some() {
                    return Err(bad("panels have no source override"));
                }
            }
            WidgetKind::Actions
            | WidgetKind::MonitorSummary
            | WidgetKind::JobQueue
            | WidgetKind::EtaAccuracy => {
                if w.monitor.is_some() || w.wiki.is_some() || w.text.is_some() {
                    return Err(bad("system widgets have no source"));
                }
            }
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
                    || w.wiki
                        .as_ref()
                        .is_none_or(|s| super::wiki::check_path(s).is_err())
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
                    options: BlockOptions { range: w.options.range, sparkline: w.options.sparkline, decimals: w.options.decimals },
                });
            }
        }
    }
    // Resolve only referenced IDs via the primary key, never an unbounded monitor listing.
    let mut kinds = std::collections::HashMap::new();
    for b in &blocks {
        if let Some(id) = &b.monitor {
            if enabled {
                if !identifier(id, 128) { return Err(ApiError::invalid("invalid local monitor reference")); }
                // Missing and foreign IDs are both local NotFound references. Never
                // resolve foreign data, including while copying another layout.
                match monitors::get(c, id) {
                    Ok(m) if m.project == project => { kinds.insert(id.clone(), m.def.kind); }
                    Ok(_) => { continue; }
                    Err(e) if e.code == crate::error::Code::NotFound => { continue; }
                    Err(e) => return Err(e),
                }
                continue;
            }
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
    if enabled {
        for b in &blocks {
            if b.monitor.is_none() { return Err(ApiError::invalid("monitor widget requires monitor")); }
        }
        blocks.retain(|b| b.monitor.as_ref().is_some_and(|id| kinds.contains_key(id)));
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
    check(t, project, slug, input, t.project_pages_v2)?;
    let exists: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_pages WHERE project=?1 AND slug=?2)",
        params![project, slug],
        |r| r.get(0),
    )?;
    let limit = if t.project_pages_v2 { 30 } else { 10 };
    if !exists && list(t, project)?.len() >= limit {
        return Err(ApiError::invalid(format!("at most {limit} pages per project")));
    }
    let widgets = serde_json::to_string(&input.widgets)?;
    t.execute("INSERT INTO project_pages(project,slug,title,position,widgets,updated_at,updated_by) VALUES(?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(project,slug) DO UPDATE SET title=excluded.title,position=excluded.position,widgets=excluded.widgets,updated_at=excluded.updated_at,updated_by=excluded.updated_by",
        params![project, slug, input.title, input.position, widgets, now(), actor])?;
    changed(t, project, slug, false)?;
    get(t, project, slug)
}

/// Keep the dashboard deletion behavior without repacking an edited snap grid
/// or validating unrelated stale references during monitor deletion.
pub fn drop_dashboard_monitor(t: &mut Tx<'_>, project: &str, id: &str) -> ApiResult<()> {
    let slug = dashboard_slug(t, project)?;
    let Some(mut page) = list(t, project)?.into_iter().find(|p| p.slug == slug) else {
        return Ok(());
    };
    let before = page.content.widgets.len();
    page.content
        .widgets
        .retain(|w| w.monitor.as_deref() != Some(id));
    if page.content.widgets.len() != before {
        t.execute(
            "UPDATE project_pages SET widgets=?1,updated_at=?2 WHERE project=?3 AND slug=?4",
            params![
                serde_json::to_string(&page.content.widgets)?,
                now(),
                project,
                slug
            ],
        )?;
        changed(t, project, &slug, false)?;
    }
    Ok(())
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
    let mut data = json!({"project":project,"slug":slug,"deleted":deleted});
    if t.project_pages_v2 {
        data["revision"] = json!(v2::advance(t, project)?);
        data["operation"] = json!(if deleted { "remove" } else { "set" });
    }
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
