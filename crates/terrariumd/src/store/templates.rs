//! Agent templates (PLAN §5b, docs/api.md §21): named agent settings that
//! an agent create copies.
//!
//! A template belongs to a project, or is global (`scope` is `''`). Its
//! spec is an agent definition without `name` and `project`, kept in the
//! normal form of `agent_template::normalize`. `revision` starts at 1 and
//! goes up at each change, so an agent can show which version it copied.
//!
//! The source of truth is the WCL file of each template in the config
//! repo (`terrarium_core::template_file`): this table is its index. Each
//! write here with a config repo (`repo`) stages the file in the same
//! transaction (`Tx::stage_file`) and records its hash in `file_hash`.
//! `crate::template_files` reads the files back into the table.
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::agent_template::{self, Lookup, Spec, TemplateDef};
use terrarium_core::template_file;

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
    /// `config` (a file in the config repo), or `fleet:<name>` (a fleet
    /// owns it; its file is in the config repo too). `api` and `file`
    /// are from before the files (migration 44), until the first load.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    /// The file in the config repo: `templates/<name>.wcl` or
    /// `templates/<project>/<name>.wcl`. `None` until the file is
    /// written (no config repo).
    pub file: Option<String>,
    /// The SHA-256 of the file that the row holds.
    #[serde(skip)]
    pub file_hash: Option<String>,
    /// The skills that an agent of the template gets now (PLAN §8.6): the
    /// default-on skills of the library and the spec's `skills`, less its
    /// `skills_off`, with the source of each. The API fills it.
    pub effective_skills: Vec<terrarium_core::skills::EffectiveSkill>,
    /// Set when the spec names a subscription that does not exist (issue
    /// #14): why an agent of the template would not start. The list of
    /// `GET /api/templates` fills it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_missing: Option<String>,
    /// Who last set or changed `spec.grants` (TER-88): `operator`,
    /// `system` (a template file), or `agent:<id>`. `None` with no grants.
    /// A spawn or a create from the template checks again that this actor
    /// may grant each secret (`crate::spawn_rights`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grants_by: Option<String>,
}

impl Template {
    /// The skill settings of the spec: `skills`, `skills_off`, and
    /// `default_skills` (default true). A bad value counts as absent.
    pub fn skill_settings(&self) -> (Vec<String>, Vec<String>, bool) {
        let list = |k: &str| -> Vec<String> {
            self.spec
                .get(k)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default()
        };
        let defaults = self
            .spec
            .get("default_skills")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        (list("skills"), list("skills_off"), defaults)
    }
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

    /// The path of the template's file in the config repo.
    pub fn rel_path(&self) -> String {
        template_file::rel_path(self.project.as_deref(), &self.name)
    }

    /// The text of the template's file, as [`stage_file`] writes it.
    pub fn render(&self) -> ApiResult<String> {
        template_file::render(
            &self.name,
            self.project.as_deref(),
            Some(&self.description),
            &self.spec,
        )
        .map_err(spec_error)
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
    let name: String = r.get("name")?;
    let project: Option<String> = r.get("project")?;
    let file_hash: Option<String> = r.get("file_hash")?;
    Ok(Template {
        id: r.get("id")?,
        file: file_hash
            .as_ref()
            .map(|_| template_file::rel_path(project.as_deref(), &name)),
        name,
        project,
        description: r.get("description")?,
        spec: serde_json::from_str(&spec).unwrap_or_default(),
        revision: r.get("revision")?,
        created_by: r.get("created_by")?,
        source: r.get("source")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        file_hash,
        effective_skills: Vec::new(),
        subscription_missing: None,
        grants_by: r.get("grants_by")?,
    })
}

/// The source of a template that a config repo file declares.
pub const SOURCE_CONFIG: &str = "config";

/// The refusal of a template file by the secret scan: `422 invalid` with
/// the findings in `details.findings`, as a commit of the config repo.
fn scan_refusal(findings: &[terrarium_core::secret_scan::Finding]) -> ApiError {
    let list: Vec<String> = findings.iter().map(ToString::to_string).collect();
    ApiError::invalid(format!(
        "the secret scan refused the template file ({} finding(s)): {}. A template holds no \
         secret: give the agent a secret grant (PLAN §16b), or for a false hit add `{}` to \
         the line",
        findings.len(),
        list.join("; "),
        terrarium_core::secret_scan::ALLOW_MARKER
    ))
    .with_details(json!({ "findings": findings }))
}

/// Stage the file of `tpl` in the config repo `repo` (it goes to disk at
/// the commit), after the secret scan, and record its hash. `None`: no
/// config repo; the row waits for its file.
pub fn stage_file(t: &mut Tx<'_>, repo: Option<&Path>, tpl: &Template) -> ApiResult<()> {
    let Some(repo) = repo else {
        return Ok(());
    };
    let rel = tpl.rel_path();
    let text = tpl.render()?;
    let findings = terrarium_core::secret_scan::scan(&rel, text.as_bytes());
    if !findings.is_empty() {
        return Err(scan_refusal(&findings));
    }
    let hash = crate::store::artifacts::sha256_hex(text.as_bytes());
    t.stage_file(repo.join(&rel), Some(text.into_bytes()));
    t.execute(
        "UPDATE agent_templates SET file_hash = ?2 WHERE id = ?1",
        params![tpl.id, hash],
    )?;
    Ok(())
}

/// Stage the removal of the file `rel` of the config repo `repo`.
fn stage_removal(t: &mut Tx<'_>, repo: Option<&Path>, rel: &str) {
    if let Some(repo) = repo {
        t.stage_file(repo.join(rel), None);
    }
}

/// A bad spec: `422 invalid` with `details.field = "spec"`.
fn spec_error(e: impl std::fmt::Display) -> ApiError {
    crate::store::schedules::field_error("spec", e)
}

/// [`normalize`], and the checks that read the store: a pi template names
/// no plan login (PLAN §16.3).
pub fn checked(conn: &Connection, spec: &Spec) -> ApiResult<Spec> {
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

/// The subscription that the spec names exists (issue #14): the check of
/// `POST /api/templates` and of a `PATCH` with a spec. A file load does not
/// run it: `GET /api/templates` flags such a template instead
/// ([`subscription_missing`]).
pub fn check_subscription(conn: &Connection, spec: &Spec) -> ApiResult<()> {
    let n = normalize(spec)?;
    let def = agent_template::to_def(&n, "x", "x").map_err(spec_error)?;
    crate::store::agents::check_subscription_exists(conn, def.harness, def.subscription.as_deref())
}

/// Why the subscription of a template's spec is missing; `None` when the
/// spec names none, names one that exists, or does not parse.
pub fn subscription_missing(conn: &Connection, spec: &Spec) -> ApiResult<Option<String>> {
    let Ok(def) = agent_template::to_def(spec, "x", "x") else {
        return Ok(None);
    };
    let Some(name) = def.subscription.as_deref().filter(|n| !n.trim().is_empty()) else {
        return Ok(None);
    };
    if crate::store::subscriptions::find(conn, name)?.is_some() {
        return Ok(None);
    }
    crate::store::agents::missing_subscription(conn, def.harness, name).map(Some)
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
    // The syntax of each grant (TER-88). Who may use them is checked at
    // the spawn or the create, against its caller.
    for g in agent_template::grants_of(&n).map_err(spec_error)? {
        crate::spawn_rights::parse_grant(&g).map_err(|e| spec_error(e.message))?;
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

/// Make a template, and stage its file in `repo`.
pub fn create(t: &mut Tx<'_>, new: &NewTemplate, repo: Option<&Path>) -> ApiResult<Template> {
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
    let grants_by = grants_by_of(t, None, &spec);
    t.execute(
        "INSERT INTO agent_templates (id, name, project, scope, description, spec, revision,
                                      created_by, source, created_at, updated_at, grants_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9, ?10)",
        params![
            id,
            new.name,
            new.project,
            new.project.as_deref().unwrap_or_default(),
            new.description,
            Value::Object(spec).to_string(),
            new.created_by,
            new.source,
            at,
            grants_by
        ],
    )?;
    stage_file(t, repo, &get(t, &id)?)?;
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

/// The `grants_by` of a template after a write of `spec` (TER-88): the
/// writer (`t.actor`) when the write sets or changes `grants`, the old
/// value when they stay the same, `None` with no grants.
fn grants_by_of(t: &Tx<'_>, before: Option<&Template>, spec: &Spec) -> Option<String> {
    let new = agent_template::grants_of(spec).unwrap_or_default();
    if new.is_empty() {
        return None;
    }
    match before {
        Some(b) if agent_template::grants_of(&b.spec).unwrap_or_default() == new => {
            b.grants_by.clone()
        }
        _ => Some(t.actor.clone()),
    }
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

/// Change a template, and stage its file in `repo` (a rename moves the
/// file). A change of the name, the description, or the spec adds 1 to
/// `revision`; an equal value is no change.
pub fn update(
    t: &mut Tx<'_>,
    id: &str,
    p: &TemplatePatch,
    repo: Option<&Path>,
) -> ApiResult<Template> {
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
    let grants_by = grants_by_of(t, Some(&before), &spec);
    t.execute(
        "UPDATE agent_templates SET name = ?2, description = ?3, spec = ?4,
                revision = revision + ?5, source = ?6, updated_at = ?7, grants_by = ?8
         WHERE id = ?1",
        params![
            id,
            name,
            description,
            Value::Object(spec).to_string(),
            i64::from(changed),
            source,
            now(),
            grants_by
        ],
    )?;
    let mut after = get(t, id)?;
    if changed {
        if after.name != before.name {
            stage_removal(t, repo, &before.rel_path());
        }
        stage_file(t, repo, &after)?;
        after = get(t, id)?;
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

/// Delete a template, and stage the removal of its file in `repo`.
pub fn delete(t: &mut Tx<'_>, id: &str, repo: Option<&Path>) -> ApiResult<()> {
    let tpl = get(t, id)?;
    t.execute("DELETE FROM agent_templates WHERE id = ?1", [id])?;
    stage_removal(t, repo, &tpl.rel_path());
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
    repo: Option<&Path>,
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
            repo,
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
            repo,
        ),
    }
}

/// The `[[templates]]` of a project file, from before the template files:
/// each one that the store does not have becomes a template with its file
/// in `repo` (one import). The file is the template from then on: the
/// project file is not read for it again. Returns one line for each
/// entry, to tell the operator to remove `[[templates]]`, and the errors.
pub fn import_file(
    t: &mut Tx<'_>,
    project: &str,
    defs: &[TemplateDef],
    repo: Option<&Path>,
) -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    for d in defs {
        let path = template_file::rel_path(Some(project), &d.name);
        if find_in(t, Some(project), &d.name)?.is_some() {
            out.push(format!(
                "projects/{project}.toml: template {} is in {path} now; remove [[templates]] from the project file",
                d.name
            ));
            continue;
        }
        let new = NewTemplate {
            name: d.name.clone(),
            project: Some(project.to_owned()),
            description: d.description.clone().unwrap_or_default(),
            spec: d.spec.clone(),
            created_by: "operator".into(),
            source: SOURCE_CONFIG.into(),
        };
        match create(t, &new, repo) {
            Ok(_) => out.push(format!(
                "projects/{project}.toml: template {} moved to {path}; remove [[templates]] from the project file",
                d.name
            )),
            Err(e) => out.push(format!(
                "projects/{project}.toml template {}: {}",
                d.name, e.message
            )),
        }
    }
    Ok(out)
}

/// One row of [`index`]: `(id, project, name, file_hash)`.
pub type IndexRow = (String, Option<String>, String, Option<String>);

/// The rows of every template.
pub fn index(conn: &Connection) -> ApiResult<Vec<IndexRow>> {
    let mut stmt = conn.prepare("SELECT id, project, name, file_hash FROM agent_templates")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Write the template of a file into the table: make it (`source =
/// config`), or change it to the file's values. A new `hash` adds 1 to
/// `revision`, also when the values are the same (a comment changed):
/// the revision names one version of the file.
pub fn apply_file(
    t: &mut Tx<'_>,
    project: Option<&str>,
    name: &str,
    description: Option<String>,
    spec: &Spec,
    hash: &str,
) -> ApiResult<Template> {
    let description = description.unwrap_or_default();
    let tpl = match find_in(t, project, name)? {
        None => create(
            t,
            &NewTemplate {
                name: name.to_owned(),
                project: project.map(str::to_owned),
                description,
                spec: spec.clone(),
                created_by: "config".into(),
                source: SOURCE_CONFIG.into(),
            },
            None,
        )?,
        Some(have) => {
            let after = update(
                t,
                &have.id,
                &TemplatePatch {
                    name: None,
                    description: Some(description),
                    spec: Some(spec.clone()),
                    source: None,
                },
                None,
            )?;
            if after.revision == have.revision && have.file_hash.as_deref() != Some(hash) {
                t.execute(
                    "UPDATE agent_templates SET revision = revision + 1, updated_at = ?2
                     WHERE id = ?1",
                    params![have.id, now()],
                )?;
                let bumped = get(t, &have.id)?;
                emit(t, &bumped, "updated")?;
            }
            after
        }
    };
    t.execute(
        "UPDATE agent_templates SET file_hash = ?2 WHERE id = ?1",
        params![tpl.id, hash],
    )?;
    get(t, &tpl.id)
}

/// A project file is gone: its file templates become config templates
/// (rows from before the template files).
pub fn detach_file(t: &Tx<'_>, project: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE agent_templates SET source = 'config' WHERE project = ?1 AND source = 'file'",
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
        let g = create(&mut t, &new("worker", None, "codex"), None).unwrap();
        let p = create(&mut t, &new("worker", Some("terra"), "claude"), None).unwrap();
        create(&mut t, &new("lone", Some("other"), "pi"), None).unwrap();
        // The normal form fills the defaults.
        assert_eq!(p.spec["policy"], json!({"targets": [], "mail_to": ["*"]}));
        assert_eq!(p.revision, 1);
        assert_eq!(g.key(), "_/worker");
        assert_eq!(p.key(), "terra/worker");
        // The name is unique in its scope only.
        let e = create(&mut t, &new("worker", Some("terra"), "pi"), None).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Conflict);
        let e = create(&mut t, &new("Bad", None, "pi"), None).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
        let e = create(&mut t, &new("x", Some("nope"), "pi"), None).unwrap_err();
        assert_eq!(e.code, crate::error::Code::NotFound);
        let mut bad = new("y", None, "pi");
        bad.spec = spec(json!({"harness": "pi", "name": "y"}));
        let e = create(&mut t, &bad, None).unwrap_err();
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
            None,
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
            None,
        )
        .unwrap();
        assert_eq!(same.revision, 2);
        delete(&mut t, &g.id, None).unwrap();
        assert!(find_in(&t, None, "worker").unwrap().is_none());
        t.execute("DELETE FROM projects WHERE name = 'other'", [])
            .unwrap();
        assert!(
            find_in(&t, Some("other"), "lone").unwrap().is_none(),
            "the project delete cascades"
        );
    }

    #[test]
    fn a_project_file_imports_its_templates_once() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let mut t = Tx::for_test(tx, "system");
        let def = |n: &str, h: &str| TemplateDef {
            name: n.into(),
            description: None,
            spec: spec(json!({"harness": h})),
        };
        let lines =
            import_file(&mut t, "terra", &[def("a", "pi"), def("b", "nope")], None).unwrap();
        assert!(
            lines[0].contains("moved to templates/terra/a.wcl"),
            "{lines:?}"
        );
        assert!(lines[1].contains("template b: "), "{lines:?}");
        let a = find_in(&t, Some("terra"), "a").unwrap().unwrap();
        assert_eq!(a.source, SOURCE_CONFIG);
        // The second read does not change the template: the file is the
        // template now.
        let lines = import_file(&mut t, "terra", &[def("a", "claude")], None).unwrap();
        assert!(
            lines[0].contains("is in templates/terra/a.wcl now"),
            "{lines:?}"
        );
        let a2 = find_in(&t, Some("terra"), "a").unwrap().unwrap();
        assert_eq!((a2.revision, &a2.spec), (1, &a.spec));
    }

    #[test]
    fn writes_stage_the_files_with_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Some(dir.path());
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let mut t = Tx::for_test(tx, "operator");
        let mut n = new("worker", Some("terra"), "claude");
        n.description = "A worker".into();
        let w = create(&mut t, &n, repo).unwrap();
        assert_eq!(w.file.as_deref(), Some("templates/terra/worker.wcl"));
        // Nothing is on disk before the commit.
        let file = dir.path().join("templates/terra/worker.wcl");
        assert!(!file.exists());
        t.apply_files_for_test().unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, w.render().unwrap());
        assert_eq!(
            w.file_hash.as_deref(),
            Some(crate::store::artifacts::sha256_hex(text.as_bytes()).as_str())
        );
        let back = template_file::parse(&text, "worker.wcl").unwrap();
        assert_eq!(normalize(&back.spec).unwrap(), w.spec);
        assert_eq!(back.description.as_deref(), Some("A worker"));

        // A rename moves the file.
        let r = update(
            &mut t,
            &w.id,
            &TemplatePatch {
                name: Some("builder".into()),
                ..Default::default()
            },
            repo,
        )
        .unwrap();
        t.apply_files_for_test().unwrap();
        assert!(!file.exists());
        assert!(dir.path().join("templates/terra/builder.wcl").is_file());
        assert_eq!(r.revision, 2);

        // The secret scan refuses a secret, and stages nothing.
        let mut bad = new("leaky", None, "claude");
        bad.spec = spec(json!({"harness": "claude",
            "system_prompt": format!("Use the key sk-ant-api03-{}", "a".repeat(40))}));
        let e = create(&mut t, &bad, repo).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid, "{e:?}");
        assert!(e.message.contains("secret scan"), "{}", e.message);
        assert!(e.message.contains("templates/leaky.wcl"), "{}", e.message);

        // A delete removes the file.
        delete(&mut t, &r.id, repo).unwrap();
        t.apply_files_for_test().unwrap();
        assert!(!dir.path().join("templates/terra/builder.wcl").exists());
    }
}
