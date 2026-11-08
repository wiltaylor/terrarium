//! The secrets of `terrarium run` (TER-142): `-e KEY=@secret:<name>` and
//! `-e KEY=@secret:<name>.<field>`.
//!
//! The CLI sends the reference as the env value. When a run starts
//! (`POST /api/agents/{id}/run`), the daemon reads the caller's grants
//! again ([`resolve`]), so a grant that the operator adds or removes while
//! the agent works applies at its next run:
//!
//! - a `read` grant: any target that the agent may use;
//! - a `run` grant: only a target that the grant names;
//! - a `use` grant, or no grant: refused.
//!
//! The value goes only into the env of the remote process
//! (`crate::executor::RemoteCmd::env`): never onto a command line, into
//! the sandbox, or into a record. The output of the run is redacted with
//! the run's own values first (`crate::executor::start_run_exec`), then
//! with the daemon matcher. Each value writes one `secret.run` audit row
//! (secret, field, env name, target, session, and run), with no value.
//!
//! The limit: the command that gets the value can still print it in a
//! form that no matcher knows (encoded, split, sent elsewhere). This
//! stops an accidental exposure in transcripts, logs, and argv, not a
//! hostile agent.

use std::collections::{BTreeMap, HashSet};

use rusqlite::Connection;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::grants::{self, Level};
use super::{MasterKey, open};
use crate::error::{ApiError, ApiResult};

/// The prefix of a secret reference in an env value.
pub const REF_PREFIX: &str = "@secret:";

/// One `KEY=@secret:<name>[.<field>]` of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// The env var name.
    pub env: String,
    /// The secret name: the project secret first, then the global one.
    pub name: String,
    /// `None`: the main secret field of the kind.
    pub field: Option<String>,
}

/// The secret references of a run's env: each value that starts with
/// [`REF_PREFIX`]. A secret name has no `.`, so the first `.` starts the
/// field.
pub fn parse_refs(env: &BTreeMap<String, String>) -> ApiResult<Vec<Ref>> {
    let mut out = Vec::new();
    for (k, v) in env {
        let Some(rest) = v.strip_prefix(REF_PREFIX) else {
            continue;
        };
        let (name, field) = match rest.split_once('.') {
            Some((n, f)) => (n, Some(f)),
            None => (rest, None),
        };
        if crate::store::check_name("secret", name).is_err() || field.is_some_and(str::is_empty) {
            return Err(ApiError::invalid(format!(
                "`{k}` is not a usable secret reference: write `{k}=@secret:<name>` or \
                 `{k}=@secret:<name>.<field>`"
            )));
        }
        out.push(Ref {
            env: k.clone(),
            name: name.to_owned(),
            field: field.map(str::to_owned),
        });
    }
    Ok(out)
}

/// One value that a run gets. `Debug` shows no value.
pub struct Injected {
    pub env: String,
    pub scope: String,
    pub name: String,
    pub field: String,
    /// The grant that allowed it: `run` or `read`.
    pub level: Level,
    pub value: Zeroizing<String>,
}

impl std::fmt::Debug for Injected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injected")
            .field("env", &self.env)
            .field("scope", &self.scope)
            .field("name", &self.name)
            .field("field", &self.field)
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

impl Injected {
    /// The redaction label: `run/<scope>/<name>` for a project secret,
    /// `run/<name>` for a global one.
    pub fn label(&self) -> String {
        format!("run/{}", super::redact::label(&self.scope, &self.name))
    }

    /// The detail of the `secret.run` audit row: no value.
    pub fn audit_detail(&self, agent: &str, target: &str, session: &str, wait: &str) -> Value {
        json!({"agent": agent, "scope": self.scope, "name": self.name,
               "field": self.field, "env": self.env, "level": self.level,
               "target": target, "session": session, "run": wait})
    }
}

/// Resolve the references `refs` of a run of the agent `agent_id` of
/// `project` on `target`, from the grants as they are now. `use_only`:
/// the tokens of the repos that go through the git proxy, which no run
/// gets.
pub fn resolve(
    conn: &Connection,
    key: &MasterKey,
    agent_id: &str,
    project: &str,
    target: &str,
    refs: &[Ref],
    use_only: &HashSet<(String, String)>,
) -> ApiResult<Vec<Injected>> {
    resolve_with(conn, key, target, refs, use_only, |name| grants::find(conn, agent_id, project, name))
}

/// Resolve only delivery grants. Agent and project grants cannot authorize a job.
pub fn resolve_delivery(
    conn: &Connection,
    key: &MasterKey,
    project: &str,
    target: &str,
    refs: &[Ref],
    use_only: &HashSet<(String, String)>,
) -> ApiResult<Vec<Injected>> {
    let all = grants::delivery_effective(conn, project)?;
    resolve_with(conn, key, target, refs, use_only, |name| {
        let found = all.iter().find(|e| e.name == name).cloned();
        if found.as_ref().is_some_and(|e| e.level != Level::Run) {
            return Err(ApiError::forbidden("delivery secrets require a target-limited run grant"));
        }
        Ok(found)
    })
}

fn resolve_with(
    conn: &Connection,
    key: &MasterKey,
    target: &str,
    refs: &[Ref],
    use_only: &HashSet<(String, String)>,
    find: impl Fn(&str) -> ApiResult<Option<grants::Effective>>,
) -> ApiResult<Vec<Injected>> {
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        let Some(e) = find(&r.name)? else {
            return Err(ApiError::not_found(format!(
                "`{}`: no secret `{}` is granted to you; `terrarium secret list` shows your grants",
                r.env, r.name
            )));
        };
        match e.level {
            Level::Use => {
                return Err(ApiError::forbidden(format!(
                    "`{}`: your grant of secret `{}/{}` is `use`: the brokers use it, and no run \
                     gets it; a run needs a `run` grant that names target `{target}`",
                    r.env, e.scope, e.name
                )));
            }
            Level::Run if !e.targets.iter().any(|t| t == target) => {
                return Err(ApiError::forbidden(format!(
                    "`{}`: your `run` grant of secret `{}/{}` names {}, not target `{target}`",
                    r.env,
                    e.scope,
                    e.name,
                    e.targets
                        .iter()
                        .map(|t| format!("`{t}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            Level::Run | Level::Read => {}
        }
        if use_only.contains(&(e.scope.clone(), e.name.clone())) {
            return Err(crate::git_proxy::use_only_error(&e.scope, &e.name));
        }
        if !e.has_value {
            return Err(ApiError::conflict(format!(
                "`{}`: secret `{}/{}` has no value yet (a placeholder from a config import)",
                r.env, e.scope, e.name
            )));
        }
        let field = match &r.field {
            Some(f) => f.clone(),
            None => grants::main_field(e.kind, &e.secret_fields).ok_or_else(|| {
                ApiError::invalid(format!(
                    "`{}`: secret `{}/{}` has several secret fields; name one: \
                     `@secret:{}.<field>` with one of {}",
                    r.env,
                    e.scope,
                    e.name,
                    e.name,
                    e.secret_fields.join(", ")
                ))
            })?,
        };
        let value = match e.fields.get(&field) {
            Some(v) => Zeroizing::new(v.as_str().map_or_else(|| v.to_string(), str::to_owned)),
            None => {
                let opened = open(conn, key, &e.scope, &e.name)?
                    .ok_or_else(|| ApiError::not_found("the secret is gone"))?;
                let Some(v) = opened.values.get(&field) else {
                    let mut all: Vec<&str> = e.fields.keys().map(String::as_str).collect();
                    all.extend(e.secret_fields.iter().map(String::as_str));
                    return Err(ApiError::invalid(format!(
                        "`{}`: secret `{}/{}` has no field `{field}`; its fields are {}",
                        r.env,
                        e.scope,
                        e.name,
                        all.join(", ")
                    )));
                };
                v.clone()
            }
        };
        out.push(Injected {
            env: r.env.clone(),
            scope: e.scope,
            name: e.name,
            field,
            level: e.level,
            value,
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::error::Code;
    use crate::secrets::grants::tests::{AGENT, db, grant, grant_run, secret};
    use crate::secrets::{GLOBAL, Kind};
    use serde_json::json;

    const VALUE: &str = "dpl_8sK2mQ7vX4tR9wL3nB6yH1cJ";

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn references_parse() {
        let r = parse_refs(&env(&[
            ("A", "1"),
            ("B", "@secret:deploy"),
            ("C", "@secret:db.pass.word"),
        ]))
        .unwrap();
        assert_eq!(
            r,
            [
                Ref {
                    env: "B".into(),
                    name: "deploy".into(),
                    field: None
                },
                Ref {
                    env: "C".into(),
                    name: "db".into(),
                    field: Some("pass.word".into())
                },
            ]
        );
        for bad in ["@secret:", "@secret:Bad", "@secret:a/b", "@secret:x."] {
            assert!(parse_refs(&env(&[("K", bad)])).is_err(), "{bad}");
        }
    }

    #[test]
    fn delivery_grants_are_isolated_from_workers_and_other_projects() {
        let c = db();
        let k = MasterKey::generate();
        let s = secret(&c, &k, "p", "deploy", Kind::ApiKey, json!({"fields":{"value":VALUE}}));
        let refs = parse_refs(&env(&[("KEY", "@secret:deploy")])).unwrap();
        let none = HashSet::new();
        grant_run(&c, &s, "delivery:p", &["box"]).unwrap();
        assert!(resolve(&c, &k, AGENT, "p", "box", &refs, &none).is_err());
        assert!(resolve_delivery(&c, &k, "q", "box", &refs, &none).is_err());
        assert!(resolve_delivery(&c, &k, "p", "web", &refs, &none).is_err());
        let got = resolve_delivery(&c, &k, "p", "box", &refs, &none).unwrap();
        assert_eq!(got[0].value.as_str(), VALUE);
        assert!(!format!("{got:?}").contains(VALUE));
        assert!(grant(&c, &s, "delivery:p", Some(Level::Read), None).is_err());
        assert!(grant(&c, &s, "delivery:p", Some(Level::Use), None).is_err());
        grants::on_project_deleted(&c, "p").unwrap();
        assert!(resolve_delivery(&c, &k, "p", "box", &refs, &none).is_err());
        // Ordinary project grants never become delivery grants.
        grant_run(&c, &s, "project:p", &["box"]).unwrap();
        assert!(resolve_delivery(&c, &k, "p", "box", &refs, &none).is_err());
    }

    #[test]
    fn a_run_grant_resolves_on_its_targets_only() {
        let c = db();
        let k = MasterKey::generate();
        let s = secret(
            &c,
            &k,
            "p",
            "deploy",
            Kind::ApiKey,
            json!({"fields": {"value": VALUE}}),
        );
        let refs = parse_refs(&env(&[("DEPLOY", "@secret:deploy")])).unwrap();
        let none = HashSet::new();
        // No grant.
        let e = resolve(&c, &k, AGENT, "p", "box", &refs, &none).unwrap_err();
        assert_eq!(e.code, Code::NotFound, "{}", e.message);
        // `use`: refused.
        grant(&c, &s, "agent:p/a", Some(Level::Use), None).unwrap();
        let e = resolve(&c, &k, AGENT, "p", "box", &refs, &none).unwrap_err();
        assert_eq!(e.code, Code::Forbidden, "{}", e.message);
        // `run` on box: box yes, web no.
        grant_run(&c, &s, "agent:p/a", &["box"]).unwrap();
        let got = resolve(&c, &k, AGENT, "p", "box", &refs, &none).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].value.as_str(), VALUE);
        assert_eq!(
            (got[0].env.as_str(), got[0].field.as_str()),
            ("DEPLOY", "value")
        );
        assert_eq!(got[0].label(), "run/p/deploy");
        assert!(!format!("{got:?}").contains(VALUE));
        let d = got[0].audit_detail(AGENT, "box", "S1", "W1");
        assert!(!d.to_string().contains(VALUE), "{d}");
        assert_eq!(d["run"], "W1");
        assert_eq!(d["target"], "box");
        let e = resolve(&c, &k, AGENT, "p", "web", &refs, &none).unwrap_err();
        assert_eq!(e.code, Code::Forbidden);
        assert!(e.message.contains("`box`"), "{}", e.message);
        assert!(!e.message.contains(VALUE));
        // A `read` grant of the project lifts the limit.
        grant(&c, &s, "project:p", Some(Level::Read), None).unwrap();
        assert!(resolve(&c, &k, AGENT, "p", "web", &refs, &none).is_ok());
        // A use-only token (git proxy) is refused.
        let only: HashSet<(String, String)> = [("p".to_owned(), "deploy".to_owned())].into();
        assert!(resolve(&c, &k, AGENT, "p", "box", &refs, &only).is_err());
    }

    #[test]
    fn fields_of_a_generic_secret() {
        let c = db();
        let k = MasterKey::generate();
        let s = secret(
            &c,
            &k,
            GLOBAL,
            "db",
            Kind::Generic,
            json!({
                "fields": {"user": "app", "password": "pw-7Hq2Lm9Xv4Rt8Wn3", "host": "h-2Kd9"},
                "secret_fields": ["password", "host"]
            }),
        );
        grant_run(&c, &s, "agent:p/a", &["box"]).unwrap();
        let none = HashSet::new();
        // Several secret fields: the reference names one.
        let refs = parse_refs(&env(&[("DB", "@secret:db")])).unwrap();
        let e = resolve(&c, &k, AGENT, "p", "box", &refs, &none).unwrap_err();
        assert_eq!(e.code, Code::Invalid, "{}", e.message);
        let refs = parse_refs(&env(&[
            ("DB_PW", "@secret:db.password"),
            ("DB_USER", "@secret:db.user"),
        ]))
        .unwrap();
        let got = resolve(&c, &k, AGENT, "p", "box", &refs, &none).unwrap();
        let vals: Vec<(&str, &str)> = got
            .iter()
            .map(|i| (i.env.as_str(), i.value.as_str()))
            .collect();
        assert_eq!(vals, [("DB_PW", "pw-7Hq2Lm9Xv4Rt8Wn3"), ("DB_USER", "app")]);
        let refs = parse_refs(&env(&[("X", "@secret:db.nope")])).unwrap();
        assert!(resolve(&c, &k, AGENT, "p", "box", &refs, &none).is_err());
    }
}
