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
//! | `read` | `use`, the env vars at start, and `terrarium secret get` |
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

/// The level of a grant. `Read` is higher than `Use`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// The brokers only. The agent cannot see the value.
    Use,
    /// The brokers, the env vars, and `terrarium secret get`.
    Read,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Read => "read",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "use" => Some(Self::Use),
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
}

impl Grantee {
    /// The value of the `grantee` column.
    pub fn key(&self) -> String {
        match self {
            Self::Agent { id, .. } => format!("agent:{id}"),
            Self::Project(p) => format!("project:{p}"),
        }
    }

    /// The project of the grantee.
    pub fn project(&self) -> &str {
        match self {
            Self::Agent { project, .. } | Self::Project(project) => project,
        }
    }

    /// The grantee for a person: `agent:<project>/<name>` or
    /// `project:<name>`.
    pub fn label(&self) -> String {
        match self {
            Self::Agent { project, name, .. } => format!("agent:{project}/{name}"),
            Self::Project(p) => format!("project:{p}"),
        }
    }
}

/// Resolve a grantee: `agent:<project>/<name>`, `agent:<ulid>`, or
/// `project:<name>`.
pub fn parse_grantee(conn: &Connection, s: &str) -> ApiResult<Grantee> {
    let bad = || {
        ApiError::invalid(format!(
            "grantee `{s}` is not `agent:<project>/<name>`, `agent:<id>`, or `project:<name>`"
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
    if let Some(p) = s.strip_prefix("project:") {
        if p.is_empty() || p == GLOBAL {
            return Err(bad());
        }
        let n: i64 = conn.query_row("SELECT count(*) FROM projects WHERE name = ?1", [p], |r| {
            r.get(0)
        })?;
        if n == 0 {
            return Err(ApiError::not_found(format!("no project `{p}`")));
        }
        return Ok(Grantee::Project(p.to_owned()));
    }
    Err(bad())
}

/// The label of a stored grantee key, for the answers: `agent:<ulid>`
/// becomes `agent:<project>/<name>` while the agent exists.
fn label_of(conn: &Connection, key: &str) -> ApiResult<String> {
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

/// Env vars that the sandbox or the daemon sets, and that the loader or
/// the shell reads. A grant cannot set them.
const RESERVED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "TZ",
    "PWD",
    "OLDPWD",
    "TMPDIR",
    "IFS",
    "ENV",
    "BASH_ENV",
    "PS4",
    "SSH_AUTH_SOCK",
];

/// Prefixes of reserved env vars: the loader (`LD_`), the daemon
/// (`TERRARIUM_`), the locale (`LC_`), and the git identity and config
/// that the daemon sets (`GIT_AUTHOR_`, `GIT_COMMITTER_`, `GIT_CONFIG`).
const RESERVED_ENV_PREFIXES: &[&str] = &[
    "LD_",
    "TERRARIUM_",
    "LC_",
    "GIT_AUTHOR_",
    "GIT_COMMITTER_",
    "GIT_CONFIG",
];

/// Check the env var name of a grant: a POSIX name (`[A-Za-z_][A-Za-z0-9_]*`,
/// at most 128 bytes), not one that the sandbox sets.
pub fn check_env_name(name: &str) -> ApiResult<()> {
    let mut cs = name.chars();
    let posix = matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && name.len() <= 128
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !posix {
        return Err(ApiError::invalid(format!(
            "`{name}` is not an env var name: a letter or `_`, then letters, digits, or `_`"
        )));
    }
    let upper = name.to_ascii_uppercase();
    let home_env = terrarium_core::HarnessId::ALL
        .iter()
        .any(|h| h.home_env() == upper);
    if RESERVED_ENV.contains(&upper.as_str())
        || home_env
        || RESERVED_ENV_PREFIXES.iter().any(|p| upper.starts_with(p))
    {
        return Err(ApiError::invalid(format!(
            "`{name}` is an env var that the sandbox sets; choose another name"
        )));
    }
    Ok(())
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
    pub created_by: String,
    pub created_at: String,
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
    /// The actor label.
    pub by: &'a str,
}

/// Write a grant (a grant to the same grantee is replaced). The answer is
/// the grant and `true` when it is new.
pub fn put(conn: &Connection, secret: &SecretInfo, g: &NewGrant<'_>) -> ApiResult<(Grant, bool)> {
    check_reach(secret, g.grantee)?;
    let level = g.level.unwrap_or_else(|| Level::default_for(secret.kind));
    if let Some(env) = g.env {
        if level != Level::Read {
            return Err(ApiError::invalid(
                "an env var puts the value into the agent's environment, where the agent can \
                 print it: `env` needs the level `read`",
            ));
        }
        check_env_name(env)?;
    }
    let key = g.grantee.key();
    let now = crate::db::now();
    let n = conn.execute(
        "UPDATE secret_grants SET level = ?3, env = ?4, created_by = ?5, created_at = ?6
         WHERE secret_id = ?1 AND grantee = ?2",
        params![secret.id, key, level.as_str(), g.env, g.by, now],
    )?;
    if n == 0 {
        conn.execute(
            "INSERT INTO secret_grants (secret_id, grantee, level, env, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![secret.id, key, level.as_str(), g.env, g.by, now],
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
        "SELECT grantee, level, env, created_by, created_at FROM secret_grants
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
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(grantee, level, env, created_by, created_at)| {
            Ok(Grant {
                scope: secret.scope.clone(),
                name: secret.name.clone(),
                grantee_label: label_of(conn, &grantee)?,
                grantee,
                level: Level::parse(&level).unwrap_or(Level::Use),
                env,
                created_by,
                created_at,
            })
        })
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
        "DELETE FROM secret_grants WHERE grantee = ?1",
        [format!("project:{name}")],
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
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols}, g.grantee, g.level, g.env
         FROM secret_grants g JOIN secrets s ON s.id = g.secret_id
         WHERE g.grantee IN (?1, ?2) AND (s.scope = 'global' OR s.scope = ?3)",
        cols = super::COLS
            .split(", ")
            .map(|c| format!("s.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))?;
    let agent_key = format!("agent:{agent_id}");
    let project_key = format!("project:{project}");
    let rows = stmt
        .query_map(params![agent_key, project_key, project], |r| {
            let info = super::row_info(r)?;
            let grantee: String = r.get(10)?;
            let level: String = r.get(11)?;
            let env: Option<String> = r.get(12)?;
            Ok((grantee, level, env, info))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_id: BTreeMap<String, (Effective, Option<String>, Option<String>)> = BTreeMap::new();
    for (grantee, level, env, info) in rows {
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
/// whether it gets an ssh-agent socket.
pub fn launch(
    conn: &Connection,
    key: &MasterKey,
    agent_id: &str,
    project: &str,
) -> ApiResult<LaunchSecrets> {
    let mut out = LaunchSecrets::default();
    for e in effective(conn, agent_id, project)? {
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
        if check_env_name(&env).is_err() {
            out.warnings.push(format!(
                "secret `{}/{}`: `{env}` is not a usable env var name; it is not set",
                e.scope, e.name
            ));
            continue;
        }
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
                by: "operator",
            },
        )
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
        for good in ["GH_TOKEN", "OPENAI_API_KEY", "_X", "db_url"] {
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
            json!({"fields": {"value": "global-x"}}),
        );
        let p = secret(
            &c,
            &k,
            "p",
            "x",
            Kind::ApiKey,
            json!({"fields": {"value": "project-x"}}),
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
        let l = launch(&c, &k, AGENT, "p").unwrap();
        assert_eq!(l.env["X_GLOBAL"].as_str(), "global-x");
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
        let l = launch(&c, &k, AGENT, "p").unwrap();
        let names: Vec<_> = l.env.keys().cloned().collect();
        assert_eq!(names, ["DB_HOST_NAME", "DB_PASSWORD"]);
    }
}
