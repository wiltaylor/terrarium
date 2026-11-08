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
//! No parse runs inside a DB closure: the caller reads the [`Context`]
//! (the board columns) under the lock, parses with [`parse_off`] on a
//! blocking thread, then writes with the context it used. The write
//! compares the context and the hash again under the lock.
//!
//! The rule engine (`crate::rules_engine`, TER-168) runs the stored rules.

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
    /// Diagnostics when a stored file no longer passes the current schema.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<serde_json::Value>,
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
            problems: Vec::new(),
            updated_by: None,
            updated_at: None,
        }
    }
}

/// What the check of a file needs: the columns of the board and the cap.
/// The project must exist.
pub fn context(conn: &Connection, project: &str) -> ApiResult<Context> {
    projects::get(conn, project)?;
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
    ApiError::invalid(e.to_string()).with_details(json!({"problems": problems(e)}))
}

pub fn problems(e: &FileError) -> Vec<serde_json::Value> {
    e.problems
        .iter()
        .map(|p| json!({"line": p.line, "column": p.column, "message": p.message}))
        .collect()
}

/// Parse `text` as a rules file named `file` on a blocking thread, outside
/// any DB closure. The inner `Err` is the file's problems.
pub async fn parse_off(
    text: String,
    file: String,
    ctx: Context,
) -> ApiResult<Result<RuleSet, FileError>> {
    tokio::task::spawn_blocking(move || rules_file::parse(&text, &file, &ctx))
        .await
        .map_err(|e| ApiError::internal(format!("rules check: {e}")))
}

/// Check `text` as the API rules file with `ctx`: `422 invalid` with the
/// problems. Off the DB lock.
pub async fn check(text: String, ctx: Context) -> ApiResult<RuleSet> {
    parse_off(text, rules_file::FILE_NAME.to_owned(), ctx)
        .await?
        .map_err(|e| file_error(&e))
}

/// The rules of `r`, read again from its text without the column check
/// (a board change after the write does not hide them). Off the DB lock.
pub async fn with_rules(mut r: Rules) -> ApiResult<Rules> {
    if !r.exists {
        return Ok(r);
    }
    let ctx = Context {
        columns: None,
        max_rules: usize::MAX,
    };
    match parse_off(r.text.clone(), rules_file::FILE_NAME.to_owned(), ctx).await? {
        Ok(s) => r.rules = s.rules,
        Err(e) => r.problems = problems(&e),
    }
    Ok(r)
}

/// The board of `project` changed between the check and the write.
fn board_changed(project: &str) -> ApiError {
    ApiError::conflict(format!(
        "the board of project `{project}` changed during the rules check; send the rules again"
    ))
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

/// The rules row of `project` (which must exist), with `rules` empty:
/// [`with_rules`] reads them off the DB lock.
pub fn get(conn: &Connection, project: &str) -> ApiResult<Rules> {
    projects::get(conn, project)?;
    let Some(r) = row(conn, project)? else {
        return Ok(Rules::none(project));
    };
    Ok(Rules {
        project: project.to_owned(),
        exists: true,
        text: r.text,
        hash: Some(r.hash),
        source: Some(r.source),
        rules: Vec::new(),
        problems: Vec::new(),
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

/// Write the rules of `project` from the API, with an audit row
/// `rules.put`. `set` is the text checked with `ctx` ([`check`], off the
/// lock); a board that no longer matches `ctx` is `409 conflict`.
/// `base_hash`: the hash the caller read (`""`: no rules yet); another
/// current hash is `409 conflict`. The rules of a file are `409
/// file_managed`.
pub fn put(
    t: &mut Tx<'_>,
    project: &str,
    text: &str,
    set: RuleSet,
    ctx: &Context,
    base_hash: Option<&str>,
) -> ApiResult<Rules> {
    if context(t, project)? != *ctx {
        return Err(board_changed(project));
    }
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
    let mut r = get(t, project)?;
    r.rules = set.rules;
    Ok(r)
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

/// A rules file of the config dir that changed: [`plan_file`] reads it
/// under the lock, [`parse_off`] checks it off the lock, [`store_file`]
/// writes it.
#[derive(Debug, Clone)]
pub struct FilePlan {
    pub project: String,
    pub text: String,
    pub hash: String,
    /// The context of the check.
    pub ctx: Context,
}

/// The plan for the rules file `text` of `project` (which must exist).
/// `None`: the rules already come from this file, unchanged.
pub fn plan_file(conn: &Connection, project: &str, text: &str) -> ApiResult<Option<FilePlan>> {
    let hash = crate::defaults::sha256_hex(text.as_bytes());
    if row(conn, project)?
        .as_ref()
        .is_some_and(|r| r.hash == hash && r.source == SOURCE_FILE)
    {
        return Ok(None);
    }
    Ok(Some(FilePlan {
        project: project.to_owned(),
        text: text.to_owned(),
        hash,
        ctx: context(conn, project)?,
    }))
}

/// Store the checked rules file of `plan` (`source = file`) with an audit
/// row `rules.file`. The hash and the board are compared again under the
/// lock: the same file already stored writes nothing, and a board that
/// changed since the check is an error of the reload (one line).
pub fn store_file(t: &mut Tx<'_>, plan: &FilePlan, set: &RuleSet) -> ApiResult<Vec<String>> {
    let project = plan.project.as_str();
    let old = row(t, project)?;
    if old
        .as_ref()
        .is_some_and(|r| r.hash == plan.hash && r.source == SOURCE_FILE)
    {
        return Ok(Vec::new());
    }
    if context(t, project)? != plan.ctx {
        return Ok(vec![format!(
            "{}: the board changed during the check; POST /api/reload again",
            file_path(project)
        )]);
    }
    store(
        t,
        project,
        &plan.text,
        &plan.hash,
        set.rules.len(),
        SOURCE_FILE,
    )?;
    t.audit(
        "rules.file",
        project,
        Some(project),
        json!({
            "file": file_path(project),
            "source": SOURCE_FILE,
            "hash": plan.hash,
            "previous_hash": old.as_ref().map(|r| r.hash.clone()),
            "rules": names(set),
        }),
    )?;
    Ok(Vec::new())
}

/// The rules file of `project` is gone from the config dir: its rules stay
/// as API rules (as for the schedules of a project file), with an audit
/// row `rules.file` with `file: null` and `source: api`.
pub fn file_gone(t: &mut Tx<'_>, project: &str) -> ApiResult<()> {
    let Some(old) = row(t, project)? else {
        return Ok(());
    };
    if old.source != SOURCE_FILE {
        return Ok(());
    }
    t.execute(
        "UPDATE project_rules SET source = ?2, updated_by = ?3, updated_at = ?4
          WHERE project = ?1",
        params![project, SOURCE_API, t.actor, now()],
    )?;
    t.audit(
        "rules.file",
        project,
        Some(project),
        json!({
            "file": null,
            "previous_file": file_path(project),
            "source": SOURCE_API,
            "hash": old.hash,
            "previous_hash": old.hash,
        }),
    )?;
    Ok(())
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
