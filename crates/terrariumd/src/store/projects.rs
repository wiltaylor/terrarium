//! Projects. The repos of a project live in [`super::repos`].

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::{Page, Paged, board, check_name};

pub use crate::store::repos::{NewRepo, Repo, attach_repo, list_repos};

/// Where a definition lives: a definition file, the API, or a fleet.
pub const SOURCE_FILE: &str = "file";
pub const SOURCE_API: &str = "api";
/// The prefix of the `source` of an object a fleet owns: `fleet:<name>`.
/// Such an object is not file-managed; the API may change it.
pub const SOURCE_FLEET: &str = "fleet:";

/// The `source` word of an object that fleet `name` owns.
pub fn fleet_source(name: &str) -> String {
    format!("{SOURCE_FLEET}{name}")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub description: String,
    /// The card ID prefix: `TER` makes `TER-42`.
    pub card_prefix: String,
    /// `file` (declared in `projects/<name>.toml`), `api`, or
    /// `fleet:<name>` (owned by a fleet).
    pub source: String,
    /// The idle alert (TER-67, `crate::idle_alert`): how long the assignee
    /// of a card in progress may rest with no wait before the leads hear of
    /// it. `off`, or a duration (`15m`). `None`: the default
    /// ([`crate::idle_alert::DEFAULT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_alert: Option<String>,
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
        idle_alert: r.get("idle_alert")?,
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

/// The names that a project cannot have: the scope words of the secret
/// store (PLAN §16b, S7). A project named `global` would get the global
/// secrets as its own scope.
pub const RESERVED_NAMES: [&str; 1] = [crate::secrets::GLOBAL];

/// Refuse a reserved project name ([`RESERVED_NAMES`]).
pub fn check_not_reserved(name: &str) -> ApiResult<()> {
    if RESERVED_NAMES.contains(&name) {
        return Err(ApiError::invalid(format!(
            "`{name}` is a reserved word (the scope of the global secrets); a project cannot \
             have this name"
        )));
    }
    Ok(())
}

/// True when a project has a reserved name: one made before the name was
/// reserved. The daemon starts, and warns.
pub fn has_reserved_name(conn: &rusqlite::Connection) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM projects WHERE name = ?1",
        [crate::secrets::GLOBAL],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Make a project, its board, and the default columns. The API, a fleet,
/// and the config import all come here.
pub fn create(t: &mut Tx<'_>, p: &NewProject, source: &str) -> ApiResult<Project> {
    check_name("project", &p.name)?;
    check_not_reserved(&p.name)?;
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
    /// The idle alert: `off` (or `0`), a duration (`15m`, `1h`), or `null`
    /// for the default.
    #[serde(default, deserialize_with = "crate::store::board::double_option")]
    pub idle_alert: Option<Option<String>>,
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
    if let Some(alert) = &patch.idle_alert {
        let alert = alert
            .as_deref()
            .map(crate::idle_alert::normalise)
            .transpose()
            .map_err(ApiError::invalid)?;
        t.execute(
            "UPDATE projects SET idle_alert = ?2 WHERE name = ?1",
            params![name, alert],
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
        json!({"description": patch.description, "card_prefix": patch.card_prefix,
               "idle_alert": patch.idle_alert}),
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
    // The project's secrets and their grants (PLAN §16b.2).
    crate::secrets::delete_project(t, name)?;
    crate::secrets::grants::on_project_deleted(t, name)?;
    // Its tools and the tool grants to it (`crate::store::tools`).
    crate::store::tools::on_project_deleted(t, name)?;
    t.audit("project.delete", name, Some(name), json!({}))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert!(valid_prefix("TER"));
        assert!(valid_prefix("T2"));
        assert!(!valid_prefix("2T"));
        assert!(!valid_prefix("ter"));
    }

    /// S7: `global` is the scope of the global secrets: no project has
    /// this name.
    #[tokio::test]
    async fn a_project_cannot_be_named_global() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        let db = crate::db::Db::new(c, crate::events::EventHub::default());
        let e = db
            .write("operator", |t| {
                create(
                    t,
                    &NewProject {
                        name: "global".into(),
                        ..Default::default()
                    },
                    SOURCE_API,
                )
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
        assert!(e.message.contains("reserved"), "{}", e.message);
        // A project named `global` from before: the daemon finds it.
        assert!(!db.read(has_reserved_name).await.unwrap());
        db.read(|c| {
            c.execute(
                "INSERT INTO projects (name, created_at, updated_at)
                 VALUES ('global', 'x', 'x')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(db.read(has_reserved_name).await.unwrap());
    }
}
