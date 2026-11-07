//! Board rules (TER-162, TER-166, docs/api.md §19.8): one rules file
//! (`rules.wcl`) for each project, checked against the schema that the
//! daemon ships (`terrarium_core::rules_file`).
//!
//! The text is the source of truth; the row keeps it with its hash. A
//! rules file comes from the API (`source = api`) or from the config dir
//! (`projects/<project>.rules.wcl`, `source = file`, [`sync_file`] at the
//! start and at `POST /api/reload`). The API does not change a file's
//! rules. Each change writes an audit row (`rules.put`, `rules.delete`,
//! `rules.file`) with the old and the new hash and the rule names.
//!
//! Nothing runs a rule yet: the engine comes with TER-168.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use terrarium_core::rules_file::{self, BoardColumn, Context, Rule, RuleSet};
use terrarium_core::template_file::FileError;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::store::projects::{SOURCE_API, SOURCE_FILE};
use crate::store::{board, projects};

/// `TERRARIUM_MAX_RULES_PER_PROJECT`: the most rules in one project's
/// file. Its own cap, apart from the schedules cap.
pub const ENV_MAX_RULES: &str = "TERRARIUM_MAX_RULES_PER_PROJECT";

/// The cap: [`ENV_MAX_RULES`], else
/// [`rules_file::DEFAULT_MAX_RULES`]. Read once.
pub fn max_per_project() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var(ENV_MAX_RULES)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(rules_file::DEFAULT_MAX_RULES)
    })
}

/// The path of a project's rules file in the config dir.
pub fn file_path(project: &str) -> String {
    format!("projects/{project}.rules.wcl")
}

/// The rules of a project.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rules {
    pub project: String,
    /// False: the project has no rules file yet (the other fields are
    /// empty).
    pub exists: bool,
    pub text: String,
    /// The SHA-256 of the text, hex. A write may name it as `base_hash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// `api` or `file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The rules as read, in the order of the file.
    pub rules: Vec<Rule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Rules {
    fn none(project: &str) -> Self {
        Self {
            project: project.to_owned(),
            exists: false,
            text: String::new(),
            hash: None,
            source: None,
            rules: Vec::new(),
            updated_by: None,
            updated_at: None,
        }
    }
}

/// What the check of a file needs: the columns of the board and the cap.
pub fn context(conn: &Connection, project: &str) -> ApiResult<Context> {
    let columns = board::column_roles(conn, project)?
        .into_iter()
        .map(|(name, role)| BoardColumn { name, role })
        .collect();
    Ok(Context {
        columns: Some(columns),
        max_rules: max_per_project(),
    })
}

/// A `422 invalid` with each problem of the file in `details.problems`
/// (`line`, `column`, `message`).
fn file_error(e: &FileError) -> ApiError {
    let problems: Vec<_> = e
        .problems
        .iter()
        .map(|p| json!({"line": p.line, "column": p.column, "message": p.message}))
        .collect();
    ApiError::invalid(e.to_string()).with_details(json!({"problems": problems}))
}

/// Check `text` as the rules file of `project` (which must exist).
pub fn check(conn: &Connection, project: &str, text: &str) -> ApiResult<RuleSet> {
    projects::get(conn, project)?;
    let ctx = context(conn, project)?;
    rules_file::parse(text, rules_file::FILE_NAME, &ctx).map_err(|e| file_error(&e))
}

struct Row {
    text: String,
    hash: String,
    source: String,
    updated_by: String,
    updated_at: String,
}

fn row(conn: &Connection, project: &str) -> ApiResult<Option<Row>> {
    Ok(conn
        .query_row(
            "SELECT text, hash, source, updated_by, updated_at FROM project_rules
              WHERE project = ?1",
            [project],
            |r| {
                Ok(Row {
                    text: r.get(0)?,
                    hash: r.get(1)?,
                    source: r.get(2)?,
                    updated_by: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// The rules of `project` (which must exist). The rules are read again
/// without the column check: a board change after the write does not
/// hide them.
pub fn get(conn: &Connection, project: &str) -> ApiResult<Rules> {
    projects::get(conn, project)?;
    let Some(r) = row(conn, project)? else {
        return Ok(Rules::none(project));
    };
    let ctx = Context {
        columns: None,
        max_rules: usize::MAX,
    };
    let rules = rules_file::parse(&r.text, rules_file::FILE_NAME, &ctx)
        .map(|s| s.rules)
        .unwrap_or_default();
    Ok(Rules {
        project: project.to_owned(),
        exists: true,
        text: r.text,
        hash: Some(r.hash),
        source: Some(r.source),
        rules,
        updated_by: Some(r.updated_by),
        updated_at: Some(r.updated_at),
    })
}

fn file_managed(project: &str) -> ApiError {
    ApiError::new(
        Code::FileManaged,
        format!(
            "the rules of project `{project}` are declared in {}; edit the file, then POST /api/reload",
            file_path(project)
        ),
    )
}

fn stale(project: &str, base: &str, have: Option<&str>) -> ApiError {
    ApiError::conflict(format!(
        "the rules of project `{project}` changed: base_hash `{base}` is not the current hash `{}`; read them again",
        have.unwrap_or("none")
    ))
    .with_details(json!({"hash": have}))
}

fn names(set: &RuleSet) -> Vec<&str> {
    set.rules.iter().map(|r| r.name.as_str()).collect()
}

/// Write the rules of `project` from the API: check the text, then store
/// it with an audit row `rules.put`. `base_hash`: the hash the caller
/// read (`""`: no rules yet); another current hash is `409 conflict`. The
/// rules of a file are `409 file_managed`.
pub fn put(t: &mut Tx<'_>, project: &str, text: &str, base_hash: Option<&str>) -> ApiResult<Rules> {
    let set = check(t, project, text)?;
    let old = row(t, project)?;
    if old.as_ref().is_some_and(|r| r.source == SOURCE_FILE) {
        return Err(file_managed(project));
    }
    let have = old.as_ref().map(|r| r.hash.as_str());
    if let Some(base) = base_hash
        && base != have.unwrap_or("")
    {
        return Err(stale(project, base, have));
    }
    let hash = crate::defaults::sha256_hex(text.as_bytes());
    store(t, project, text, &hash, set.rules.len(), SOURCE_API)?;
    t.audit(
        "rules.put",
        project,
        Some(project),
        json!({
            "hash": hash,
            "previous_hash": have,
            "rules": names(&set),
        }),
    )?;
    get(t, project)
}

fn store(
    t: &Tx<'_>,
    project: &str,
    text: &str,
    hash: &str,
    count: usize,
    source: &str,
) -> ApiResult<()> {
    t.execute(
        "INSERT INTO project_rules (project, text, hash, rule_count, source, updated_by, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(project) DO UPDATE SET text = excluded.text, hash = excluded.hash,
             rule_count = excluded.rule_count, source = excluded.source,
             updated_by = excluded.updated_by, updated_at = excluded.updated_at",
        params![
            project,
            text,
            hash,
            count as i64,
            source,
            t.actor,
            now()
        ],
    )?;
    Ok(())
}

/// Delete the rules of `project` from the API, with an audit row
/// `rules.delete`. `false`: there were none.
pub fn delete(t: &mut Tx<'_>, project: &str, base_hash: Option<&str>) -> ApiResult<bool> {
    projects::get(t, project)?;
    let Some(old) = row(t, project)? else {
        return Ok(false);
    };
    if old.source == SOURCE_FILE {
        return Err(file_managed(project));
    }
    if let Some(base) = base_hash
        && base != old.hash
    {
        return Err(stale(project, base, Some(&old.hash)));
    }
    t.execute("DELETE FROM project_rules WHERE project = ?1", [project])?;
    t.audit(
        "rules.delete",
        project,
        Some(project),
        json!({"previous_hash": old.hash}),
    )?;
    Ok(true)
}

/// Load the rules file of `project` from the config dir (`text`; `None`:
/// no file). A changed file that checks becomes the rules (`source =
/// file`), with an audit row `rules.file`; a file that does not check
/// keeps the last good rules, and its errors come back (one line each).
/// No file: rules of a file that is gone become API rules, as for the
/// schedules of a project file.
pub fn sync_file(t: &mut Tx<'_>, project: &str, text: Option<&str>) -> ApiResult<Vec<String>> {
    let old = row(t, project)?;
    let Some(text) = text else {
        if old.as_ref().is_some_and(|r| r.source == SOURCE_FILE) {
            t.execute(
                "UPDATE project_rules SET source = ?2 WHERE project = ?1",
                params![project, SOURCE_API],
            )?;
        }
        return Ok(Vec::new());
    };
    let hash = crate::defaults::sha256_hex(text.as_bytes());
    if old
        .as_ref()
        .is_some_and(|r| r.hash == hash && r.source == SOURCE_FILE)
    {
        return Ok(Vec::new());
    }
    let ctx = context(t, project)?;
    let set = match rules_file::parse(text, &file_path(project), &ctx) {
        Ok(s) => s,
        Err(e) => return Ok(e.to_string().lines().map(str::to_owned).collect()),
    };
    store(t, project, text, &hash, set.rules.len(), SOURCE_FILE)?;
    t.audit(
        "rules.file",
        project,
        Some(project),
        json!({
            "file": file_path(project),
            "hash": hash,
            "previous_hash": old.as_ref().map(|r| r.hash.clone()),
            "rules": names(&set),
        }),
    )?;
    Ok(Vec::new())
}

/// The projects whose rules come from a file.
pub fn file_projects(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT project FROM project_rules WHERE source = ?1 ORDER BY project")?;
    let rows = stmt
        .query_map([SOURCE_FILE], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(rows)
}

/// The API rules of every project, by project name: the export of the
/// config repo. The rules of a file are in the config dir already.
pub fn of_api(conn: &Connection) -> ApiResult<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT project, text FROM project_rules WHERE source = ?1 ORDER BY project")?;
    let rows = stmt
        .query_map([SOURCE_API], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
