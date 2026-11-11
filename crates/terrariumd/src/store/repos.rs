//! The repositories of a project (PLAN §9). A repo row is data: the URL,
//! the default branch, the checkout directory, and the *name* of a secret
//! in `credential` (the project's secret of that name, else the global
//! one). `crate::git` does the clones; `crate::secrets` holds the secrets,
//! sealed (PLAN §16b). A global secret needs a grant to the project,
//! unless the operator, a sysadmin, or a project file attached the repo
//! (`credential_admin`, PLAN §16b.6a).

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::config::PushVia;

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
    /// The name of a secret (PLAN §16b), never the secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The operator, a sysadmin, or a project file attached the repo, so
    /// `credential` can name a global secret with no grant (PLAN
    /// §16b.6a). False: a project_admin attached it, and a global secret
    /// needs a grant to the project.
    #[serde(default)]
    pub credential_admin: bool,
    /// The checkout directory name.
    pub path: String,
    /// A tracked dir of the repo whose files the daemon copies into
    /// `.git/hooks` at each clone and update (`crate::git::Hooks`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_dir: Option<String>,
    /// Branch globs that the daemon's `pre-push` hook refuses
    /// (`crate::git::Hooks`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_deny: Vec<String>,
    /// `proxy`, `direct`, or absent (the default rule,
    /// `crate::git_proxy::push_via`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_via: Option<PushVia>,
    /// The path that the checkouts take now, for the API answer:
    /// `proxy`, `direct`, or `direct (<why push_deny is only the hook>)`
    /// (`crate::git_proxy::annotate`). Not stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Warnings for the operator, for the API answer
    /// (`crate::git_proxy::annotate`). Not stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub source: String,
    pub created_at: String,
}

impl Repo {
    /// The owner of `credential`, for `crate::secrets::load`.
    pub fn owner(&self) -> crate::secrets::Owner<'_> {
        crate::secrets::Owner::Repo {
            project: &self.project,
            name: &self.name,
            admin_set: self.credential_admin,
        }
    }

    /// The hooks that a checkout of the repo gets.
    pub fn hooks(&self) -> crate::git::Hooks {
        crate::git::Hooks {
            dir: self.hooks_dir.clone(),
            push_deny: self.push_deny.clone(),
            provenance: None,
        }
    }
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
        credential_admin: r.get("credential_admin")?,
        hooks_dir: r.get("hooks_dir")?,
        push_deny: serde_json::from_str(&r.get::<_, String>("push_deny")?).unwrap_or_default(),
        push_via: r
            .get::<_, Option<String>>("push_via")?
            .and_then(|v| PushVia::parse(&v).ok()),
        via: None,
        warnings: Vec::new(),
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
/// part of the URL names the repo. `branch` is an alias of
/// `default_branch`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRepo {
    #[serde(default)]
    pub name: Option<String>,
    pub url: String,
    #[serde(default, alias = "branch")]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    /// See [`Repo::hooks_dir`].
    #[serde(default)]
    pub hooks_dir: Option<String>,
    /// See [`Repo::push_deny`].
    #[serde(default)]
    pub push_deny: Vec<String>,
    /// See [`Repo::push_via`].
    #[serde(default)]
    pub push_via: Option<PushVia>,
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
/// on agent create (`checkout.on_agent_create`). A `credential` that
/// names a global secret is `403` when the project has no grant of it and
/// the actor is not the operator or a sysadmin (PLAN §16b.6a). A project
/// file (`source` `file`) counts as the operator.
pub fn attach_repo(t: &mut Tx<'_>, project: &str, r: &NewRepo, source: &str) -> ApiResult<Repo> {
    projects::get(t, project)?;
    // A URL with spaces at the ends would not match git's view of it
    // (the credential helper, the proxy rule; review P8).
    let trimmed = NewRepo {
        url: r.url.trim().to_owned(),
        ..r.clone()
    };
    let r = &trimmed;
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
    let hooks_dir = r
        .hooks_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_owned);
    if let Some(d) = &hooks_dir {
        terrarium_core::config::check_hooks_dir(d).map_err(ApiError::invalid)?;
    }
    terrarium_core::config::check_push_deny(&r.push_deny).map_err(ApiError::invalid)?;
    if r.push_via == Some(PushVia::Proxy) {
        crate::git_proxy::check_proxy_url(&r.url).map_err(ApiError::invalid)?;
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
    let admin = source == projects::SOURCE_FILE || crate::secrets::is_admin_actor(t, &t.actor)?;
    if let Some(c) = &r.credential {
        crate::secrets::check_global_use(t, project, c, admin)?;
    }
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO repos (id, project, name, url, default_branch, credential, path, source,
                            created_at, credential_admin, hooks_dir, push_deny, push_via)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            id,
            project,
            name,
            r.url,
            r.default_branch,
            r.credential,
            r.path,
            source,
            now(),
            admin,
            hooks_dir,
            serde_json::to_string(&r.push_deny)?,
            r.push_via.map(PushVia::as_str)
        ],
    )?;
    t.audit(
        "repo.attach",
        &format!("{project}/{name}"),
        Some(project),
        json!({"url": r.url, "source": source, "hooks_dir": hooks_dir,
               "push_deny": r.push_deny, "push_via": r.push_via}),
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

/// The most open bound agents of a project that [`remove`] checks for a
/// checkout. More than this refuses the delete (TER-394: a bounded check
/// that fails closed).
pub const REMOVE_BOUND_CAP: i64 = 200;

/// `owner/name` of a forge URL, lower case, for the match against a
/// `github` pipeline (`git@github.com:o/r.git`, `https://github.com/o/r`).
fn forge_slug(url: &str) -> Option<String> {
    let mut parts = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .filter(|s| !s.is_empty());
    let name = parts.next()?.trim_end_matches(".git");
    let owner = parts.next()?;
    Some(format!("{owner}/{name}").to_ascii_lowercase())
}

/// The workspaces of the open bound agents of `project` (TER-74), at most
/// [`REMOVE_BOUND_CAP`] + 1 rows (one more shows the cap is hit). Reads the
/// partial index `spawns_bound_open`.
pub fn bound_workspaces(
    conn: &Connection,
    project: &str,
) -> ApiResult<Vec<(String, std::path::PathBuf)>> {
    let mut stmt = conn.prepare(
        "SELECT a.name, a.workspace FROM spawns s JOIN agents a ON a.id = s.id
         WHERE s.finished_at IS NULL AND s.bound_kind IS NOT NULL AND s.project = ?1
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![project, REMOVE_BOUND_CAP + 1], |r| {
            Ok((
                r.get::<_, String>(0)?,
                std::path::PathBuf::from(r.get::<_, String>(1)?),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The bound agents among `bound` whose workspace has a git checkout at
/// the repo's `path`. A checkout at another `--path` is not seen. Errors
/// when `bound` is over [`REMOVE_BOUND_CAP`].
pub fn bound_checkouts(
    repo: &Repo,
    bound: &[(String, std::path::PathBuf)],
) -> ApiResult<Vec<String>> {
    if bound.len() as i64 > REMOVE_BOUND_CAP {
        return Err(ApiError::conflict(format!(
            "project `{}` has more than {REMOVE_BOUND_CAP} open bound agents; \
             cannot check their checkouts of `{}`: try again when fewer run",
            repo.project, repo.name
        )));
    }
    Ok(bound
        .iter()
        .filter(|(_, ws)| std::fs::symlink_metadata(ws.join(&repo.path).join(".git")).is_ok())
        .map(|(name, _)| name.clone())
        .collect())
}

/// Delete the registration of `repo` (`DELETE /api/repos/{id}`, TER-658).
/// The clones in workspaces and on targets stay. Refused (`409`) while a
/// queued or running job, the delivery `next_repos`, a `github` pipeline
/// of the same `owner/name`, or a bound agent's checkout (`checked_out`,
/// from [`bound_checkouts`]) uses it. A repo of a project file is
/// `file_managed`. Each check reads an index or a capped set. Audited as
/// `repo.delete`, and a `repo.delete` event.
pub fn remove(t: &mut Tx<'_>, id: &str, checked_out: &[String]) -> ApiResult<Repo> {
    let repo = find(t, id)?.ok_or_else(|| ApiError::not_found(format!("no repo `{id}`")))?;
    let key = format!("{}/{}", repo.project, repo.name);
    if repo.source == projects::SOURCE_FILE {
        return Err(ApiError::new(
            crate::error::Code::FileManaged,
            format!(
                "repo `{key}` is declared in projects/{}.toml; remove it there, then POST /api/reload",
                repo.project
            ),
        ));
    }
    let mut uses: Vec<String> = Vec::new();
    // jobs_project_state (project, state, created_at).
    let mut stmt = t.prepare(
        "SELECT card, kind, state FROM jobs
         WHERE project = ?1 AND state IN ('queued', 'running') AND repo = ?2 LIMIT 5",
    )?;
    let jobs = stmt
        .query_map(params![repo.project, repo.name], |r| {
            Ok(format!(
                "{} {} job of {}",
                r.get::<_, String>(2)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(0)?
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    uses.extend(jobs);
    if crate::store::jobs::delivery(t, &repo.project)?
        .next_repos
        .contains(&repo.name)
    {
        uses.push("the delivery config (`next_repos`)".to_owned());
    }
    // At most `pipelines::MAX_PER_PROJECT` rows, by UNIQUE (project, name).
    if let Some(slug) = forge_slug(&repo.url) {
        let mut stmt = t.prepare(
            "SELECT name, json_extract(def, '$.source.repo') FROM pipelines
             WHERE project = ?1 AND adapter = 'github'",
        )?;
        let pipes = stmt
            .query_map([&repo.project], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        uses.extend(
            pipes
                .into_iter()
                .filter(|(_, r)| r.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&slug)))
                .map(|(n, _)| format!("pipeline `{n}`")),
        );
    }
    uses.extend(
        checked_out
            .iter()
            .map(|a| format!("the checkout of bound agent `{a}`")),
    );
    if !uses.is_empty() {
        return Err(ApiError::conflict(format!(
            "repo `{key}` is in use: {}; remove those first",
            uses.join(", ")
        ))
        .with_details(json!({"uses": uses})));
    }
    t.execute("DELETE FROM repos WHERE id = ?1", [&repo.id])?;
    t.audit(
        "repo.delete",
        &key,
        Some(&repo.project),
        json!({"id": repo.id, "url": repo.url, "source": repo.source}),
    )?;
    t.emit(
        crate::events::EventDraft::new(
            "repo.delete",
            format!("repo {key} removed (the clones stay)"),
            json!({"repo": repo.name, "repo_id": repo.id, "url": repo.url}),
        )
        .project(repo.project.clone()),
    )?;
    Ok(repo)
}

/// Every repo of a project, by name.
/// Each repo of each project.
pub fn all(conn: &Connection) -> ApiResult<Vec<Repo>> {
    let mut stmt = conn.prepare("SELECT * FROM repos ORDER BY project, name")?;
    let rows = stmt
        .query_map([], repo_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

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
