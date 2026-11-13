//! Whole-layout mutations. Call only inside Db::write's IMMEDIATE transaction.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    Board,
    Tests,
    Schedules,
    Rules,
    Agents,
    Repos,
    Knowledge,
    Pipelines,
    Calendar,
    Logs,
}

impl Builtin {
    pub const ALL: [Self; 10] = [
        Self::Board,
        Self::Tests,
        Self::Schedules,
        Self::Rules,
        Self::Agents,
        Self::Repos,
        Self::Knowledge,
        Self::Pipelines,
        Self::Calendar,
        Self::Logs,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::Tests => "tests",
            Self::Schedules => "schedules",
            Self::Rules => "rules",
            Self::Agents => "agents",
            Self::Repos => "repos",
            Self::Knowledge => "knowledge",
            Self::Pipelines => "pipelines",
            Self::Calendar => "calendar",
            Self::Logs => "logs",
        }
    }

    fn kinds(self) -> Vec<WidgetKind> {
        use WidgetKind::*;
        match self {
            Self::Board => vec![Board],
            Self::Tests => vec![Tests],
            Self::Schedules => vec![Schedules],
            Self::Rules => vec![Rules],
            Self::Agents => vec![Agents],
            Self::Repos => vec![Repos],
            Self::Knowledge => vec![WikiIndex, Memory, Db],
            Self::Pipelines => vec![Pipelines],
            Self::Calendar => vec![Calendar],
            Self::Logs => vec![Mail, Events],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutPage {
    pub slug: String,
    pub title: String,
    pub position: u32,
    pub widgets: Vec<Widget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<Builtin>,
}

impl LayoutPage {
    fn input(&self) -> PageInput {
        PageInput {
            title: self.title.clone(),
            position: self.position,
            widgets: self.widgets.clone(),
        }
    }
}

impl From<ProjectPage> for LayoutPage {
    fn from(p: ProjectPage) -> Self {
        Self {
            slug: p.slug,
            title: p.content.title,
            position: p.content.position,
            widgets: p.content.widgets,
            builtin: p.builtin,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutInput {
    pub base_revision: u64,
    pub pages: Vec<LayoutPage>,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub revision: u64,
    pub pages: Vec<ProjectPage>,
}

pub fn revision(c: &Connection, project: &str) -> ApiResult<u64> {
    Ok(c.query_row(
        "SELECT revision FROM project_page_layouts WHERE project=?1",
        [project],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or(0))
}

pub fn snapshot(c: &Connection, project: &str) -> ApiResult<Snapshot> {
    Ok(Snapshot {
        revision: revision(c, project)?,
        pages: list(c, project)?,
    })
}

pub(super) fn advance(t: &Tx<'_>, project: &str) -> ApiResult<u64> {
    t.execute(
        "INSERT INTO project_page_layouts(project,revision) VALUES(?1,1)
        ON CONFLICT(project) DO UPDATE SET revision=revision+1",
        [project],
    )?;
    revision(t, project)
}

fn base(t: &Tx<'_>, project: &str, expected: u64) -> ApiResult<()> {
    if !t.project_pages_v2 {
        return Err(ApiError::not_found("project pages v2 is disabled"));
    }
    projects::get(t, project)?;
    let current = revision(t, project)?;
    if current != expected {
        let mut e = ApiError::conflict("page layout changed");
        e.details = Some(json!({"revision": current}));
        return Err(e);
    }
    Ok(())
}

fn write_page(t: &Tx<'_>, project: &str, page: &LayoutPage, actor: &str) -> ApiResult<()> {
    t.execute("INSERT INTO project_pages(project,slug,title,position,widgets,updated_at,updated_by,builtin)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
        ON CONFLICT(project,slug) DO UPDATE SET title=excluded.title,position=excluded.position,
        widgets=excluded.widgets,updated_at=excluded.updated_at,updated_by=excluded.updated_by,builtin=excluded.builtin",
        params![project, page.slug, page.title, page.position, serde_json::to_string(&page.widgets)?,
            now(), actor, page.builtin.map(Builtin::slug)])?;
    Ok(())
}

fn finish(t: &mut Tx<'_>, project: &str, operation: &str) -> ApiResult<Snapshot> {
    let revision = advance(t, project)?;
    let data = json!({"project": project, "revision": revision, "operation": operation});
    t.audit("page.layout", project, Some(project), data.clone())?;
    t.emit(
        EventDraft::new(
            "page.updated",
            format!("page layout {project} updated"),
            data,
        )
        .project(project.to_owned()),
    )?;
    snapshot(t, project)
}

pub fn replace_layout(
    t: &mut Tx<'_>,
    project: &str,
    input: &LayoutInput,
    actor: &str,
    operation: &str,
) -> ApiResult<Snapshot> {
    base(t, project, input.base_revision)?;
    if input.pages.len() > 30 {
        return Err(ApiError::invalid("at most 30 pages per project"));
    }
    let mut slugs = HashSet::new();
    let mut builtins = HashSet::new();
    for (i, page) in input.pages.iter().enumerate() {
        if page.position as usize != i
            || !slugs.insert(&page.slug)
            || page.builtin.is_some_and(|b| !builtins.insert(b))
        {
            return Err(ApiError::invalid(
                "positions must match array order, slugs and builtins must be unique",
            ));
        }
        check(t, project, &page.slug, &page.input(), true)?;
    }
    t.execute("DELETE FROM project_pages WHERE project=?1", [project])?;
    for page in &input.pages {
        write_page(t, project, page, actor)?;
    }
    finish(t, project, operation)
}

/// The same canonical definition is used for upgrades, new projects and reset.
pub fn default_page(builtin: Builtin, slug: String, position: u32) -> LayoutPage {
    let name = builtin.slug();
    LayoutPage {
        slug,
        title: format!("{}{}", name[..1].to_ascii_uppercase(), &name[1..]),
        position,
        builtin: Some(builtin),
        widgets: builtin
            .kinds()
            .into_iter()
            .flat_map(|kind| {
                let views = if kind == WidgetKind::Calendar {
                    &kind.views()[..1]
                } else {
                    kind.views()
                };
                views.iter().map(move |view| (kind, *view))
            })
            .enumerate()
            .map(|(i, (kind, view))| Widget {
                id: format!("{name}-{i}"),
                kind,
                x: 0,
                y: i as u32 * 12,
                w: 12,
                h: 12,
                auto_height: None,
                title: None,
                text: None,
                wiki: None,
                monitor: None,
                options: WidgetOptions {
                    view: Some(view.into()),
                    ..Default::default()
                },
                frame: Some(Frame::Viewport),
            })
            .collect(),
    }
}

fn unused_slug(pages: &[ProjectPage], builtin: Builtin) -> String {
    let root = builtin.slug();
    if !pages.iter().any(|p| p.slug == root) {
        return root.into();
    }
    // There are at most 30 pages, so a free deterministic suffix always exists.
    for n in 1..=31 {
        let slug = format!("{root}-{n}");
        if !pages.iter().any(|p| p.slug == slug) {
            return slug;
        }
    }
    unreachable!("bounded page inventory")
}

/// Projects whose stored layout is not initialized yet, by name.
pub fn uninitialized_projects(c: &Connection) -> ApiResult<Vec<String>> {
    let mut stmt = c.prepare(
        "SELECT p.name FROM projects p LEFT JOIN project_page_layouts l ON l.project=p.name
        WHERE COALESCE(l.initialized,0)=0 ORDER BY p.name",
    )?;
    let names = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(names)
}

/// Explicit, idempotent initializer. No GET or SQL trigger calls this. With
/// the capability ON (TER-1077), the daemon calls it for every project at
/// startup before it serves requests, and in project creation.
pub fn initialize_defaults(t: &mut Tx<'_>, project: &str, actor: &str) -> ApiResult<()> {
    base(t, project, revision(t, project)?)?;
    let initialized: bool = t
        .query_row(
            "SELECT initialized FROM project_page_layouts WHERE project=?1",
            [project],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if initialized {
        return Ok(());
    }
    let mut pages = list(t, project)?;
    if pages.len() + Builtin::ALL.len() > 30 {
        return Err(ApiError::invalid("defaults exceed 30 pages"));
    }
    let mut position = pages.iter().map(|p| p.content.position).max().unwrap_or(0);
    for builtin in Builtin::ALL {
        position = position
            .checked_add(1)
            .ok_or_else(|| ApiError::invalid("page position overflow"))?;
        let page = default_page(builtin, unused_slug(&pages, builtin), position);
        write_page(t, project, &page, actor)?;
        pages.push(get(t, project, &page.slug)?);
    }
    t.execute(
        "INSERT INTO project_page_layouts(project,initialized) VALUES(?1,1)
        ON CONFLICT(project) DO UPDATE SET initialized=1",
        [project],
    )?;
    finish(t, project, "initialize")?;
    Ok(())
}

pub fn reset_builtin(
    t: &mut Tx<'_>,
    project: &str,
    expected: u64,
    builtin: Builtin,
    actor: &str,
) -> ApiResult<Snapshot> {
    base(t, project, expected)?;
    let pages = list(t, project)?;
    let (slug, position) = if let Some(p) = pages.iter().find(|p| p.builtin == Some(builtin)) {
        (p.slug.clone(), p.content.position)
    } else {
        if pages.len() >= 30 {
            return Err(ApiError::invalid("at most 30 pages per project"));
        }
        (
            unused_slug(&pages, builtin),
            pages
                .iter()
                .map(|p| p.content.position)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| ApiError::invalid("page position overflow"))?,
        )
    };
    write_page(t, project, &default_page(builtin, slug, position), actor)?;
    finish(t, project, "reset")
}

pub fn copy_layout(
    t: &mut Tx<'_>,
    target: &str,
    source: &str,
    expected: u64,
    confirm: bool,
    actor: &str,
) -> ApiResult<Snapshot> {
    base(t, target, expected)?;
    if !confirm {
        return Err(ApiError::invalid("copy requires confirm=true"));
    }
    let pages = list(t, source)?
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let mut p = LayoutPage::from(p);
            p.position = i as u32;
            p
        })
        .collect();
    replace_layout(
        t,
        target,
        &LayoutInput {
            base_revision: expected,
            pages,
        },
        actor,
        "copy",
    )
}
