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
use crate::secrets::{self, GLOBAL, Kind, SecretInfo};
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
    /// Settings used by the existing managed container, retained across edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_settings: Option<DockerSettings>,
    /// Create-time settings changed. Replacing a permanent container needs consent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recreate_required: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerSettings {
    pub image: Option<String>,
    pub limits: Option<terrarium_core::limits::TargetLimits>,
}

impl DockerSettings {
    pub fn equivalent(&self, other: &Self) -> bool {
        let normalized = |settings: &Self| {
            let mut limits = settings
                .limits
                .as_ref()
                .and_then(|l| l.resolve().ok())
                .unwrap_or_default();
            limits.swap_bytes = limits.swap_bytes.filter(|v| *v > 0);
            limits.io_weight = limits.io_weight.map(|w| {
                let w = u32::from(w);
                u16::try_from(10 + ((w - 1) * 990 + 9999 / 2) / 9999).unwrap_or(1000)
            });
            limits
        };
        self.image == other.image && normalized(self) == normalized(other)
    }
    pub fn of(def: &TargetDef) -> Self {
        Self {
            image: def.image.clone(),
            limits: def.limits.clone(),
        }
    }
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
        TargetKind::Vnc => "vnc",
        TargetKind::Rdp => "rdp",
        TargetKind::Desktop => "desktop",
        TargetKind::Exedev => "exedev",
    }
}

/// The `address` of a `vnc` or `rdp` target with its port: `port` when
/// set, else the port in `address`, else 5900 or 3389.
pub fn display_address(def: &TargetDef) -> ApiResult<String> {
    use terrarium_host::display::remote::{RDP_PORT, VNC_PORT, address};
    let default = if def.kind == TargetKind::Rdp {
        RDP_PORT
    } else {
        VNC_PORT
    };
    let raw = def
        .address
        .as_deref()
        .ok_or_else(|| ApiError::invalid(format!("target `{}` has no `address`", def.name)))?;
    // With a default port of 0, only an address with its own port passes.
    if let Some(p) = def.port
        && address(raw, 0).is_ok()
    {
        return Err(ApiError::invalid(format!(
            "give the port in `address` ({raw}) or in `port` ({p}), not in both"
        )));
    }
    address(raw, def.port.unwrap_or(default)).map_err(|e| ApiError::invalid(e.to_string()))
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
    if let Some(run_as) = &def.windows_run_as {
        if def.kind != TargetKind::Host {
            return Err(ApiError::invalid("windows_run_as requires a host target"));
        }
        let parts: Vec<_> = run_as.account.split('\\').collect();
        if parts.len() > 2
            || parts.iter().any(|p| p.trim().is_empty())
            || run_as.account.len() > 256
            || run_as.account.chars().any(char::is_control)
        {
            return Err(ApiError::invalid("windows_run_as.account must be NAME or DOMAIN\\NAME without control characters"));
        }
        check_name("secret", &run_as.password_secret)?;
    }
    if let Some(base) = &def.work_root {
        crate::target_roots::check_base(base)?;
    }
    if def.sync_workspace && !matches!(def.kind, TargetKind::Ssh | TargetKind::Exedev) {
        return Err(ApiError::invalid(
            "sync_workspace requires an ssh or exedev target",
        ));
    }
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
    // A Windows user name can hold a space; it goes in a frame, never on
    // a command line.
    for (f, v) in [("username", &def.username), ("domain", &def.domain)] {
        if let Some(v) = v
            && (v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
        {
            return Err(ApiError::invalid(format!(
                "bad `{f}`: 1 to 256 characters with no control characters"
            )));
        }
    }
    if let Some(n) = &def.display_name {
        terrarium_core::display::check_display_name(n).map_err(ApiError::invalid)?;
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
        TargetKind::Vnc | TargetKind::Rdp => {
            need(k, "host` (the host agent that connects", def.host.is_some())?;
            need(k, "address", def.address.is_some())?;
            display_address(def)?;
        }
        TargetKind::Desktop => need(k, "host", def.host.is_some())?,
        TargetKind::Exedev => crate::exedev::validate(def)?,
    }
    if def.exedev.is_some() && def.kind != TargetKind::Exedev {
        return Err(ApiError::invalid(format!(
            "`[exedev]` is for an exedev target, not `{k}`"
        )));
    }
    if (def.display.is_some() || def.display_name.is_some())
        && !matches!(def.kind, TargetKind::Docker | TargetKind::Desktop)
    {
        return Err(ApiError::invalid(format!(
            "`display` and `display_name` are for a docker or desktop target, not `{k}`"
        )));
    }
    if def.display_name.is_some() && def.kind == TargetKind::Docker && def.display.is_none() {
        return Err(ApiError::invalid(
            "`display_name` needs `display` (x11, wayland, or auto) on a docker target",
        ));
    }
    if def.username.is_some() && !matches!(def.kind, TargetKind::Vnc | TargetKind::Rdp) {
        return Err(ApiError::invalid(format!(
            "`username` is for a vnc or rdp target, not `{k}`"
        )));
    }
    if def.domain.is_some() && def.kind != TargetKind::Rdp {
        return Err(ApiError::invalid(format!(
            "`domain` is for an rdp target, not `{k}`"
        )));
    }
    if def.kind.display_only()
        && (def.image.is_some()
            || def.container.is_some()
            || def.machine.is_some()
            || def.lab.is_some())
    {
        return Err(ApiError::invalid(format!(
            "a `{k}` target has only a display: no `image`, `container`, `machine`, or `lab`"
        )));
    }
    if def.kind == TargetKind::Desktop && (def.address.is_some() || def.credential.is_some()) {
        return Err(ApiError::invalid(
            "a `desktop` target is the screen of its host agent: no `address` or `credential`",
        ));
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
    if def.ephemeral
        && matches!(
            def.kind,
            TargetKind::Host
                | TargetKind::Ssh
                | TargetKind::Vnc
                | TargetKind::Rdp
                | TargetKind::Desktop
        )
    {
        return Err(ApiError::invalid(format!(
            "a `{k}` target is permanent; `ephemeral` needs a docker or vmlab target"
        )));
    }
    if def.ephemeral && def.kind == TargetKind::Docker && def.image.is_none() {
        return Err(ApiError::invalid(
            "an ephemeral docker target needs `image`; it is run on agent start",
        ));
    }
    validate_limits(def)?;
    Ok(())
}

/// The rules of `limits` (docs/api.md §17.2, "Resource limits"): good
/// values, and a kind that can apply them. A `host` target applies them
/// with systemd on the host; a `docker` target with the flags of the
/// `docker run` that Terrarium makes, so it needs `image`.
fn validate_limits(def: &TargetDef) -> ApiResult<()> {
    let Some(l) = &def.limits else {
        return Ok(());
    };
    let k = kind_str(def.kind);
    match def.kind {
        TargetKind::Host => {}
        TargetKind::Docker if def.image.is_some() => {}
        TargetKind::Docker => {
            return Err(ApiError::invalid(
                "`limits` on a docker target needs `image`: Docker sets the limits when                  Terrarium runs the container, and a `container` alone is not run by Terrarium",
            ));
        }
        TargetKind::Vmlab => {
            return Err(ApiError::invalid(
                "`limits` are not for a vmlab target: the lab file sets the size of each VM                  (cpus, memory)",
            ));
        }
        TargetKind::Ssh => {
            return Err(ApiError::invalid(
                "`limits` are not for an ssh target: the daemon cannot apply them on the                  remote machine; use a host agent there and a `host` target",
            ));
        }
        TargetKind::Vnc | TargetKind::Rdp | TargetKind::Desktop => {
            return Err(ApiError::invalid(format!(
                "`limits` are not for a `{k}` target: it has only a display"
            )));
        }
        TargetKind::Exedev => {
            return Err(ApiError::invalid(
                "`limits` are not for an exedev target: `[exedev] cpu` and `memory_gb` set \
                 the size of the VM",
            ));
        }
    }
    l.resolve().map_err(ApiError::invalid)?;
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
        // `limits = {}` (for example after a patch removed the last
        // limit) is no limits.
        if o.get("limits")
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
        {
            o.remove("limits");
        }
    }
    Ok(v.to_string())
}

fn insert(t: &mut Tx<'_>, def: &TargetDef, source: &str, admin: bool) -> ApiResult<()> {
    let at = now();
    t.execute(
        "INSERT INTO targets (name, project, kind, def, source, created_at, updated_at,
                              credential_admin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
        params![
            def.name,
            def.project,
            kind_str(def.kind),
            def_json(def)?,
            source,
            at,
            admin
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

/// Make an API target. The project must exist. A project target that
/// names a global secret needs a grant to the project, unless the actor
/// is the operator or a sysadmin ([`check_global_credential`]).
pub fn create(t: &mut Tx<'_>, def: &TargetDef) -> ApiResult<TargetRow> {
    validate(def)?;
    if let Some(p) = &def.project {
        crate::store::projects::get(t, p)?;
    }
    check_credential(t, def, None)?;
    let admin = secrets::is_admin_actor(t, &t.actor)?;
    check_global_credential(t, def, admin)?;
    if find(t, &def.name)?.is_some() {
        return Err(ApiError::conflict(format!("target `{}` exists", def.name)));
    }
    insert(t, def, SOURCE_API, admin)?;
    get(t, &def.name)
}

/// Whether the operator, a sysadmin, or a target file set the definition
/// of target `name` (the `credential_admin` column, PLAN §16b.6a). False
/// for a target that is not in the table.
pub fn credential_admin(conn: &Connection, name: &str) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            "SELECT credential_admin FROM targets WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// The `credential_admin` of a write of `def` by the actor of `t`: true
/// when the actor is the operator or a sysadmin. A write by another
/// actor keeps an admin mark (`before`) only when it changes nothing but
/// the fields that a running target takes (`keep`,
/// `snapshot_before_task`): a project_admin cannot move an operator's
/// login to another address.
pub fn admin_after_write(
    t: &Tx<'_>,
    def: &TargetDef,
    before: Option<(&TargetDef, bool)>,
) -> ApiResult<bool> {
    admin_after_write_as(t, &t.actor, def, before)
}

fn admin_after_write_as(
    t: &Connection,
    actor: &str,
    def: &TargetDef,
    before: Option<(&TargetDef, bool)>,
) -> ApiResult<bool> {
    if secrets::is_admin_actor(t, actor)? {
        return Ok(true);
    }
    let Some((old, true)) = before else {
        return Ok(false);
    };
    let mut a = old.clone();
    let mut b = def.clone();
    for d in [&mut a, &mut b] {
        d.keep = false;
        d.snapshot_before_task = false;
    }
    Ok(a == b)
}

/// The save-time check of a project target's `credential` against the
/// rule of PLAN §16b.6a: `403 forbidden` when it names a global secret,
/// the project has no grant of it, and `admin` is false.
pub fn check_global_credential(conn: &Connection, def: &TargetDef, admin: bool) -> ApiResult<()> {
    match (&def.project, &def.credential) {
        (Some(p), Some(name)) => secrets::check_global_use(conn, p, name, admin),
        _ => Ok(()),
    }
}

/// The owner of the `credential` of `def`, for `crate::secrets::load`.
pub fn owner(def: &TargetDef) -> secrets::Owner<'_> {
    secrets::Owner::Target {
        name: &def.name,
        project: def.project.as_deref(),
    }
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
    update_with_live_limits(t, name, patch, false, false)
}

/// Only the API executor may opt into limits it has acknowledged on the host.
pub fn update_with_live_limits(
    t: &mut Tx<'_>,
    name: &str,
    patch: &Value,
    live: bool,
    acknowledged: bool,
) -> ApiResult<TargetRow> {
    let (def, admin) = prepare_update(t, &t.actor, name, patch, live)?;
    let current = get(t, name)?;
    invalidate_root_plan(t, &current.def, Some(&def))?;
    if def.kind == TargetKind::Docker
        && !def.ephemeral
        && def.container.is_none()
        && (def.image != current.def.image || def.limits != current.def.limits)
    {
        let mut rt = current.runtime.clone();
        if acknowledged && let Some(baseline) = &mut rt.docker_settings {
            baseline.limits = DockerSettings::of(&def).limits;
        }
        rt.recreate_required = rt
            .docker_settings
            .as_ref()
            .is_some_and(|baseline| !baseline.equivalent(&DockerSettings::of(&def)));
        set_runtime(t, name, &rt)?;
    }
    t.execute(
        "UPDATE targets SET project = ?2, kind = ?3, def = ?4, updated_at = ?5,
                            credential_admin = ?6
         WHERE name = ?1",
        params![
            name,
            def.project,
            kind_str(def.kind),
            def_json(&def)?,
            now(),
            admin
        ],
    )?;
    if patch.as_object().is_some_and(|p| p.contains_key("address")) {
        for address in [current.def.address.as_deref(), def.address.as_deref()]
            .into_iter()
            .flatten()
        {
            t.execute(
                "DELETE FROM job_host_keys WHERE hostname = ?1",
                [super::jobs::address_key(address)],
            )?;
        }
    }
    get(t, name)
}

/// Validate without changing stored defaults before host acknowledgement.
pub fn prepare_update(
    t: &Connection,
    actor: &str,
    name: &str,
    patch: &Value,
    live: bool,
) -> ApiResult<(TargetDef, bool)> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    let Some(p) = patch.as_object() else {
        return Err(ApiError::bad_request("the body must be a JSON object"));
    };
    let mut v = serde_json::to_value(&current.def)?;
    let Some(o) = v.as_object_mut() else {
        return Err(ApiError::internal("a target is not an object"));
    };
    if is_running(&current) {
        let mut changed: Vec<&str> = p
            .iter()
            .filter(|(k, val)| {
                k.as_str() != "name" && o.get(k.as_str()).unwrap_or(&Value::Null) != *val
            })
            .map(|(k, _)| k.as_str())
            .filter(|k| !RUNNING_PATCHABLE.contains(k))
            .collect();
        // A `host` target is always `running` while its host agent is
        // online. Its limits apply to each new session, so they change.
        if current.def.kind == TargetKind::Host || (live && current.def.kind == TargetKind::Docker)
        {
            changed.retain(|k| *k != "limits");
        }
        changed.sort_unstable();
        if !changed.is_empty() {
            return Err(ApiError::conflict(format!(
                "target `{name}` is running: only `keep` and `snapshot_before_task` change \
                 now; bring it down first to change {}",
                changed
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .with_details(json!({"state": "running", "fields": changed})));
        }
    }
    if !live && p.contains_key("limits") && current.def.container.is_some() {
        refuse_docker_limits(&current, p.get("limits"), o.get("limits"))?;
    }
    for (k, val) in p {
        if k == "name" && val.as_str() != Some(name) {
            return Err(ApiError::invalid("a target cannot be renamed"));
        }
        if val.is_null() {
            o.remove(k);
        } else if let (Some(Value::Object(old)), Value::Object(change)) = (o.get_mut(k), val) {
            // `limits` merges: `{"limits": {"cpu": 2}}` keeps the memory.
            merge(old, change);
        } else {
            o.insert(k.clone(), val.clone());
        }
    }
    let mut def: TargetDef = serde_json::from_value(v)
        .map_err(|e| ApiError::bad_request(format!("bad target field: {e}")))?;
    def.name = name.to_owned();
    validate(&def)?;
    guard_reserved_update(t, &current.def, &def)?;
    if let Some(p) = &def.project {
        crate::store::projects::get(t, p)?;
    }
    check_credential(t, &def, Some(&current.def))?;
    let was = credential_admin(t, name)?;
    let admin = admin_after_write_as(t, actor, &def, Some((&current.def, was)))?;
    check_global_credential(t, &def, admin)?;
    Ok((def, admin))
}

/// RFC 7396 merge of one object into another: `null` removes a key.
fn merge(into: &mut serde_json::Map<String, Value>, change: &serde_json::Map<String, Value>) {
    for (k, v) in change {
        match (into.get_mut(k), v) {
            (_, Value::Null) => {
                into.remove(k);
            }
            (Some(Value::Object(a)), Value::Object(b)) => merge(a, b),
            _ => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Docker sets the limits of a container when it makes it. A docker
/// target whose container exists but is stopped keeps its old limits on
/// `up` (a `docker start`), so a change of `limits` then is `409 conflict`.
/// An ephemeral target gets a new container on each `up`.
fn refuse_docker_limits(
    current: &TargetRow,
    patch: Option<&Value>,
    now: Option<&Value>,
) -> ApiResult<()> {
    let d = &current.def;
    if d.kind != TargetKind::Docker || d.ephemeral || current.state != "stopped" {
        return Ok(());
    }
    let mut after = now.cloned().unwrap_or(Value::Null);
    match (after.as_object_mut(), patch) {
        (Some(a), Some(Value::Object(b))) => merge(a, b),
        (_, p) => after = p.cloned().unwrap_or(Value::Null),
    }
    let empty = |v: &Value| v.is_null() || v.as_object().is_some_and(serde_json::Map::is_empty);
    if after == now.cloned().unwrap_or(Value::Null) || (empty(&after) && now.is_none_or(empty)) {
        return Ok(());
    }
    Err(ApiError::conflict(format!(
        "target `{}` has a stopped container: Docker sets the limits when it makes the \
         container, so `up` would start it with the old limits. Remove the container first \
         (`POST /api/targets/{}/down` with `{{\"destroy\": true}}`), then change `limits`",
        d.name, d.name
    ))
    .with_details(json!({"state": current.state, "fields": ["limits"]})))
}

/// The fields that a PATCH may change while the target runs.
const RUNNING_PATCHABLE: [&str; 2] = ["keep", "snapshot_before_task"];

/// The target, or one of its lab instances, runs.
fn is_running(r: &TargetRow) -> bool {
    r.state == "running" || r.runtime.instances.values().any(|i| i.state == "running")
}

/// Delete an API target.
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<TargetRow> {
    let current = get(t, name)?;
    refuse_file(&current)?;
    super::reservations::guard_identity(t, "target", name)?;
    invalidate_root_plan(t, &current.def, None)?;
    t.execute("DELETE FROM targets WHERE name = ?1", [name])?;
    Ok(current)
}

/// Drop only this target's plans. The remote directories remain untouched.
fn invalidate_root_plan(t: &mut Tx<'_>, old: &TargetDef, new: Option<&TargetDef>) -> ApiResult<()> {
    if let Some(new) = new
        && crate::target_roots::same_location(&serde_json::to_string(old)?, new)
    {
        return Ok(());
    }
    t.execute(
        "DELETE FROM agent_target_roots WHERE target=?1",
        [&old.name],
    )?;
    t.execute(
        "DELETE FROM agent_target_root_warnings WHERE target=?1",
        [&old.name],
    )?;
    Ok(())
}

fn guard_reserved_update(conn: &Connection, old: &TargetDef, new: &TargetDef) -> ApiResult<()> {
    let mut old_value = serde_json::to_value(old)?;
    let mut new_value = serde_json::to_value(new)?;
    for value in [&mut old_value, &mut new_value] {
        if let Some(object) = value.as_object_mut() {
            for field in ["limits", "keep", "snapshot_before_task"] {
                object.remove(field);
            }
        }
    }
    if old_value != new_value {
        super::reservations::guard_identity(conn, "target", &old.name)?;
    }
    Ok(())
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

/// Remove snapshot names from a target's `snapshots`. Returns the names
/// that were there.
pub fn remove_snapshots(t: &mut Tx<'_>, name: &str, gone: &[String]) -> ApiResult<Vec<String>> {
    let current = get(t, name)?;
    let (removed, kept): (Vec<String>, Vec<String>) = current
        .snapshots
        .into_iter()
        .partition(|s| gone.contains(s));
    if !removed.is_empty() {
        t.execute(
            "UPDATE targets SET snapshots = ?2 WHERE name = ?1",
            params![name, json!(kept).to_string()],
        )?;
    }
    Ok(removed)
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
        // A file is not refused for its credential: a missing secret must
        // not drop a declared target at a start. The reload reports it as
        // a warning, and the target view shows it.
        if let Some(problem) = credential_problem(t, def)? {
            tracing::warn!(target = %def.name, "targets/{}.toml: {problem}", def.name);
            errors.push(format!("targets/{}.toml: warning: {problem}", def.name));
        }
        match find(t, &def.name)? {
            None => {
                // A file is the operator's: it can name a global secret
                // with no grant (PLAN §16b.6a).
                insert(t, def, SOURCE_FILE, true)?;
                changes.added.push(def.name.clone());
            }
            Some(r) if r.def != *def || r.source != SOURCE_FILE => {
                if let Err(e) = guard_reserved_update(t, &r.def, def) {
                    errors.push(format!("targets/{}.toml: {}", def.name, e.message));
                    continue;
                }
                invalidate_root_plan(t, &r.def, Some(def))?;
                t.execute(
                    "UPDATE targets SET project = ?2, kind = ?3, def = ?4, source = 'file',
                                        updated_at = ?5, credential_admin = 1
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
        if let Err(e) = super::reservations::guard_identity(t, "target", &name) {
            errors.push(format!("targets/{name}.toml: {}", e.message));
            continue;
        }
        let current = get(t, &name)?;
        invalidate_root_plan(t, &current.def, None)?;
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

// ---------------------------------------------------------- credentials

/// The secret kind that the `credential` of a target kind names
/// (PLAN §16b.6a): an `ssh_key` for `ssh`, a `login` for `vnc` and `rdp`.
/// `None`: the daemon has no rule for the credential of this kind.
pub fn credential_kind(k: TargetKind) -> Option<Kind> {
    match k {
        TargetKind::Ssh | TargetKind::Exedev => Some(Kind::SshKey),
        TargetKind::Vnc | TargetKind::Rdp => Some(Kind::Login),
        _ => None,
    }
}

/// The secret that the `credential` of `def` names, as the executor finds
/// it: for a project target, the project's secret first, then the global
/// one; for a global target, the global one only.
pub fn credential_secret(conn: &Connection, def: &TargetDef) -> ApiResult<Option<SecretInfo>> {
    let Some(name) = &def.credential else {
        return Ok(None);
    };
    if let Some(p) = def.project.as_deref().filter(|p| *p != GLOBAL)
        && let Some(s) = secrets::get(conn, p, name)?
    {
        return Ok(Some(s));
    }
    secrets::get(conn, GLOBAL, name)
}

/// Why the `credential` of `def` does not work, or `None` when it works
/// (or when the kind has no rule).
pub fn credential_problem(conn: &Connection, def: &TargetDef) -> ApiResult<Option<String>> {
    let (Some(name), Some(want)) = (&def.credential, credential_kind(def.kind)) else {
        return Ok(None);
    };
    let k = kind_str(def.kind);
    let Some(s) = credential_secret(conn, def)? else {
        let wher = match &def.project {
            Some(p) => format!("project `{p}` or global"),
            None => "global".to_owned(),
        };
        let mut m = format!("credential `{name}`: there is no secret `{name}` ({wher})");
        if def.project.is_none() {
            let others: Vec<String> = secrets::list(conn, None)?
                .into_iter()
                .filter(|s| s.name == *name && s.scope != GLOBAL)
                .map(|s| format!("`{}`", s.scope))
                .collect();
            if !others.is_empty() {
                m.push_str(&format!(
                    "; a global target can use a global secret only, and `{name}` is a secret \
                     of project {}",
                    others.join(", ")
                ));
            }
        }
        if want == Kind::SshKey {
            m.push_str(&format!(
                "; {k} targets need kind ssh_key: store one with `terrarium secret set {name} \
                 --kind ssh_key`, or make one with `terrarium secret generate {name}`"
            ));
        } else {
            m.push_str(&format!(
                "; {k} targets need kind login: store one with `terrarium secret set {name} \
                 --kind login`"
            ));
        }
        return Ok(Some(m));
    };
    if s.kind != want {
        return Ok(Some(format!(
            "credential `{name}`: secret `{}/{name}` is of kind {}; {k} targets need kind {want}",
            s.scope, s.kind
        )));
    }
    Ok(None)
}

/// The save-time check of the `credential` of a target (PLAN §16b.6a):
/// `422 invalid` when it does not name a usable secret. `before` is the
/// stored definition: a change that keeps the kind, the project, and the
/// credential passes, so an old target with a bad credential can still
/// take other changes (its view shows the warning).
pub fn check_credential(
    conn: &Connection,
    def: &TargetDef,
    before: Option<&TargetDef>,
) -> ApiResult<()> {
    if before.is_some_and(|b| {
        b.kind == def.kind && b.project == def.project && b.credential == def.credential
    }) {
        return Ok(());
    }
    match credential_problem(conn, def)? {
        Some(m) => Err(ApiError::invalid(m)),
        None => Ok(()),
    }
}

/// The login of a target, as the target view shows it: the secret that
/// `credential` names, and the fingerprint of an SSH key. Never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialView {
    /// The secret name (the `credential` field).
    pub name: String,
    /// `global` or the project of the secret; `null` when it is missing.
    pub scope: Option<String>,
    /// The kind of the secret; `null` when it is missing.
    pub kind: Option<Kind>,
    /// An `ssh_key`: the SHA-256 fingerprint of its public key
    /// (`SHA256:...`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Why the credential does not work, when it does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The [`CredentialView`] of a target, or `None` when it has no
/// `credential`.
pub fn credential_view(conn: &Connection, def: &TargetDef) -> ApiResult<Option<CredentialView>> {
    let Some(name) = &def.credential else {
        return Ok(None);
    };
    let s = credential_secret(conn, def)?;
    let fingerprint = s
        .as_ref()
        .filter(|s| s.kind == Kind::SshKey)
        .and_then(secrets::ssh::fingerprint_of);
    // A placeholder (PLAN §16b.5) passes the save check, so that a config
    // import can bring the target and the name of its login together; the
    // view reports that it has no value yet.
    // A project target that a project_admin set, and a global secret with
    // no grant to the project: each use is refused (PLAN §16b.6a).
    let refused = match &def.project {
        Some(p) => secrets::global_use_problem(conn, p, name, credential_admin(conn, &def.name)?)?,
        None => None,
    };
    let warning = match credential_problem(conn, def)? {
        Some(w) => Some(w),
        None => refused.or_else(|| {
            s.as_ref()
                .filter(|s| !s.has_value)
                .map(|s| secrets::no_value(&s.scope, &s.name).message)
        }),
    };
    Ok(Some(CredentialView {
        name: name.clone(),
        scope: s.as_ref().map(|s| s.scope.clone()),
        kind: s.as_ref().map(|s| s.kind),
        fingerprint,
        warning,
    }))
}

/// The targets that use the secret `scope/name` as their `credential`:
/// for a project secret, the targets of that project; for a global
/// secret, the global targets and the project targets whose project has
/// no secret of that name.
pub fn targets_using_secret(conn: &Connection, scope: &str, name: &str) -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    for r in list(conn)? {
        if r.def.credential.as_deref() != Some(name) {
            continue;
        }
        let uses = match r.def.project.as_deref() {
            None => scope == GLOBAL,
            Some(p) if scope == GLOBAL => secrets::get(conn, p, name)?.is_none(),
            Some(p) => p == scope,
        };
        if uses {
            out.push(r.def.name);
        }
    }
    Ok(out)
}

/// The hook of a secret delete (PLAN §16b.6a): `409 conflict` that names
/// the targets which use the secret, unless `force`. With `force`, the
/// answer is the targets that keep a dangling `credential` (their view
/// shows a warning).
pub fn guard_secret_delete(
    conn: &Connection,
    scope: &str,
    name: &str,
    force: bool,
) -> ApiResult<Vec<String>> {
    let users = targets_using_secret(conn, scope, name)?;
    if users.is_empty() || force {
        return Ok(users);
    }
    Err(ApiError::conflict(format!(
        "secret `{scope}/{name}` is the credential of target {}; change the target first, or \
         delete with `?force=true` (the target then keeps a name with no secret)",
        users
            .iter()
            .map(|u| format!("`{u}`"))
            .collect::<Vec<_>>()
            .join(", "),
    ))
    .with_details(json!({"targets": users})))
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

    #[tokio::test]
    async fn workspace_sync_updates_only_when_not_running() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        db.write("operator", |t| {
            create(t, &def("kind='ssh'\naddress='u@h'", "sync"))?;
            update(t, "sync", &json!({"sync_workspace": true}))?;
            assert!(get(t, "sync")?.def.sync_workspace);
            set_state(t, "sync", "running", None)?;
            assert!(update(t, "sync", &json!({"sync_workspace": false})).is_err());
            assert!(get(t, "sync")?.def.sync_workspace);
            set_state(t, "sync", "stopped", None)?;
            update(t, "sync", &json!({"sync_workspace": false}))?;
            assert!(!get(t, "sync")?.def.sync_workspace);
            set_state(t, "sync", "running", None)?;
            assert!(update(t, "sync", &json!({"sync_workspace": true})).is_err());
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn root_plans_follow_target_identity_and_removal() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        db.write("operator", |t| {
            let a = def("kind='ssh'\naddress='u@h'\nwork_root='/old'", "a");
            create(t, &a)?;
            create(t, &def("kind='ssh'\naddress='u@other'", "b"))?;
            let seed = |t: &mut Tx<'_>, target: &str| -> ApiResult<()> {
                t.execute("INSERT INTO agent_target_roots (agent,project,name,target,definition,base) VALUES ('holder','p','holder',?1,'{}','/fallback')", [target])?;
                t.execute("INSERT INTO agent_target_root_warnings (agent,target) VALUES ('holder',?1)", [target])?;
                Ok(())
            };
            let count = |t: &Tx<'_>, target: &str| -> i64 {
                t.query_row("SELECT count(*) FROM agent_target_roots WHERE target=?1", [target], |r| r.get(0)).unwrap()
            };
            seed(t, "a")?;
            seed(t, "b")?;
            update(t, "a", &json!({"keep":true}))?;
            assert_eq!(count(t, "a"), 1);
            update(t, "a", &json!({"work_root":"/new"}))?;
            assert_eq!(count(t, "a"), 0);
            assert_eq!(count(t, "b"), 1);
            let warnings: i64 = t.query_row("SELECT count(*) FROM agent_target_root_warnings WHERE target='a'", [], |r| r.get(0))?;
            assert_eq!(warnings, 0);
            seed(t, "a")?;
            update(t, "a", &json!({"address":"u@new"}))?;
            assert_eq!(count(t, "a"), 0);
            seed(t, "a")?;
            delete(t, "a")?;
            create(t, &a)?;
            assert_eq!(count(t, "a"), 0);
            seed(t, "a")?;
            let mut changed = a.clone();
            changed.work_root = Some("/file-root".into());
            sync_files(t, &[changed.clone()])?;
            assert_eq!(count(t, "a"), 0);
            seed(t, "a")?;
            sync_files(t, &[changed])?;
            assert_eq!(count(t, "a"), 1);
            sync_files(t, &[])?;
            assert_eq!(count(t, "a"), 0);
            assert_eq!(count(t, "b"), 1);
            Ok(())
        }).await.unwrap();
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
