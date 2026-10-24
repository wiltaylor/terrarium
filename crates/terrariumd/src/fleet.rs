//! Fleet files in the daemon (PLAN §20, Phase 6; `docs/fleet.md`): who
//! owns which object, the plan of a fleet against the store, and the
//! apply.
//!
//! A fleet owns the objects it made or took over. The ownership lives in
//! the `fleet_objects` table, with the state of each object right after
//! the last apply. The object rows show the owner as `source =
//! fleet:<name>`. Such a row is not file-managed, so the API can still
//! change it; `plan` then shows the change as drift, and the next apply
//! writes the file values again (the file is the truth). An object with
//! `source = file` belongs to its definition file, and a fleet that
//! declares it gets a conflict.
//!
//! Migration 9 in `db.rs` makes the tables.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::Role;
use terrarium_core::config::{AgentDef, ProjectDef, RepoDef, SubscriptionDef, TargetDef};
use terrarium_core::fleet::{self, Fleet, Issue, Kind};

use crate::actor::Actor;
use crate::api::Shared;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::store::agents::{self, AgentPatch, AgentView};
use crate::store::projects::{
    self, NewProject, NewRepo, ProjectPatch, SOURCE_API, SOURCE_FILE, fleet_source,
};
use crate::store::{Page, repos, subscriptions, targets};

/// The `source` word of an object a fleet owns: `fleet:<name>`.
pub use crate::store::projects::SOURCE_FLEET as SOURCE_PREFIX;

/// What a plan does with one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Create,
    Update,
    Unchanged,
    /// An agent or target that the fleet owns and the file no longer
    /// declares. Only `--prune` deletes it.
    Delete,
    /// A project or subscription that the fleet owns and the file no longer
    /// declares. `--prune` gives it up: it stays as an API object, with
    /// its board, cards, and samples.
    Release,
    /// Another owner has the object: a definition file or another fleet.
    Conflict,
}

/// One field that an apply changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub field: String,
    pub from: Value,
    pub to: Value,
}

/// One object of a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: Kind,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub action: Action,
    /// The fields that changed outside the fleet since the last apply, or
    /// `["(deleted)"]` when the object is gone. The apply writes the file
    /// values again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<String>,
    /// The object exists and no fleet owns it yet: the apply takes it over.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adopt: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
    /// Why the entry is a conflict, or a note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// An agent with a live session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,
    /// After an apply: `done` or `skipped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The agent ID, for the role and self checks.
    #[serde(skip)]
    pub id: Option<String>,
    /// The roles that the apply grants or revokes.
    #[serde(skip)]
    pub roles_touched: Vec<Role>,
    /// The policy entries that the apply adds and that need a sysadmin
    /// (`policy::policy_grants`).
    #[serde(skip)]
    pub policy_grants: Vec<String>,
}

/// The counts of a plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub create: usize,
    pub update: usize,
    pub unchanged: usize,
    pub delete: usize,
    pub release: usize,
    pub conflict: usize,
    pub drift: usize,
}

/// A fleet file against the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub fleet: String,
    /// No errors: the plan can be applied (when it has no conflicts).
    pub valid: bool,
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
    pub entries: Vec<Entry>,
    pub summary: Summary,
}

impl Plan {
    fn invalid(fleet: String, issues: Vec<Issue>) -> Self {
        let (errors, warnings) = issues.into_iter().partition(Issue::is_error);
        Self {
            fleet,
            valid: false,
            errors,
            warnings,
            entries: Vec::new(),
            summary: Summary::default(),
        }
    }

    fn count(&mut self) {
        let mut s = Summary::default();
        for e in &self.entries {
            match e.action {
                Action::Create => s.create += 1,
                Action::Update => s.update += 1,
                Action::Unchanged => s.unchanged += 1,
                Action::Delete => s.delete += 1,
                Action::Release => s.release += 1,
                Action::Conflict => s.conflict += 1,
            }
            if !e.drift.is_empty() {
                s.drift += 1;
            }
        }
        self.summary = s;
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.action == Action::Conflict)
    }

    /// The `delete` and `release` entries.
    pub fn removals(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.action, Action::Delete | Action::Release))
    }
}

/// An ownership row.
#[derive(Debug, Clone)]
struct Owned {
    kind: Kind,
    key: String,
    fleet: String,
    project: Option<String>,
    applied: Value,
}

fn kind_from(s: &str) -> Option<Kind> {
    serde_json::from_value(json!(s)).ok()
}

fn owned_rows(conn: &Connection, fleet: Option<&str>) -> ApiResult<Vec<Owned>> {
    let mut stmt = conn.prepare(
        "SELECT kind, key, fleet, project, applied FROM fleet_objects
         WHERE ?1 IS NULL OR fleet = ?1 ORDER BY kind, key",
    )?;
    let rows = stmt
        .query_map([fleet], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(kind, key, fleet, project, applied)| {
            Some(Owned {
                kind: kind_from(&kind)?,
                key,
                fleet,
                project,
                applied: serde_json::from_str(&applied).unwrap_or(Value::Null),
            })
        })
        .collect())
}

fn owner(conn: &Connection, kind: Kind, key: &str) -> ApiResult<Option<Owned>> {
    Ok(owned_rows(conn, None)?
        .into_iter()
        .find(|o| o.kind == kind && o.key == key))
}

fn record(
    t: &Tx<'_>,
    fleet: &str,
    kind: Kind,
    key: &str,
    project: Option<&str>,
    applied: &Value,
) -> ApiResult<()> {
    t.execute(
        "INSERT INTO fleet_objects (kind, key, fleet, project, applied, applied_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (kind, key) DO UPDATE SET fleet = excluded.fleet,
             project = excluded.project, applied = excluded.applied,
             applied_at = excluded.applied_at",
        params![
            kind.as_str(),
            key,
            fleet,
            project,
            applied.to_string(),
            now()
        ],
    )?;
    label(t, kind, key, Some(fleet))
}

fn forget(t: &Tx<'_>, kind: Kind, key: &str) -> ApiResult<()> {
    t.execute(
        "DELETE FROM fleet_objects WHERE kind = ?1 AND key = ?2",
        params![kind.as_str(), key],
    )?;
    label(t, kind, key, None)
}

/// Keep the `source` column of an object equal to its owner:
/// `fleet:<name>` while `fleet` owns it, `api` when the fleet gives it up.
/// A `file` row stays `file`: the definition file wins.
fn label(t: &Tx<'_>, kind: Kind, key: &str, fleet: Option<&str>) -> ApiResult<()> {
    let (table, key_expr) = match kind {
        Kind::Project => ("projects", "name"),
        Kind::Agent => ("agents", "project || '/' || name"),
        Kind::Target => ("targets", "name"),
        Kind::Subscription => ("subscriptions", "name"),
    };
    match fleet {
        Some(f) => t.execute(
            &format!("UPDATE {table} SET source = ?2 WHERE {key_expr} = ?1 AND source <> 'file'"),
            params![key, fleet_source(f)],
        )?,
        None => t.execute(
            &format!(
                "UPDATE {table} SET source = ?2 WHERE {key_expr} = ?1 AND source GLOB 'fleet:*'"
            ),
            params![key, SOURCE_API],
        )?,
    };
    Ok(())
}

/// The fleet that owns an object, as a `source` word: `fleet:<name>`.
pub fn source_of(conn: &Connection, kind: Kind, key: &str) -> ApiResult<Option<String>> {
    Ok(owner(conn, kind, key)?.map(|o| fleet_source(&o.fleet)))
}

// ---- The comparable state of each kind ("spec") ----

fn repo_spec(name: &str, url: &str, branch: Option<&str>, cred: Option<&str>, path: &str) -> Value {
    json!({
        "name": name, "url": url, "default_branch": branch,
        "credential": cred, "path": path,
    })
}

fn desired_repos(def: &ProjectDef) -> Vec<Value> {
    let mut v: Vec<Value> = def
        .repos
        .iter()
        .map(|r: &RepoDef| {
            repo_spec(
                &r.name,
                &r.url,
                r.default_branch.as_deref(),
                r.credential.as_deref(),
                r.path.as_deref().unwrap_or(&r.name),
            )
        })
        .collect();
    v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    v
}

fn all_repos(conn: &Connection, project: &str) -> ApiResult<Vec<repos::Repo>> {
    let mut out = Vec::new();
    let mut page = Page {
        limit: Some(Page::MAX),
        cursor: None,
    };
    loop {
        let p = repos::list_repos(conn, project, &page)?;
        out.extend(p.items);
        match p.next {
            Some(c) => page.cursor = Some(c),
            None => return Ok(out),
        }
    }
}

/// The live state of a project: the repos with the names in `names`, and
/// the checkout list of the last apply (the store has no checkout list).
fn live_project(
    conn: &Connection,
    p: &projects::Project,
    names: &BTreeSet<String>,
    checkout: &Value,
) -> ApiResult<Value> {
    let mut repos: Vec<Value> = all_repos(conn, &p.name)?
        .into_iter()
        .filter(|r| names.contains(&r.name))
        .map(|r| {
            repo_spec(
                &r.name,
                &r.url,
                r.default_branch.as_deref(),
                r.credential.as_deref(),
                &r.path,
            )
        })
        .collect();
    repos.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({
        "description": p.description,
        "prefix": p.card_prefix,
        "repos": repos,
        "checkout": checkout,
    }))
}

fn desired_project(def: &ProjectDef, live_prefix: Option<&str>) -> Value {
    json!({
        "description": def.description.clone().unwrap_or_default(),
        "prefix": def.prefix.as_deref().or(live_prefix),
        "repos": desired_repos(def),
        "checkout": def.checkout.on_agent_create,
    })
}

fn role_list(roles: &[Role]) -> Vec<Role> {
    let mut r: Vec<Role> = roles
        .iter()
        .copied()
        .filter(|r| *r != Role::Member)
        .collect();
    r.sort_by_key(|r| serde_json::to_string(r).unwrap_or_default());
    r.dedup();
    r
}

fn agent_spec(def: &AgentDef) -> Value {
    json!({
        "harness": def.harness,
        "subscription": def.subscription,
        "model": def.model,
        "prompt": def.prompt,
        "roles": role_list(&def.roles),
        "policy": def.policy,
        "restart": def.restart,
        "new_session_on_prompt_change": def.new_session_on_prompt_change,
    })
}

fn view_def(v: &AgentView) -> AgentDef {
    let a = &v.agent;
    AgentDef {
        name: a.name.clone(),
        project: a.project.clone(),
        harness: a.harness,
        subscription: a.subscription.clone(),
        model: a.model.clone(),
        prompt: a.prompt.clone(),
        roles: a.roles.clone(),
        policy: a.policy.clone(),
        restart: a.restart,
        new_session_on_prompt_change: a.new_session_on_prompt_change,
    }
}

fn target_spec(def: &TargetDef) -> Value {
    let mut v = serde_json::to_value(def).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("name");
    }
    v
}

fn subscription_spec(def: &SubscriptionDef) -> Value {
    json!({
        "provider": def.provider,
        "plan": def.plan,
        "harnesses": def.harnesses,
        "probe": def.probe,
        "alert_at": def.alert_at,
    })
}

fn live_subscription(s: &subscriptions::Subscription) -> Value {
    json!({
        "provider": s.provider,
        "plan": s.plan,
        "harnesses": s.harnesses,
        "probe": s.probe,
        "alert_at": s.alert_at,
    })
}

/// The fields of two specs that differ.
fn diff(to: &Value, from: &Value) -> Vec<Change> {
    let keys: BTreeSet<&String> = to
        .as_object()
        .into_iter()
        .chain(from.as_object())
        .flat_map(|o| o.keys())
        .collect();
    keys.into_iter()
        .filter_map(|k| {
            let (a, b) = (&from[k.as_str()], &to[k.as_str()]);
            (a != b).then(|| Change {
                field: k.clone(),
                from: a.clone(),
                to: b.clone(),
            })
        })
        .collect()
}

fn find_agent(conn: &Connection, project: &str, name: &str) -> ApiResult<Option<AgentView>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM agents WHERE project = ?1 AND name = ?2",
            params![project, name],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| agents::get(conn, &id)).transpose()
}

/// What a plan knows of one live object.
struct Live {
    spec: Value,
    source: String,
}

fn file_of(kind: Kind, key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key);
    format!("{}s/{name}.toml", kind.as_str())
}

/// Make one entry from the desired spec, the live object, and its owner.
fn classify(
    fleet: &str,
    kind: Kind,
    key: &str,
    project: Option<&str>,
    desired: &Value,
    live: Option<Live>,
    owner: Option<&Owned>,
) -> Entry {
    let mut e = Entry {
        kind,
        key: key.to_owned(),
        project: project.map(str::to_owned),
        action: Action::Unchanged,
        drift: Vec::new(),
        adopt: false,
        changes: Vec::new(),
        reason: None,
        live: false,
        result: None,
        id: None,
        roles_touched: Vec::new(),
        policy_grants: Vec::new(),
    };
    if let Some(o) = owner.filter(|o| o.fleet != fleet) {
        e.action = Action::Conflict;
        e.reason = Some(format!("owned by fleet `{}`", o.fleet));
        return e;
    }
    let Some(live) = live else {
        e.action = Action::Create;
        if owner.is_some() {
            e.drift = vec!["(deleted)".into()];
        }
        e.changes = diff(desired, &Value::Null);
        return e;
    };
    if live.source == SOURCE_FILE {
        e.action = Action::Conflict;
        e.reason = Some(format!(
            "declared in {}; the definition file wins",
            file_of(kind, key)
        ));
        return e;
    }
    if let Some(o) = owner {
        e.drift = diff(&live.spec, &o.applied)
            .into_iter()
            .map(|c| c.field)
            .collect();
    } else {
        e.adopt = true;
    }
    e.changes = diff(desired, &live.spec);
    e.action = if e.changes.is_empty() {
        Action::Unchanged
    } else {
        Action::Update
    };
    e
}

/// The plan of `fleet` against the store. The caller checked the file.
pub fn plan(conn: &Connection, f: &Fleet) -> ApiResult<Plan> {
    let all_owned = owned_rows(conn, None)?;
    let owner_of = |kind: Kind, key: &str| {
        all_owned
            .iter()
            .find(|o| o.kind == kind && o.key == key)
            .cloned()
    };
    let mut entries = Vec::new();

    for s in &f.subscriptions {
        let live = subscriptions::find(conn, &s.name)?.map(|l| Live {
            spec: live_subscription(&l),
            source: l.source,
        });
        let o = owner_of(Kind::Subscription, &s.name);
        entries.push(classify(
            &f.name,
            Kind::Subscription,
            &s.name,
            None,
            &subscription_spec(s),
            live,
            o.as_ref(),
        ));
    }
    for p in &f.projects {
        let o = owner_of(Kind::Project, &p.def.name);
        let live_row = projects::find(conn, &p.def.name)?;
        let applied = o.as_ref().map(|o| &o.applied);
        let mut names: BTreeSet<String> = p.def.repos.iter().map(|r| r.name.clone()).collect();
        if let Some(a) = applied {
            names.extend(
                a["repos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r["name"].as_str().map(str::to_owned)),
            );
        }
        let checkout = applied.map_or_else(|| json!([]), |a| a["checkout"].clone());
        let live = match &live_row {
            Some(row) => Some(Live {
                spec: live_project(conn, row, &names, &checkout)?,
                source: row.source.clone(),
            }),
            None => None,
        };
        let desired = desired_project(&p.def, live_row.as_ref().map(|r| r.card_prefix.as_str()));
        entries.push(classify(
            &f.name,
            Kind::Project,
            &p.def.name,
            Some(&p.def.name),
            &desired,
            live,
            o.as_ref(),
        ));
    }
    for t in f.all_targets() {
        let live = targets::find(conn, &t.name)?.map(|r| Live {
            spec: target_spec(&r.def),
            source: r.source,
        });
        let o = owner_of(Kind::Target, &t.name);
        entries.push(classify(
            &f.name,
            Kind::Target,
            &t.name,
            t.project.as_deref(),
            &target_spec(t),
            live,
            o.as_ref(),
        ));
    }
    for a in f.agents() {
        let key = fleet::agent_key(&a.def);
        let view = find_agent(conn, &a.def.project, &a.def.name)?;
        let live = view.as_ref().map(|v| Live {
            spec: agent_spec(&view_def(v)),
            source: v.source.clone(),
        });
        let o = owner_of(Kind::Agent, &key);
        let mut e = classify(
            &f.name,
            Kind::Agent,
            &key,
            Some(&a.def.project),
            &agent_spec(&a.def),
            live,
            o.as_ref(),
        );
        let want = role_list(&a.def.roles);
        let have = view
            .as_ref()
            .map(|v| role_list(&v.agent.roles))
            .unwrap_or_default();
        e.roles_touched = want
            .iter()
            .filter(|r| !have.contains(r))
            .chain(have.iter().filter(|r| !want.contains(r)))
            .copied()
            .collect();
        let project = a.def.project.as_str();
        let project_target = |n: &str| {
            f.all_targets()
                .any(|t| t.name == n && t.project.as_deref() == Some(project))
                || agents::project_target(conn, project, n)
        };
        e.policy_grants = policy::policy_grants(
            project,
            view.as_ref().map(|v| &v.agent.policy),
            &a.def.policy,
            &project_target,
        );
        if let Some(v) = &view {
            e.id = Some(v.agent.id.to_string());
            e.live = v.agent.status.is_live();
        }
        entries.push(e);
    }

    // Objects the fleet owns and the file no longer declares.
    let declared: BTreeSet<(Kind, String)> =
        f.objects().into_iter().map(|o| (o.kind, o.key)).collect();
    let mut gone: Vec<&Owned> = all_owned
        .iter()
        .filter(|o| o.fleet == f.name && !declared.contains(&(o.kind, o.key.clone())))
        .collect();
    // Remove in the reverse order of the apply: agents first.
    gone.sort_by_key(|o| std::cmp::Reverse(o.kind));
    for o in gone {
        let mut e = classify(
            &f.name,
            o.kind,
            &o.key,
            o.project.as_deref(),
            &Value::Null,
            None,
            None,
        );
        e.changes.clear();
        let (exists, source, id, live) = match o.kind {
            Kind::Agent => {
                let (p, n) = o.key.split_once('/').unwrap_or(("", &o.key));
                match find_agent(conn, p, n)? {
                    Some(v) => (
                        true,
                        v.source.clone(),
                        Some(v.agent.id.to_string()),
                        v.agent.status.is_live(),
                    ),
                    None => (false, String::new(), None, false),
                }
            }
            Kind::Target => match targets::find(conn, &o.key)? {
                Some(r) => (true, r.source, None, false),
                None => (false, String::new(), None, false),
            },
            Kind::Project => match projects::find(conn, &o.key)? {
                Some(r) => (true, r.source, None, false),
                None => (false, String::new(), None, false),
            },
            Kind::Subscription => match subscriptions::find(conn, &o.key)? {
                Some(r) => (true, r.source, None, false),
                None => (false, String::new(), None, false),
            },
        };
        e.id = id;
        e.live = live;
        e.action = match o.kind {
            Kind::Agent | Kind::Target if exists && source != SOURCE_FILE => Action::Delete,
            _ => Action::Release,
        };
        if !exists {
            e.reason = Some("the object is gone; the apply drops the ownership".into());
        } else if source == SOURCE_FILE {
            e.reason = Some(format!(
                "a definition file took it over ({})",
                file_of(o.kind, &o.key)
            ));
        }
        entries.push(e);
    }

    let mut plan = Plan {
        fleet: f.name.clone(),
        valid: true,
        errors: Vec::new(),
        warnings: Vec::new(),
        entries,
        summary: Summary::default(),
    };
    plan.count();
    Ok(plan)
}

// ---- Checks that need the store ----

/// The prompt templates directory: `TERRARIUM_PROMPTS_DIR`, else `/prompts`.
pub fn prompts_dir() -> PathBuf {
    std::env::var_os("TERRARIUM_PROMPTS_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("/prompts"), PathBuf::from)
}

/// The checks of a fleet against the store: the references to
/// subscriptions and targets outside the file, the prompt templates, and
/// the rules of each kind.
pub fn check_against(conn: &Connection, f: &Fleet, prompts: &Path) -> ApiResult<Vec<Issue>> {
    let mut issues = Vec::new();
    let refs = f.references();
    for (name, users) in &refs.subscriptions {
        if subscriptions::find(conn, name)?.is_none() {
            issues.push(Issue::error(
                "subscriptions",
                format!(
                    "{} name subscription `{name}`; define it in [[subscriptions]] or in the daemon",
                    users.join(", ")
                ),
            ));
        }
    }
    for (name, users) in &refs.targets {
        match targets::find(conn, name)? {
            None => issues.push(Issue::error(
                "targets",
                format!(
                    "{} list target `{name}` in policy.targets; it is neither in the fleet nor in the daemon",
                    users.join(", ")
                ),
            )),
            Some(r) => {
                if let Some(tp) = &r.def.project {
                    for u in users.iter().filter(|u| !u.starts_with(&format!("{tp}/"))) {
                        issues.push(Issue::error(
                            "targets",
                            format!(
                                "{u} lists target `{name}`, a target of project `{tp}`; an agent uses global targets and those of its own project"
                            ),
                        ));
                    }
                }
            }
        }
    }
    for (name, users) in &refs.prompts {
        if name == "default" || name.contains('/') || name.contains("..") {
            if name != "default" {
                issues.push(Issue::error(
                    "prompts",
                    format!("the prompt template name `{name}` is not valid"),
                ));
            }
            continue;
        }
        if !prompts.is_dir() {
            issues.push(Issue::warning(
                "prompts",
                format!(
                    "cannot check prompt template `{name}`: {} is not a directory",
                    prompts.display()
                ),
            ));
        } else if !prompts.join(format!("{name}.md")).is_file() {
            issues.push(Issue::error(
                "prompts",
                format!(
                    "{} use prompt template `{name}`, which is not in {}",
                    users.join(", "),
                    prompts.display()
                ),
            ));
        }
    }
    for t in f.all_targets() {
        if let Err(e) = targets::validate(t) {
            let at = match &t.project {
                Some(p) => format!("projects.{p}.targets.{}", t.name),
                None => format!("targets.{}", t.name),
            };
            issues.push(Issue::error(at, e.message));
        }
    }
    for s in &f.subscriptions {
        if let Err(e) = subscriptions::check_name(&s.name) {
            issues.push(Issue::error(format!("subscriptions.{}", s.name), e.message));
        }
        if s.provider.trim().is_empty() {
            issues.push(Issue::error(
                format!("subscriptions.{}", s.name),
                "the subscription needs a provider",
            ));
        }
    }
    for a in f.agents() {
        if let Some(name) = &a.def.subscription
            && !f.subscriptions.iter().any(|s| s.name == *name)
            && let Some(s) = subscriptions::find(conn, name)?
            && !s.harnesses.is_empty()
            && !s.harnesses.contains(&a.def.harness)
        {
            issues.push(Issue::warning(
                format!("projects.{}.agents.{}", a.def.project, a.def.name),
                format!(
                    "subscription `{name}` lists harnesses {:?}, not `{}`",
                    s.harnesses.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
                    a.def.harness
                ),
            ));
        }
    }
    Ok(issues)
}

/// Parse, check, and plan a fleet file in one read.
pub fn plan_text(
    conn: &Connection,
    text: &str,
    fallback: Option<&str>,
) -> ApiResult<(Option<Fleet>, Plan)> {
    let parsed = fleet::parse(text, fallback);
    let mut issues = parsed.issues;
    let Some(f) = parsed.fleet else {
        return Ok((None, Plan::invalid(String::new(), issues)));
    };
    if !issues.iter().any(Issue::is_error) {
        issues.extend(check_against(conn, &f, &prompts_dir())?);
    }
    if issues.iter().any(Issue::is_error) {
        return Ok((Some(f.clone()), Plan::invalid(f.name, issues)));
    }
    let mut p = plan(conn, &f)?;
    p.warnings = issues;
    Ok((Some(f), p))
}

// ---- Policy ----

/// May `actor` apply this plan? PLAN §10: the operator and sysadmins may
/// apply any fleet; a project_admin may apply a fleet that touches only
/// its own project (no new projects, no global targets, no
/// subscriptions); a member may not apply.
///
/// The objects that the fleet owns now and the file no longer declares
/// count too, with `prune` or not (`docs/security-review.md` F10): an
/// apply replaces the stored content, so a later prune of it deletes
/// them. Only a prune checks that an agent does not delete itself.
pub fn authorize(actor: &Actor, plan: &Plan, prune: bool) -> ApiResult<()> {
    if let Actor::Agent(a) = actor
        && !a.is_sysadmin()
        && !a.roles.contains(&Role::ProjectAdmin)
    {
        return Err(ApiError::forbidden(
            "planning or applying a fleet needs project_admin, sysadmin, or the operator",
        ));
    }
    for e in &plan.entries {
        if e.action == Action::Conflict {
            continue;
        }
        let removal = matches!(e.action, Action::Delete | Action::Release);
        let check = || -> ApiResult<()> {
            match e.kind {
                Kind::Subscription => {
                    policy::manage_subscriptions(actor, "declaring a subscription in a fleet")
                }
                Kind::Project if e.action == Action::Create => {
                    policy::sysadmin(actor, "creating a project from a fleet")
                }
                Kind::Project => {
                    policy::admin_project(actor, &e.key, "changing a project from a fleet")
                }
                Kind::Target => policy::define_target(actor, e.project.as_deref()),
                Kind::Agent => {
                    let project = e.project.as_deref().unwrap_or_default();
                    policy::admin_project(actor, project, "managing agents from a fleet")?;
                    if prune
                        && e.action == Action::Delete
                        && let (Some(me), Some(id)) = (actor.agent(), &e.id)
                        && me.id == *id
                    {
                        return Err(ApiError::forbidden("an agent cannot delete itself"));
                    }
                    if !removal {
                        for role in &e.roles_touched {
                            policy::change_role(
                                actor,
                                e.id.as_deref().unwrap_or(""),
                                project,
                                *role,
                            )?;
                        }
                        policy::grant_policy(actor, &e.policy_grants)?;
                    }
                    Ok(())
                }
            }
        };
        match check() {
            Err(err) if removal && !prune => {
                return Err(ApiError::forbidden(format!(
                    "fleet `{}` owns {} `{}`, which the file drops; changing the fleet: {}",
                    plan.fleet, e.kind, e.key, err.message
                )));
            }
            r => r?,
        }
    }
    Ok(())
}

/// The stored record of a fleet: `(origin, applied_by)`.
fn stored(conn: &Connection, name: &str) -> ApiResult<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT origin, applied_by FROM fleets WHERE name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// May `actor` write `f` over the stored fleet of the same name
/// (`docs/security-review.md` F10)? [`authorize`] checks the objects; this
/// checks the record:
///
/// - A fleet that declares nothing replaces a stored fleet only for a
///   sysadmin or the operator.
/// - An API apply (`origin = Some("api")`) over a fleet from a file needs
///   `confirm`: the file is the source, and the next reload applies it
///   again. `None` (a plan) skips this check.
pub fn check_overwrite(
    conn: &Connection,
    actor: &Actor,
    f: &Fleet,
    origin: Option<&str>,
    confirm: bool,
) -> ApiResult<()> {
    let Some((was, by)) = stored(conn, &f.name)? else {
        return Ok(());
    };
    if f.objects().is_empty() {
        policy::sysadmin(
            actor,
            &format!(
                "replacing fleet `{}` (applied by {by}) with a fleet that declares nothing",
                f.name
            ),
        )?;
    }
    if let Some(origin) = origin
        && origin == "api"
        && was.starts_with("file:")
        && !confirm
    {
        return Err(ApiError::new(
            Code::ConfirmRequired,
            format!(
                "fleet `{}` comes from {was}; an API apply replaces its record until the next \
                 reload; add confirm to apply it anyway",
                f.name
            ),
        )
        .with_details(json!({"fleet": f.name, "origin": was, "applied_by": by})));
    }
    Ok(())
}

// ---- Apply ----

/// What an apply changed in the store.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub plan: Option<Plan>,
    /// The agents that the apply made: (ID, project).
    pub created_agents: Vec<(String, String)>,
}

/// Where a fleet came from.
#[derive(Debug, Clone)]
pub struct Origin {
    /// `api` or `file:<path>`.
    pub origin: String,
    pub content: String,
}

fn conflict_error(plan: &Plan) -> ApiError {
    let list: Vec<String> = plan
        .conflicts()
        .map(|e| {
            format!(
                "{} {}: {}",
                e.kind,
                e.key,
                e.reason.as_deref().unwrap_or("")
            )
        })
        .collect();
    ApiError::conflict(format!(
        "fleet `{}` has {} conflict(s): {}",
        plan.fleet,
        list.len(),
        list.join("; ")
    ))
    .with_details(serde_json::to_value(plan).unwrap_or(Value::Null))
}

/// Apply a checked fleet in the transaction `t`: plan again, check the
/// policy, then create, update, and (with `prune`) delete. Any failure
/// rolls the whole apply back.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    t: &mut Tx<'_>,
    f: &Fleet,
    origin: &Origin,
    actor: &Actor,
    prune: bool,
    confirm: bool,
    state_dir: &Path,
) -> ApiResult<Applied> {
    let mut plan = plan(t, f)?;
    authorize(actor, &plan, prune)?;
    check_overwrite(t, actor, f, Some(&origin.origin), confirm)?;
    let previous = stored(t, &f.name)?;
    if plan.conflicts().next().is_some() {
        return Err(conflict_error(&plan));
    }
    let mut out = Applied::default();
    let name = f.name.clone();

    let source = fleet_source(&name);
    for s in &f.subscriptions {
        subscriptions::upsert_def(t, s, &source)?;
        let live = subscriptions::get(t, &s.name)?;
        record(
            t,
            &name,
            Kind::Subscription,
            &s.name,
            None,
            &live_subscription(&live),
        )?;
    }
    for p in &f.projects {
        apply_project(t, &name, &p.def)?;
    }
    for d in f.all_targets() {
        apply_target(t, d)?;
        let live = targets::get(t, &d.name)?;
        record(
            t,
            &name,
            Kind::Target,
            &d.name,
            d.project.as_deref(),
            &target_spec(&live.def),
        )?;
    }
    for a in f.agents() {
        let key = fleet::agent_key(&a.def);
        let id = match find_agent(t, &a.def.project, &a.def.name)? {
            Some(v) => {
                apply_agent(t, &v, &a.def)?;
                v.agent.id.to_string()
            }
            None => {
                let v = agents::create(t, &a.def, state_dir, &source)?;
                let id = v.agent.id.to_string();
                out.created_agents.push((id.clone(), a.def.project.clone()));
                id
            }
        };
        let live = agents::get(t, &id)?;
        record(
            t,
            &name,
            Kind::Agent,
            &key,
            Some(&a.def.project),
            &agent_spec(&view_def(&live)),
        )?;
    }
    for e in plan.entries.iter_mut() {
        let removal = matches!(e.action, Action::Delete | Action::Release);
        if removal && !prune {
            e.result = Some("skipped".into());
            continue;
        }
        if e.action == Action::Delete {
            match e.kind {
                Kind::Agent => {
                    if let Some(id) = &e.id {
                        agents::delete(t, id)?;
                    }
                }
                Kind::Target => {
                    targets::delete(t, &e.key)?;
                }
                _ => {}
            }
        }
        if removal {
            forget(t, e.kind, &e.key)?;
        }
        e.result = Some("done".into());
    }
    t.execute(
        "INSERT INTO fleets (name, description, origin, content, applied_at, applied_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (name) DO UPDATE SET description = excluded.description,
             origin = excluded.origin, content = excluded.content,
             applied_at = excluded.applied_at, applied_by = excluded.applied_by",
        params![
            name,
            f.description,
            origin.origin,
            origin.content,
            now(),
            t.actor.clone()
        ],
    )?;
    t.audit(
        "fleet.apply",
        &name,
        f.projects.first().map(|p| p.def.name.as_str()),
        json!({
            "origin": origin.origin,
            "prune": prune,
            "summary": plan.summary,
            "previous_origin": previous.as_ref().map(|p| &p.0),
            "previous_applied_by": previous.as_ref().map(|p| &p.1),
        }),
    )?;
    out.plan = Some(plan);
    Ok(out)
}

fn apply_project(t: &mut Tx<'_>, fleet_name: &str, def: &ProjectDef) -> ApiResult<()> {
    let prior = owner(t, Kind::Project, &def.name)?;
    let mut managed: BTreeSet<String> = def.repos.iter().map(|r| r.name.clone()).collect();
    if let Some(o) = &prior {
        managed.extend(
            o.applied["repos"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r["name"].as_str().map(str::to_owned)),
        );
    }
    match projects::find(t, &def.name)? {
        None => {
            projects::create(
                t,
                &NewProject {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    card_prefix: def.prefix.clone(),
                },
                &fleet_source(fleet_name),
            )?;
        }
        Some(p) => {
            let description = def.description.clone().unwrap_or_default();
            let prefix = def.prefix.clone().filter(|x| *x != p.card_prefix);
            if description != p.description || prefix.is_some() {
                projects::update(
                    t,
                    &def.name,
                    &ProjectPatch {
                        description: Some(description),
                        card_prefix: prefix,
                    },
                )?;
            }
        }
    }
    // Repos: the file list replaces the repos the fleet applied before,
    // and takes over API repos with the same name. Equal repos stay, so
    // their IDs stay.
    let wanted = desired_repos(def);
    let live = all_repos(t, &def.name)?;
    for name in &managed {
        let want = wanted.iter().find(|w| w["name"] == name.as_str());
        let have = live.iter().find(|r| r.name == *name).map(|r| {
            repo_spec(
                &r.name,
                &r.url,
                r.default_branch.as_deref(),
                r.credential.as_deref(),
                &r.path,
            )
        });
        if want == have.as_ref() {
            continue;
        }
        t.execute(
            "DELETE FROM repos WHERE project = ?1 AND name = ?2",
            params![def.name, name],
        )?;
        if let Some(r) = def.repos.iter().find(|r| r.name == *name) {
            projects::attach_repo(
                t,
                &def.name,
                &NewRepo {
                    name: Some(r.name.clone()),
                    url: r.url.clone(),
                    default_branch: r.default_branch.clone(),
                    credential: r.credential.clone(),
                    path: r.path.clone(),
                },
                SOURCE_API,
            )?;
        }
    }
    let row = projects::get(t, &def.name)?;
    let names: BTreeSet<String> = def.repos.iter().map(|r| r.name.clone()).collect();
    let spec = live_project(t, &row, &names, &json!(def.checkout.on_agent_create))?;
    record(
        t,
        fleet_name,
        Kind::Project,
        &def.name,
        Some(&def.name),
        &spec,
    )
}

fn apply_target(t: &mut Tx<'_>, def: &TargetDef) -> ApiResult<()> {
    match targets::find(t, &def.name)? {
        None => {
            targets::create(t, def)?;
        }
        Some(r) if r.def != *def => {
            targets::validate(def)?;
            if let Some(p) = &def.project {
                projects::get(t, p)?;
            }
            t.execute(
                "UPDATE targets SET project = ?2, kind = ?3, def = ?4, updated_at = ?5 WHERE name = ?1",
                params![
                    def.name,
                    def.project,
                    targets::kind_str(def.kind),
                    target_spec(def).to_string(),
                    now()
                ],
            )?;
            t.audit(
                "target.update",
                &def.name,
                def.project.as_deref(),
                json!({"source": "fleet"}),
            )?;
        }
        Some(_) => {}
    }
    Ok(())
}

fn apply_agent(t: &mut Tx<'_>, v: &AgentView, def: &AgentDef) -> ApiResult<()> {
    let id = v.agent.id.to_string();
    if agent_spec(&view_def(v)) == agent_spec(def) {
        return Ok(());
    }
    let patch = AgentPatch {
        harness: Some(def.harness),
        subscription: Some(def.subscription.clone()),
        model: Some(def.model.clone()),
        prompt: Some(def.prompt.clone()),
        policy: Some(def.policy.clone()),
        restart: Some(def.restart),
        new_session_on_prompt_change: Some(def.new_session_on_prompt_change),
    };
    agents::write_def(t, &id, &patch)?;
    let want = role_list(&def.roles);
    if role_list(&v.agent.roles) != want {
        agents::set_roles(t, &id, &def.project, &want)?;
    }
    t.audit(
        "agent.update",
        &id,
        Some(&def.project),
        json!({"source": "fleet", "harness": def.harness, "prompt": def.prompt}),
    )?;
    Ok(())
}

// ---- Reads ----

/// A row of `GET /api/fleet`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetRow {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub origin: String,
    pub applied_at: String,
    pub applied_by: String,
    /// The objects the fleet owns.
    pub objects: usize,
    /// The projects of those objects.
    pub projects: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

pub fn list(conn: &Connection) -> ApiResult<Vec<FleetRow>> {
    let owned = owned_rows(conn, None)?;
    let mut stmt = conn.prepare(
        "SELECT name, description, origin, applied_at, applied_by FROM fleets ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(FleetRow {
                name: r.get(0)?,
                description: r.get(1)?,
                origin: r.get(2)?,
                applied_at: r.get(3)?,
                applied_by: r.get(4)?,
                objects: 0,
                projects: Vec::new(),
                content: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|mut row| {
            let mine: Vec<&Owned> = owned.iter().filter(|o| o.fleet == row.name).collect();
            row.objects = mine.len();
            row.projects = mine
                .iter()
                .filter_map(|o| o.project.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            row
        })
        .collect())
}

pub fn get(conn: &Connection, name: &str) -> ApiResult<FleetRow> {
    let mut row = list(conn)?
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| ApiError::not_found(format!("no fleet `{name}`")))?;
    row.content = conn
        .query_row("SELECT content FROM fleets WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(row)
}

/// A fleet that declares one project as it is now: its repos, agents, and
/// project targets. Subscriptions and global targets stay references.
pub fn export(
    conn: &Connection,
    project: &str,
    name: &str,
    checkout: Vec<String>,
) -> ApiResult<Fleet> {
    let p = projects::get(conn, project)?;
    let checkout = match owner(conn, Kind::Project, project)? {
        Some(o) if checkout.is_empty() => {
            serde_json::from_value(o.applied["checkout"].clone()).unwrap_or_default()
        }
        _ => checkout,
    };
    let repos = all_repos(conn, project)?
        .into_iter()
        .map(|r| RepoDef {
            path: (r.path != r.name).then(|| r.path.clone()),
            name: r.name,
            url: r.url,
            default_branch: r.default_branch,
            credential: r.credential,
        })
        .collect();
    let mut agent_views = Vec::new();
    let mut page = Page {
        limit: Some(Page::MAX),
        cursor: None,
    };
    loop {
        let got = agents::list(conn, Some(project), &page)?;
        agent_views.extend(got.items);
        match got.next {
            Some(c) => page.cursor = Some(c),
            None => break,
        }
    }
    agent_views.sort_by(|a, b| a.agent.name.cmp(&b.agent.name));
    let targets = targets::list(conn)?
        .into_iter()
        .filter(|r| r.def.project.as_deref() == Some(project))
        .map(|r| r.def)
        .collect();
    Ok(Fleet {
        version: fleet::FLEET_VERSION,
        name: name.to_owned(),
        description: (!p.description.is_empty()).then(|| p.description.clone()),
        defaults: fleet::FleetDefaults::default(),
        subscriptions: Vec::new(),
        targets: Vec::new(),
        projects: vec![fleet::FleetProject {
            def: ProjectDef {
                name: p.name.clone(),
                description: (!p.description.is_empty()).then_some(p.description),
                prefix: Some(p.card_prefix),
                repos,
                checkout: terrarium_core::config::CheckoutDef {
                    on_agent_create: checkout,
                },
            },
            agents: agent_views
                .iter()
                .map(|v| fleet::FleetAgent {
                    def: view_def(v),
                    start: false,
                })
                .collect(),
            targets,
        }],
    })
}

// ---- The config dir ----

/// Apply the fleet files of the config dir (`*.fleet.toml` and
/// `fleets/*.toml`) as the system: no prune, no start, no clone. Each
/// fleet is one transaction. The answer is the list of errors, for the
/// sync report of `POST /api/reload`.
pub async fn apply_config_dir(state: &Shared) -> Vec<String> {
    let dir = state.config_dir.clone();
    let files = fleet::files_in(&dir);
    let mut errors = Vec::new();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in files {
        let rel = path
            .strip_prefix(&dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{rel}: cannot read: {e}"));
                continue;
            }
        };
        let fallback = fleet::name_from_path(&path);
        let parsed = fleet::parse(&text, fallback.as_deref());
        let f = match parsed.ok() {
            Ok(f) => f,
            Err(issues) => {
                errors.extend(
                    issues
                        .iter()
                        .filter(|i| i.is_error())
                        .map(|i| format!("{rel}: {i}")),
                );
                continue;
            }
        };
        if let Some(first) = seen.get(&f.name) {
            errors.push(format!(
                "{rel}: fleet `{}` is declared in {} already",
                f.name,
                first.strip_prefix(&dir).unwrap_or(first).display()
            ));
            continue;
        }
        seen.insert(f.name.clone(), path.clone());
        let origin = Origin {
            origin: format!("file:{rel}"),
            content: text,
        };
        let state_dir = state.state_dir.clone();
        let result = state
            .db
            .write("system", move |t| {
                let issues = check_against(t, &f, &prompts_dir())?;
                let bad: Vec<String> = issues
                    .iter()
                    .filter(|i| i.is_error())
                    .map(ToString::to_string)
                    .collect();
                if !bad.is_empty() {
                    return Err(ApiError::new(Code::Invalid, bad.join("; ")));
                }
                apply(t, &f, &origin, &Actor::Operator, false, false, &state_dir)
            })
            .await;
        match result {
            Ok(a) => {
                if let Some(p) = a.plan {
                    tracing::info!(
                        fleet = %p.fleet,
                        create = p.summary.create,
                        update = p.summary.update,
                        drift = p.summary.drift,
                        "fleet file applied"
                    );
                }
            }
            Err(e) => errors.push(format!("{rel}: {}", e.message)),
        }
    }
    errors
}
