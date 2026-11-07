//! The secrets in the config repo export (PLAN §8.3, §16b.5).
//!
//! The export holds no secret. It holds the names, the kinds, and the
//! plain fields of the secrets, the names of their secret fields, and the
//! grants by name. One file for each scope:
//!
//! | Path | Holds |
//! |---|---|
//! | `export/secrets/global.toml` | the global secrets and their grants |
//! | `export/secrets/<project>.toml` | the secrets of one project and their grants |
//!
//! ```toml
//! version = 1
//! scope = "global"
//!
//! [[secret]]
//! name = "gh"
//! kind = "github_token"
//! secret_fields = ["value"]
//!
//! [secret.fields]
//! host = "github.com"
//!
//! [[secret.grant]]
//! grantee = "agent:web/lead"
//! level = "read"
//! env = "GH_TOKEN"
//!
//! [[secret.grant]]
//! grantee = "agent:web/deployer"
//! level = "run"
//! targets = ["prod-1", "prod-2"]
//! ```
//!
//! A grantee is `agent:<project>/<name>` or `project:<name>`, never an
//! agent ID of this instance.
//!
//! The import ([`plan`], then [`apply`]) makes the store match the files,
//! with these limits:
//!
//! - A missing secret becomes a placeholder: a row with no value
//!   (`sealed` is NULL). It does not open, and a grant of it gives
//!   nothing, until the operator sets the value.
//! - The plain fields of a secret change only when they differ. The
//!   value never changes. A secret with a value keeps its kind and the
//!   names of its secret fields (the value is sealed with them), and an
//!   `ssh_key` with a value keeps its derived `public_key`.
//! - A secret with a value that the file does not name stays, with a
//!   warning. A placeholder that the file does not name goes, unless a
//!   target uses it.
//! - The grants of each secret in the file match the file. A grant to an
//!   agent or a project that does not exist yet is skipped with a warning;
//!   the next import adds it.
//! - A new grant, or a wider one (a higher level, a new `run` target, a
//!   new or changed `env`),
//!   of a secret that has a value waits for the operator (S8): the plan
//!   shows it with `"approval": "needed"`, and an apply holds it
//!   (`"approval": "held"`) unless the operator approves the grants
//!   ([`apply`] with `approve`). A grant of a placeholder applies: it
//!   gives nothing until the operator sets the value.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::grants::{self, Level, NewGrant};
use super::{GLOBAL, Kind, SecretInfo, kinds, scope_project};
use crate::error::{ApiError, ApiResult};

/// The dir of the secrets files, in the repo.
pub const DIR: &str = "export/secrets";
/// The version of the file format.
pub const VERSION: u32 = 1;

/// The path in the repo of the secrets file of `scope`.
pub fn path_of(scope: &str) -> String {
    format!("{DIR}/{scope}.toml")
}

/// A plain field: text, or a list of text (`hosts` of a `token`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Text(String),
    List(Vec<String>),
}

impl FieldValue {
    fn to_json(&self) -> Value {
        match self {
            Self::Text(s) => Value::String(s.clone()),
            Self::List(l) => Value::Array(l.iter().cloned().map(Value::String).collect()),
        }
    }
}

/// One secrets file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub version: u32,
    /// `global` or a project.
    pub scope: String,
    #[serde(default, rename = "secret")]
    pub secrets: Vec<Entry>,
}

/// One secret of a file: no value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// The names of the secret fields. A typed kind fixes them.
    #[serde(default)]
    pub secret_fields: Vec<String>,
    /// The plain fields.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldValue>,
    #[serde(default, rename = "grant")]
    pub grants: Vec<GrantEntry>,
}

/// One grant of a secret, by grantee name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantEntry {
    /// `agent:<project>/<name>` or `project:<name>`.
    pub grantee: String,
    /// `None`: [`Level::default_for`] the kind.
    #[serde(default)]
    pub level: Option<Level>,
    #[serde(default)]
    pub env: Option<String>,
    /// The targets of a `run` grant (TER-142).
    #[serde(default)]
    pub targets: Vec<String>,
}

// ---- Export ----

/// A TOML basic string. A JSON string is one: the same escapes.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

/// A TOML key: bare when it can be, else quoted (a `.` would make a
/// dotted key).
fn key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_owned()
    } else {
        quote(k)
    }
}

fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(quote(s)),
        Value::Array(a) => {
            let items: Option<Vec<String>> = a.iter().map(|i| i.as_str().map(quote)).collect();
            Some(format!("[{}]", items?.join(", ")))
        }
        _ => None,
    }
}

/// The stored grantee label of a grant, when it names no ID of this
/// instance: `agent:<project>/<name>` or `project:<name>`.
fn portable(label: &str) -> bool {
    label.starts_with("project:") || (label.starts_with("agent:") && label.contains('/'))
}

/// The text of the secrets file of one scope.
fn render(conn: &Connection, scope: &str, secrets: &[SecretInfo]) -> ApiResult<String> {
    let mut out = format!(
        "# The secrets of `{scope}`, as the config repo export holds them (PLAN 16b.5).\n\
         # terrariumd writes this file. It holds the names, the kinds, the plain fields,\n\
         # and the grants. It holds no value. An import makes a missing secret as a\n\
         # placeholder with no value; the operator then sets the value.\n\
         version = {VERSION}\n\
         scope = {}\n",
        quote(scope)
    );
    for s in secrets {
        out.push_str("\n[[secret]]\n");
        out.push_str(&format!("name = {}\n", quote(&s.name)));
        out.push_str(&format!("kind = {}\n", quote(s.kind.as_str())));
        let names: Vec<String> = s.secret_fields.iter().map(|n| quote(n)).collect();
        out.push_str(&format!("secret_fields = [{}]\n", names.join(", ")));
        if !s.fields.is_empty() {
            out.push_str("\n[secret.fields]\n");
            for (k, v) in &s.fields {
                let Some(text) = value_text(v) else { continue };
                // The public half of an SSH key and its fingerprint are
                // long base64 runs, and not secret.
                let marker = if s.kind == Kind::SshKey && (k == "public_key" || k == "fingerprint")
                {
                    format!(" # {}", terrarium_core::secret_scan::ALLOW_MARKER)
                } else {
                    String::new()
                };
                out.push_str(&format!("{} = {text}{marker}\n", key(k)));
            }
        }
        let mut list: Vec<grants::Grant> = grants::list(conn, s)?
            .into_iter()
            .filter(|g| portable(&g.grantee_label))
            .collect();
        list.sort_by(|a, b| a.grantee_label.cmp(&b.grantee_label));
        for g in list {
            out.push_str("\n[[secret.grant]]\n");
            out.push_str(&format!("grantee = {}\n", quote(&g.grantee_label)));
            out.push_str(&format!("level = {}\n", quote(g.level.as_str())));
            if let Some(env) = &g.env {
                out.push_str(&format!("env = {}\n", quote(env)));
            }
            if !g.targets.is_empty() {
                let ts: Vec<String> = g.targets.iter().map(|t| quote(t)).collect();
                out.push_str(&format!("targets = [{}]\n", ts.join(", ")));
            }
        }
    }
    Ok(out)
}

/// The secrets files of the store: `(path in the repo, content)`, one for
/// each scope that has a secret.
pub fn files(conn: &Connection) -> ApiResult<Vec<(String, String)>> {
    let mut by_scope: BTreeMap<String, Vec<SecretInfo>> = BTreeMap::new();
    for s in super::list(conn, None)? {
        by_scope.entry(s.scope.clone()).or_default().push(s);
    }
    let mut out = Vec::new();
    for (scope, mut list) in by_scope {
        list.sort_by(|a, b| a.name.cmp(&b.name));
        out.push((path_of(&scope), render(conn, &scope, &list)?));
    }
    Ok(out)
}

// ---- Import ----

/// One change of an import, as the answer shows it. No value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    /// `create` (a placeholder), `update` (the plain fields), `delete` (a
    /// placeholder), `grant`, or `revoke`.
    pub action: String,
    /// `<scope>/<name>`.
    pub secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grantee: Option<String>,
    /// A new or wider grant of a secret with a value (S8): `needed` in a
    /// plan, `held` when an apply did not make it, `approved` when the
    /// operator approved it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    /// A grant: the level that the file gives (`use`, `run`, or `read`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// A grant: the env var that the file gives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// A `run` grant: the targets that the file gives.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// A grant that changes a stored grant: the stored one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub was: Option<GrantWas>,
}

/// The stored grant that a grant change replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrantWas {
    pub level: Level,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
}

/// The `approval` of a grant that waits for the operator, in a plan.
pub const APPROVAL_NEEDED: &str = "needed";
/// The `approval` of a grant that an apply did not make.
pub const APPROVAL_HELD: &str = "held";
/// The `approval` of a grant that the operator approved.
pub const APPROVAL_GIVEN: &str = "approved";

/// What an import does, or did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub changes: Vec<Change>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
enum Op {
    Create {
        scope: String,
        name: String,
        kind: Kind,
        plain: Map<String, Value>,
        secret_fields: Vec<String>,
    },
    Update {
        scope: String,
        name: String,
        kind: Kind,
        plain: Map<String, Value>,
        secret_fields: Vec<String>,
    },
    Delete {
        scope: String,
        name: String,
    },
    Grant {
        scope: String,
        name: String,
        grantee: String,
        level: Level,
        env: Option<String>,
        targets: Vec<String>,
        /// A new or wider grant of a secret with a value: the operator
        /// approves it (S8).
        approval: bool,
    },
    Revoke {
        scope: String,
        name: String,
        key: String,
    },
}

/// The plan of an import: the store writes, and the report.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    ops: Vec<Op>,
    pub report: Report,
}

impl Plan {
    fn push(&mut self, op: Op, action: &str, scope: &str, name: &str, grantee: Option<String>) {
        self.ops.push(op);
        self.report.changes.push(Change {
            action: action.to_owned(),
            secret: format!("{scope}/{name}"),
            grantee,
            approval: None,
            level: None,
            env: None,
            targets: Vec::new(),
            was: None,
        });
    }

    /// True when the plan has a grant that waits for the operator.
    pub fn needs_approval(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, Op::Grant { approval: true, .. }))
    }

    fn warn(&mut self, m: String) {
        self.report.warnings.push(m);
    }
}

fn project_exists(conn: &Connection, p: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row("SELECT count(*) FROM projects WHERE name = ?1", [p], |r| {
        r.get(0)
    })?;
    Ok(n > 0)
}

/// The scope that the path of a secrets file names.
fn scope_of_path(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    name.strip_suffix(".toml").map(str::to_owned)
}

/// Plan the import of the secrets files `files` (`(path, content)`).
/// The import looks at `global`, at each scope with a file, and at each
/// project in `projects` (the projects of the export): a scope with no
/// file has no secrets in the export.
pub fn plan(conn: &Connection, files: &[(String, String)], projects: &[String]) -> ApiResult<Plan> {
    let mut p = Plan::default();
    let mut desired: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    let mut skip: BTreeSet<String> = BTreeSet::new();
    for (path, text) in files {
        match toml::from_str::<File>(text) {
            Ok(f) if f.version != VERSION => {
                p.warn(format!(
                    "{path}: version {} is not {VERSION}; its secrets are not imported",
                    f.version
                ));
                skip.insert(f.scope);
            }
            Ok(f) if desired.contains_key(&f.scope) => {
                p.warn(format!(
                    "{path}: scope `{}` is in another file too; this file is not imported",
                    f.scope
                ));
            }
            Ok(f) => {
                desired.insert(f.scope, f.secrets);
            }
            Err(e) => {
                // The message of the TOML parser quotes the line: a secrets
                // file holds no value, so it shows no secret.
                p.warn(format!(
                    "{path}: not a secrets file: {}",
                    e.message().trim()
                ));
                if let Some(s) = scope_of_path(path) {
                    skip.insert(s);
                }
            }
        }
    }
    let mut scopes: BTreeSet<String> = BTreeSet::from([GLOBAL.to_owned()]);
    scopes.extend(desired.keys().cloned());
    scopes.extend(projects.iter().cloned());
    for scope in scopes {
        if skip.contains(&scope) {
            continue;
        }
        let entries = desired.remove(&scope).unwrap_or_default();
        if scope != GLOBAL && !project_exists(conn, &scope)? {
            if !entries.is_empty() {
                p.warn(format!(
                    "project `{scope}` does not exist yet; its secrets wait for the next import"
                ));
            }
            continue;
        }
        plan_scope(conn, &mut p, &scope, entries)?;
    }
    Ok(p)
}

fn plan_scope(conn: &Connection, p: &mut Plan, scope: &str, entries: Vec<Entry>) -> ApiResult<()> {
    let existing: BTreeMap<String, SecretInfo> = super::list(conn, Some(scope))?
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for e in entries {
        let label = format!("{scope}/{}", e.name);
        if !seen.insert(e.name.clone()) {
            p.warn(format!(
                "secret `{label}` is in the file more than once; the first one counts"
            ));
            continue;
        }
        if let Err(err) = crate::store::check_name("secret", &e.name) {
            p.warn(format!("secret `{label}`: {}", err.message));
            continue;
        }
        let mut plain: Map<String, Value> = e
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), v.to_json()))
            .collect();
        let secret_fields = if e.kind == Kind::Generic || !e.secret_fields.is_empty() {
            e.secret_fields.clone()
        } else {
            kinds::fixed_secret_fields(e.kind)
        };
        if let Err(err) = kinds::check_plain(e.kind, &plain, &secret_fields) {
            p.warn(format!(
                "secret `{label}`: {}; the import leaves it as it is",
                err.message
            ));
            continue;
        }
        super::ssh::fill_fingerprint(e.kind, &mut plain);
        let cur = existing.get(&e.name);
        let kind = match cur {
            None => {
                p.push(
                    Op::Create {
                        scope: scope.to_owned(),
                        name: e.name.clone(),
                        kind: e.kind,
                        plain,
                        secret_fields,
                    },
                    "create",
                    scope,
                    &e.name,
                    None,
                );
                e.kind
            }
            Some(c) if c.has_value => {
                if c.kind != e.kind {
                    p.warn(format!(
                        "secret `{label}` is a {} with a value here and a {} in the file; the \
                         import keeps the stored secret",
                        c.kind, e.kind
                    ));
                } else {
                    if c.kind != Kind::SshKey && c.fields != plain {
                        if let Some(clash) = plain.keys().find(|k| c.secret_fields.contains(k)) {
                            p.warn(format!(
                                "secret `{label}`: `{clash}` is a secret field here and a plain \
                                 field in the file; the import keeps the stored fields"
                            ));
                        } else {
                            p.push(
                                Op::Update {
                                    scope: scope.to_owned(),
                                    name: e.name.clone(),
                                    kind: c.kind,
                                    plain,
                                    secret_fields: c.secret_fields.clone(),
                                },
                                "update",
                                scope,
                                &e.name,
                                None,
                            );
                        }
                    }
                    let a: BTreeSet<&String> = c.secret_fields.iter().collect();
                    let b: BTreeSet<&String> = secret_fields.iter().collect();
                    if a != b {
                        p.warn(format!(
                            "secret `{label}`: the secret fields here ({}) are not those of the \
                             file ({}); the stored value stays",
                            c.secret_fields.join(", "),
                            secret_fields.join(", ")
                        ));
                    }
                }
                c.kind
            }
            Some(c) => {
                if c.kind != e.kind || c.fields != plain || c.secret_fields != secret_fields {
                    p.push(
                        Op::Update {
                            scope: scope.to_owned(),
                            name: e.name.clone(),
                            kind: e.kind,
                            plain,
                            secret_fields,
                        },
                        "update",
                        scope,
                        &e.name,
                        None,
                    );
                }
                e.kind
            }
        };
        plan_grants(conn, p, scope, &e.name, kind, cur, &e.grants)?;
    }
    for (name, c) in &existing {
        if seen.contains(name) {
            continue;
        }
        let label = format!("{scope}/{name}");
        if c.has_value {
            p.warn(format!(
                "secret `{label}` has a value and is not in the export; the import keeps it"
            ));
            continue;
        }
        let users = crate::store::targets::targets_using_secret(conn, scope, name)?;
        if !users.is_empty() {
            p.warn(format!(
                "placeholder `{label}` is not in the export, but target {} uses it; the import \
                 keeps it",
                users.join(", ")
            ));
            continue;
        }
        p.push(
            Op::Delete {
                scope: scope.to_owned(),
                name: name.clone(),
            },
            "delete",
            scope,
            name,
            None,
        );
    }
    Ok(())
}

fn plan_grants(
    conn: &Connection,
    p: &mut Plan,
    scope: &str,
    name: &str,
    kind: Kind,
    cur: Option<&SecretInfo>,
    want: &[GrantEntry],
) -> ApiResult<()> {
    let label = format!("{scope}/{name}");
    let stored: BTreeMap<String, grants::Grant> = match cur {
        Some(c) => grants::list(conn, c)?
            .into_iter()
            .map(|g| (g.grantee.clone(), g))
            .collect(),
        None => BTreeMap::new(),
    };
    let mut keep: BTreeSet<String> = BTreeSet::new();
    for g in want {
        let grantee = match grants::parse_grantee(conn, &g.grantee) {
            Ok(x) => x,
            Err(err) => {
                p.warn(format!(
                    "secret `{label}`: the grant to `{}` is skipped: {}; the next import adds it",
                    g.grantee, err.message
                ));
                continue;
            }
        };
        let key = grantee.key();
        keep.insert(key.clone());
        if scope != GLOBAL && grantee.project() != scope {
            p.warn(format!(
                "secret `{label}` is a secret of project `{scope}`; the grant to `{}` is skipped",
                g.grantee
            ));
            continue;
        }
        let level = g.level.unwrap_or_else(|| Level::default_for(kind));
        if let Some(env) = &g.env {
            let bad = if level == Level::Read {
                grants::check_env_name(env).err().map(|e| e.message)
            } else {
                Some("`env` needs the level `read`".to_owned())
            };
            if let Some(why) = bad {
                p.warn(format!(
                    "secret `{label}`: the grant to `{}` is skipped: {why}",
                    g.grantee
                ));
                continue;
            }
        }
        let targets = match grants::check_run_targets(level, &g.targets) {
            Ok(t) => t,
            Err(err) => {
                p.warn(format!(
                    "secret `{label}`: the grant to `{}` is skipped: {}",
                    g.grantee, err.message
                ));
                continue;
            }
        };
        let old = stored.get(&key);
        let same = old
            .is_some_and(|s| s.level == level && s.env == g.env && s.targets == targets);
        if !same {
            // A grant of a value: new, or wider (S8): a higher level, a
            // `run` target that the stored grant does not name, a new env.
            let wider = match old {
                None => true,
                Some(s) => {
                    level > s.level
                        || (level == Level::Run
                            && s.level == Level::Run
                            && targets.iter().any(|t| !s.targets.contains(t)))
                        || (g.env.is_some() && g.env != s.env)
                }
            };
            let approval = wider && cur.is_some_and(|c| c.has_value);
            p.push(
                Op::Grant {
                    scope: scope.to_owned(),
                    name: name.to_owned(),
                    grantee: key,
                    level,
                    env: g.env.clone(),
                    targets: targets.clone(),
                    approval,
                },
                "grant",
                scope,
                name,
                Some(grantee.label()),
            );
            if let Some(c) = p.report.changes.last_mut() {
                c.level = Some(level);
                c.env = g.env.clone();
                c.targets = targets;
                c.was = old.map(|s| GrantWas {
                    level: s.level,
                    env: s.env.clone(),
                    targets: s.targets.clone(),
                });
                if approval {
                    c.approval = Some(APPROVAL_NEEDED.to_owned());
                }
            }
        }
    }
    for (key, g) in stored {
        if !keep.contains(&key) {
            p.push(
                Op::Revoke {
                    scope: scope.to_owned(),
                    name: name.to_owned(),
                    key,
                },
                "revoke",
                scope,
                name,
                Some(g.grantee_label),
            );
        }
    }
    Ok(())
}

fn get_info(conn: &Connection, scope: &str, name: &str) -> ApiResult<SecretInfo> {
    super::get(conn, scope, name)?
        .ok_or_else(|| ApiError::internal(format!("secret `{scope}/{name}` is gone")))
}

/// Run a plan in one transaction, with an audit row for each change
/// (`secret.put`, `secret.delete`, `secret.grant`, `secret.revoke`, with
/// `"source": "config"`). `by` is the actor label. The caller builds the
/// redaction matcher again after it (`super::redact::rebuild`).
///
/// A new or wider grant of a secret with a value runs only with
/// `approve` (the operator approves it, S8). Without it, the grant does
/// not change, and its change in the answer has `"approval": "held"`. The
/// answer is the report of the plan with the approvals.
pub fn apply(t: &crate::db::Tx<'_>, plan: Plan, by: &str, approve: bool) -> ApiResult<Report> {
    let now = crate::db::now();
    let Plan { ops, mut report } = plan;
    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Create {
                scope,
                name,
                kind,
                plain,
                secret_fields,
            } => {
                t.execute(
                    "INSERT INTO secrets (id, scope, name, kind, plain, secret_fields, sealed,
                                          created_at, updated_at, created_by)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?7, ?8)",
                    params![
                        ulid::Ulid::new().to_string(),
                        scope,
                        name,
                        kind.as_str(),
                        Value::Object(plain.clone()).to_string(),
                        serde_json::to_string(secret_fields)?,
                        now,
                        by
                    ],
                )?;
                t.audit(
                    "secret.put",
                    &format!("{scope}/{name}"),
                    scope_project(scope),
                    json!({
                        "scope": scope, "name": name, "kind": kind, "created": true,
                        "placeholder": true, "source": "config",
                        "plain_fields": plain.keys().collect::<Vec<_>>(),
                        "secret_fields": secret_fields,
                    }),
                )?;
            }
            Op::Update {
                scope,
                name,
                kind,
                plain,
                secret_fields,
            } => {
                // The kind and the secret field names change only on a
                // placeholder: a value is sealed with its kind.
                t.execute(
                    "UPDATE secrets SET kind = ?3, plain = ?4, secret_fields = ?5, updated_at = ?6
                     WHERE scope = ?1 AND name = ?2 AND (sealed IS NULL OR kind = ?3)",
                    params![
                        scope,
                        name,
                        kind.as_str(),
                        Value::Object(plain.clone()).to_string(),
                        serde_json::to_string(secret_fields)?,
                        now
                    ],
                )?;
                t.audit(
                    "secret.put",
                    &format!("{scope}/{name}"),
                    scope_project(scope),
                    json!({
                        "scope": scope, "name": name, "kind": kind, "created": false,
                        "source": "config",
                        "plain_fields": plain.keys().collect::<Vec<_>>(),
                        "secret_fields": secret_fields,
                    }),
                )?;
            }
            Op::Delete { scope, name } => {
                let n = t.execute(
                    "DELETE FROM secrets WHERE scope = ?1 AND name = ?2 AND sealed IS NULL",
                    params![scope, name],
                )?;
                if n > 0 {
                    t.audit(
                        "secret.delete",
                        &format!("{scope}/{name}"),
                        scope_project(scope),
                        json!({"scope": scope, "name": name, "placeholder": true,
                               "source": "config"}),
                    )?;
                }
            }
            Op::Grant {
                scope,
                name,
                grantee,
                level,
                env,
                targets,
                approval,
            } => {
                if *approval {
                    let mark = if approve {
                        APPROVAL_GIVEN
                    } else {
                        APPROVAL_HELD
                    };
                    if let Some(c) = report.changes.get_mut(i) {
                        c.approval = Some(mark.to_owned());
                    }
                    if !approve {
                        continue;
                    }
                }
                let info = get_info(t, scope, name)?;
                let g = grants::parse_grantee(t, grantee)?;
                let (grant, created) = grants::put(
                    t,
                    &info,
                    &NewGrant {
                        grantee: &g,
                        level: Some(*level),
                        env: env.as_deref(),
                        targets,
                        by,
                    },
                )?;
                t.audit(
                    "secret.grant",
                    &format!("{scope}/{name}"),
                    scope_project(scope),
                    json!({
                        "scope": scope, "name": name, "kind": info.kind,
                        "grantee": grant.grantee, "grantee_label": grant.grantee_label,
                        "level": grant.level, "env": grant.env, "targets": grant.targets,
                        "created": created, "source": "config", "approved": *approval,
                    }),
                )?;
            }
            Op::Revoke { scope, name, key } => {
                let info = get_info(t, scope, name)?;
                if grants::delete(t, &info, key)? {
                    t.audit(
                        "secret.revoke",
                        &format!("{scope}/{name}"),
                        scope_project(scope),
                        json!({"scope": scope, "name": name, "kind": info.kind,
                               "grantee": key, "source": "config"}),
                    )?;
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::secrets::MasterKey;
    use crate::secrets::grants::tests::{AGENT, db, grant, secret};

    /// Real-looking values: none may reach the export.
    fn github_token() -> String {
        format!("ghp_{}", "R3alLook1ngT0kenAbCdEf".repeat(2))
    }

    fn store_with_values(c: &Connection, k: &MasterKey) -> Vec<String> {
        let gh = github_token();
        let pem = crate::secrets::ssh::tests::ed25519_key();
        let password = "Tr0ub4dor&3-horse-battery".to_owned();
        let s = secret(
            c,
            k,
            GLOBAL,
            "gh",
            Kind::GithubToken,
            json!({"fields": {"value": gh}}),
        );
        grant(c, &s, "agent:p/a", Some(Level::Read), Some("GH_TOKEN")).unwrap();
        grant(c, &s, "project:q", None, None).unwrap();
        let k2 = secret(
            c,
            k,
            GLOBAL,
            "deploy",
            Kind::SshKey,
            json!({"fields": {"private_key": pem}}),
        );
        grant(c, &k2, "agent:p/a", None, None).unwrap();
        secret(
            c,
            k,
            "p",
            "win",
            Kind::Login,
            json!({"fields": {"username": "bob", "password": password, "url": "rdp://h"}}),
        );
        secret(
            c,
            k,
            "p",
            "db",
            Kind::Generic,
            json!({"fields": {"user": "app", "pw": password}, "secret_fields": ["pw"]}),
        );
        vec![gh, pem, password]
    }

    #[test]
    fn the_export_has_names_and_grants_and_no_value() {
        let c = db();
        let k = MasterKey::generate();
        let values = store_with_values(&c, &k);
        let files = files(&c).unwrap();
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            ["export/secrets/global.toml", "export/secrets/p.toml"]
        );
        for (path, text) in &files {
            for v in &values {
                assert!(!text.contains(v.trim()), "{path} holds a value:\n{text}");
            }
            assert!(!text.contains("PRIVATE KEY"), "{text}");
            assert!(!text.contains(AGENT), "{text}");
            let found = terrarium_core::secret_scan::scan(path, text.as_bytes());
            assert!(found.is_empty(), "{path}: {found:?}\n{text}");
            // It parses back.
            toml::from_str::<File>(text).unwrap();
        }
        let global = &files[0].1;
        assert!(global.contains("name = \"gh\""), "{global}");
        assert!(global.contains("kind = \"github_token\""), "{global}");
        assert!(global.contains("host = \"github.com\""), "{global}");
        assert!(global.contains("grantee = \"agent:p/a\""), "{global}");
        assert!(global.contains("env = \"GH_TOKEN\""), "{global}");
        assert!(global.contains("grantee = \"project:q\""), "{global}");
        assert!(global.contains("public_key = \"ssh-ed25519 "), "{global}");
        assert!(global.contains("fingerprint = \"SHA256:"), "{global}");
        let p = &files[1].1;
        assert!(p.contains("username = \"bob\""), "{p}");
        assert!(p.contains("user = \"app\""), "{p}");
        assert!(p.contains("secret_fields = [\"pw\"]"), "{p}");
    }

    fn run(c: &mut Connection, files: &[(String, String)]) -> Report {
        let t = c.transaction().unwrap();
        let tx = crate::db::Tx::for_test(t, "operator");
        let plan = plan(&tx, files, &[]).unwrap();
        let report = apply(&tx, plan, "operator", true).unwrap();
        tx.commit_for_test().unwrap();
        report
    }

    fn row_sealed(c: &Connection, scope: &str, name: &str) -> Option<Vec<u8>> {
        c.query_row(
            "SELECT sealed FROM secrets WHERE scope = ?1 AND name = ?2",
            [scope, name],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn an_import_makes_placeholders_and_grants_and_is_idempotent() {
        let src = db();
        let k = MasterKey::generate();
        store_with_values(&src, &k);
        let files = files(&src).unwrap();

        let mut c = db();
        let r = run(&mut c, &files);
        assert!(r.warnings.is_empty(), "{r:?}");
        let created: Vec<&str> = r
            .changes
            .iter()
            .filter(|x| x.action == "create")
            .map(|x| x.secret.as_str())
            .collect();
        assert_eq!(created, ["global/deploy", "global/gh", "p/db", "p/win"]);
        for (scope, name) in [
            ("global", "gh"),
            ("global", "deploy"),
            ("p", "win"),
            ("p", "db"),
        ] {
            let info = crate::secrets::get(&c, scope, name).unwrap().unwrap();
            assert!(!info.has_value, "{scope}/{name}");
            assert!(row_sealed(&c, scope, name).is_none());
            let e = crate::secrets::open(&c, &k, scope, name).unwrap_err();
            assert_eq!(e.code, crate::error::Code::Invalid);
            assert!(e.message.contains("has no value yet"), "{}", e.message);
        }
        let gh = crate::secrets::get(&c, GLOBAL, "gh").unwrap().unwrap();
        assert_eq!(gh.secret_fields, ["value"]);
        assert_eq!(gh.fields["host"], "github.com");
        let db_ = crate::secrets::get(&c, "p", "db").unwrap().unwrap();
        assert_eq!(db_.secret_fields, ["pw"]);
        assert_eq!(db_.fields["user"], "app");
        let labels: Vec<_> = grants::list(&c, &gh)
            .unwrap()
            .into_iter()
            .map(|g| (g.grantee_label, g.level, g.env))
            .collect();
        assert_eq!(
            labels,
            [
                (
                    "agent:p/a".to_owned(),
                    Level::Read,
                    Some("GH_TOKEN".to_owned())
                ),
                ("project:q".to_owned(), Level::Use, None),
            ]
        );
        // The audit rows.
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM audit WHERE action = 'secret.put'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 4);
        // The export of the imported store is the same text.
        assert_eq!(super::files(&c).unwrap(), files);
        // A second import changes nothing.
        let again = run(&mut c, &files);
        assert_eq!(again, Report::default());
        // The placeholders give nothing at a start, with a warning each.
        let l = grants::launch(&c, &k, AGENT, "p", &Default::default()).unwrap();
        assert!(l.env.is_empty());
        assert!(l.git_hosts.is_empty());
        assert!(!l.ssh_keys);
        assert_eq!(l.warnings.len(), 2, "{:?}", l.warnings);
        assert!(l.warnings.iter().all(|w| w.contains("no value yet")));
        assert!(
            grants::token_for_host(&c, &k, AGENT, "p", "github.com")
                .unwrap()
                .is_none()
        );
        assert!(grants::ssh_keys(&c, AGENT, "p").unwrap().is_empty());
        // A placeholder that the file drops goes; its grants go with it.
        let none = vec![(
            path_of(GLOBAL),
            "version = 1\nscope = \"global\"\n".to_owned(),
        )];
        let r = run(&mut c, &none);
        let deleted: Vec<_> = r
            .changes
            .iter()
            .filter(|x| x.action == "delete")
            .map(|x| x.secret.as_str())
            .collect();
        assert_eq!(deleted, ["global/deploy", "global/gh"]);
        assert!(crate::secrets::get(&c, GLOBAL, "gh").unwrap().is_none());
    }

    #[test]
    fn a_secret_with_a_value_is_never_overwritten_or_deleted() {
        let mut c = db();
        let k = MasterKey::generate();
        let values = store_with_values(&c, &k);
        let before = row_sealed(&c, GLOBAL, "gh").unwrap();
        let file = "version = 1\nscope = \"global\"\n\n\
                    [[secret]]\nname = \"gh\"\nkind = \"github_token\"\n\n\
                    [secret.fields]\nhost = \"ghe.example\"\n\n\
                    [[secret]]\nname = \"deploy\"\nkind = \"token\"\n"
            .to_owned();
        let r = run(&mut c, &[(path_of(GLOBAL), file)]);
        // The plain field changes; the value stays and still opens.
        assert!(
            r.changes
                .iter()
                .any(|x| x.action == "update" && x.secret == "global/gh"),
            "{r:?}"
        );
        assert_eq!(row_sealed(&c, GLOBAL, "gh").unwrap(), before);
        let o = crate::secrets::open(&c, &k, GLOBAL, "gh").unwrap().unwrap();
        assert_eq!(o.values["value"].as_str(), values[0]);
        assert_eq!(o.info.fields["host"], "ghe.example");
        // The kind of a secret with a value stays, with a warning.
        let d = crate::secrets::get(&c, GLOBAL, "deploy").unwrap().unwrap();
        assert_eq!(d.kind, Kind::SshKey);
        assert!(d.has_value);
        assert!(
            r.warnings.iter().any(|w| w.contains("global/deploy")),
            "{r:?}"
        );
        // The grants follow the file: the file has none.
        assert!(
            r.changes
                .iter()
                .any(|x| x.action == "revoke" && x.grantee.as_deref() == Some("agent:p/a"))
        );
        assert!(grants::list(&c, &o.info).unwrap().is_empty());
        // No file for `p`: its secrets have values and stay, with a warning.
        let r = run(&mut c, &[]);
        assert!(crate::secrets::get(&c, "p", "win").unwrap().is_some());
        assert!(
            crate::secrets::get(&c, GLOBAL, "gh")
                .unwrap()
                .unwrap()
                .has_value
        );
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("global/gh") && w.contains("keeps it")),
            "{r:?}"
        );
        assert!(r.changes.iter().all(|x| x.action != "delete"), "{r:?}");
    }

    #[test]
    fn grants_to_missing_agents_wait_with_a_warning() {
        let mut c = db();
        let file = "version = 1\nscope = \"global\"\n\n\
                    [[secret]]\nname = \"k\"\nkind = \"api_key\"\n\n\
                    [secret.fields]\nenv = \"K_KEY\"\n\n\
                    [[secret.grant]]\ngrantee = \"agent:p/later\"\nlevel = \"read\"\n\n\
                    [[secret.grant]]\ngrantee = \"agent:p/a\"\n"
            .to_owned();
        let r = run(&mut c, &[(path_of(GLOBAL), file.clone())]);
        assert!(
            r.warnings.iter().any(|w| w.contains("agent:p/later")),
            "{r:?}"
        );
        let info = crate::secrets::get(&c, GLOBAL, "k").unwrap().unwrap();
        let g = grants::list(&c, &info).unwrap();
        assert_eq!(g.len(), 1);
        // The default level of an api_key is `read`.
        assert_eq!(g[0].level, Level::Read);
        // The agent comes; the next import adds its grant.
        c.execute(
            "INSERT INTO agents (id, project, name, harness, workspace, created_at, updated_at)
             VALUES ('01K8AGENT0000000000000000C', 'p', 'later', 'claude', '', 'x', 'x')",
            [],
        )
        .unwrap();
        let r = run(&mut c, &[(path_of(GLOBAL), file)]);
        assert!(r.warnings.is_empty(), "{r:?}");
        assert_eq!(grants::list(&c, &info).unwrap().len(), 2);
        // A bad file is a warning, and its scope stays as it is.
        let r = run(&mut c, &[(path_of(GLOBAL), "version = [".to_owned())]);
        assert!(r.changes.is_empty());
        assert_eq!(r.warnings.len(), 1, "{r:?}");
        assert!(crate::secrets::get(&c, GLOBAL, "k").unwrap().is_some());
    }

    #[test]
    fn a_target_with_a_placeholder_credential_reports_it() {
        let mut c = db();
        let file = "version = 1\nscope = \"global\"\n\n\
                    [[secret]]\nname = \"box\"\nkind = \"ssh_key\"\n"
            .to_owned();
        run(&mut c, &[(path_of(GLOBAL), file)]);
        let def: terrarium_core::config::TargetDef = serde_json::from_value(json!({
            "name": "box", "kind": "ssh", "address": "h", "credential": "box"
        }))
        .unwrap();
        // The save check passes: the import can bring the target too.
        crate::store::targets::check_credential(&c, &def, None).unwrap();
        let v = crate::store::targets::credential_view(&c, &def)
            .unwrap()
            .unwrap();
        assert!(
            v.warning
                .as_deref()
                .unwrap_or_default()
                .contains("no value yet"),
            "{v:?}"
        );
        let dir = tempfile::tempdir().unwrap();
        crate::secrets::crypto::write_key(
            &crate::secrets::crypto::key_path(dir.path()),
            &MasterKey::generate(),
        )
        .unwrap();
        let e = crate::secrets::require(&c, dir.path(), None, "box").unwrap_err();
        assert!(e.message.contains("no value yet"), "{}", e.message);
    }

    fn run_with(c: &mut Connection, files: &[(String, String)], approve: bool) -> Report {
        let t = c.transaction().unwrap();
        let tx = crate::db::Tx::for_test(t, "operator");
        let plan = plan(&tx, files, &[]).unwrap();
        let report = apply(&tx, plan, "operator", approve).unwrap();
        tx.commit_for_test().unwrap();
        report
    }

    fn with_approval<'a>(r: &'a Report, approval: &str) -> Vec<(&'a str, &'a str)> {
        r.changes
            .iter()
            .filter(|x| x.approval.as_deref() == Some(approval))
            .map(|x| (x.secret.as_str(), x.grantee.as_deref().unwrap_or_default()))
            .collect()
    }

    fn grant_rows(c: &Connection, scope: &str, name: &str) -> Vec<(String, Level, Option<String>)> {
        let info = crate::secrets::get(c, scope, name).unwrap().unwrap();
        grants::list(c, &info)
            .unwrap()
            .into_iter()
            .map(|g| (g.grantee_label, g.level, g.env))
            .collect()
    }

    /// S8: a config import does not grant a value by itself. A new or a
    /// wider grant of a secret with a value waits for the operator; a
    /// grant of a placeholder applies.
    #[test]
    fn an_import_holds_new_and_wider_grants_of_a_secret_with_a_value() {
        let mut c = db();
        let k = MasterKey::generate();
        let gh = secret(
            &c,
            &k,
            GLOBAL,
            "gh",
            Kind::GithubToken,
            json!({"fields": {"value": github_token(), "host": "github.com"}}),
        );
        grant(&c, &gh, "project:q", Some(Level::Use), None).unwrap();
        let file = "version = 1\nscope = \"global\"\n\n\
                    [[secret]]\nname = \"gh\"\nkind = \"github_token\"\n\n\
                    [secret.fields]\nhost = \"github.com\"\n\n\
                    [[secret.grant]]\ngrantee = \"agent:p/a\"\nlevel = \"read\"\n\
                    env = \"GH_TOKEN\"\n\n\
                    [[secret.grant]]\ngrantee = \"project:q\"\nlevel = \"read\"\n\n\
                    [[secret]]\nname = \"new\"\nkind = \"token\"\n\n\
                    [[secret.grant]]\ngrantee = \"agent:p/a\"\nlevel = \"read\"\n\
                    env = \"NEW_TOKEN\"\n"
            .to_owned();
        let files = vec![(path_of(GLOBAL), file)];
        let wait = [("global/gh", "agent:p/a"), ("global/gh", "project:q")];

        // The plan shows the grants that wait.
        let p = plan(&c, &files, &[]).unwrap();
        assert!(p.needs_approval());
        assert_eq!(with_approval(&p.report, APPROVAL_NEEDED), wait);
        // Each grant shows its level and env, and the grant that it widens.
        let shown: Vec<_> = p
            .report
            .changes
            .iter()
            .filter(|x| x.approval.is_some())
            .map(|x| (x.level, x.env.as_deref(), x.was.clone()))
            .collect();
        assert_eq!(
            shown,
            [
                (Some(Level::Read), Some("GH_TOKEN"), None),
                (
                    Some(Level::Read),
                    None,
                    Some(GrantWas {
                        level: Level::Use,
                        env: None,
                        targets: vec![]
                    })
                ),
            ]
        );

        // An apply without the approval holds them. The grant of the
        // placeholder applies: it gives nothing.
        let r = run_with(&mut c, &files, false);
        assert_eq!(with_approval(&r, APPROVAL_HELD), wait, "{r:?}");
        assert_eq!(
            grant_rows(&c, GLOBAL, "gh"),
            [("project:q".to_owned(), Level::Use, None)]
        );
        assert_eq!(
            grant_rows(&c, GLOBAL, "new"),
            [(
                "agent:p/a".to_owned(),
                Level::Read,
                Some("NEW_TOKEN".to_owned())
            )]
        );

        // The operator approves them.
        let r = run_with(&mut c, &files, true);
        assert_eq!(with_approval(&r, APPROVAL_GIVEN), wait, "{r:?}");
        assert_eq!(
            grant_rows(&c, GLOBAL, "gh"),
            [
                (
                    "agent:p/a".to_owned(),
                    Level::Read,
                    Some("GH_TOKEN".to_owned())
                ),
                ("project:q".to_owned(), Level::Read, None),
            ]
        );
        let approved: i64 = c
            .query_row(
                "SELECT count(*) FROM audit WHERE action = 'secret.grant'
                 AND json_extract(detail, '$.approved') = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(approved, 2);

        // A narrower grant needs no approval.
        let narrow = "version = 1\nscope = \"global\"\n\n\
                      [[secret]]\nname = \"gh\"\nkind = \"github_token\"\n\n\
                      [secret.fields]\nhost = \"github.com\"\n\n\
                      [[secret.grant]]\ngrantee = \"project:q\"\nlevel = \"use\"\n"
            .to_owned();
        let p = plan(&c, &[(path_of(GLOBAL), narrow)], &[]).unwrap();
        assert!(!p.needs_approval(), "{:?}", p.report);
    }

    /// TER-142: a `run` grant goes into the file with its targets and comes
    /// back the same. A `run` grant with no target is skipped with a
    /// warning, and a new target is a wider grant (S8).
    #[test]
    fn run_grants_round_trip_and_a_new_target_is_wider() {
        let mut c = db();
        let k = MasterKey::generate();
        let s = secret(
            &c,
            &k,
            GLOBAL,
            "dk",
            Kind::ApiKey,
            json!({"fields": {"value": "dk_9fQ2mX7vL4tR8wN3pB6yH1cJ"}}),
        );
        grants::tests::grant_run(&c, &s, "agent:p/a", &["web", "box"]).unwrap();
        let files = files(&c).unwrap();
        let (_, text) = files.iter().find(|(p, _)| *p == path_of(GLOBAL)).unwrap();
        assert!(text.contains("level = \"run\"\ntargets = [\"box\", \"web\"]"), "{text}");
        // The same file plans nothing.
        let p = plan(&c, &files, &[]).unwrap();
        assert!(
            !p.report.changes.iter().any(|x| x.action == "grant"),
            "{:?}",
            p.report
        );
        let file = |targets: &str| {
            format!(
                "version = 1\nscope = \"global\"\n\n\
                 [[secret]]\nname = \"dk\"\nkind = \"api_key\"\n\n\
                 [[secret.grant]]\ngrantee = \"agent:p/a\"\nlevel = \"run\"\n{targets}"
            )
        };
        // A new target waits for the operator; fewer targets do not.
        let wide = vec![(path_of(GLOBAL), file("targets = [\"box\", \"ci\"]\n"))];
        let p = plan(&c, &wide, &[]).unwrap();
        assert!(p.needs_approval(), "{:?}", p.report);
        let g = p.report.changes.iter().find(|x| x.action == "grant").unwrap();
        assert_eq!(g.targets, ["box", "ci"]);
        assert_eq!(g.was.as_ref().unwrap().targets, ["box", "web"]);
        let narrow = vec![(path_of(GLOBAL), file("targets = [\"box\"]\n"))];
        let p = plan(&c, &narrow, &[]).unwrap();
        assert!(!p.needs_approval(), "{:?}", p.report);
        run_with(&mut c, &narrow, false);
        assert_eq!(
            grants::list(&c, &s).unwrap()[0].targets,
            ["box".to_owned()]
        );
        // No target: skipped with a warning; the stored grant stays.
        let none = vec![(path_of(GLOBAL), file(""))];
        let p = plan(&c, &none, &[]).unwrap();
        assert!(
            p.report.warnings.iter().any(|w| w.contains("`run` grant")),
            "{:?}",
            p.report
        );
    }
}
