//! Tool proxies (GitHub #21, the wiki page `design/tool-proxies`, P1a):
//! the definitions and the grants. No run path yet.
//!
//! - **Definitions.** `tools/<name>.toml` files of the config repo (source
//!   `file`, [`sync_files`]) and `POST /api/tools` (source `api`). The row
//!   keeps the `ToolDef` as JSON. [`validate`] adds to
//!   `ToolDef::check` the checks that need the store: the project, the
//!   target and its scope, the secrets of `env_secrets` and their scope,
//!   and the names of the sandbox `PATH`.
//! - **Grants.** `tool_grants`: one tool to an agent (`agent:<ulid>`) or a
//!   project (`project:<name>`), as `secret_grants`. A project tool goes
//!   only to its own project. Only the operator and sysadmins grant in P1
//!   (the API checks it). Removing a tool removes its grants (the foreign
//!   key); removing an agent or a project removes the grants to it.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;
use terrarium_core::tool::ToolDef;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::secrets::GLOBAL;
use crate::secrets::grants::{Grantee, parse_grantee};
use crate::store::projects::{SOURCE_API, SOURCE_FILE};
use crate::store::sync::Changes;

/// A stored tool.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolRow {
    #[serde(flatten)]
    pub def: ToolDef,
    /// `global` or `project`.
    pub scope: &'static str,
    /// `file` or `api`.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One grant of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolGrant {
    pub tool: String,
    /// `agent:<ulid>` or `project:<name>`.
    pub grantee: String,
    /// `agent:<project>/<name>` or `project:<name>`.
    pub grantee_label: String,
    pub created_by: String,
    pub created_at: String,
}

const COLS: &str = "name, def, source, created_at, updated_at";

fn row(r: &Row<'_>) -> rusqlite::Result<ToolRow> {
    let name: String = r.get(0)?;
    let def: String = r.get(1)?;
    let mut def: ToolDef = serde_json::from_str(&def).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    def.name = name;
    Ok(ToolRow {
        scope: if def.project.is_some() {
            "project"
        } else {
            "global"
        },
        def,
        source: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

/// The directories of the sandbox `PATH` that come from the host
/// (`crate::sandbox::SANDBOX_PATH`).
const PATH_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
];

/// Names that a tool may never take, also on a host that does not have
/// them: the tools that the agents and the prompts use, and the harnesses.
const RESERVED: &[&str] = &[
    "terrarium",
    "git",
    "ssh",
    "scp",
    "sh",
    "bash",
    "env",
    "sudo",
    "python",
    "python3",
    "node",
    "npm",
    "npx",
    "uv",
    "uvx",
    "pip",
    "cargo",
    "rustc",
    "make",
    "gh",
    "curl",
    "wget",
    "rg",
    "claude",
    "codex",
    "copilot",
    "pi",
    "bwrap",
];

/// Would a tool named `name` shadow a program of the sandbox `PATH`? The
/// shim goes after `/opt/terrarium/bin` on `PATH`, but an agent that types
/// `git` must get git, never a tool.
pub fn shadows(name: &str, extra_dirs: &[&Path]) -> bool {
    RESERVED.contains(&name)
        || PATH_DIRS
            .iter()
            .map(Path::new)
            .chain(extra_dirs.iter().copied())
            .any(|d| d.join(name).exists())
}

/// The checks of design D1 that need the store. `extra_dirs`: more `PATH`
/// dirs of the sandbox (the `bin/` of the tools dir).
pub fn validate(conn: &Connection, def: &ToolDef, extra_dirs: &[&Path]) -> ApiResult<()> {
    def.check().map_err(ApiError::invalid)?;
    if shadows(&def.name, extra_dirs) {
        return Err(ApiError::invalid(format!(
            "tool name `{}` is a program on the sandbox PATH: pick another name, so `{}` stays that program",
            def.name, def.name
        )));
    }
    if let Some(p) = &def.project {
        crate::store::projects::get(conn, p)?;
    }
    let target = crate::store::targets::find(conn, &def.target)?.ok_or_else(|| {
        ApiError::invalid(format!("tool `{}`: no target `{}`", def.name, def.target))
    })?;
    if let Some(p) = &def.project
        && target.def.project.as_deref() != Some(p.as_str())
    {
        return Err(ApiError::invalid(format!(
            "project tool `{}` names target `{}`, which is not a target of project `{p}`",
            def.name, def.target
        )));
    }
    let scope = def.project.as_deref().unwrap_or(GLOBAL);
    for (env, secret) in &def.env_secrets {
        if crate::secrets::get(conn, scope, secret)?.is_none() {
            let whose = def
                .project
                .as_deref()
                .map_or("global".to_owned(), |p| format!("project `{p}`"));
            return Err(ApiError::invalid(format!(
                "tool `{}`: env_secrets `{env}` names `{secret}`, which is not a {whose} secret",
                def.name
            )));
        }
    }
    Ok(())
}

fn def_json(def: &ToolDef) -> ApiResult<String> {
    // The name is the key of the row.
    let mut v = serde_json::to_value(def)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("name");
    }
    Ok(serde_json::to_string(&v)?)
}

pub fn list(conn: &Connection) -> ApiResult<Vec<ToolRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM tools ORDER BY name"))?;
    let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn find(conn: &Connection, name: &str) -> ApiResult<Option<ToolRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM tools WHERE name = ?1"),
            [name],
            row,
        )
        .optional()?)
}

pub fn get(conn: &Connection, name: &str) -> ApiResult<ToolRow> {
    find(conn, name)?.ok_or_else(|| ApiError::not_found(format!("no tool `{name}`")))
}

fn insert(t: &mut Tx<'_>, def: &ToolDef, source: &str) -> ApiResult<()> {
    let at = now();
    t.execute(
        "INSERT INTO tools (name, project, def, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![def.name, def.project, def_json(def)?, source, at],
    )?;
    Ok(())
}

/// `POST /api/tools`: a new tool from the API.
pub fn create(t: &mut Tx<'_>, def: &ToolDef, extra_dirs: &[&Path]) -> ApiResult<ToolRow> {
    validate(t, def, extra_dirs)?;
    if find(t, &def.name)?.is_some() {
        return Err(ApiError::conflict(format!("tool `{}` exists", def.name)));
    }
    insert(t, def, SOURCE_API)?;
    get(t, &def.name)
}

fn refuse_file(r: &ToolRow) -> ApiResult<()> {
    if r.source == SOURCE_FILE {
        return Err(ApiError::new(
            Code::FileManaged,
            format!(
                "tool `{}` is declared in tools/{}.toml; edit the file, then POST /api/reload",
                r.def.name, r.def.name
            ),
        ));
    }
    Ok(())
}

/// `PATCH /api/tools/{name}`: a JSON merge patch of the definition. A
/// `null` removes a field. The name and the project do not change: a
/// change of the project would move the grants out of their scope.
pub fn update(
    t: &mut Tx<'_>,
    name: &str,
    patch: &Value,
    extra_dirs: &[&Path],
) -> ApiResult<ToolRow> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    let Some(p) = patch.as_object() else {
        return Err(ApiError::bad_request("the body must be a JSON object"));
    };
    let mut v = serde_json::to_value(&current.def)?;
    let Some(o) = v.as_object_mut() else {
        return Err(ApiError::internal("a tool is not an object"));
    };
    for (k, val) in p {
        if k == "name" && val.as_str() != Some(name) {
            return Err(ApiError::invalid("a tool cannot be renamed"));
        }
        if k == "project" && val.as_str() != current.def.project.as_deref() {
            return Err(ApiError::invalid(
                "the project of a tool does not change: delete it and define it again",
            ));
        }
        if val.is_null() {
            o.remove(k);
        } else {
            o.insert(k.clone(), val.clone());
        }
    }
    let mut def: ToolDef = serde_json::from_value(v)
        .map_err(|e| ApiError::bad_request(format!("bad tool field: {e}")))?;
    def.name = name.to_owned();
    validate(t, &def, extra_dirs)?;
    t.execute(
        "UPDATE tools SET def = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, def_json(&def)?, now()],
    )?;
    get(t, name)
}

/// `DELETE /api/tools/{name}`: the tool and its grants.
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<ToolRow> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    t.execute("DELETE FROM tool_grants WHERE tool = ?1", [name])?;
    t.execute("DELETE FROM tools WHERE name = ?1", [name])?;
    Ok(current)
}

/// The `tools/<name>.toml` files: add, change, and remove the `file` rows.
/// A file that fails a check is left out, with an error line; its row (if
/// any) goes, as the file is not a valid tool now. A tool of the API with
/// the name of a file becomes the file's.
pub fn sync_files(
    t: &mut Tx<'_>,
    defs: &[ToolDef],
    extra_dirs: &[&Path],
) -> ApiResult<(Changes, Vec<String>)> {
    let mut changes = Changes::default();
    let mut errors = Vec::new();
    let mut kept = Vec::new();
    for def in defs {
        if let Err(e) = validate(t, def, extra_dirs) {
            errors.push(format!("tools/{}.toml: {}", def.name, e.message));
            continue;
        }
        kept.push(def.name.clone());
        match find(t, &def.name)? {
            None => {
                insert(t, def, SOURCE_FILE)?;
                changes.added.push(def.name.clone());
            }
            Some(r) if r.def != *def || r.source != SOURCE_FILE => {
                // A move to another project drops the grants of the old scope.
                if r.def.project != def.project {
                    t.execute("DELETE FROM tool_grants WHERE tool = ?1", [&def.name])?;
                }
                t.execute(
                    "UPDATE tools SET project = ?2, def = ?3, source = 'file', updated_at = ?4
                     WHERE name = ?1",
                    params![def.name, def.project, def_json(def)?, now()],
                )?;
                changes.updated.push(def.name.clone());
            }
            Some(_) => {}
        }
    }
    let gone: Vec<String> = list(t)?
        .into_iter()
        .filter(|r| r.source == SOURCE_FILE && !kept.contains(&r.def.name))
        .map(|r| r.def.name)
        .collect();
    for name in gone {
        t.execute("DELETE FROM tool_grants WHERE tool = ?1", [&name])?;
        t.execute("DELETE FROM tools WHERE name = ?1", [&name])?;
        changes.removed.push(name);
    }
    Ok((changes, errors))
}

// ---------------------------------------------------------------- grants

type GrantCols = (String, String, String, String);

fn grant_row(r: &Row<'_>) -> rusqlite::Result<GrantCols> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

fn to_grant(
    conn: &Connection,
    (tool, grantee, created_by, created_at): GrantCols,
) -> ApiResult<ToolGrant> {
    Ok(ToolGrant {
        grantee_label: crate::secrets::grants::label_of(conn, &grantee)?,
        tool,
        grantee,
        created_by,
        created_at,
    })
}

/// The grants of `tool`.
pub fn grants_of(conn: &Connection, tool: &str) -> ApiResult<Vec<ToolGrant>> {
    get(conn, tool)?;
    let mut stmt = conn.prepare(
        "SELECT tool, grantee, created_by, created_at FROM tool_grants WHERE tool = ?1
         ORDER BY grantee",
    )?;
    let rows = stmt
        .query_map([tool], grant_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(|r| to_grant(conn, r)).collect()
}

/// Give `tool` to `grantee` (`agent:<project>/<name>`, `agent:<ulid>`, or
/// `project:<name>`). A project tool goes only to its own project. A grant
/// that exists is `409`.
pub fn grant(
    t: &mut Tx<'_>,
    tool: &str,
    grantee: &str,
    by: &str,
) -> ApiResult<(ToolGrant, Grantee)> {
    let r = get(t, tool)?;
    let g = parse_grantee(t, grantee)?;
    if let Some(p) = &r.def.project
        && g.project() != p
    {
        return Err(ApiError::invalid(format!(
            "tool `{tool}` belongs to project `{p}`: it goes only to that project, not to {}",
            g.label()
        )));
    }
    let key = g.key();
    let n = t.execute(
        "INSERT OR IGNORE INTO tool_grants (tool, grantee, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![tool, key, by, now()],
    )?;
    if n == 0 {
        return Err(ApiError::conflict(format!(
            "{} has tool `{tool}` already",
            g.label()
        )));
    }
    let row = t.query_row(
        "SELECT tool, grantee, created_by, created_at FROM tool_grants WHERE tool = ?1 AND grantee = ?2",
        params![tool, key],
        grant_row,
    )?;
    Ok((to_grant(t, row)?, g))
}

/// Take `tool` from `grantee`. A grant that does not exist is `404`.
pub fn revoke(t: &mut Tx<'_>, tool: &str, grantee: &str) -> ApiResult<Grantee> {
    get(t, tool)?;
    let g = parse_grantee(t, grantee)?;
    let n = t.execute(
        "DELETE FROM tool_grants WHERE tool = ?1 AND grantee = ?2",
        params![tool, g.key()],
    )?;
    if n == 0 {
        return Err(ApiError::not_found(format!(
            "{} has no grant of tool `{tool}`",
            g.label()
        )));
    }
    Ok(g)
}

/// The tools that agent `id` of `project` may run: its own grants and its
/// project's, one row each, in name order.
pub fn granted_to(conn: &Connection, id: &str, project: &str) -> ApiResult<Vec<ToolRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM tools WHERE name IN
           (SELECT tool FROM tool_grants WHERE grantee = ?1 OR grantee = ?2)
         ORDER BY name"
    ))?;
    let rows = stmt
        .query_map(
            params![format!("agent:{id}"), format!("project:{project}")],
            row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    // A project tool only ever reaches its own project; the rows say so too.
    Ok(rows
        .into_iter()
        .filter(|r| r.def.project.as_deref().is_none_or(|p| p == project))
        .collect())
}

/// The agent was removed: the grants to it go.
pub fn on_agent_deleted(conn: &Connection, id: &str) -> ApiResult<usize> {
    Ok(conn.execute(
        "DELETE FROM tool_grants WHERE grantee = ?1",
        [format!("agent:{id}")],
    )?)
}

/// The project was removed: the grants to it go, and its tools.
pub fn on_project_deleted(conn: &Connection, name: &str) -> ApiResult<usize> {
    let n = conn.execute(
        "DELETE FROM tool_grants WHERE grantee = ?1 OR tool IN (SELECT name FROM tools WHERE project = ?2)",
        params![format!("project:{name}"), name],
    )?;
    conn.execute("DELETE FROM tools WHERE project = ?1", [name])?;
    Ok(n)
}
