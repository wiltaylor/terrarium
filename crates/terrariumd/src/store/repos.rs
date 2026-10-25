//! The repositories of a project (PLAN §9). A repo row is data: the URL,
//! the default branch, the checkout directory, and the *name* of a
//! credential in `<state>/credentials`. The secret never enters the
//! database. `crate::git` does the clones; `crate::credentials` holds the
//! secrets.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::{Page, Paged, check_name, projects};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub project: String,
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// A key name in `/state/credentials`, never the secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The checkout directory name.
    pub path: String,
    pub source: String,
    pub created_at: String,
}

fn repo_row(r: &Row<'_>) -> rusqlite::Result<Repo> {
    let name: String = r.get("name")?;
    Ok(Repo {
        id: r.get("id")?,
        project: r.get("project")?,
        path: r
            .get::<_, Option<String>>("path")?
            .unwrap_or_else(|| name.clone()),
        name,
        url: r.get("url")?,
        default_branch: r.get("default_branch")?,
        credential: r.get("credential")?,
        source: r.get("source")?,
        created_at: r.get("created_at")?,
    })
}

pub fn list_repos(conn: &Connection, project: &str, page: &Page) -> ApiResult<Paged<Repo>> {
    let limit = page.limit();
    let mut stmt = conn
        .prepare("SELECT * FROM repos WHERE project = ?1 AND name > ?2 ORDER BY name LIMIT ?3")?;
    let rows = stmt
        .query_map(
            params![project, page.cursor().unwrap_or(""), (limit + 1) as i64],
            repo_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |r| r.name.clone()))
}

/// The body of `POST /api/projects/{name}/repos`. Without a name, the last
/// part of the URL names the repo.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRepo {
    #[serde(default)]
    pub name: Option<String>,
    pub url: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

fn name_from_url(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(url);
    last.trim_end_matches(".git").to_ascii_lowercase()
}

/// Attach a repo. A clone happens on `POST /api/repos/{id}/checkout` and
/// on agent create (`checkout.on_agent_create`).
pub fn attach_repo(t: &mut Tx<'_>, project: &str, r: &NewRepo, source: &str) -> ApiResult<Repo> {
    projects::get(t, project)?;
    if r.url.trim().is_empty() {
        return Err(ApiError::invalid("the repo needs a url"));
    }
    if crate::git::is_local_url(&r.url)
        && let Some(id) = t.actor.strip_prefix("agent:")
    {
        let roles = crate::store::agents::get(t, id)?.agent.roles;
        crate::policy::attach_local_repo(&roles)?;
    }
    let name = r.name.clone().unwrap_or_else(|| name_from_url(&r.url));
    check_name("repo", &name)?;
    if let Some(path) = &r.path
        && (path.is_empty() || path.contains('/') || path.starts_with('.'))
    {
        return Err(ApiError::invalid(format!("bad checkout path `{path}`")));
    }
    let exists: i64 = t.query_row(
        "SELECT count(*) FROM repos WHERE project = ?1 AND name = ?2",
        params![project, name],
        |r| r.get(0),
    )?;
    if exists > 0 {
        return Err(ApiError::conflict(format!(
            "project `{project}` has a repo `{name}` already"
        )));
    }
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO repos (id, project, name, url, default_branch, credential, path, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            project,
            name,
            r.url,
            r.default_branch,
            r.credential,
            r.path,
            source,
            now()
        ],
    )?;
    t.audit(
        "repo.attach",
        &format!("{project}/{name}"),
        Some(project),
        json!({"url": r.url, "source": source}),
    )?;
    Ok(t.query_row("SELECT * FROM repos WHERE id = ?1", [&id], repo_row)?)
}

/// One repo by ULID.
pub fn find(conn: &Connection, id: &str) -> ApiResult<Option<Repo>> {
    Ok(conn
        .query_row("SELECT * FROM repos WHERE id = ?1", [id], repo_row)
        .optional()?)
}

/// One repo by name in a project.
pub fn find_by_name(conn: &Connection, project: &str, name: &str) -> ApiResult<Option<Repo>> {
    Ok(conn
        .query_row(
            "SELECT * FROM repos WHERE project = ?1 AND name = ?2",
            params![project, name],
            repo_row,
        )
        .optional()?)
}

/// A repo by key: a ULID, `project/name`, or a name in `context` (the
/// project of the calling agent).
pub fn resolve(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<Repo> {
    let found = match find(conn, key)? {
        Some(r) => Some(r),
        None => match key.split_once('/') {
            Some((project, name)) => find_by_name(conn, project, name)?,
            None => match context {
                Some(project) => find_by_name(conn, project, key)?,
                None => None,
            },
        },
    };
    found.ok_or_else(|| ApiError::not_found(format!("no repo `{key}`")))
}

/// Every repo of a project, by name.
pub fn all_for_project(conn: &Connection, project: &str) -> ApiResult<Vec<Repo>> {
    let mut stmt = conn.prepare("SELECT * FROM repos WHERE project = ?1 ORDER BY name")?;
    let rows = stmt
        .query_map([project], repo_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_from_urls() {
        assert_eq!(
            name_from_url("git@github.com:wiltaylor/terrarium.git"),
            "terrarium"
        );
        assert_eq!(
            name_from_url("https://github.com/wiltaylor/CopilotCustomSystemPrompt.git"),
            "copilotcustomsystemprompt"
        );
        assert_eq!(name_from_url("https://x/y/"), "y");
    }
}
