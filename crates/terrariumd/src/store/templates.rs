//! Agent templates (PLAN §5b, docs/api.md §21): named agent settings that
//! an agent create copies.
//!
//! A template belongs to a project, or is global (`scope` is `''`). Its
//! spec is an agent definition without `name` and `project`, kept in the
//! normal form of `agent_template::normalize`. `revision` starts at 1 and
//! goes up at each change, so an agent can show which version it copied.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::agent_template::{self, Lookup, Spec, TemplateDef};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::{check_name, projects};

/// One template, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    /// The project. `None` is a global template.
    pub project: Option<String>,
    pub description: String,
    pub spec: Spec,
    pub revision: i64,
    pub created_by: String,
    /// `api`, `file` (a `[[templates]]` of a project file), or
    /// `fleet:<name>`.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// The template that an agent was made from: `agents.template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateRef {
    pub id: String,
    pub name: String,
    pub project: Option<String>,
    pub revision: i64,
}

impl Template {
    /// `project/name`, or `_/name` for a global template.
    pub fn key(&self) -> String {
        terrarium_core::fleet::template_key(self.project.as_deref(), &self.name)
    }

    /// The name that picks this template for an agent of its project:
    /// `name`, or `global:name` for a global template.
    pub fn use_key(&self) -> String {
        match self.project {
            Some(_) => self.name.clone(),
            None => format!("{}{}", agent_template::GLOBAL_PREFIX, self.name),
        }
    }

    pub fn reference(&self) -> TemplateRef {
        TemplateRef {
            id: self.id.clone(),
            name: self.name.clone(),
            project: self.project.clone(),
            revision: self.revision,
        }
    }

    /// The file form: `[[templates]]` and `[[projects.templates]]`.
    pub fn def(&self) -> TemplateDef {
        TemplateDef {
            name: self.name.clone(),
            description: (!self.description.is_empty()).then(|| self.description.clone()),
            spec: self.spec.clone(),
        }
    }
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<Template> {
    let spec: String = r.get("spec")?;
    Ok(Template {
        id: r.get("id")?,
        name: r.get("name")?,
        project: r.get("project")?,
        description: r.get("description")?,
        spec: serde_json::from_str(&spec).unwrap_or_default(),
        revision: r.get("revision")?,
        created_by: r.get("created_by")?,
        source: r.get("source")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

/// A bad spec: `422 invalid` with `details.field = "spec"`.
fn spec_error(e: impl std::fmt::Display) -> ApiError {
    crate::store::schedules::field_error("spec", e)
}

/// [`normalize`], and the checks that read the store: a pi template names
/// no plan login (PLAN §16.3).
fn checked(conn: &Connection, spec: &Spec) -> ApiResult<Spec> {
    let n = normalize(spec)?;
    let def = agent_template::to_def(&n, "x", "x").map_err(spec_error)?;
    crate::store::agents::check_plan_logins(
        conn,
        def.harness,
        def.subscription.as_deref(),
        &def.subscription_pool,
    )
    .map_err(|e| spec_error(e.message))?;
    Ok(n)
}

/// Check a spec and give its normal form.
pub fn normalize(spec: &Spec) -> ApiResult<Spec> {
    let n = agent_template::normalize(spec).map_err(spec_error)?;
    // The checks of an agent create that need no name and no project.
    let def = agent_template::to_def(&n, "x", "x").map_err(spec_error)?;
    if def.policy.mail_to.iter().any(String::is_empty) {
        return Err(spec_error("policy.mail_to has an empty entry"));
    }
    if let Some(bad) = def.policy.cross_project.iter().find(|e| {
        e.split_once('/')
            .is_none_or(|(p, n)| p.is_empty() || n.is_empty())
    }) {
        return Err(spec_error(format!(
            "policy.cross_project entry `{bad}` must be `project/agent` or `project/*`"
        )));
    }
    Ok(n)
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Template> {
    conn.query_row(
        "SELECT * FROM agent_templates WHERE id = ?1",
        [id],
        from_row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("no template `{id}`")))
}

/// The template `name` of `project` (`None`: global).
pub fn find_in(
    conn: &Connection,
    project: Option<&str>,
    name: &str,
) -> ApiResult<Option<Template>> {
    Ok(conn
        .query_row(
            "SELECT * FROM agent_templates WHERE scope = ?1 AND name = ?2",
            params![project.unwrap_or_default(), name],
            from_row,
        )
        .optional()?)
}

/// Find a template by ULID, `project/name`, `_/name` or `global:name` (a
/// global one), or a name: first in `context` (the caller's project),
/// then global.
pub fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<Template> {
    let key = key.trim();
    if key.parse::<ulid::Ulid>().is_ok()
        && let Ok(t) = get(conn, key)
    {
        return Ok(t);
    }
    let found = match (agent_template::lookup(key), key.split_once('/')) {
        (Lookup::Global(n), _) | (_, Some(("_", n))) => find_in(conn, None, n)?,
        (_, Some((p, n))) => find_in(conn, Some(p), n)?,
        (Lookup::Nearest(n), None) => match context {
            Some(p) => match find_in(conn, Some(p), n)? {
                Some(t) => Some(t),
                None => find_in(conn, None, n)?,
            },
            None => find_in(conn, None, n)?,
        },
    };
    found.ok_or_else(|| ApiError::not_found(format!("no template `{key}`")))
}

/// The template that `key` picks for an agent of `project`: the
/// project's template, else the global one; `global:name` picks the
/// global one. A template of another project is `422 invalid`: an agent
/// uses its project's templates and the global ones.
pub fn resolve(conn: &Connection, project: &str, key: &str) -> ApiResult<Template> {
    let t = find(conn, key, Some(project)).map_err(|e| {
        if e.code == crate::error::Code::NotFound {
            ApiError::not_found(format!(
                "no template `{key}` in project `{project}` or among the global templates"
            ))
        } else {
            e
        }
    })?;
    match &t.project {
        Some(p) if p != project => Err(crate::store::schedules::field_error(
            "template",
            format!(
                "template `{}` belongs to project `{p}`; an agent of `{project}` uses the templates of its project and the global ones",
                t.key()
            ),
        )),
        _ => Ok(t),
    }
}

/// The filters of a list.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only this project's templates (and the global ones with `global`).
    pub project: Option<String>,
    /// The global templates too (with `project`), or only them (without).
    pub global: bool,
    /// Without `project` and `global`: all templates.
    pub all: bool,
}

/// Templates, global first, then by project and name.
pub fn list(conn: &Connection, f: &ListFilter) -> ApiResult<Vec<Template>> {
    let mut stmt = conn.prepare("SELECT * FROM agent_templates ORDER BY scope, name")?;
    let all: Vec<Template> = stmt.query_map([], from_row)?.collect::<Result<_, _>>()?;
    Ok(all
        .into_iter()
        .filter(|t| {
            f.all
                || match (&t.project, &f.project) {
                    (None, _) => f.global,
                    (Some(p), Some(want)) => p == want,
                    (Some(_), None) => false,
                }
        })
        .collect())
}

/// The templates of one project (`None`: the global ones).
pub fn of_scope(conn: &Connection, project: Option<&str>) -> ApiResult<Vec<Template>> {
    let mut stmt = conn.prepare("SELECT * FROM agent_templates WHERE scope = ?1 ORDER BY name")?;
    let rows = stmt
        .query_map([project.unwrap_or_default()], from_row)?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// A new template.
#[derive(Debug, Clone)]
pub struct NewTemplate {
    pub name: String,
    pub project: Option<String>,
    pub description: String,
    pub spec: Spec,
    pub created_by: String,
    pub source: String,
}

fn emit(t: &mut Tx<'_>, tpl: &Template, change: &str) -> ApiResult<()> {
    let mut d = EventDraft::new(
        "template",
        format!("template {} {change}", tpl.key()),
        json!({"change": change, "template": tpl.id, "name": tpl.name, "key": tpl.key(),
               "revision": tpl.revision}),
    );
    if let Some(p) = &tpl.project {
        d = d.project(p.clone());
    }
    t.emit(d)?;
    Ok(())
}

/// Make a template.
pub fn create(t: &mut Tx<'_>, new: &NewTemplate) -> ApiResult<Template> {
    check_name("template", &new.name)
        .map_err(|e| crate::store::schedules::field_error("name", e.message))?;
    if let Some(p) = &new.project
        && projects::find(t, p)?.is_none()
    {
        return Err(ApiError::not_found(format!("no project `{p}`")));
    }
    let spec = checked(t, &new.spec)?;
    if find_in(t, new.project.as_deref(), &new.name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "template `{}` exists in {}",
            new.name,
            scope_words(new.project.as_deref())
        )));
    }
    let id = ulid::Ulid::new().to_string();
    let at = now();
    t.execute(
        "INSERT INTO agent_templates (id, name, project, scope, description, spec, revision,
                                      created_by, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9)",
        params![
            id,
            new.name,
            new.project,
            new.project.as_deref().unwrap_or_default(),
            new.description,
            Value::Object(spec).to_string(),
            new.created_by,
            new.source,
            at
        ],
    )?;
    let tpl = get(t, &id)?;
    t.audit(
        "template.create",
        &tpl.id,
        tpl.project.as_deref(),
        json!({"name": tpl.name, "spec": tpl.spec, "source": tpl.source}),
    )?;
    emit(t, &tpl, "created")?;
    Ok(tpl)
}

fn scope_words(project: Option<&str>) -> String {
    match project {
        Some(p) => format!("project `{p}`"),
        None => "the global scope".into(),
    }
}

/// The fields of a change. `None` keeps the value.
#[derive(Debug, Clone, Default)]
pub struct TemplatePatch {
    pub name: Option<String>,
    pub description: Option<String>,
    /// The whole spec: it replaces the old one.
    pub spec: Option<Spec>,
    /// A new `source` (a fleet or a file takes the template over).
    pub source: Option<String>,
}

/// Change a template. A change of the name, the description, or the spec
/// adds 1 to `revision`; an equal value is no change.
pub fn update(t: &mut Tx<'_>, id: &str, p: &TemplatePatch) -> ApiResult<Template> {
    let before = get(t, id)?;
    let name = match &p.name {
        Some(n) if *n != before.name => {
            check_name("template", n)
                .map_err(|e| crate::store::schedules::field_error("name", e.message))?;
            if find_in(t, before.project.as_deref(), n)?.is_some() {
                return Err(ApiError::conflict(format!(
                    "template `{n}` exists in {}",
                    scope_words(before.project.as_deref())
                )));
            }
            n.clone()
        }
        _ => before.name.clone(),
    };
    let spec = match &p.spec {
        Some(s) => checked(t, s)?,
        None => before.spec.clone(),
    };
    let description = p
        .description
        .clone()
        .unwrap_or_else(|| before.description.clone());
    let changed = name != before.name || spec != before.spec || description != before.description;
    let source = p.source.clone().unwrap_or_else(|| before.source.clone());
    if !changed && source == before.source {
        return Ok(before);
    }
    t.execute(
        "UPDATE agent_templates SET name = ?2, description = ?3, spec = ?4,
                revision = revision + ?5, source = ?6, updated_at = ?7
         WHERE id = ?1",
        params![
            id,
            name,
            description,
            Value::Object(spec).to_string(),
            i64::from(changed),
            source,
            now()
        ],
    )?;
    let after = get(t, id)?;
    if changed {
        t.audit(
            "template.update",
            id,
            after.project.as_deref(),
            json!({"name": after.name, "revision": after.revision,
                   "spec": p.spec.as_ref().map(|_| &after.spec),
                   "description": p.description}),
        )?;
        emit(t, &after, "updated")?;
    }
    Ok(after)
}

pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let tpl = get(t, id)?;
    t.execute("DELETE FROM agent_templates WHERE id = ?1", [id])?;
    t.audit(
        "template.delete",
        id,
        tpl.project.as_deref(),
        json!({"name": tpl.name, "revision": tpl.revision}),
    )?;
    emit(t, &tpl, "deleted")?;
    Ok(())
}

/// Write the template of a project file or a fleet: make it, or change
/// it to the file values and give it `source`.
pub fn upsert_def(
    t: &mut Tx<'_>,
    project: Option<&str>,
    def: &TemplateDef,
    source: &str,
    actor: &str,
) -> ApiResult<Template> {
    match find_in(t, project, &def.name)? {
        Some(have) => update(
            t,
            &have.id,
            &TemplatePatch {
                name: None,
                description: Some(def.description.clone().unwrap_or_default()),
                spec: Some(def.spec.clone()),
                source: Some(source.to_owned()),
            },
        ),
        None => create(
            t,
            &NewTemplate {
                name: def.name.clone(),
                project: project.map(str::to_owned),
                description: def.description.clone().unwrap_or_default(),
                spec: def.spec.clone(),
                created_by: actor.to_owned(),
                source: source.to_owned(),
            },
        ),
    }
}

/// Sync the `[[templates]]` of a project file: write each one with
/// `source = file`, and delete the file templates of the project that the
/// file no longer has. Returns the errors, one line each.
pub fn sync_file(t: &mut Tx<'_>, project: &str, defs: &[TemplateDef]) -> ApiResult<Vec<String>> {
    let mut errors = Vec::new();
    for d in defs {
        if let Err(e) = upsert_def(t, Some(project), d, projects::SOURCE_FILE, "operator") {
            errors.push(format!(
                "projects/{project}.toml template {}: {}",
                d.name, e.message
            ));
        }
    }
    for tpl in of_scope(t, Some(project))? {
        if tpl.source == projects::SOURCE_FILE && !defs.iter().any(|d| d.name == tpl.name) {
            delete(t, &tpl.id)?;
        }
    }
    Ok(errors)
}

/// A project file is gone: its file templates become API templates.
pub fn detach_file(t: &Tx<'_>, project: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE agent_templates SET source = 'api' WHERE project = ?1 AND source = 'file'",
        [project],
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        crate::db::migrate(&mut c).unwrap();
        for p in ["terra", "other"] {
            c.execute(
                "INSERT INTO projects (name, description, created_at, updated_at)
                 VALUES (?1, '', '2026-01-01', '2026-01-01')",
                [p],
            )
            .unwrap();
        }
        c
    }

    fn spec(v: Value) -> Spec {
        match v {
            Value::Object(m) => m,
            _ => panic!(),
        }
    }

    fn new(name: &str, project: Option<&str>, harness: &str) -> NewTemplate {
        NewTemplate {
            name: name.into(),
            project: project.map(str::to_owned),
            description: String::new(),
            spec: spec(json!({"harness": harness})),
            created_by: "operator".into(),
            source: "api".into(),
        }
    }

    #[test]
    fn crud_scopes_and_resolution() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let mut t = Tx::for_test(tx, "operator");
        let g = create(&mut t, &new("worker", None, "codex")).unwrap();
        let p = create(&mut t, &new("worker", Some("terra"), "claude")).unwrap();
        create(&mut t, &new("lone", Some("other"), "pi")).unwrap();
        // The normal form fills the defaults.
        assert_eq!(p.spec["policy"], json!({"targets": [], "mail_to": ["*"]}));
        assert_eq!(p.revision, 1);
        assert_eq!(g.key(), "_/worker");
        assert_eq!(p.key(), "terra/worker");
        // The name is unique in its scope only.
        let e = create(&mut t, &new("worker", Some("terra"), "pi")).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Conflict);
        let e = create(&mut t, &new("Bad", None, "pi")).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
        let e = create(&mut t, &new("x", Some("nope"), "pi")).unwrap_err();
        assert_eq!(e.code, crate::error::Code::NotFound);
        let mut bad = new("y", None, "pi");
        bad.spec = spec(json!({"harness": "pi", "name": "y"}));
        let e = create(&mut t, &bad).unwrap_err();
        assert_eq!(e.details, Some(json!({"field": "spec"})));

        // A project's template wins over the global one; `global:` and
        // `_/` pick the global one; a template of another project is
        // refused.
        assert_eq!(resolve(&t, "terra", "worker").unwrap().id, p.id);
        assert_eq!(resolve(&t, "other", "worker").unwrap().id, g.id);
        assert_eq!(resolve(&t, "terra", "global:worker").unwrap().id, g.id);
        assert_eq!(resolve(&t, "terra", "_/worker").unwrap().id, g.id);
        assert_eq!(resolve(&t, "terra", &p.id).unwrap().id, p.id);
        let e = resolve(&t, "terra", "other/lone").unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
        let e = resolve(&t, "terra", "lone").unwrap_err();
        assert_eq!(e.code, crate::error::Code::NotFound);

        // Lists: global first.
        let all = list(
            &t,
            &ListFilter {
                all: true,
                ..Default::default()
            },
        )
        .unwrap();
        let keys: Vec<String> = all.iter().map(Template::key).collect();
        assert_eq!(keys, ["_/worker", "other/lone", "terra/worker"]);
        let mine = list(
            &t,
            &ListFilter {
                project: Some("terra".into()),
                global: true,
                all: false,
            },
        )
        .unwrap();
        assert_eq!(mine.len(), 2);

        // A change adds 1 to the revision; the same values do not.
        let u = update(
            &mut t,
            &p.id,
            &TemplatePatch {
                spec: Some(spec(json!({"harness": "claude", "model": "m"}))),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(u.revision, 2);
        let same = update(
            &mut t,
            &p.id,
            &TemplatePatch {
                spec: Some(spec(json!({"harness": "claude", "model": "m"}))),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(same.revision, 2);
        delete(&mut t, &g.id).unwrap();
        assert!(find_in(&t, None, "worker").unwrap().is_none());
        t.execute("DELETE FROM projects WHERE name = 'other'", [])
            .unwrap();
        assert!(
            find_in(&t, Some("other"), "lone").unwrap().is_none(),
            "the project delete cascades"
        );
    }

    #[test]
    fn a_project_file_syncs_its_templates() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let mut t = Tx::for_test(tx, "system");
        let def = |n: &str, h: &str| TemplateDef {
            name: n.into(),
            description: None,
            spec: spec(json!({"harness": h})),
        };
        let errors = sync_file(&mut t, "terra", &[def("a", "pi"), def("b", "nope")]).unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        let a = find_in(&t, Some("terra"), "a").unwrap().unwrap();
        assert_eq!(a.source, "file");
        sync_file(&mut t, "terra", &[def("a", "claude")]).unwrap();
        let a2 = find_in(&t, Some("terra"), "a").unwrap().unwrap();
        assert_eq!((a2.id.as_str(), a2.revision), (a.id.as_str(), 2));
        sync_file(&mut t, "terra", &[]).unwrap();
        assert!(find_in(&t, Some("terra"), "a").unwrap().is_none());
    }
}
