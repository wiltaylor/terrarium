//! Projects and their repositories.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::{Page, Paged, board, check_name};

/// Where a definition lives.
pub const SOURCE_FILE: &str = "file";
pub const SOURCE_API: &str = "api";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub description: String,
    /// The card ID prefix: `TER` makes `TER-42`.
    pub card_prefix: String,
    /// `file` (declared in `projects/<name>.toml`) or `api`.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

fn project_row(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        name: r.get("name")?,
        description: r
            .get::<_, Option<String>>("description")?
            .unwrap_or_default(),
        card_prefix: r.get("prefix")?,
        source: r.get("source")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub fn find(conn: &Connection, name: &str) -> ApiResult<Option<Project>> {
    Ok(conn
        .query_row(
            "SELECT * FROM projects WHERE name = ?1",
            [name],
            project_row,
        )
        .optional()?)
}

pub fn get(conn: &Connection, name: &str) -> ApiResult<Project> {
    find(conn, name)?.ok_or_else(|| ApiError::not_found(format!("no project `{name}`")))
}

/// Projects by name. `only` limits the list to one project (an agent's).
pub fn list(conn: &Connection, page: &Page, only: Option<&str>) -> ApiResult<Paged<Project>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(
        "SELECT * FROM projects WHERE name > ?1 AND (?2 IS NULL OR name = ?2)
         ORDER BY name LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![page.cursor().unwrap_or(""), only, (limit + 1) as i64],
            project_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |p| p.name.clone()))
}

fn valid_prefix(p: &str) -> bool {
    let mut chars = p.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && p.len() <= 10
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn prefix_taken(conn: &Connection, prefix: &str, except: Option<&str>) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM projects WHERE prefix = ?1 AND (?2 IS NULL OR name <> ?2)",
        params![prefix, except],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// A free prefix made from the name: the first three letters and digits in
/// upper case, then longer, then with a number.
pub fn derive_prefix(conn: &Connection, name: &str) -> ApiResult<String> {
    let mut letters: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if !letters.starts_with(|c: char| c.is_ascii_uppercase()) {
        letters.insert(0, 'P');
    }
    letters.truncate(10);
    for len in 3..=letters.len().max(3) {
        let candidate: String = letters.chars().take(len).collect();
        if !prefix_taken(conn, &candidate, None)? {
            return Ok(candidate);
        }
    }
    let base: String = letters.chars().take(3).collect();
    for n in 2..10_000 {
        let candidate = format!("{base}{n}");
        if !prefix_taken(conn, &candidate, None)? {
            return Ok(candidate);
        }
    }
    Err(ApiError::internal("no free card prefix"))
}

fn check_prefix(conn: &Connection, prefix: &str, project: Option<&str>) -> ApiResult<()> {
    if !valid_prefix(prefix) {
        return Err(ApiError::invalid(format!(
            "bad card prefix `{prefix}`: use 1-10 of A-Z and 0-9, starting with a letter"
        )));
    }
    if prefix_taken(conn, prefix, project)? {
        return Err(ApiError::conflict(format!(
            "card prefix `{prefix}` is in use by another project"
        )));
    }
    Ok(())
}

/// The body of `POST /api/projects`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewProject {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "prefix")]
    pub card_prefix: Option<String>,
}

/// Make a project, its board, and the default columns.
pub fn create(t: &mut Tx<'_>, p: &NewProject, source: &str) -> ApiResult<Project> {
    check_name("project", &p.name)?;
    if find(t, &p.name)?.is_some() {
        return Err(ApiError::conflict(format!("project `{}` exists", p.name)));
    }
    let prefix = match &p.card_prefix {
        Some(prefix) => {
            check_prefix(t, prefix, None)?;
            prefix.clone()
        }
        None => derive_prefix(t, &p.name)?,
    };
    let at = now();
    t.execute(
        "INSERT INTO projects (name, description, prefix, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![p.name, p.description, prefix, source, at],
    )?;
    board::create_board(t, &p.name)?;
    t.audit(
        "project.create",
        &p.name,
        Some(&p.name),
        json!({"card_prefix": prefix, "source": source}),
    )?;
    get(t, &p.name)
}

/// The body of `PATCH /api/projects/{name}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPatch {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "prefix")]
    pub card_prefix: Option<String>,
}

/// Change a project. The card prefix may change only while the project has
/// no cards, so card IDs stay stable.
pub fn update(t: &mut Tx<'_>, name: &str, patch: &ProjectPatch) -> ApiResult<Project> {
    let before = get(t, name)?;
    if let Some(prefix) = patch
        .card_prefix
        .as_deref()
        .filter(|p| *p != before.card_prefix)
    {
        check_prefix(t, prefix, Some(name))?;
        let cards: i64 = t.query_row(
            "SELECT count(*) FROM cards WHERE project = ?1",
            [name],
            |r| r.get(0),
        )?;
        if cards > 0 {
            return Err(ApiError::conflict(
                "the card prefix cannot change after the first card",
            ));
        }
        t.execute(
            "UPDATE projects SET prefix = ?2 WHERE name = ?1",
            params![name, prefix],
        )?;
    }
    if let Some(d) = &patch.description {
        t.execute(
            "UPDATE projects SET description = ?2 WHERE name = ?1",
            params![name, d],
        )?;
    }
    t.execute(
        "UPDATE projects SET updated_at = ?2 WHERE name = ?1",
        params![name, now()],
    )?;
    t.audit(
        "project.update",
        name,
        Some(name),
        json!({"description": patch.description, "card_prefix": patch.card_prefix}),
    )?;
    get(t, name)
}

/// Delete a project with its board and repos. The caller checks that no
/// agent is left in it.
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<()> {
    get(t, name)?;
    let agents: i64 = t.query_row(
        "SELECT count(*) FROM agents WHERE project = ?1",
        [name],
        |r| r.get(0),
    )?;
    if agents > 0 {
        return Err(ApiError::conflict(format!(
            "project `{name}` has {agents} agent(s); delete them first"
        )));
    }
    // Cards point at columns without a cascade; drop them first.
    t.execute("DELETE FROM cards WHERE project = ?1", [name])?;
    t.execute("DELETE FROM projects WHERE name = ?1", [name])?;
    t.audit("project.delete", name, Some(name), json!({}))?;
    Ok(())
}

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

/// Attach a repo as data. No clone happens yet.
pub fn attach_repo(t: &mut Tx<'_>, project: &str, r: &NewRepo, source: &str) -> ApiResult<Repo> {
    get(t, project)?;
    if r.url.trim().is_empty() {
        return Err(ApiError::invalid("the repo needs a url"));
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

    #[test]
    fn prefixes() {
        assert!(valid_prefix("TER"));
        assert!(valid_prefix("T2"));
        assert!(!valid_prefix("2T"));
        assert!(!valid_prefix("ter"));
    }
}
