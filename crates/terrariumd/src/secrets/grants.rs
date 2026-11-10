//! Grants: which agent gets which secret (PLAN §16b.3, §16b.4).
//!
//! A row of `secret_grants` gives one secret to one grantee: an agent
//! (`agent:<ulid>`) or a whole project (`project:<name>`). A project grant
//! applies to each agent of the project, also to the agents that start
//! later.
//!
//! | Level | The agent gets |
//! |---|---|
//! | `use` | the brokers: the git credential helper and the ssh-agent socket |
//! | `run` | `use`, and the value in `terrarium run -e KEY=@secret:<name>` on the named targets |
//! | `read` | `run` on any target, the env vars at start, and `terrarium secret get` |
//!
//! A `run` grant names one or more targets (TER-142). The daemon resolves
//! the value when a run starts, only on one of them, and puts it into the
//! env of the remote process. It gives no env var at the start and no
//! `secret get`.
//!
//! The effective grants of an agent ([`effective`]) are its own grants and
//! the grants of its project, one entry for each secret, with the highest
//! level. A project secret goes only to its own project: the store checks
//! it when it writes a grant, and again when it reads the grants.
//!
//! Removing an agent or a project removes its grants ([`on_agent_deleted`],
//! [`on_project_deleted`]). Removing a secret removes its grants (the
//! foreign key cascades).

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use super::{GLOBAL, Kind, MasterKey, Opened, SecretInfo};
use crate::error::{ApiError, ApiResult};

/// The env var that a granted `github_token` with no `env` name sets.
pub const GH_TOKEN: &str = "GH_TOKEN";

/// The level of a grant: `Use` < `Run` < `Read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// The brokers only. The agent cannot see the value.
    Use,
    /// `Use`, and the value in the env of `terrarium run` on the targets
    /// of the grant (TER-142). No env var at the start, no `secret get`.
    Run,
    /// The brokers, the env vars, and `terrarium secret get`.
    Read,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Run => "run",
            Self::Read => "read",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "use" => Some(Self::Use),
            "run" => Some(Self::Run),
            "read" => Some(Self::Read),
            _ => None,
        }
    }

    /// The level of a grant that names no level: `use` for a kind that a
    /// broker serves (`token`, `github_token`, `ssh_key`), `read` for the
    /// other kinds, which no broker serves (a `use` grant of them gives
    /// nothing).
    pub fn default_for(kind: Kind) -> Self {
        match kind {
            Kind::Token | Kind::GithubToken | Kind::SshKey => Self::Use,
            Kind::ApiKey | Kind::Login | Kind::Generic => Self::Read,
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A grantee, resolved: an agent that exists, or a project that exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grantee {
    Agent {
        id: String,
        project: String,
        name: String,
    },
    Project(String),
    /// Merge recipes only. Never included in an agent's effective grants.
    Delivery(String),
}

impl Grantee {
    /// The value of the `grantee` column.
    pub fn key(&self) -> String {
        match self {
            Self::Agent { id, .. } => format!("agent:{id}"),
            Self::Project(p) => format!("project:{p}"),
            Self::Delivery(p) => format!("delivery:{p}"),
        }
    }

    /// The project of the grantee.
    pub fn project(&self) -> &str {
        match self {
            Self::Agent { project, .. } | Self::Project(project) | Self::Delivery(project) => {
                project
            }
        }
    }

    /// The grantee for a person: `agent:<project>/<name>` or
    /// `project:<name>`.
    pub fn label(&self) -> String {
        match self {
            Self::Agent { project, name, .. } => format!("agent:{project}/{name}"),
            Self::Project(p) => format!("project:{p}"),
            Self::Delivery(p) => format!("delivery:{p}"),
        }
    }
}

/// Resolve a grantee: `agent:<project>/<name>`, `agent:<ulid>`, or
/// `project:<name>`.
pub fn parse_grantee(conn: &Connection, s: &str) -> ApiResult<Grantee> {
    let bad = || {
        ApiError::invalid(format!(
            "grantee `{s}` is not `agent:<project>/<name>`, `agent:<id>`, `project:<name>`, or `delivery:<name>`"
        ))
    };
    if let Some(key) = s.strip_prefix("agent:") {
        if key.is_empty() || (!key.contains('/') && key.parse::<terrarium_core::AgentId>().is_err())
        {
            return Err(bad());
        }
        let v = crate::store::agents::find(conn, key, None)
            .map_err(|_| ApiError::not_found(format!("no agent `{key}`")))?;
        return Ok(Grantee::Agent {
            id: v.agent.id.to_string(),
            project: v.agent.project,
            name: v.agent.name,
        });
    }
    if let Some(p) = s
        .strip_prefix("project:")
        .or_else(|| s.strip_prefix("delivery:"))
    {
        if p.is_empty() || p == GLOBAL {
            return Err(bad());
        }
        let n: i64 = conn.query_row("SELECT count(*) FROM projects WHERE name = ?1", [p], |r| {
            r.get(0)
        })?;
        if n == 0 {
            return Err(ApiError::not_found(format!("no project `{p}`")));
        }
        return Ok(if s.starts_with("delivery:") {
            Grantee::Delivery(p.to_owned())
        } else {
            Grantee::Project(p.to_owned())
        });
    }
    Err(bad())
}

/// The label of a stored grantee key, for the answers: `agent:<ulid>`
/// becomes `agent:<project>/<name>` while the agent exists.
pub(crate) fn label_of(conn: &Connection, key: &str) -> ApiResult<String> {
    if let Some(id) = key.strip_prefix("agent:") {
        let found: Option<(String, String)> = conn
            .query_row(
                "SELECT project, name FROM agents WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((p, n)) = found {
            return Ok(format!("agent:{p}/{n}"));
        }
    }
    Ok(key.to_owned())
}

/// Check an env var name of a grant (docs/api.md §5.2, "The name rule"):
/// it matches `^[A-Z][A-Z0-9_]{0,127}$`, and it is not in the deny set
/// ([`terrarium_core::env_deny`]: the names, the prefixes, the suffixes,
/// and the harness home vars). The daemon checks each final name (also each
/// `<ENV>_<FIELD>` of a `generic` secret) when it writes a grant, when a
/// config import writes one, and again at each launch.
pub fn check_env_name(name: &str) -> ApiResult<()> {
    let mut cs = name.bytes();
    let shape = matches!(cs.next(), Some(c) if c.is_ascii_uppercase())
        && name.len() <= 128
        && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_');
    if !shape {
        return Err(ApiError::invalid(format!(
            "`{name}` is not a usable env var name: an upper-case letter, then upper-case \
             letters, digits, or `_` (at most 128)"
        )));
    }
    if terrarium_core::env_deny::denied(name) {
        return Err(ApiError::invalid(format!(
            "`{name}` is an env var that the sandbox, the loader, a shell, a language runtime, \
             a proxy, a library that ssh loads, or a harness reads or sets; choose another name"
        )));
    }
    Ok(())
}

/// The final env var names of a grant with the base name `env` of
/// `secret`: `env` itself, or `<ENV>_<FIELD>` for each secret field of a
/// `generic` secret with more than one.
fn final_names(env: &str, kind: Kind, secret_fields: &[String]) -> Vec<String> {
    match main_field(kind, secret_fields) {
        Some(_) => vec![env.to_owned()],
        None => secret_fields.iter().map(|f| generic_var(env, f)).collect(),
    }
}

/// One grant of one secret, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    /// `global` or a project.
    pub scope: String,
    /// The name of the secret.
    pub name: String,
    /// The stored grantee: `agent:<ulid>` or `project:<name>`.
    pub grantee: String,
    /// `agent:<project>/<name>` or `project:<name>`.
    pub grantee_label: String,
    pub level: Level,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// The targets of a `run` grant, sorted. Empty for the other levels.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    pub created_by: String,
    pub created_at: String,
}

/// At most this many targets on one `run` grant.
pub const MAX_RUN_TARGETS: usize = 32;

/// Check the targets of a grant at `level`: a `run` grant needs 1 to
/// [`MAX_RUN_TARGETS`] target names, the other levels none. The answer is
/// sorted with no repeat. This checks the shape only; the caller checks
/// that each target exists ([`check_targets_exist`]).
pub fn check_run_targets(level: Level, targets: &[String]) -> ApiResult<Vec<String>> {
    if level != Level::Run {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        return Err(ApiError::invalid(format!(
            "`targets` names the targets of a `run` grant; a `{level}` grant takes none"
        )));
    }
    let mut out: Vec<String> = targets.iter().map(|t| t.trim().to_owned()).collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        return Err(ApiError::invalid(
            "a `run` grant names the targets where the daemon may inject the value: give one \
             or more in `targets`",
        ));
    }
    if out.len() > MAX_RUN_TARGETS {
        return Err(ApiError::invalid(format!(
            "a `run` grant names at most {MAX_RUN_TARGETS} targets; it got {}",
            out.len()
        )));
    }
    for t in &out {
        crate::store::check_name("target", t)?;
    }
    Ok(out)
}

/// Each of `targets` exists, and a project target belongs to the project
/// of the grantee.
pub fn check_targets_exist(conn: &Connection, project: &str, targets: &[String]) -> ApiResult<()> {
    for name in targets {
        let row = crate::store::targets::find(conn, name)?
            .ok_or_else(|| ApiError::not_found(format!("no target `{name}`")))?;
        if let Some(p) = &row.def.project
            && p != project
        {
            return Err(ApiError::forbidden(format!(
                "target `{name}` belongs to project `{p}`; a grant to an agent of `{project}` \
                 cannot name it"
            )));
        }
    }
    Ok(())
}

/// The JSON of the `targets` column: NULL with no target.
fn targets_column(targets: &[String]) -> Option<String> {
    (!targets.is_empty()).then(|| serde_json::to_string(targets).unwrap_or_default())
}

/// Read the `targets` column.
fn targets_of(v: Option<String>) -> Vec<String> {
    v.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Who may hold a grant of `secret`: a global secret goes to any agent or
/// project; a project secret goes to its project or an agent of it.
fn check_reach(secret: &SecretInfo, grantee: &Grantee) -> ApiResult<()> {
    if secret.scope == GLOBAL || secret.scope == grantee.project() {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "secret `{}/{}` is a secret of project `{}`; it goes only to that project or its agents, \
         not to {}",
        secret.scope,
        secret.name,
        secret.scope,
        grantee.label()
    )))
}

/// The fields of a new grant.
pub struct NewGrant<'a> {
    pub grantee: &'a Grantee,
    /// `None`: [`Level::default_for`] the kind.
    pub level: Option<Level>,
    pub env: Option<&'a str>,
    /// The targets of a `run` grant ([`check_run_targets`]).
    pub targets: &'a [String],
    /// The actor label.
    pub by: &'a str,
}

/// Write a grant (a grant to the same grantee is replaced). The answer is
/// the grant and `true` when it is new.
pub fn put(conn: &Connection, secret: &SecretInfo, g: &NewGrant<'_>) -> ApiResult<(Grant, bool)> {
    check_reach(secret, g.grantee)?;
    let level = g.level.unwrap_or_else(|| Level::default_for(secret.kind));
    if matches!(g.grantee, Grantee::Delivery(_)) && (level != Level::Run || g.env.is_some()) {
        return Err(ApiError::invalid(
            "a delivery grant requires level run, named targets, and no startup env",
        ));
    }
    if let Some(env) = g.env {
        if level != Level::Read {
            return Err(ApiError::invalid(
                "an env var puts the value into the agent's environment, where the agent can \
                 print it: `env` needs the level `read`",
            ));
        }
        check_env_name(env)?;
        for name in final_names(env, secret.kind, &secret.secret_fields) {
            check_env_name(&name)?;
        }
    }
    let targets = check_run_targets(level, g.targets)?;
    let key = g.grantee.key();
    let now = crate::db::now();
    let tcol = targets_column(&targets);
    let n = conn.execute(
        "UPDATE secret_grants SET level = ?3, env = ?4, created_by = ?5, created_at = ?6,
                                  targets = ?7
         WHERE secret_id = ?1 AND grantee = ?2",
        params![secret.id, key, level.as_str(), g.env, g.by, now, tcol],
    )?;
    if n == 0 {
        conn.execute(
            "INSERT INTO secret_grants
                 (secret_id, grantee, level, env, created_by, created_at, targets)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![secret.id, key, level.as_str(), g.env, g.by, now, tcol],
        )?;
    }
    Ok((
        Grant {
            scope: secret.scope.clone(),
            name: secret.name.clone(),
            grantee: key,
            grantee_label: g.grantee.label(),
            level,
            env: g.env.map(str::to_owned),
            targets,
            created_by: g.by.to_owned(),
            created_at: now,
        },
        n == 0,
    ))
}

/// Remove the grant of `secret` to `grantee` (a stored key). False when
/// there is none.
pub fn delete(conn: &Connection, secret: &SecretInfo, grantee: &str) -> ApiResult<bool> {
    let n = conn.execute(
        "DELETE FROM secret_grants WHERE secret_id = ?1 AND grantee = ?2",
        params![secret.id, grantee],
    )?;
    Ok(n > 0)
}

/// The grants of one secret, by grantee.
pub fn list(conn: &Connection, secret: &SecretInfo) -> ApiResult<Vec<Grant>> {
    let mut stmt = conn.prepare(
        "SELECT grantee, level, env, created_by, created_at, targets FROM secret_grants
         WHERE secret_id = ?1 ORDER BY grantee",
    )?;
    let rows = stmt
        .query_map([&secret.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(grantee, level, env, created_by, created_at, targets)| {
            let level = Level::parse(&level).unwrap_or(Level::Use);
            Ok(Grant {
                scope: secret.scope.clone(),
                name: secret.name.clone(),
                grantee_label: label_of(conn, &grantee)?,
                grantee,
                level,
                env,
                targets: if level == Level::Run {
                    targets_of(targets)
                } else {
                    Vec::new()
                },
                created_by,
                created_at,
            })
        })
        .collect()
}

/// The direct grants of one grantee (a stored key such as `agent:<ulid>`),
/// each with its secret, in one query (TER-391): no scan over every
/// secret of every scope. Only secrets that the grantee's project
/// reaches count (a global one, or one of that project), as in
/// [`effective`]. Project grants are not included: this is one grantee.
pub fn list_by_grantee(conn: &Connection, grantee: &str, project: &str) -> ApiResult<Vec<Grant>> {
    let mut stmt = conn.prepare(
        "SELECT s.scope, s.name, g.level, g.env, g.targets, g.created_by, g.created_at
         FROM secret_grants g JOIN secrets s ON s.id = g.secret_id
         WHERE g.grantee = ?1 AND (s.scope = 'global' OR s.scope = ?2)
         ORDER BY (s.scope = 'global'), s.name, s.scope",
    )?;
    let rows = stmt
        .query_map(params![grantee, project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let label = label_of(conn, grantee)?;
    rows.into_iter()
        .map(
            |(scope, name, level, env, targets, created_by, created_at)| {
                let level = Level::parse(&level).unwrap_or(Level::Use);
                Ok(Grant {
                    grantee_label: label.clone(),
                    grantee: grantee.to_owned(),
                    level,
                    env,
                    targets: if level == Level::Run {
                        targets_of(targets)
                    } else {
                        Vec::new()
                    },
                    scope,
                    name,
                    created_by,
                    created_at,
                })
            },
        )
        .collect()
}

/// The agent was removed: its grants go.
pub fn on_agent_deleted(conn: &Connection, id: &str) -> ApiResult<usize> {
    Ok(conn.execute(
        "DELETE FROM secret_grants WHERE grantee = ?1",
        [format!("agent:{id}")],
    )?)
}

/// The project was removed: the grants to the project go. (The grants of
/// its secrets go with the secrets; its agents were removed first.)
pub fn on_project_deleted(conn: &Connection, name: &str) -> ApiResult<usize> {
    Ok(conn.execute(
        "DELETE FROM secret_grants WHERE grantee IN (?1, ?2)",
        [format!("project:{name}"), format!("delivery:{name}")],
    )?)
}

/// A secret that an agent may use: its own grant and its project's grant,
/// merged. `Debug` and the JSON hold no value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Effective {
    #[serde(skip)]
    pub id: String,
    pub scope: String,
    pub name: String,
    pub kind: Kind,
    /// The highest level of the grants.
    pub level: Level,
    /// The env var of a `read` grant: the agent's own grant first, then
    /// the project's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// With the level `run`: the targets where `terrarium run` may get the
    /// value, the union of the `run` grants. Empty for `use` and `read`
    /// (a `read` grant has no target limit).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// The plain fields.
    pub fields: Map<String, Value>,
    /// The names of the secret fields.
    pub secret_fields: Vec<String>,
    /// The secret holds a value. False: a placeholder (PLAN §16b.5); the
    /// grant gives nothing until the operator sets the value.
    pub has_value: bool,
    /// The grants that give it: `agent` and/or `project`.
    pub via: Vec<String>,
}

/// The effective grants of the agent `agent_id` of `project`: one entry
/// for each secret, the project secrets first, then by name.
pub fn effective(conn: &Connection, agent_id: &str, project: &str) -> ApiResult<Vec<Effective>> {
    effective_keys(
        conn,
        &format!("agent:{agent_id}"),
        &format!("project:{project}"),
        project,
    )
}

/// Grants available only to merge recipes, never to workers or brokers.
pub fn delivery_effective(conn: &Connection, project: &str) -> ApiResult<Vec<Effective>> {
    let principal = format!("delivery:{project}");
    effective_keys(conn, &principal, &principal, project)
}

fn effective_keys(
    conn: &Connection,
    agent_key: &str,
    project_key: &str,
    project: &str,
) -> ApiResult<Vec<Effective>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols}, g.grantee, g.level, g.env, g.targets
         FROM secret_grants g JOIN secrets s ON s.id = g.secret_id
         WHERE g.grantee IN (?1, ?2) AND (s.scope = 'global' OR s.scope = ?3)",
        cols = super::COLS
            .split(", ")
            .map(|c| format!("s.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))?;
    let rows = stmt
        .query_map(params![agent_key, project_key, project], |r| {
            let info = super::row_info(r)?;
            let grantee: String = r.get(10)?;
            let level: String = r.get(11)?;
            let env: Option<String> = r.get(12)?;
            let targets: Option<String> = r.get(13)?;
            Ok((grantee, level, env, targets, info))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_id: BTreeMap<String, (Effective, Option<String>, Option<String>)> = BTreeMap::new();
    for (grantee, level, env, targets, info) in rows {
        let level = Level::parse(&level).unwrap_or(Level::Use);
        let own = grantee == agent_key;
        let e = by_id.entry(info.id.clone()).or_insert_with(|| {
            (
                Effective {
                    id: info.id.clone(),
                    scope: info.scope.clone(),
                    name: info.name.clone(),
                    kind: info.kind,
                    level,
                    env: None,
                    targets: Vec::new(),
                    fields: info.fields.clone(),
                    secret_fields: info.secret_fields.clone(),
                    has_value: info.has_value,
                    via: Vec::new(),
                },
                None,
                None,
            )
        });
        e.0.level = e.0.level.max(level);
        if level == Level::Run {
            e.0.targets.extend(targets_of(targets));
        }
        e.0.via
            .push(if own { "agent" } else { "project" }.to_owned());
        // An env name counts only on a `read` grant.
        let env = env.filter(|_| level == Level::Read);
        if own {
            e.1 = env;
        } else {
            e.2 = env;
        }
    }
    let mut out: Vec<Effective> = by_id
        .into_values()
        .map(|(mut e, own_env, project_env)| {
            e.env = own_env.or(project_env);
            e.via.sort();
            // The targets limit only a `run` level; a `read` grant has none.
            if e.level == Level::Run {
                e.targets.sort();
                e.targets.dedup();
            } else {
                e.targets.clear();
            }
            e
        })
        .collect();
    out.sort_by(|a, b| {
        (a.scope == GLOBAL, &a.name, &a.scope).cmp(&(b.scope == GLOBAL, &b.name, &b.scope))
    });
    Ok(out)
}

/// The granted secret named `name` for an agent: the project secret
/// first, then the global one. Only a granted secret counts, so a project
/// secret that the agent has no grant for does not hide a granted global
/// one.
pub fn find(
    conn: &Connection,
    agent_id: &str,
    project: &str,
    name: &str,
) -> ApiResult<Option<Effective>> {
    let all = effective(conn, agent_id, project)?;
    let mut named = all.into_iter().filter(|e| e.name == name);
    let first = named.next();
    // `effective` sorts the project secrets first.
    Ok(first)
}

/// The main secret field of a kind: what `terrarium secret get` prints
/// and what an env var holds. `generic`: its only secret field.
pub fn main_field(info_kind: Kind, secret_fields: &[String]) -> Option<String> {
    match info_kind {
        Kind::ApiKey | Kind::Token | Kind::GithubToken => Some("value".into()),
        Kind::Login => Some("password".into()),
        Kind::SshKey => Some("private_key".into()),
        Kind::Generic => match secret_fields {
            [one] => Some(one.clone()),
            _ => None,
        },
    }
}

/// The env var name of a field of a `generic` secret with more than one
/// secret field: `<ENV>_<FIELD>`, upper case, with `_` for each other
/// character.
fn generic_var(env: &str, field: &str) -> String {
    let f: String = field
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{env}_{f}")
}

/// What an agent's start gets from its grants. `Debug` shows no value.
#[derive(Default)]
pub struct LaunchSecrets {
    /// The env vars of the `read` grants, by name. The values are secret:
    /// never log them.
    pub env: BTreeMap<String, Zeroizing<String>>,
    /// The git hosts of the granted `token` and `github_token` secrets: the
    /// credential helper answers for them.
    pub git_hosts: Vec<String>,
    /// The agent has at least one `ssh_key` grant: it gets an ssh-agent
    /// socket.
    pub ssh_keys: bool,
    /// Grants that give nothing at this start (a secret with no value, a
    /// clash of env names). No value.
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for LaunchSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSecrets")
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("git_hosts", &self.git_hosts)
            .field("ssh_keys", &self.ssh_keys)
            .field("warnings", &self.warnings)
            .finish()
    }
}

/// The git hosts of a granted secret: `hosts` of a `token`, `host` of a
/// `github_token`.
pub fn git_hosts(e: &Effective) -> Vec<String> {
    match e.kind {
        Kind::Token => e
            .fields
            .get("hosts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default(),
        Kind::GithubToken => vec![
            e.fields
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or(super::kinds::DEFAULT_GITHUB_HOST)
                .to_ascii_lowercase(),
        ],
        _ => Vec::new(),
    }
}

fn open_effective(conn: &Connection, key: &MasterKey, e: &Effective) -> ApiResult<Option<Opened>> {
    super::open(conn, key, &e.scope, &e.name)
}

/// What the start of the agent `agent_id` of `project` gets from its
/// grants (PLAN §16b.4): the env vars of the `read` grants with an `env`
/// name (a `github_token` with none sets `GH_TOKEN`), the git hosts, and
/// whether it gets an ssh-agent socket. A secret in `use_only` (the
/// token of a repo that goes through the git proxy,
/// `crate::git_proxy::use_only_secrets`) gives no env var and no git host.
pub fn launch(
    conn: &Connection,
    key: &MasterKey,
    agent_id: &str,
    project: &str,
    use_only: &std::collections::HashSet<(String, String)>,
) -> ApiResult<LaunchSecrets> {
    let mut out = LaunchSecrets::default();
    for e in effective(conn, agent_id, project)? {
        if use_only.contains(&(e.scope.clone(), e.name.clone())) {
            out.warnings.push(format!(
                "secret `{}/{}` is the token of a repo that goes through the git proxy (use \
                 only): its grant gives no env var and no git credential",
                e.scope, e.name
            ));
            continue;
        }
        if !e.has_value {
            // A placeholder from a config import: no broker, no env var.
            out.warnings.push(format!(
                "secret `{}/{}` has no value yet (a placeholder from a config import); its \
                 grant gives nothing at this start",
                e.scope, e.name
            ));
            continue;
        }
        out.git_hosts.extend(git_hosts(&e));
        if e.kind == Kind::SshKey {
            out.ssh_keys = true;
        }
        if e.level != Level::Read {
            continue;
        }
        let env = match (&e.env, e.kind) {
            (Some(v), _) => v.clone(),
            (None, Kind::GithubToken) => GH_TOKEN.to_owned(),
            (None, _) => continue,
        };
        let opened = match open_effective(conn, key, &e) {
            Ok(Some(o)) => o,
            Ok(None) => continue,
            Err(err) => {
                out.warnings.push(format!(
                    "secret `{}/{}` does not open, so `{env}` is not set: {}",
                    e.scope, e.name, err.message
                ));
                continue;
            }
        };
        let vars: Vec<(String, Zeroizing<String>)> =
            match main_field(e.kind, &opened.info.secret_fields) {
                Some(f) => opened
                    .values
                    .get(&f)
                    .map(|v| vec![(env.clone(), v.clone())])
                    .unwrap_or_default(),
                None => opened
                    .values
                    .iter()
                    .map(|(f, v)| (generic_var(&env, f), v.clone()))
                    .collect(),
            };
        for (name, value) in vars {
            // The one check of each final name before it goes into the
            // launch env: a grant row from an older daemon or an import
            // can hold a name that the rule refuses now.
            if let Err(err) = check_env_name(&name) {
                out.warnings.push(format!(
                    "secret `{}/{}`: `{name}` is not set: {}",
                    e.scope, e.name, err.message
                ));
                continue;
            }
            if out.env.contains_key(&name) {
                out.warnings.push(format!(
                    "secret `{}/{}`: `{name}` is set by another grant already; this one is not set",
                    e.scope, e.name
                ));
                continue;
            }
            out.env.insert(name, value);
        }
    }
    out.git_hosts.sort();
    out.git_hosts.dedup();
    Ok(out)
}

/// The granted `token` or `github_token` secret for the git host `host`
/// (any level), opened. The project secrets come first. A placeholder
/// with no value does not count.
pub fn token_for_host(
    conn: &Connection,
    key: &MasterKey,
    agent_id: &str,
    project: &str,
    host: &str,
) -> ApiResult<Option<(Effective, Opened)>> {
    let host = host.to_ascii_lowercase();
    for e in effective(conn, agent_id, project)? {
        if !e.has_value || !git_hosts(&e).contains(&host) {
            continue;
        }
        if let Some(o) = open_effective(conn, key, &e)? {
            return Ok(Some((e, o)));
        }
    }
    Ok(None)
}

/// The granted `ssh_key` secrets of an agent (any level) that hold a
/// value. A placeholder gives no key to the ssh-agent.
pub fn ssh_keys(conn: &Connection, agent_id: &str, project: &str) -> ApiResult<Vec<Effective>> {
    Ok(effective(conn, agent_id, project)?
        .into_iter()
        .filter(|e| e.kind == Kind::SshKey && e.has_value)
        .collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use crate::secrets::{NewSecret, PutMode, SecretInput};
    use serde_json::json;

    pub(crate) const AGENT: &str = "01K8AGENT0000000000000000A";
    pub(crate) const OTHER: &str = "01K8AGENT0000000000000000B";

    /// TER-490: the host agent refuses a multi-line docker env value for
    /// each name that a grant cannot set, from the same list
    /// ([`terrarium_core::env_deny`]), plus its own names. The exact names
    /// that the daemon sends multi-line values under
    /// ([`terrarium_host::docker::CLI_ALLOWED_ENV`]) are the one exception
    /// (TER-541): a grant cannot set them, and the fallback lets them
    /// through. Any other `TERRARIUM_*` name is refused.
    #[test]
    fn the_host_agent_refuses_every_name_that_a_grant_refuses() {
        use terrarium_core::env_deny::{DENIED_ENV, DENIED_ENV_PREFIXES, DENIED_ENV_SUFFIXES};
        use terrarium_host::docker::{CLI_ALLOWED_ENV, steers_the_cli};
        let mut names: Vec<String> = DENIED_ENV.iter().map(|n| n.to_string()).collect();
        names.extend(DENIED_ENV_PREFIXES.iter().map(|p| format!("{p}X")));
        names.extend(DENIED_ENV_SUFFIXES.iter().map(|p| format!("X{p}")));
        names.extend(
            terrarium_core::HarnessId::ALL
                .iter()
                .map(|h| h.home_env().to_owned()),
        );
        names.extend(
            [
                "OPENSSL_CONF",
                "OPENSSL_MODULES",
                "OPENSSL_ENGINES",
                "KRB5_CONFIG",
                "KRB5CCNAME",
                "DBUS_SESSION_BUS_ADDRESS",
                "TERRARIUM_GIT_TRAILERS_FILE",
            ]
            .map(String::from),
        );
        for n in &names {
            assert!(check_env_name(n).is_err(), "{n}");
            assert!(steers_the_cli(n), "{n}");
        }
        for n in CLI_ALLOWED_ENV {
            assert!(check_env_name(n).is_err(), "{n}");
            assert!(!steers_the_cli(n), "{n}");
        }
        assert!(check_env_name("DEPLOY_KEY").is_ok());
        assert!(!steers_the_cli("DEPLOY_KEY"));
    }

    pub(crate) fn db() -> Connection {
        let c = crate::secrets::tests::db();
        c.execute(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('q', 'x', 'x')",
            [],
        )
        .unwrap();
        for (id, project, name) in [(AGENT, "p", "a"), (OTHER, "q", "b")] {
            c.execute(
                "INSERT INTO agents (id, project, name, harness, workspace, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'claude', '', 'x', 'x')",
                params![id, project, name],
            )
            .unwrap();
        }
        c
    }

    pub(crate) fn secret(
        c: &Connection,
        k: &MasterKey,
        scope: &str,
        name: &str,
        kind: Kind,
        v: Value,
    ) -> SecretInfo {
        let i: SecretInput = serde_json::from_value(v).unwrap();
        crate::secrets::put(
            c,
            k,
            PutMode::Upsert,
            &NewSecret {
                scope,
                name,
                kind,
                input: &i,
                by: "operator",
                strict: true,
            },
        )
        .unwrap()
        .0
    }

    pub(crate) fn grant(
        c: &Connection,
        s: &SecretInfo,
        grantee: &str,
        level: Option<Level>,
        env: Option<&str>,
    ) -> ApiResult<(Grant, bool)> {
        let g = parse_grantee(c, grantee)?;
        put(
            c,
            s,
            &NewGrant {
                grantee: &g,
                level,
                env,
                targets: &[],
                by: "operator",
            },
        )
    }

    pub(crate) fn grant_run(
        c: &Connection,
        s: &SecretInfo,
        grantee: &str,
        targets: &[&str],
    ) -> ApiResult<(Grant, bool)> {
        let g = parse_grantee(c, grantee)?;
        let targets: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
        put(
            c,
            s,
            &NewGrant {
                grantee: &g,
                level: Some(Level::Run),
                env: None,
                targets: &targets,
                by: "operator",
            },
        )
    }

    #[test]
    fn levels_order_use_run_read() {
        assert!(Level::Use < Level::Run && Level::Run < Level::Read);
        assert_eq!(Level::parse("run"), Some(Level::Run));
        assert_eq!(Level::Run.as_str(), "run");
        assert_eq!(
            serde_json::to_value(Level::Run).unwrap(),
            serde_json::json!("run")
        );
    }

    /// TER-142: a `run` grant names its targets; it gives no env var at
    /// the start; the targets of the agent's and the project's `run`
    /// grants add up, and a `read` grant lifts the limit.
    #[test]
    fn run_grants_name_targets_and_give_no_env() {
        let c = db();
        let k = MasterKey::generate();
        let s = secret(
            &c,
            &k,
            "p",
            "deploy",
            Kind::ApiKey,
            json!({"fields": {"value": "dpl_8sK2mQ7vX4tR9wL3nB6yH1"}}),
        );
        // `run` needs targets; the other levels take none.
        let e = grant_run(&c, &s, "agent:p/a", &[]).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid, "{}", e.message);
        let g = parse_grantee(&c, "agent:p/a").unwrap();
        let e = put(
            &c,
            &s,
            &NewGrant {
                grantee: &g,
                level: Some(Level::Read),
                env: None,
                targets: &["box".to_owned()],
                by: "operator",
            },
        )
        .unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid, "{}", e.message);
        // An env var needs `read`, also on `run`.
        let e = put(
            &c,
            &s,
            &NewGrant {
                grantee: &g,
                level: Some(Level::Run),
                env: Some("DEPLOY"),
                targets: &["box".to_owned()],
                by: "operator",
            },
        )
        .unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid, "{}", e.message);
        assert!(grant_run(&c, &s, "agent:p/a", &["Bad Name"]).is_err());

        let (g, created) = grant_run(&c, &s, "agent:p/a", &["web", "box", "box"]).unwrap();
        assert!(created);
        assert_eq!(g.level, Level::Run);
        assert_eq!(g.targets, ["box", "web"]);
        assert_eq!(list(&c, &s).unwrap()[0].targets, ["box", "web"]);
        grant_run(&c, &s, "project:p", &["ci"]).unwrap();
        let e = find(&c, AGENT, "p", "deploy").unwrap().unwrap();
        assert_eq!(e.level, Level::Run);
        assert_eq!(e.targets, ["box", "ci", "web"]);
        // No env var and no git host at the start.
        let l = launch(&c, &k, AGENT, "p", &Default::default()).unwrap();
        assert!(l.env.is_empty(), "{l:?}");
        // A `read` grant of the project lifts the target limit.
        grant(&c, &s, "project:p", Some(Level::Read), None).unwrap();
        let e = find(&c, AGENT, "p", "deploy").unwrap().unwrap();
        assert_eq!(e.level, Level::Read);
        assert!(e.targets.is_empty());
        // Back to `use`: the targets go from the row.
        grant(&c, &s, "agent:p/a", Some(Level::Use), None).unwrap();
        let row = list(&c, &s)
            .unwrap()
            .into_iter()
            .find(|g| g.grantee == format!("agent:{AGENT}"))
            .unwrap();
        assert!(row.targets.is_empty());
        let raw: Option<String> = c
            .query_row(
                "SELECT targets FROM secret_grants WHERE grantee = ?1",
                [format!("agent:{AGENT}")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, None);
    }

    #[test]
    fn grantees_resolve_and_follow_the_scope_of_the_secret() {
        let c = db();
        let k = MasterKey::generate();
        let g = secret(
            &c,
            &k,
            GLOBAL,
            "g",
            Kind::ApiKey,
            json!({"fields": {"value": "v1"}}),
        );
        let p = secret(
            &c,
            &k,
            "p",
            "s",
            Kind::ApiKey,
            json!({"fields": {"value": "v2"}}),
        );
        assert_eq!(
            parse_grantee(&c, "agent:p/a").unwrap().key(),
            format!("agent:{AGENT}")
        );
        assert_eq!(
            parse_grantee(&c, &format!("agent:{AGENT}"))
                .unwrap()
                .label(),
            "agent:p/a"
        );
        assert_eq!(parse_grantee(&c, "project:q").unwrap().key(), "project:q");
        for bad in [
            "a",
            "agent:",
            "project:",
            "project:global",
            "agent:noslash",
            "x:y",
        ] {
            assert!(parse_grantee(&c, bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_grantee(&c, "project:none").unwrap_err().code,
            crate::error::Code::NotFound
        );
        // A global secret goes anywhere.
        assert!(grant(&c, &g, "agent:q/b", None, None).is_ok());
        assert!(grant(&c, &g, "project:p", None, None).is_ok());
        // A project secret goes to its project only.
        assert!(grant(&c, &p, "agent:p/a", None, None).is_ok());
        assert!(grant(&c, &p, "project:p", None, None).is_ok());
        let e = grant(&c, &p, "agent:q/b", None, None).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Forbidden);
        assert!(grant(&c, &p, "project:q", None, None).is_err());
        let names: Vec<_> = list(&c, &p)
            .unwrap()
            .into_iter()
            .map(|g| g.grantee_label)
            .collect();
        assert_eq!(names, ["agent:p/a", "project:p"]);
    }

    #[test]
    fn env_needs_read_and_a_free_name() {
        let c = db();
        let k = MasterKey::generate();
        let t = secret(
            &c,
            &k,
            GLOBAL,
            "t",
            Kind::Token,
            json!({"fields": {"value": "tok"}}),
        );
        let s = secret(
            &c,
            &k,
            GLOBAL,
            "k",
            Kind::SshKey,
            json!({"fields": {"private_key": crate::secrets::ssh::tests::ed25519_key()}}),
        );
        let e = grant(&c, &t, "agent:p/a", Some(Level::Use), Some("TOK")).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
        // A token defaults to `use`, so an env with no level is refused too.
        assert!(grant(&c, &t, "agent:p/a", None, Some("TOK")).is_err());
        assert!(grant(&c, &t, "agent:p/a", Some(Level::Read), Some("TOK")).is_ok());
        // The ssh_key default is `use`.
        let (g, created) = grant(&c, &s, "agent:p/a", None, None).unwrap();
        assert!(created);
        assert_eq!(g.level, Level::Use);
        let (g, created) = grant(&c, &s, "agent:p/a", Some(Level::Read), None).unwrap();
        assert!(!created);
        assert_eq!(g.level, Level::Read);
        for bad in [
            "PATH",
            "HOME",
            "LD_PRELOAD",
            "TERRARIUM_TOKEN",
            "terrarium_x",
            "1A",
            "A-B",
            "",
            "CLAUDE_CONFIG_DIR",
            "SSH_AUTH_SOCK",
            "GIT_CONFIG_COUNT",
            "LC_ALL",
            "BASH_ENV",
        ] {
            assert!(check_env_name(bad).is_err(), "{bad}");
        }
        for good in ["GH_TOKEN", "STRIPE_API_KEY", "X_1", "DB_URL"] {
            assert!(check_env_name(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn effective_merges_agent_and_project_grants() {
        let c = db();
        let k = MasterKey::generate();
        let g = secret(
            &c,
            &k,
            GLOBAL,
            "x",
            Kind::ApiKey,
            json!({"fields": {"value": "global-X-Value-1"}}),
        );
        let p = secret(
            &c,
            &k,
            "p",
            "x",
            Kind::ApiKey,
            json!({"fields": {"value": "project-X-Value-1"}}),
        );
        let h = secret(
            &c,
            &k,
            GLOBAL,
            "gh",
            Kind::GithubToken,
            json!({"fields": {"value": "ghp_x", "host": "GitHub.example"}}),
        );
        grant(&c, &g, "project:p", Some(Level::Use), None).unwrap();
        grant(&c, &g, "agent:p/a", Some(Level::Read), Some("X_GLOBAL")).unwrap();
        grant(&c, &h, "project:p", Some(Level::Read), None).unwrap();
        let all = effective(&c, AGENT, "p").unwrap();
        let x = all
            .iter()
            .find(|e| e.scope == GLOBAL && e.name == "x")
            .unwrap();
        assert_eq!(x.level, Level::Read);
        assert_eq!(x.via, ["agent", "project"]);
        assert_eq!(x.env.as_deref(), Some("X_GLOBAL"));
        // The project secret has no grant: `x` is the global one.
        assert_eq!(find(&c, AGENT, "p", "x").unwrap().unwrap().scope, GLOBAL);
        grant(&c, &p, "agent:p/a", Some(Level::Use), None).unwrap();
        assert_eq!(find(&c, AGENT, "p", "x").unwrap().unwrap().scope, "p");
        // The other agent has none of them.
        assert!(effective(&c, OTHER, "q").unwrap().is_empty());
        // The env at start: the read grants, and GH_TOKEN for the github_token.
        let l = launch(&c, &k, AGENT, "p", &Default::default()).unwrap();
        assert_eq!(l.env["X_GLOBAL"].as_str(), "global-X-Value-1");
        assert_eq!(l.env[GH_TOKEN].as_str(), "ghp_x");
        assert_eq!(l.env.len(), 2);
        assert_eq!(l.git_hosts, ["github.example"]);
        assert!(!l.ssh_keys);
        assert!(!format!("{l:?}").contains("ghp_x"));
        let (e, o) = token_for_host(&c, &k, AGENT, "p", "GITHUB.example")
            .unwrap()
            .unwrap();
        assert_eq!(e.name, "gh");
        assert_eq!(o.values["value"].as_str(), "ghp_x");
        assert!(
            token_for_host(&c, &k, OTHER, "q", "github.example")
                .unwrap()
                .is_none()
        );
        // Removing the agent removes its grants; the project's stay.
        on_agent_deleted(&c, AGENT).unwrap();
        let all = effective(&c, AGENT, "p").unwrap();
        assert_eq!(all.len(), 2);
        on_project_deleted(&c, "p").unwrap();
        assert!(effective(&c, AGENT, "p").unwrap().is_empty());
    }

    #[test]
    fn generic_secrets_set_one_var_per_field() {
        let c = db();
        let k = MasterKey::generate();
        let s = secret(
            &c,
            &k,
            "p",
            "db",
            Kind::Generic,
            json!({
                "fields": {"user": "u", "password": "pw", "host-name": "h"},
                "secret_fields": ["password", "host-name"]
            }),
        );
        grant(&c, &s, "agent:p/a", Some(Level::Read), Some("DB")).unwrap();
        let l = launch(&c, &k, AGENT, "p", &Default::default()).unwrap();
        let names: Vec<_> = l.env.keys().cloned().collect();
        assert_eq!(names, ["DB_HOST_NAME", "DB_PASSWORD"]);
    }

    /// S1 (secret manager review, 2026-10-29): the final name of each
    /// var, `<ENV>_<FIELD>` of a `generic` secret too, passes the name
    /// rule, so no grant sets a loader, shell, git, or daemon var.
    #[test]
    fn generic_env_names_pass_the_name_rule() {
        let c = db();
        let k = MasterKey::generate();
        let s = secret(
            &c,
            &k,
            "p",
            "x",
            Kind::Generic,
            json!({
                "fields": {"preload": "/w/evil.so", "library_path": "/tmp"},
                "secret_fields": ["preload", "library_path"]
            }),
        );
        // The grant API refuses a base name whose final names break the rule.
        let e = grant(&c, &s, "agent:p/a", Some(Level::Read), Some("LD")).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid, "{}", e.message);
        // Grant rows that an older daemon or a config import wrote: the
        // launch refuses each final name with a warning, and does not fail.
        for (name, fields, env) in [
            ("x", ["preload", "library_path"], "LD"),
            ("y", ["socket", "agent"], "TERRARIUM"),
            ("z", ["config_count", "config_key_0"], "GIT"),
            ("w", ["env", "y"], "BASH"),
            ("v", ["preload", "x"], "PYTHON"),
        ] {
            let values: Map<String, Value> = fields
                .iter()
                .map(|f| ((*f).to_owned(), json!("v1")))
                .collect();
            let s = match crate::secrets::get(&c, "p", name).unwrap() {
                Some(s) => s,
                None => secret(
                    &c,
                    &k,
                    "p",
                    name,
                    Kind::Generic,
                    json!({"fields": values, "secret_fields": fields}),
                ),
            };
            c.execute(
                "INSERT OR REPLACE INTO secret_grants
                 (secret_id, grantee, level, env, created_by, created_at)
                 VALUES (?1, ?2, 'read', ?3, 'test', '2026-10-29T00:00:00Z')",
                params![s.id, format!("agent:{AGENT}"), env],
            )
            .unwrap();
        }
        let l = launch(&c, &k, AGENT, "p", &Default::default()).unwrap();
        let names: Vec<&String> = l.env.keys().collect();
        assert!(names.is_empty(), "{names:?}");
        for n in [
            "LD_PRELOAD",
            "TERRARIUM_SOCKET",
            "GIT_CONFIG_COUNT",
            "BASH_ENV",
            "PYTHON_PRELOAD",
        ] {
            assert!(
                l.warnings.iter().any(|w| w.contains(n)),
                "{n}: {:?}",
                l.warnings
            );
        }
    }

    #[test]
    fn the_name_rule_is_an_allowlist_shape_and_a_deny_set() {
        let long = "A".repeat(129);
        for bad in [
            "LD_AUDIT",
            "GCONV_PATH",
            "GLIBC_TUNABLES",
            "MALLOC_CHECK_",
            "LOCPATH",
            "NLSPATH",
            "HOSTALIASES",
            "RES_OPTIONS",
            "LOCALDOMAIN",
            "TMPDIR",
            "GIT_DIR",
            "GIT_SSH_COMMAND",
            "BASH_FUNC_X",
            "BASH_ENV",
            "ENV",
            "SHELLOPTS",
            "PS4",
            "PROMPT_COMMAND",
            "IFS",
            "CDPATH",
            "NODE_OPTIONS",
            "NPM_CONFIG_PREFIX",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PERL5OPT",
            "PERLLIB",
            "RUBYOPT",
            "GEM_HOME",
            "JAVA_TOOL_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "CLASSPATH",
            "DOTNET_STARTUP_HOOKS",
            "GOFLAGS",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "SSL_CERT_FILE",
            "CURL_CA_BUNDLE",
            "REQUESTS_CA_BUNDLE",
            "NODE_EXTRA_CA_CERTS",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CONFIG_DIR",
            "OPENAI_API_KEY",
            "CODEX_HOME",
            "COPILOT_GITHUB_TOKEN",
            "PI_CODING_AGENT_DIR",
            "DISABLE_TELEMETRY",
            "IS_SANDBOX",
            "GEMINI_API_KEY",
            "XDG_CONFIG_HOME",
            "TERRARIUM_SOCKET",
            "SSH_AUTH_SOCK",
            "HOME",
            "PATH",
            "USER",
            "LOGNAME",
            "SHELL",
            "TERM",
            "LANG",
            "LC_ALL",
            "TZ",
            "PWD",
            "OLDPWD",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "_X",
            "db_url",
            "A-B",
            "1A",
            "",
            long.as_str(),
        ] {
            assert!(check_env_name(bad).is_err(), "{bad}");
        }
        let longest = "A".repeat(128);
        for good in ["GH_TOKEN", "DB_URL", "X", "STRIPE_KEY_2", longest.as_str()] {
            assert!(check_env_name(good).is_ok(), "{good}");
        }
    }
}
