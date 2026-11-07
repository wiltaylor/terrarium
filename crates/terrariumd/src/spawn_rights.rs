//! The rights that a spawn gives an ephemeral agent beyond its template
//! (TER-65): targets, secret grants, and the account pool. Each one comes
//! from the owner, and none goes past what the owner holds at the spawn.
//!
//! - **Targets** (`targets`): a target in the owner's `policy.targets` goes
//!   into the child's `policy.targets`. A target that the owner holds by
//!   an active lease gives the child a lease that ends with the owner's
//!   lease or with the child's lifetime, whichever comes first. An
//!   exclusive lease of the owner is `409`: it has room for one holder.
//!   Any other target is `403`, with the target named.
//! - **Grants** (`grants`): `<name>` or `<scope>/<name>`, with `:use`,
//!   `:run`, `:run=<target>,<target>`, or `:read`. The owner must hold the
//!   secret (its own grant or its project's) at that level or higher
//!   (`use` < `run` < `read`). With no level, the child gets the kind's
//!   default level, at most the owner's. A `read` grant keeps the owner's
//!   env var name. A `run` grant (TER-142) does not go past the owner's
//!   targets: an owner with `run` gives a subset of its own run targets
//!   (all of them when the ask names none); an owner with `read` names the
//!   targets, each one it may use (policy or lease). The child's grants go
//!   with its row at the close (`grants::on_agent_deleted`).
//! - **Pool**: a template with no `subscription` and no
//!   `subscription_pool` takes the owner's account and pool when the owner
//!   runs the same harness, so the quota switch covers the child.
//!
//! The operator as owner holds every right; its spawns are checked only
//! for targets and secrets that exist and that the project can reach.

use rusqlite::Connection;
use serde_json::{Value, json};
use terrarium_core::Role;
use terrarium_core::config::{AgentDef, Policy};

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::secrets::grants::{self, Grantee, Level, NewGrant};
use crate::secrets::{self, GLOBAL};
use crate::store::agents::AgentView;
use crate::store::leases;

/// At most this many `targets` and `grants` each in one spawn.
pub const MAX_ASKS: usize = 32;

/// One secret that a spawn asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAsk {
    /// `global` or a project; `None`: the project secret first, then the
    /// global one.
    pub scope: Option<String>,
    pub name: String,
    /// `None`: the kind's default, at most the owner's level.
    pub level: Option<Level>,
    /// The targets of a `run` ask (`:run=<t1>,<t2>`). Empty: the owner's
    /// run targets.
    pub targets: Vec<String>,
}

impl GrantAsk {
    fn label(&self) -> String {
        match &self.scope {
            Some(s) => format!("{s}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// Read `<name>`, `<scope>/<name>`, each with an optional `:use`,
/// `:run`, `:run=<target>,<target>`, or `:read`.
pub fn parse_grant(s: &str) -> ApiResult<GrantAsk> {
    let s = s.trim();
    let bad = || {
        ApiError::invalid(format!(
            "grant `{s}` is not `<name>`, `<scope>/<name>`, `<name>:<level>`, or \
             `<scope>/<name>:<level>` (level `use`, `run`, `run=<target>,<target>`, or `read`)"
        ))
    };
    let mut targets = Vec::new();
    let (path, level) = match s.rsplit_once(':') {
        Some((p, l)) => {
            let l = match l.split_once('=') {
                Some(("run", ts)) => {
                    targets = ts
                        .split(',')
                        .map(|t| t.trim().to_owned())
                        .collect::<Vec<_>>();
                    if targets.iter().any(String::is_empty) {
                        return Err(bad());
                    }
                    "run"
                }
                Some(_) => return Err(bad()),
                None => l,
            };
            (p, Some(Level::parse(l).ok_or_else(bad)?))
        }
        None => (s, None),
    };
    let (scope, name) = match path.split_once('/') {
        Some((sc, n)) => (Some(sc.to_owned()), n),
        None => (None, path),
    };
    if name.is_empty() || name.contains('/') || scope.as_deref().is_some_and(str::is_empty) {
        return Err(bad());
    }
    Ok(GrantAsk {
        scope,
        name: name.to_owned(),
        level,
        targets,
    })
}

/// Clean a list of names: trimmed, no empty one, no repeat, at most
/// [`MAX_ASKS`].
pub fn clean_list(what: &str, list: &[String]) -> ApiResult<Vec<String>> {
    if list.len() > MAX_ASKS {
        return Err(ApiError::invalid(format!(
            "a spawn takes at most {MAX_ASKS} {what}; it got {}",
            list.len()
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    for s in list {
        let s = s.trim();
        if s.is_empty() {
            return Err(ApiError::invalid(format!("an entry of `{what}` is empty")));
        }
        if !out.iter().any(|o| o == s) {
            out.push(s.to_owned());
        }
    }
    Ok(out)
}

/// The owner of a spawn, as the rights see it: `None` for the operator.
pub type OwnerView<'a> = Option<&'a AgentView>;

fn owner_label(owner: OwnerView<'_>) -> String {
    match owner {
        Some(v) => format!("`{}/{}`", v.agent.project, v.agent.name),
        None => "the operator".to_owned(),
    }
}

/// How the child gets one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetGrant {
    /// Into its `policy.targets`.
    Policy(String),
    /// A lease that ends with the owner's lease or the child's lifetime.
    Lease {
        target: String,
        target_project: Option<String>,
    },
}

impl TargetGrant {
    pub fn name(&self) -> &str {
        match self {
            Self::Policy(t) | Self::Lease { target: t, .. } => t,
        }
    }
}

/// Check the `targets` of a spawn into `project` against the owner.
pub fn check_targets(
    conn: &Connection,
    owner: OwnerView<'_>,
    project: &str,
    targets: &[String],
) -> ApiResult<Vec<TargetGrant>> {
    let mut out = Vec::with_capacity(targets.len());
    for name in targets {
        let row = crate::store::targets::find(conn, name)?
            .ok_or_else(|| ApiError::not_found(format!("no target `{name}`")))?;
        let tp = row.def.project.clone();
        if let Some(p) = &tp
            && p != project
        {
            return Err(ApiError::forbidden(format!(
                "target `{name}` belongs to project `{p}`; an agent of `{project}` cannot use it"
            )));
        }
        let Some(v) = owner else {
            out.push(TargetGrant::Policy(name.clone()));
            continue;
        };
        if v.agent.policy.targets.iter().any(|t| t == name) {
            out.push(TargetGrant::Policy(name.clone()));
            continue;
        }
        let id = v.agent.id.to_string();
        let lease = leases::active_of(conn, &id)?
            .into_iter()
            .find(|l| l.target == *name);
        match lease {
            Some(l) if l.exclusive => {
                return Err(ApiError::conflict(format!(
                    "{} holds an exclusive lease on target `{name}`: it has room for one \
                     holder, so the ephemeral agent cannot share it",
                    owner_label(owner)
                )));
            }
            Some(_) => out.push(TargetGrant::Lease {
                target: name.clone(),
                target_project: tp,
            }),
            None => {
                return Err(ApiError::forbidden(format!(
                    "spawning with target `{name}` needs the owner's right to use it: {} has \
                     it neither in its policy.targets nor by an active lease",
                    owner_label(owner)
                )));
            }
        }
    }
    Ok(out)
}

/// One secret grant that the child gets.
#[derive(Debug, Clone)]
pub struct SecretGrant {
    pub info: secrets::SecretInfo,
    pub level: Level,
    pub env: Option<String>,
    /// The targets of a `run` grant, sorted. Empty for the other levels.
    pub targets: Vec<String>,
}

/// Check the `grants` of a spawn into `project` against the owner.
pub fn check_grants(
    conn: &Connection,
    owner: OwnerView<'_>,
    project: &str,
    asks: &[GrantAsk],
) -> ApiResult<Vec<SecretGrant>> {
    let mut out: Vec<SecretGrant> = Vec::with_capacity(asks.len());
    for ask in asks {
        if let Some(s) = &ask.scope
            && s != GLOBAL
            && s != project
        {
            return Err(ApiError::forbidden(format!(
                "secret `{}` is a secret of project `{s}`; an agent of `{project}` cannot hold it",
                ask.label()
            )));
        }
        let g = match owner {
            None => operator_grant(conn, project, ask)?,
            Some(v) => owner_grant(conn, v, ask)?,
        };
        if out.iter().any(|o| o.info.id == g.info.id) {
            return Err(ApiError::invalid(format!(
                "secret `{}/{}` is in `grants` twice",
                g.info.scope, g.info.name
            )));
        }
        out.push(g);
    }
    Ok(out)
}

fn operator_grant(conn: &Connection, project: &str, ask: &GrantAsk) -> ApiResult<SecretGrant> {
    let scopes: Vec<&str> = match &ask.scope {
        Some(s) => vec![s.as_str()],
        None => vec![project, GLOBAL],
    };
    for sc in scopes {
        if let Some(info) = secrets::get(conn, sc, &ask.name)? {
            let level = ask.level.unwrap_or_else(|| Level::default_for(info.kind));
            let targets = grants::check_run_targets(level, &ask.targets)?;
            grants::check_targets_exist(conn, project, &targets)?;
            return Ok(SecretGrant {
                info,
                level,
                env: None,
                targets,
            });
        }
    }
    Err(ApiError::not_found(format!("no secret `{}`", ask.label())))
}

fn owner_grant(conn: &Connection, v: &AgentView, ask: &GrantAsk) -> ApiResult<SecretGrant> {
    let id = v.agent.id.to_string();
    let held = grants::effective(conn, &id, &v.agent.project)?
        .into_iter()
        .find(|e| e.name == ask.name && ask.scope.as_deref().is_none_or(|s| s == e.scope));
    let Some(e) = held else {
        return Err(ApiError::forbidden(format!(
            "spawning with secret `{}` needs the owner to hold it: `{}/{}` has no grant of it",
            ask.label(),
            v.agent.project,
            v.agent.name
        )));
    };
    let level = match ask.level {
        Some(l) if l > e.level => {
            return Err(ApiError::forbidden(format!(
                "spawning with secret `{}/{}` at level `{l}` needs the owner to hold that \
                 level: `{}/{}` holds `{}`",
                e.scope, e.name, v.agent.project, v.agent.name, e.level
            )));
        }
        Some(l) => l,
        None => Level::default_for(e.kind).min(e.level),
    };
    let info = secrets::get(conn, &e.scope, &e.name)?
        .ok_or_else(|| ApiError::not_found(format!("no secret `{}/{}`", e.scope, e.name)))?;
    let targets = if level == Level::Run {
        run_targets(conn, v, &e, ask)?
    } else {
        grants::check_run_targets(level, &ask.targets)?
    };
    Ok(SecretGrant {
        info,
        level,
        env: e.env.filter(|_| level == Level::Read),
        targets,
    })
}

/// The targets of a child's `run` grant of the secret `e` that the owner
/// `v` holds (TER-142): never past the owner's targets. An owner with
/// `run` gives its own run targets, or the subset that the ask names. An
/// owner with `read` has no target limit on the secret, so the ask names
/// the targets, and each one is a target that the owner may use (its
/// `policy.targets` or an active lease).
fn run_targets(
    conn: &Connection,
    v: &AgentView,
    e: &grants::Effective,
    ask: &GrantAsk,
) -> ApiResult<Vec<String>> {
    let who = format!("`{}/{}`", v.agent.project, v.agent.name);
    if e.level == Level::Run {
        if ask.targets.is_empty() {
            return grants::check_run_targets(Level::Run, &e.targets);
        }
        let want = grants::check_run_targets(Level::Run, &ask.targets)?;
        if let Some(t) = want.iter().find(|t| !e.targets.contains(t)) {
            return Err(ApiError::forbidden(format!(
                "spawning with secret `{}/{}` at level `run` on target `{t}` needs the owner's \
                 run grant on it: {who} may run it on {} only",
                e.scope,
                e.name,
                e.targets.join(", ")
            )));
        }
        return Ok(want);
    }
    if ask.targets.is_empty() {
        return Err(ApiError::invalid(format!(
            "{who} holds secret `{}/{}` at `{}`, which names no targets: name the targets of \
             the child's grant, `{}:run=<target>,<target>`",
            e.scope, e.name, e.level, e.name
        )));
    }
    let want = grants::check_run_targets(Level::Run, &ask.targets)?;
    grants::check_targets_exist(conn, &v.agent.project, &want)?;
    let id = v.agent.id.to_string();
    let leased = leases::active_of(conn, &id)?;
    for t in &want {
        let usable =
            v.agent.policy.targets.iter().any(|p| p == t) || leased.iter().any(|l| l.target == *t);
        if !usable {
            return Err(ApiError::forbidden(format!(
                "spawning with secret `{}/{}` at level `run` on target `{t}` needs the owner's \
                 right to use that target: {who} has it neither in its policy.targets nor by \
                 an active lease",
                e.scope, e.name
            )));
        }
    }
    Ok(want)
}

/// Give the owner's account and pool to a child whose template sets none
/// (TER-65 point 4).
pub fn inherit_pool(def: &mut AgentDef, owner: OwnerView<'_>) {
    let Some(v) = owner else { return };
    if def.subscription.is_some() || !def.subscription_pool.is_empty() {
        return;
    }
    if v.agent.harness != def.harness {
        return;
    }
    def.subscription = v.agent.subscription.clone();
    def.subscription_pool = v.agent.subscription_pool.clone();
}

/// Keep `policy.thread_open` (the right `thread:open`, TER-93) on a child
/// only when its owner holds it: the operator (`None`), a sysadmin, or an
/// owner with `thread_open`. The template files take no `thread_open` key
/// today; this keeps the rule of TER-65 (no right past the owner's) if a
/// definition ever carries it.
pub fn limit_thread_open(policy: &mut Policy, owner: Option<(&Policy, &[Role])>) {
    if let Some((p, roles)) = owner
        && !p.thread_open
        && !roles.contains(&Role::Sysadmin)
    {
        policy.thread_open = false;
    }
}

/// Write the leases and the grants of the new child `child` (`name` in
/// `project`), whose lifetime ends at `deadline`. The answer is the audit
/// part: the targets and the grants, with no secret value.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    t: &mut Tx<'_>,
    owner: OwnerView<'_>,
    child: &str,
    name: &str,
    project: &str,
    deadline: &str,
    targets: &[TargetGrant],
    secret_grants: &[SecretGrant],
) -> ApiResult<Value> {
    let mut leased = Vec::new();
    for tg in targets {
        let TargetGrant::Lease {
            target,
            target_project,
        } = tg
        else {
            continue;
        };
        // The owner's lease, read again in this write.
        let ends = match owner {
            Some(v) => {
                let id = v.agent.id.to_string();
                let l = leases::active_of(t, &id)?
                    .into_iter()
                    .find(|l| l.target == *target)
                    .ok_or_else(|| {
                        ApiError::forbidden(format!(
                            "the owner's lease on target `{target}` ended during the spawn"
                        ))
                    })?;
                l.expires_at
            }
            None => None,
        };
        let expires_at = match ends {
            Some(e) if e.as_str() < deadline => e,
            _ => deadline.to_owned(),
        };
        let l = leases::grant(
            t,
            &leases::Grant {
                target: target.clone(),
                target_project: target_project.clone(),
                agent: child.to_owned(),
                agent_project: project.to_owned(),
                expires_at: Some(expires_at),
                exclusive: false,
                reason: Some(format!("spawned by {}", owner_label(owner))),
            },
        )?;
        leased.push(json!({"target": l.target, "lease": l.id, "expires_at": l.expires_at}));
    }
    let by = t.actor.clone();
    let grantee = Grantee::Agent {
        id: child.to_owned(),
        project: project.to_owned(),
        name: name.to_owned(),
    };
    let mut granted = Vec::new();
    for g in secret_grants {
        let (row, _) = grants::put(
            t,
            &g.info,
            &NewGrant {
                grantee: &grantee,
                level: Some(g.level),
                env: g.env.as_deref(),
                targets: &g.targets,
                by: &by,
            },
        )?;
        t.audit(
            "secret.grant",
            &format!("{}/{}", row.scope, row.name),
            secrets::scope_project(&row.scope),
            json!({
                "scope": row.scope, "name": row.name, "kind": g.info.kind,
                "grantee": row.grantee, "grantee_label": row.grantee_label,
                "level": row.level, "env": row.env, "targets": row.targets, "created": true,
                "spawn_owner": owner.map(|v| v.agent.id.to_string()),
            }),
        )?;
        let mut one = json!({"scope": row.scope, "name": row.name, "level": row.level});
        if !row.targets.is_empty() {
            one["targets"] = json!(row.targets);
        }
        granted.push(one);
    }
    let policy: Vec<&str> = targets
        .iter()
        .filter(|t| matches!(t, TargetGrant::Policy(_)))
        .map(TargetGrant::name)
        .collect();
    Ok(json!({"targets": policy, "leases": leased, "grants": granted}))
}

/// End the leases of the closed child `child` (its grants go with its
/// row).
pub fn on_close(t: &mut Tx<'_>, child: &str) -> ApiResult<()> {
    for l in leases::active_of(t, child)? {
        leases::revoke(t, &l.target, child)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn grants_parse() {
        assert_eq!(
            parse_grant("gh").unwrap(),
            GrantAsk {
                scope: None,
                name: "gh".into(),
                level: None,
                targets: vec![]
            }
        );
        assert_eq!(
            parse_grant("global/gh:read").unwrap(),
            GrantAsk {
                scope: Some("global".into()),
                name: "gh".into(),
                level: Some(Level::Read),
                targets: vec![]
            }
        );
        assert_eq!(parse_grant("p/k:use").unwrap().level, Some(Level::Use));
        // TER-142: `run`, with or without its targets.
        let r = parse_grant("p/k:run").unwrap();
        assert_eq!((r.level, r.targets.len()), (Some(Level::Run), 0));
        let r = parse_grant("k:run=box, web").unwrap();
        assert_eq!(r.level, Some(Level::Run));
        assert_eq!(r.targets, ["box", "web"]);
        for bad in [
            "",
            ":read",
            "gh:write",
            "/gh",
            "a/b/c",
            "p/",
            "k:run=",
            "k:run=a,,b",
            "k:read=box",
            "k:use=box",
        ] {
            assert!(parse_grant(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn thread_open_stays_only_when_the_owner_holds_it() {
        let on = Policy {
            thread_open: true,
            ..Policy::default()
        };
        let off = Policy::default();
        let child = |owner: Option<(&Policy, &[Role])>| {
            let mut p = on.clone();
            limit_thread_open(&mut p, owner);
            p.thread_open
        };
        assert!(child(None));
        assert!(child(Some((&on, &[]))));
        assert!(!child(Some((&off, &[]))));
        assert!(!child(Some((&off, &[Role::ProjectAdmin]))));
        assert!(child(Some((&off, &[Role::Sysadmin]))));
    }

    #[test]
    fn lists_are_cleaned() {
        let l = clean_list("targets", &["a".into(), " a ".into(), "b".into()]).unwrap();
        assert_eq!(l, ["a", "b"]);
        assert!(clean_list("targets", &[" ".into()]).is_err());
        let many: Vec<String> = (0..=MAX_ASKS).map(|i| i.to_string()).collect();
        assert!(clean_list("targets", &many).is_err());
    }
}
