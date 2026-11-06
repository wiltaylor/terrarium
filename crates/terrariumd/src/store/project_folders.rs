//! Project folders (docs/api.md §6.12, TER-48): the folders that the
//! dashboard shows under Projects in its sidebar. Folders nest by
//! `parent_id` (`None` is the root), at most [`MAX_DEPTH`] levels deep. A
//! project's place is a row in `project_placements`; a project without a
//! row sits at the root. Folders only order the sidebar: they change no
//! right, scope, or name.
//!
//! In each parent, folders come first, in `position` order, then the
//! projects, in `position` order. Each write renumbers the positions of
//! the parent that it changes from 0.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};

/// The deepest level of a folder: a root folder is level 1.
pub const MAX_DEPTH: usize = 8;

/// The longest folder name, in characters.
pub const MAX_NAME: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Placement {
    pub project: String,
    pub folder_id: Option<String>,
    pub position: i64,
}

/// `GET /api/project-folders`: every folder and every placed project.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tree {
    pub folders: Vec<Folder>,
    pub placements: Vec<Placement>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewFolder {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
}

/// A change of a folder. `parent_id` is three-way: absent keeps the
/// parent, `null` moves the folder to the root, an ID moves it there.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, with = "double_option")]
    pub parent_id: Option<Option<String>>,
    #[serde(default)]
    pub position: Option<i64>,
}

/// The place of a project: `folder_id` `null` is the root.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    pub folder_id: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}

fn folder_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: r.get(0)?,
        name: r.get(1)?,
        parent_id: r.get(2)?,
        position: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

const FOLDER_COLS: &str = "id, name, parent_id, position, created_at, updated_at";

/// Every folder (by parent, then position) and every placement.
pub fn list(conn: &Connection) -> ApiResult<Tree> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FOLDER_COLS} FROM project_folders
          ORDER BY parent_id IS NOT NULL, parent_id, position, name"
    ))?;
    let folders = stmt
        .query_map([], folder_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT project, folder_id, position FROM project_placements
          ORDER BY folder_id IS NOT NULL, folder_id, position, project",
    )?;
    let placements = stmt
        .query_map([], |r| {
            Ok(Placement {
                project: r.get(0)?,
                folder_id: r.get(1)?,
                position: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Tree {
        folders,
        placements,
    })
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Folder> {
    conn.query_row(
        &format!("SELECT {FOLDER_COLS} FROM project_folders WHERE id = ?1"),
        [id],
        folder_row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("no project folder `{id}`")))
}

/// Trim a folder name and check it: 1 to [`MAX_NAME`] characters, no
/// control characters.
pub fn clean_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::invalid("a folder name cannot be empty"));
    }
    if name.chars().count() > MAX_NAME {
        return Err(ApiError::invalid(format!(
            "a folder name has at most {MAX_NAME} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(ApiError::invalid(
            "a folder name cannot have control characters",
        ));
    }
    Ok(name.to_owned())
}

/// The parent of each folder.
fn parents(conn: &Connection) -> ApiResult<HashMap<String, Option<String>>> {
    let mut stmt = conn.prepare("SELECT id, parent_id FROM project_folders")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(rows)
}

/// The level of `id`: a root folder is 1. Stops at a loop.
fn level(parents: &HashMap<String, Option<String>>, id: &str) -> usize {
    let mut seen = HashSet::new();
    let mut at = Some(id.to_owned());
    let mut n = 0;
    while let Some(cur) = at {
        if !seen.insert(cur.clone()) {
            break;
        }
        n += 1;
        at = parents.get(&cur).cloned().flatten();
    }
    n
}

/// The levels of the subtree of `id`, `id` itself included: 1 for a
/// folder without subfolders.
fn height(parents: &HashMap<String, Option<String>>, id: &str) -> usize {
    let mut best = 1;
    let mut stack = vec![(id.to_owned(), 1)];
    let mut seen = HashSet::new();
    while let Some((cur, h)) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        best = best.max(h);
        for (child, parent) in parents {
            if parent.as_deref() == Some(cur.as_str()) {
                stack.push((child.clone(), h + 1));
            }
        }
    }
    best
}

/// Whether `id` is `ancestor` or below it.
fn is_within(parents: &HashMap<String, Option<String>>, id: &str, ancestor: &str) -> bool {
    let mut seen = HashSet::new();
    let mut at = Some(id.to_owned());
    while let Some(cur) = at {
        if cur == ancestor {
            return true;
        }
        if !seen.insert(cur.clone()) {
            return false;
        }
        at = parents.get(&cur).cloned().flatten();
    }
    false
}

fn too_deep() -> ApiError {
    ApiError::invalid(format!("folders nest at most {MAX_DEPTH} levels deep"))
}

/// Put `id` at `position` among the folders of `parent` (the end when
/// `None`), and renumber them from 0.
fn order_folders(
    t: &Tx<'_>,
    parent: Option<&str>,
    id: &str,
    position: Option<i64>,
) -> ApiResult<()> {
    let mut stmt = t.prepare(
        "SELECT id FROM project_folders
          WHERE parent_id IS ?1 AND id <> ?2
          ORDER BY position, name, id",
    )?;
    let mut ids = stmt
        .query_map(params![parent, id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    ids.insert(slot(position, ids.len()), id.to_owned());
    for (n, f) in ids.iter().enumerate() {
        t.execute(
            "UPDATE project_folders SET position = ?2 WHERE id = ?1",
            params![f, n as i64],
        )?;
    }
    Ok(())
}

/// Put `project` at `position` among the placed projects of `folder`
/// (the end when `None`), and renumber them from 0.
fn order_projects(
    t: &Tx<'_>,
    folder: Option<&str>,
    project: &str,
    position: Option<i64>,
) -> ApiResult<()> {
    let mut stmt = t.prepare(
        "SELECT project FROM project_placements
          WHERE folder_id IS ?1 AND project <> ?2
          ORDER BY position, project",
    )?;
    let mut names = stmt
        .query_map(params![folder, project], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    names.insert(slot(position, names.len()), project.to_owned());
    for (n, p) in names.iter().enumerate() {
        t.execute(
            "UPDATE project_placements SET position = ?2 WHERE project = ?1",
            params![p, n as i64],
        )?;
    }
    Ok(())
}

fn slot(position: Option<i64>, len: usize) -> usize {
    match position {
        Some(p) if p >= 0 => usize::try_from(p).unwrap_or(len).min(len),
        Some(_) => 0,
        None => len,
    }
}

/// Make a folder under `parent_id` (the root when `None`), at the end.
pub fn create(t: &mut Tx<'_>, new: &NewFolder) -> ApiResult<Folder> {
    let name = clean_name(&new.name)?;
    let parent = new.parent_id.as_deref().filter(|p| !p.is_empty());
    if let Some(p) = parent {
        get(t, p)?;
        if level(&parents(t)?, p) + 1 > MAX_DEPTH {
            return Err(too_deep());
        }
    }
    let id = ulid::Ulid::new().to_string();
    let at = now();
    t.execute(
        "INSERT INTO project_folders (id, name, parent_id, position, created_at, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?4)",
        params![id, name, parent, at],
    )?;
    order_folders(t, parent, &id, None)?;
    t.audit(
        "project_folder.create",
        &id,
        None,
        json!({"name": name, "parent_id": parent}),
    )?;
    get(t, &id)
}

/// Rename, move, or reorder a folder. A move into the folder itself or
/// below it, or one that nests deeper than [`MAX_DEPTH`], is refused.
pub fn update(t: &mut Tx<'_>, id: &str, patch: &FolderPatch) -> ApiResult<Folder> {
    let old = get(t, id)?;
    let name = match &patch.name {
        Some(n) => clean_name(n)?,
        None => old.name.clone(),
    };
    let parent = match &patch.parent_id {
        Some(p) => p.clone().filter(|p| !p.is_empty()),
        None => old.parent_id.clone(),
    };
    let moved = parent != old.parent_id;
    if let (true, Some(p)) = (moved, parent.as_deref()) {
        get(t, p)?;
        let tree = parents(t)?;
        if is_within(&tree, p, id) {
            return Err(ApiError::invalid(
                "a folder cannot move into itself or one of its subfolders",
            ));
        }
        if level(&tree, p) + height(&tree, id) > MAX_DEPTH {
            return Err(too_deep());
        }
    }
    t.execute(
        "UPDATE project_folders SET name = ?2, parent_id = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, name, parent, now()],
    )?;
    if moved || patch.position.is_some() {
        let position = match (moved, patch.position) {
            (_, Some(p)) => Some(p),
            (true, None) => None,
            (false, None) => Some(old.position),
        };
        order_folders(t, parent.as_deref(), id, position)?;
        if moved {
            renumber(t, old.parent_id.as_deref())?;
        }
    }
    t.audit(
        "project_folder.update",
        id,
        None,
        json!({"name": name, "parent_id": parent, "position": patch.position}),
    )?;
    get(t, id)
}

/// Renumber the folders and projects of `parent` from 0, in their order.
fn renumber(t: &Tx<'_>, parent: Option<&str>) -> ApiResult<()> {
    let first: Option<String> = t
        .query_row(
            "SELECT id FROM project_folders WHERE parent_id IS ?1 ORDER BY position, name, id",
            params![parent],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(f) = first {
        order_folders(t, parent, &f, Some(0))?;
    }
    let first: Option<String> = t
        .query_row(
            "SELECT project FROM project_placements WHERE folder_id IS ?1 ORDER BY position, project",
            params![parent],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(p) = first {
        order_projects(t, parent, &p, Some(0))?;
    }
    Ok(())
}

/// Delete a folder. Its subfolders and projects move up to its parent, at
/// its place, in their order. Moving up one level cannot nest deeper.
pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let old = get(t, id)?;
    let parent = old.parent_id.as_deref();

    // The folders of the parent, with the children of `id` in its place.
    let mut stmt = t.prepare(
        "SELECT id, parent_id IS ?2 FROM project_folders
          WHERE parent_id IS ?1 OR parent_id = ?2
          ORDER BY parent_id IS ?2, position, name, id",
    )?;
    let rows = stmt
        .query_map(params![parent, id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let (children, siblings): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(_, c)| !*c);
    let mut order = Vec::new();
    for (f, _) in siblings {
        if f == id {
            order.extend(children.iter().map(|(c, _)| c.clone()));
        } else {
            order.push(f);
        }
    }
    let at = now();
    for (n, f) in order.iter().enumerate() {
        t.execute(
            "UPDATE project_folders SET parent_id = ?2, position = ?3, updated_at = ?4 WHERE id = ?1",
            params![f, parent, n as i64, at],
        )?;
    }

    // The projects of `id` go after the projects of the parent.
    let base: i64 = t.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM project_placements WHERE folder_id IS ?1",
        params![parent],
        |r| r.get(0),
    )?;
    let moved = t.execute(
        "UPDATE project_placements SET folder_id = ?2, position = position + ?3 WHERE folder_id = ?1",
        params![id, parent, base],
    )?;
    t.execute("DELETE FROM project_folders WHERE id = ?1", [id])?;
    renumber(t, parent)?;
    t.audit(
        "project_folder.delete",
        id,
        None,
        json!({"name": old.name, "parent_id": parent, "children": children.len(), "projects": moved}),
    )?;
    Ok(())
}

/// Put `project` in `place.folder_id` (the root when `None`), at
/// `place.position` among the placed projects there (the end when
/// `None`).
pub fn place(t: &mut Tx<'_>, project: &str, place: &Place) -> ApiResult<Placement> {
    crate::store::projects::get(t, project)?;
    let folder = place.folder_id.as_deref().filter(|f| !f.is_empty());
    if let Some(f) = folder {
        get(t, f)?;
    }
    let old: Option<Option<String>> = t
        .query_row(
            "SELECT folder_id FROM project_placements WHERE project = ?1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    t.execute(
        "INSERT INTO project_placements (project, folder_id, position) VALUES (?1, ?2, 0)
         ON CONFLICT (project) DO UPDATE SET folder_id = excluded.folder_id",
        params![project, folder],
    )?;
    order_projects(t, folder, project, place.position)?;
    if let Some(from) = old.filter(|o| o.as_deref() != folder) {
        renumber(t, from.as_deref())?;
    }
    t.audit(
        "project.folder",
        project,
        Some(project),
        json!({"folder_id": folder, "position": place.position}),
    )?;
    let position = t.query_row(
        "SELECT position FROM project_placements WHERE project = ?1",
        [project],
        |r| r.get(0),
    )?;
    Ok(Placement {
        project: project.to_owned(),
        folder_id: folder.map(str::to_owned),
        position,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tree(pairs: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), b.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn levels_and_heights() {
        let t = tree(&[("a", None), ("b", Some("a")), ("c", Some("b")), ("d", None)]);
        assert_eq!(level(&t, "a"), 1);
        assert_eq!(level(&t, "c"), 3);
        assert_eq!(height(&t, "a"), 3);
        assert_eq!(height(&t, "c"), 1);
        assert!(is_within(&t, "c", "a"));
        assert!(is_within(&t, "a", "a"));
        assert!(!is_within(&t, "d", "a"));
    }

    #[test]
    fn names_are_trimmed_and_checked() {
        assert_eq!(clean_name("  Work ").unwrap(), "Work");
        assert!(clean_name("   ").is_err());
        assert!(clean_name("a\nb").is_err());
        assert!(clean_name(&"x".repeat(65)).is_err());
        assert!(clean_name(&"é".repeat(64)).is_ok());
    }

    #[test]
    fn slots_clamp() {
        assert_eq!(slot(None, 3), 3);
        assert_eq!(slot(Some(1), 3), 1);
        assert_eq!(slot(Some(9), 3), 3);
        assert_eq!(slot(Some(-1), 3), 0);
    }
}
