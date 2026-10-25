//! Target rows (PLAN §19): `targets/<name>.toml` files (source `file`),
//! targets made through the API (source `api`), and targets that a fleet
//! owns (source `fleet:<name>`).
//!
//! Target names are one namespace across projects, as the file names are.
//! The row keeps the definition as JSON, and the last state that a
//! lifecycle verb saw.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::config::{TargetDef, TargetKind};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::store::projects::{SOURCE_API, SOURCE_FILE};
use crate::store::sync::Changes;
use crate::store::{check_name, json_col};

/// A stored target.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetRow {
    pub def: TargetDef,
    /// `file`, `api`, or `fleet:<name>` (owned by a fleet).
    pub source: String,
    /// `running`, `stopped`, `absent`, or `unknown`.
    pub state: String,
    pub snapshots: Vec<String>,
    pub state_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// What runs now: the attached workspaces and the lab instances.
    pub runtime: Runtime,
}

/// The runtime record of a target (docs/api.md §17.2, "Workspaces").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    /// The agent workspaces (`<project>/<agent>`) that the running
    /// container or lab instance has attached.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attached: Vec<String>,
    /// A `lab_repo` target: the lab dir of its instance on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab: Option<String>,
    /// A `lab_repo` target: the commit of its worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// A `lab_from_workspace` target: one instance for each holder, by the
    /// agent ULID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instances: BTreeMap<String, Instance>,
}

/// One lab instance of a `lab_from_workspace` target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    /// `project/name` of the holder.
    pub agent_name: String,
    /// The lab dir of the instance on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab: Option<String>,
    /// `running`, `stopped`, `absent`, or `unknown`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attached: Vec<String>,
}

const COLS: &str = "name, def, source, state, snapshots, state_at, created_at, updated_at, runtime";

fn row(r: &Row<'_>) -> rusqlite::Result<TargetRow> {
    let name: String = r.get(0)?;
    let def: String = r.get(1)?;
    let mut def: TargetDef = serde_json::from_str(&def).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    def.name = name;
    let snapshots: String = r.get(4)?;
    Ok(TargetRow {
        def,
        source: r.get(2)?,
        state: r.get(3)?,
        snapshots: json_col(&snapshots),
        state_at: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        runtime: json_col(&r.get::<_, String>(8)?),
    })
}

pub fn kind_str(k: TargetKind) -> &'static str {
    match k {
        TargetKind::Host => "host",
        TargetKind::Ssh => "ssh",
        TargetKind::Docker => "docker",
        TargetKind::Vmlab => "vmlab",
    }
}

fn positional(field: &str, v: &Option<String>) -> ApiResult<()> {
    if let Some(v) = v
        && (v.is_empty() || v.starts_with('-') || v.chars().any(char::is_whitespace))
    {
        return Err(ApiError::invalid(format!(
            "bad `{field}` `{v}`: not empty, no spaces, and not starting with `-`"
        )));
    }
    Ok(())
}

fn need(kind: &str, field: &str, present: bool) -> ApiResult<()> {
    if present {
        Ok(())
    } else {
        Err(ApiError::invalid(format!(
            "a `{kind}` target needs `{field}`"
        )))
    }
}

/// Check a definition against the rules of its kind (PLAN §19).
pub fn validate(def: &TargetDef) -> ApiResult<()> {
    check_name("target", &def.name)?;
    if let Some(p) = &def.project {
        check_name("project", p)?;
    }
    for (f, v) in [
        ("host", &def.host),
        ("address", &def.address),
        ("credential", &def.credential),
        ("image", &def.image),
        ("container", &def.container),
        ("machine", &def.machine),
        ("lab_ref", &def.lab_ref),
    ] {
        positional(f, v)?;
    }
    if let Some(p) = &def.lab_path
        && (p.starts_with('/')
            || p.split('/').any(|s| s == ".." || s.starts_with('-'))
            || p.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        return Err(ApiError::invalid(format!(
            "bad `lab_path` `{p}`: a relative directory with no `..`"
        )));
    }
    let k = kind_str(def.kind);
    match def.kind {
        TargetKind::Host => need(k, "host", def.host.is_some())?,
        TargetKind::Ssh => need(k, "address", def.address.is_some())?,
        TargetKind::Docker => {
            need(k, "host", def.host.is_some())?;
            need(
                k,
                "image` or `container",
                def.image.is_some() || def.container.is_some(),
            )?;
        }
        TargetKind::Vmlab => {
            need(k, "host", def.host.is_some())?;
            let modes = [
                def.lab.is_some(),
                def.lab_repo.is_some(),
                def.lab_from_workspace,
            ]
            .iter()
            .filter(|m| **m)
            .count();
            if modes != 1 {
                return Err(ApiError::invalid(
                    "a `vmlab` target needs one of `lab` (a lab dir on the host), `lab_repo` \
                     (a repo of the project), and `lab_from_workspace = true`",
                ));
            }
            if (def.lab_repo.is_some() || def.lab_from_workspace) && def.project.is_none() {
                return Err(ApiError::invalid(
                    "`lab_repo` and `lab_from_workspace` need a project target",
                ));
            }
            if def.lab_from_workspace && def.snapshot_before_task {
                return Err(ApiError::invalid(
                    "`snapshot_before_task` does not work with `lab_from_workspace`: each \
                     holder has its own instance",
                ));
            }
            if def.lab.is_none() && def.machine.is_none() {
                return Err(ApiError::invalid(
                    "`lab_repo` and `lab_from_workspace` need `machine`: the machine that \
                     runs the commands and gets the workspace",
                ));
            }
            if def.lab.is_some() && (def.lab_ref.is_some() || def.lab_path.is_some()) {
                return Err(ApiError::invalid(
                    "`lab_ref` and `lab_path` go with `lab_repo` or `lab_from_workspace`",
                ));
            }
            if let Some(lab) = &def.lab
                && !lab.is_absolute()
            {
                return Err(ApiError::invalid(format!(
                    "`lab` must be an absolute path, not `{}`",
                    lab.display()
                )));
            }
        }
    }
    if def.kind != TargetKind::Vmlab
        && (def.lab_repo.is_some()
            || def.lab_ref.is_some()
            || def.lab_path.is_some()
            || def.lab_from_workspace)
    {
        return Err(ApiError::invalid(format!(
            "`lab_repo`, `lab_ref`, `lab_path`, and `lab_from_workspace` are for a vmlab target, not `{k}`"
        )));
    }
    if def.ephemeral && matches!(def.kind, TargetKind::Host | TargetKind::Ssh) {
        return Err(ApiError::invalid(format!(
            "a `{k}` target is permanent; `ephemeral` needs a docker or vmlab target"
        )));
    }
    if def.ephemeral && def.kind == TargetKind::Docker && def.image.is_none() {
        return Err(ApiError::invalid(
            "an ephemeral docker target needs `image`; it is run on agent start",
        ));
    }
    Ok(())
}

pub fn list(conn: &Connection) -> ApiResult<Vec<TargetRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM targets ORDER BY name"))?;
    let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn find(conn: &Connection, name: &str) -> ApiResult<Option<TargetRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM targets WHERE name = ?1"),
            [name],
            row,
        )
        .optional()?)
}

pub fn get(conn: &Connection, name: &str) -> ApiResult<TargetRow> {
    find(conn, name)?.ok_or_else(|| ApiError::not_found(format!("no target `{name}`")))
}

fn def_json(def: &TargetDef) -> ApiResult<String> {
    let mut v = serde_json::to_value(def)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("name");
    }
    Ok(v.to_string())
}

fn insert(t: &mut Tx<'_>, def: &TargetDef, source: &str) -> ApiResult<()> {
    let at = now();
    t.execute(
        "INSERT INTO targets (name, project, kind, def, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            def.name,
            def.project,
            kind_str(def.kind),
            def_json(def)?,
            source,
            at
        ],
    )?;
    Ok(())
}

/// The number of targets of `project`.
pub fn count_in_project(conn: &Connection, project: &str) -> ApiResult<usize> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM targets WHERE project = ?1",
        [project],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// Store the runtime record of a target.
pub fn set_runtime(t: &mut Tx<'_>, name: &str, rt: &Runtime) -> ApiResult<()> {
    t.execute(
        "UPDATE targets SET runtime = ?2 WHERE name = ?1",
        params![name, serde_json::to_string(rt)?],
    )?;
    Ok(())
}

/// Make an API target. The project must exist.
pub fn create(t: &mut Tx<'_>, def: &TargetDef) -> ApiResult<TargetRow> {
    validate(def)?;
    if let Some(p) = &def.project {
        crate::store::projects::get(t, p)?;
    }
    if find(t, &def.name)?.is_some() {
        return Err(ApiError::conflict(format!("target `{}` exists", def.name)));
    }
    insert(t, def, SOURCE_API)?;
    get(t, &def.name)
}

fn refuse_file(r: &TargetRow) -> ApiResult<()> {
    if r.source == SOURCE_FILE {
        return Err(ApiError::new(
            Code::FileManaged,
            format!(
                "target `{}` is declared in targets/{}.toml; edit the file, then POST /api/reload",
                r.def.name, r.def.name
            ),
        ));
    }
    Ok(())
}

/// Apply a JSON merge patch to an API target: `null` removes a field.
/// `name` cannot change.
pub fn update(t: &mut Tx<'_>, name: &str, patch: &Value) -> ApiResult<TargetRow> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    let Some(p) = patch.as_object() else {
        return Err(ApiError::bad_request("the body must be a JSON object"));
    };
    let mut v = serde_json::to_value(&current.def)?;
    let Some(o) = v.as_object_mut() else {
        return Err(ApiError::internal("a target is not an object"));
    };
    for (k, val) in p {
        if k == "name" && val.as_str() != Some(name) {
            return Err(ApiError::invalid("a target cannot be renamed"));
        }
        if val.is_null() {
            o.remove(k);
        } else {
            o.insert(k.clone(), val.clone());
        }
    }
    let mut def: TargetDef = serde_json::from_value(v)
        .map_err(|e| ApiError::bad_request(format!("bad target field: {e}")))?;
    def.name = name.to_owned();
    validate(&def)?;
    if let Some(p) = &def.project {
        crate::store::projects::get(t, p)?;
    }
    t.execute(
        "UPDATE targets SET project = ?2, kind = ?3, def = ?4, updated_at = ?5 WHERE name = ?1",
        params![
            name,
            def.project,
            kind_str(def.kind),
            def_json(&def)?,
            now()
        ],
    )?;
    get(t, name)
}

/// Delete an API target.
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<TargetRow> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    t.execute("DELETE FROM targets WHERE name = ?1", [name])?;
    Ok(current)
}

/// Record the state that a lifecycle verb saw, and a snapshot name to add.
/// The state `absent` clears the snapshots: a destroyed target has none.
pub fn set_state(
    t: &mut Tx<'_>,
    name: &str,
    state: &str,
    add_snapshot: Option<&str>,
) -> ApiResult<()> {
    let current = get(t, name)?;
    let mut snaps = current.snapshots;
    if state == "absent" {
        snaps.clear();
    }
    if let Some(s) = add_snapshot
        && !snaps.iter().any(|x| x == s)
    {
        snaps.push(s.to_owned());
    }
    t.execute(
        "UPDATE targets SET state = ?2, snapshots = ?3, state_at = ?4 WHERE name = ?1",
        params![name, state, json!(snaps).to_string(), now()],
    )?;
    Ok(())
}

/// Sync `targets/*.toml` into the table, as `store::sync` does for agents:
/// a file target is made or updated and marked `file`; an API target with
/// the same name becomes `file`; a file target whose file is gone is
/// deleted. Bad definitions go into `errors` of the returned changes.
pub fn sync_files(t: &mut Tx<'_>, defs: &[TargetDef]) -> ApiResult<(Changes, Vec<String>)> {
    let mut changes = Changes::default();
    let mut errors = Vec::new();
    let mut kept = Vec::new();
    for def in defs {
        if let Err(e) = validate(def) {
            errors.push(format!("targets/{}.toml: {}", def.name, e.message));
            continue;
        }
        kept.push(def.name.clone());
        match find(t, &def.name)? {
            None => {
                insert(t, def, SOURCE_FILE)?;
                changes.added.push(def.name.clone());
            }
            Some(r) if r.def != *def || r.source != SOURCE_FILE => {
                t.execute(
                    "UPDATE targets SET project = ?2, kind = ?3, def = ?4, source = 'file',
                                        updated_at = ?5
                     WHERE name = ?1",
                    params![
                        def.name,
                        def.project,
                        kind_str(def.kind),
                        def_json(def)?,
                        now()
                    ],
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
        t.execute("DELETE FROM targets WHERE name = ?1", [&name])?;
        changes.removed.push(name);
    }
    if !changes.added.is_empty() || !changes.updated.is_empty() || !changes.removed.is_empty() {
        t.audit(
            "targets.sync",
            "targets",
            None,
            serde_json::to_value(&changes)?,
        )?;
    }
    Ok((changes, errors))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn def(text: &str, name: &str) -> TargetDef {
        let mut d: TargetDef = toml::from_str(text).unwrap();
        d.name = name.into();
        d
    }

    #[test]
    fn validation_follows_the_kind() {
        assert!(validate(&def("kind = \"host\"\nhost = \"lab1\"", "a")).is_ok());
        assert!(validate(&def("kind = \"host\"", "a")).is_err());
        assert!(validate(&def("kind = \"ssh\"\naddress = \"u@h\"", "a")).is_ok());
        assert!(validate(&def("kind = \"ssh\"\naddress = \"-oProxy=x\"", "a")).is_err());
        assert!(validate(&def("kind = \"docker\"\nhost = \"h\"", "a")).is_err());
        assert!(
            validate(&def(
                "kind = \"docker\"\nhost = \"h\"\nimage = \"alpine\"",
                "a"
            ))
            .is_ok()
        );
        assert!(validate(&def("kind = \"vmlab\"\nhost = \"h\"\nlab = \"rel\"", "a")).is_err());
        assert!(
            validate(&def(
                "kind = \"vmlab\"\nhost = \"h\"\nlab = \"/l\"\nmachine = \"m\"",
                "a"
            ))
            .is_ok()
        );
        assert!(
            validate(&def(
                "kind = \"ssh\"\naddress = \"u@h\"\nephemeral = true",
                "a"
            ))
            .is_err()
        );
        assert!(validate(&def("kind = \"host\"\nhost = \"h\"", "Bad")).is_err());
    }
}
