//! Fleet files: one TOML file that declares projects, their agents and
//! targets, global targets, and subscriptions at once (PLAN §20, Phase 6).
//! `docs/fleet.md` is the reference.
//!
//! A fleet file reuses the definition structs of [`crate::config`] as
//! nested tables, so there is one schema:
//!
//! ```toml
//! version = 1
//! name = "terrarium"
//!
//! [defaults]                 # fields for each agent that does not set them
//! harness = "claude"
//! start = false
//!
//! [[projects]]               # a ProjectDef
//! name = "terrarium"
//!
//! [[projects.agents]]        # an AgentDef without `project`, plus `start`
//! name = "lead"
//! roles = ["project_admin"]
//!
//! [[projects.targets]]       # a TargetDef without `project`
//! name = "buildbox"
//! kind = "ssh"
//! address = "build@host"
//! ```
//!
//! This module parses and checks a file without the daemon. The daemon
//! resolves the references to objects outside the file (subscriptions,
//! global targets, prompt templates) and diffs the fleet against its store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{AgentDef, Policy, ProjectDef, RestartPolicy, SubscriptionDef, TargetDef};
use crate::harness::HarnessId;
use crate::role::Role;

/// The schema version that this build reads.
pub const FLEET_VERSION: u32 = 1;

/// The file name suffix of a fleet file in the config dir.
pub const FLEET_SUFFIX: &str = ".fleet.toml";

/// Names that collide with the fixed routes under `/api/fleet/`.
pub const RESERVED_NAMES: [&str; 4] = ["validate", "plan", "apply", "export"];

const TOP_KEYS: &[&str] = &[
    "version",
    "name",
    "description",
    "defaults",
    "subscriptions",
    "targets",
    "projects",
];
const PROJECT_KEYS: &[&str] = &[
    "name",
    "description",
    "prefix",
    "repos",
    "checkout",
    "agents",
    "targets",
    "schedules",
];
const AGENT_KEYS: &[&str] = &[
    "name",
    "harness",
    "subscription",
    "model",
    "prompt",
    "roles",
    "policy",
    "restart",
    "new_session_on_prompt_change",
    "start",
];
const POLICY_KEYS: &[&str] = &["targets", "mail_to", "cross_project"];
const REPO_KEYS: &[&str] = &["name", "url", "default_branch", "credential", "path"];
const CHECKOUT_KEYS: &[&str] = &["on_agent_create"];
const TARGET_KEYS: &[&str] = &[
    "name",
    "kind",
    "project",
    "host",
    "address",
    "port",
    "credential",
    "image",
    "container",
    "lab",
    "machine",
    "ephemeral",
    "keep",
    "snapshot_before_task",
];
const SUBSCRIPTION_KEYS: &[&str] = &["name", "provider", "plan", "harnesses", "probe", "alert_at"];
const SCHEDULE_KEYS: &[&str] = &["name", "enabled", "catch_up", "spec", "action"];

/// Values for each agent that does not set them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_session_on_prompt_change: Option<bool>,
    /// Start the agent on `fleet apply --start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<bool>,
}

impl FleetDefaults {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// An agent of a fleet project: the agent definition and the start flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetAgent {
    /// `project` is the enclosing project; the defaults are applied.
    pub def: AgentDef,
    /// Start the agent on `fleet apply --start`.
    pub start: bool,
}

/// A project of a fleet, with its agents and project targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetProject {
    pub def: ProjectDef,
    pub agents: Vec<FleetAgent>,
    /// `project` is the enclosing project.
    pub targets: Vec<TargetDef>,
}

/// A parsed fleet file, with the defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fleet {
    pub version: u32,
    pub name: String,
    pub description: Option<String>,
    pub defaults: FleetDefaults,
    /// Subscriptions that the fleet defines. A subscription that an agent
    /// only names must exist in the daemon.
    pub subscriptions: Vec<SubscriptionDef>,
    /// Global targets.
    pub targets: Vec<TargetDef>,
    pub projects: Vec<FleetProject>,
}

/// How bad an [`Issue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

/// A problem in a fleet file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub level: Level,
    /// Where in the file: `projects.terrarium.agents.lead`, for example.
    pub at: String,
    pub message: String,
}

impl Issue {
    pub fn error(at: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            at: at.into(),
            message: message.into(),
        }
    }

    pub fn warning(at: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            at: at.into(),
            message: message.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.level == Level::Error
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.level {
            Level::Error => "error",
            Level::Warning => "warning",
        };
        if self.at.is_empty() {
            write!(f, "{level}: {}", self.message)
        } else {
            write!(f, "{level}: {}: {}", self.at, self.message)
        }
    }
}

/// The kind of an object that a fleet declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Subscription,
    Project,
    Target,
    Agent,
    /// A schedule of a project (`[[projects.schedules]]`). Its key is
    /// `project/name`.
    Schedule,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Project => "project",
            Self::Target => "target",
            Self::Agent => "agent",
            Self::Schedule => "schedule",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One object of a fleet, by kind and key. The key of an agent is
/// `project/name`; the other keys are the name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: Kind,
    pub key: String,
    /// The project of an agent or a project target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// Names that the fleet uses but does not define. The daemon checks that
/// they exist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct References {
    /// Subscription name → the agents (`project/name`) that name it.
    pub subscriptions: BTreeMap<String, Vec<String>>,
    /// Target name → the agents that list it in `policy.targets`.
    pub targets: BTreeMap<String, Vec<String>>,
    /// Prompt template name → the agents that use it.
    pub prompts: BTreeMap<String, Vec<String>>,
}

/// Lower-case letters, digits, `-` and `_`, 1 to 63 characters, starting
/// with a letter or digit: the rule of the daemon for project, agent,
/// target, and subscription names.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The fleet name from a file name: `terrarium.fleet.toml` → `terrarium`.
pub fn name_from_path(path: &Path) -> Option<String> {
    let file = path.file_name()?.to_str()?;
    let stem = file
        .strip_suffix(FLEET_SUFFIX)
        .or_else(|| file.strip_suffix(".toml"))?;
    Some(stem.to_owned())
}

/// The fleet files of a config dir: `<dir>/*.fleet.toml` and
/// `<dir>/fleets/*.toml`, sorted. A missing directory has no files.
pub fn files_in(dir: &Path) -> Vec<PathBuf> {
    let list = |d: &Path, keep: &dyn Fn(&str) -> bool| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(keep))
            .collect()
    };
    let mut files = list(dir, &|n| n.ends_with(FLEET_SUFFIX));
    files.extend(list(&dir.join("fleets"), &|n| n.ends_with(".toml")));
    files.sort();
    files.dedup();
    files
}

/// The result of [`parse`]: the fleet when the TOML is readable, and every
/// issue found on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub fleet: Option<Fleet>,
    pub issues: Vec<Issue>,
}

impl Parsed {
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(Issue::is_error)
    }

    /// The fleet when there is no error.
    pub fn ok(self) -> Result<Fleet, Vec<Issue>> {
        match self.fleet {
            Some(f) if !self.issues.iter().any(Issue::is_error) => Ok(f),
            _ => Err(self.issues),
        }
    }
}

/// Parse and check a fleet file. `fallback_name` names a fleet whose file
/// has no `name` (the file stem, see [`name_from_path`]).
///
/// The result has the issues of [`Fleet::check`] too. A definition that
/// does not parse is left out of the fleet, with an error.
pub fn parse(text: &str, fallback_name: Option<&str>) -> Parsed {
    let mut issues = Vec::new();
    let mut top: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => {
            return Parsed {
                fleet: None,
                issues: vec![Issue::error("", format!("bad TOML: {e}"))],
            };
        }
    };
    unknown_keys(&top, TOP_KEYS, "", &mut issues);

    let version = match top.remove("version") {
        None => FLEET_VERSION,
        Some(toml::Value::Integer(v)) => u32::try_from(v).unwrap_or(0),
        Some(_) => {
            issues.push(Issue::error("version", "`version` must be an integer"));
            FLEET_VERSION
        }
    };
    let name = match top.remove("name") {
        Some(toml::Value::String(s)) => s,
        Some(_) => {
            issues.push(Issue::error("name", "`name` must be a string"));
            String::new()
        }
        None => fallback_name.unwrap_or_default().to_owned(),
    };
    let description = match top.remove("description") {
        Some(toml::Value::String(s)) => Some(s),
        Some(_) => {
            issues.push(Issue::error(
                "description",
                "`description` must be a string",
            ));
            None
        }
        None => None,
    };
    let defaults: FleetDefaults = match top.remove("defaults") {
        None => FleetDefaults::default(),
        Some(v) => v.try_into().unwrap_or_else(|e| {
            issues.push(Issue::error("defaults", clean(&e)));
            FleetDefaults::default()
        }),
    };
    let defaults_table = toml::Table::try_from(&defaults).unwrap_or_default();

    let subscriptions = tables(&mut top, "subscriptions", "", &mut issues)
        .into_iter()
        .filter_map(|(at, t)| {
            unknown_keys(&t, SUBSCRIPTION_KEYS, &at, &mut issues);
            def::<SubscriptionDef>(t, &at, &mut issues)
        })
        .collect();
    let targets = tables(&mut top, "targets", "", &mut issues)
        .into_iter()
        .filter_map(|(at, t)| {
            unknown_keys(&t, TARGET_KEYS, &at, &mut issues);
            if t.contains_key("project") {
                issues.push(Issue::error(
                    &at,
                    "a global target has no `project`; put a project target under its [[projects]] as [[projects.targets]]",
                ));
                return None;
            }
            def::<TargetDef>(t, &at, &mut issues)
        })
        .collect();
    let projects = tables(&mut top, "projects", "", &mut issues)
        .into_iter()
        .filter_map(|(at, t)| project(t, &at, &defaults_table, &mut issues))
        .collect();

    let fleet = Fleet {
        version,
        name,
        description,
        defaults,
        subscriptions,
        targets,
        projects,
    };
    issues.extend(fleet.check());
    Parsed {
        fleet: Some(fleet),
        issues,
    }
}

/// A TOML error without the source snippet that `toml` adds.
fn clean(e: &toml::de::Error) -> String {
    e.message().trim().to_owned()
}

fn unknown_keys(t: &toml::Table, known: &[&str], at: &str, issues: &mut Vec<Issue>) {
    for k in t.keys().filter(|k| !known.contains(&k.as_str())) {
        issues.push(Issue::error(
            at,
            format!("unknown key `{k}`; known keys: {}", known.join(", ")),
        ));
    }
}

/// The tables of the array `key` in `t`, each with its place in the file.
fn tables(
    t: &mut toml::Table,
    key: &str,
    parent: &str,
    issues: &mut Vec<Issue>,
) -> Vec<(String, toml::Table)> {
    let prefix = if parent.is_empty() {
        key.to_owned()
    } else {
        format!("{parent}.{key}")
    };
    match t.remove(key) {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items
            .into_iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                toml::Value::Table(t) => {
                    let at = match t.get("name").and_then(toml::Value::as_str) {
                        Some(n) => format!("{prefix}.{n}"),
                        None => format!("{prefix}[{i}]"),
                    };
                    Some((at, t))
                }
                _ => {
                    issues.push(Issue::error(
                        format!("{prefix}[{i}]"),
                        format!("each entry of `{key}` must be a table: [[{prefix}]]"),
                    ));
                    None
                }
            })
            .collect(),
        Some(_) => {
            issues.push(Issue::error(
                &prefix,
                format!("`{key}` must be an array of tables: [[{prefix}]]"),
            ));
            Vec::new()
        }
    }
}

fn def<T: for<'de> Deserialize<'de>>(
    t: toml::Table,
    at: &str,
    issues: &mut Vec<Issue>,
) -> Option<T> {
    if !t.contains_key("name") {
        issues.push(Issue::error(at, "`name` is required in a fleet file"));
        return None;
    }
    match t.try_into::<T>() {
        Ok(d) => Some(d),
        Err(e) => {
            issues.push(Issue::error(at, clean(&e)));
            None
        }
    }
}

fn project(
    mut t: toml::Table,
    at: &str,
    defaults: &toml::Table,
    issues: &mut Vec<Issue>,
) -> Option<FleetProject> {
    unknown_keys(&t, PROJECT_KEYS, at, issues);
    let agent_tables = tables(&mut t, "agents", at, issues);
    let target_tables = tables(&mut t, "targets", at, issues);
    for (sub, keys) in [("checkout", CHECKOUT_KEYS)] {
        if let Some(toml::Value::Table(s)) = t.get(sub) {
            unknown_keys(s, keys, &format!("{at}.{sub}"), issues);
        }
    }
    if let Some(toml::Value::Array(repos)) = t.get("repos") {
        for (i, r) in repos.iter().enumerate() {
            if let toml::Value::Table(r) = r {
                unknown_keys(r, REPO_KEYS, &format!("{at}.repos[{i}]"), issues);
            }
        }
    }
    if let Some(toml::Value::Array(list)) = t.get("schedules") {
        for (i, r) in list.iter().enumerate() {
            if let toml::Value::Table(r) = r {
                let n = r.get("name").and_then(toml::Value::as_str);
                let at = match n {
                    Some(n) => format!("{at}.schedules.{n}"),
                    None => format!("{at}.schedules[{i}]"),
                };
                unknown_keys(r, SCHEDULE_KEYS, &at, issues);
            }
        }
    }
    let def: ProjectDef = def(t, at, issues)?;
    let agents = agent_tables
        .into_iter()
        .filter_map(|(at, mut a)| {
            unknown_keys(&a, AGENT_KEYS, &at, issues);
            if a.contains_key("project") {
                issues.push(Issue::error(
                    &at,
                    "an agent takes its project from the enclosing [[projects]]; remove `project`",
                ));
                a.remove("project");
            }
            if let Some(toml::Value::Table(p)) = a.get("policy") {
                unknown_keys(p, POLICY_KEYS, &format!("{at}.policy"), issues);
            }
            for (k, v) in defaults {
                if !a.contains_key(k) {
                    a.insert(k.clone(), v.clone());
                }
            }
            let start = match a.remove("start") {
                None => false,
                Some(toml::Value::Boolean(b)) => b,
                Some(_) => {
                    issues.push(Issue::error(&at, "`start` must be true or false"));
                    false
                }
            };
            if !a.contains_key("harness") {
                issues.push(Issue::error(
                    &at,
                    "the agent has no `harness`, and [defaults] gives none",
                ));
                return None;
            }
            a.insert("project".into(), toml::Value::String(def.name.clone()));
            let def: AgentDef = self::def(a, &at, issues)?;
            Some(FleetAgent { def, start })
        })
        .collect();
    let targets = target_tables
        .into_iter()
        .filter_map(|(at, mut tt)| {
            unknown_keys(&tt, TARGET_KEYS, &at, issues);
            match tt.get("project").and_then(toml::Value::as_str) {
                Some(p) if p != def.name => {
                    issues.push(Issue::error(
                        &at,
                        format!(
                            "a project target belongs to its enclosing project `{}`, not `{p}`",
                            def.name
                        ),
                    ));
                    return None;
                }
                _ => {}
            }
            tt.insert("project".into(), toml::Value::String(def.name.clone()));
            self::def::<TargetDef>(tt, &at, issues)
        })
        .collect();
    Some(FleetProject {
        def,
        agents,
        targets,
    })
}

impl Fleet {
    /// Every agent of every project.
    pub fn agents(&self) -> impl Iterator<Item = &FleetAgent> {
        self.projects.iter().flat_map(|p| p.agents.iter())
    }

    /// Every target: the global ones, then each project's.
    pub fn all_targets(&self) -> impl Iterator<Item = &TargetDef> {
        self.targets
            .iter()
            .chain(self.projects.iter().flat_map(|p| p.targets.iter()))
    }

    /// Every object, in apply order: subscriptions, projects, targets,
    /// agents.
    pub fn objects(&self) -> Vec<ObjectRef> {
        let mut out: Vec<ObjectRef> = self
            .subscriptions
            .iter()
            .map(|s| ObjectRef {
                kind: Kind::Subscription,
                key: s.name.clone(),
                project: None,
            })
            .collect();
        out.extend(self.projects.iter().map(|p| ObjectRef {
            kind: Kind::Project,
            key: p.def.name.clone(),
            project: Some(p.def.name.clone()),
        }));
        out.extend(self.all_targets().map(|t| ObjectRef {
            kind: Kind::Target,
            key: t.name.clone(),
            project: t.project.clone(),
        }));
        out.extend(self.agents().map(|a| ObjectRef {
            kind: Kind::Agent,
            key: agent_key(&a.def),
            project: Some(a.def.project.clone()),
        }));
        out.extend(self.projects.iter().flat_map(|p| {
            p.def.schedules.iter().map(|s| ObjectRef {
                kind: Kind::Schedule,
                key: format!("{}/{}", p.def.name, s.name),
                project: Some(p.def.name.clone()),
            })
        }));
        out
    }

    /// The names the fleet uses but does not define.
    pub fn references(&self) -> References {
        let mut refs = References::default();
        let subs: BTreeSet<&str> = self.subscriptions.iter().map(|s| s.name.as_str()).collect();
        for a in self.agents() {
            let who = agent_key(&a.def);
            if let Some(s) = &a.def.subscription
                && !subs.contains(s.as_str())
            {
                refs.subscriptions
                    .entry(s.clone())
                    .or_default()
                    .push(who.clone());
            }
            for t in &a.def.policy.targets {
                if !self.all_targets().any(|d| d.name == *t) {
                    refs.targets.entry(t.clone()).or_default().push(who.clone());
                }
            }
            if let Some(p) = a.def.prompt.as_ref().filter(|p| !p.is_empty()) {
                refs.prompts.entry(p.clone()).or_default().push(who.clone());
            }
        }
        refs
    }

    /// The checks that need no daemon: names, versions, duplicates, and
    /// references inside the file.
    pub fn check(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.version != FLEET_VERSION {
            issues.push(Issue::error(
                "version",
                format!(
                    "this build reads fleet version {FLEET_VERSION}, not {}",
                    self.version
                ),
            ));
        }
        if self.name.is_empty() {
            issues.push(Issue::error("name", "the fleet needs a `name`"));
        } else if !valid_name(&self.name) {
            issues.push(Issue::error(
                "name",
                format!(
                    "bad fleet name `{}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit",
                    self.name
                ),
            ));
        } else if RESERVED_NAMES.contains(&self.name.as_str()) {
            issues.push(Issue::error(
                "name",
                format!(
                    "`{}` is a reserved word; pick another fleet name",
                    self.name
                ),
            ));
        }
        if self.projects.is_empty() && self.targets.is_empty() && self.subscriptions.is_empty() {
            issues.push(Issue::warning("", "the fleet declares nothing"));
        }

        let mut seen: BTreeMap<(Kind, String), usize> = BTreeMap::new();
        for o in self.objects() {
            *seen.entry((o.kind, o.key.clone())).or_default() += 1;
            let name = o.key.rsplit('/').next().unwrap_or(&o.key);
            if !valid_name(name) {
                issues.push(Issue::error(
                    at_of(&o),
                    format!(
                        "bad {} name `{name}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit",
                        o.kind
                    ),
                ));
            }
        }
        for ((kind, key), n) in &seen {
            if *n > 1 {
                let what = match kind {
                    Kind::Target => "target names are one namespace across projects",
                    Kind::Agent => "agent names are unique in a project",
                    Kind::Schedule => "schedule names are unique in a project",
                    _ => "names are unique",
                };
                issues.push(Issue::error(
                    format!("{}s", kind.as_str()),
                    format!("{kind} `{key}` is declared {n} times; {what}"),
                ));
            }
        }

        let subs: BTreeMap<&str, &SubscriptionDef> = self
            .subscriptions
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        for p in &self.projects {
            let at = format!("projects.{}", p.def.name);
            let repos: BTreeSet<&str> = p.def.repos.iter().map(|r| r.name.as_str()).collect();
            for r in &p.def.checkout.on_agent_create {
                if !repos.contains(r.as_str()) {
                    issues.push(Issue::error(
                        format!("{at}.checkout"),
                        format!(
                            "`on_agent_create` names `{r}`, which is not a repo of the project"
                        ),
                    ));
                }
            }
            for a in &p.agents {
                let at = format!("{at}.agents.{}", a.def.name);
                for t in &a.def.policy.targets {
                    let other = self.all_targets().find(|d| {
                        d.name == *t && d.project.as_deref().is_some_and(|x| x != p.def.name)
                    });
                    if let Some(d) = other {
                        issues.push(Issue::error(
                            &at,
                            format!(
                                "policy.targets names `{t}`, a target of project `{}`; an agent uses global targets and those of its own project",
                                d.project.as_deref().unwrap_or_default()
                            ),
                        ));
                    }
                }
                if let Some(s) = a.def.subscription.as_deref().and_then(|s| subs.get(s))
                    && !s.harnesses.is_empty()
                    && !s.harnesses.contains(&a.def.harness)
                {
                    issues.push(Issue::warning(
                        &at,
                        format!(
                            "subscription `{}` lists harnesses {:?}, not `{}`",
                            s.name,
                            s.harnesses.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
                            a.def.harness
                        ),
                    ));
                }
                if a.def.roles.contains(&Role::Sysadmin) {
                    issues.push(Issue::warning(
                        &at,
                        "the sysadmin role can be granted by the operator only; an agent cannot apply this fleet",
                    ));
                }
                if let Some(bad) = a.def.policy.cross_project.iter().find(|e| {
                    e.split_once('/')
                        .is_none_or(|(p, n)| p.is_empty() || n.is_empty())
                }) {
                    issues.push(Issue::error(
                        &at,
                        format!("policy.cross_project entry `{bad}` must be `project/agent` or `project/*`"),
                    ));
                }
            }
        }
        issues
    }

    /// Write the fleet as a fleet file. [`parse`] reads it back as the same
    /// fleet. Each agent is written with all its fields (the defaults are
    /// applied), so each agent reads on its own.
    pub fn to_toml(&self) -> String {
        let mut w = Writer::default();
        let mut head = toml::Table::new();
        head.insert(
            "version".into(),
            toml::Value::Integer(i64::from(self.version)),
        );
        head.insert("name".into(), toml::Value::String(self.name.clone()));
        if let Some(d) = &self.description {
            head.insert("description".into(), toml::Value::String(d.clone()));
        }
        w.table(None, &head, &["version", "name", "description"]);
        if !self.defaults.is_empty() {
            w.table(Some("[defaults]"), &table_of(&self.defaults), AGENT_KEYS);
        }
        for s in &self.subscriptions {
            w.table(Some("[[subscriptions]]"), &table_of(s), SUBSCRIPTION_KEYS);
        }
        for t in &self.targets {
            w.table(Some("[[targets]]"), &target_table(t), TARGET_KEYS);
        }
        for p in &self.projects {
            let mut t = table_of(&p.def);
            let repos = t.remove("repos");
            let checkout = t.remove("checkout");
            t.remove("schedules");
            w.table(Some("[[projects]]"), &t, PROJECT_KEYS);
            if let Some(toml::Value::Array(repos)) = repos {
                for r in repos.iter().filter_map(toml::Value::as_table) {
                    w.table(Some("[[projects.repos]]"), r, REPO_KEYS);
                }
            }
            if !p.def.checkout.on_agent_create.is_empty()
                && let Some(toml::Value::Table(c)) = checkout
            {
                w.table(Some("[projects.checkout]"), &c, CHECKOUT_KEYS);
            }
            for a in &p.agents {
                let mut at = table_of(&a.def);
                at.remove("project");
                if a.def.policy == Policy::default() {
                    at.remove("policy");
                }
                if a.start {
                    at.insert("start".into(), toml::Value::Boolean(true));
                }
                let policy = at.remove("policy");
                w.table(Some("[[projects.agents]]"), &at, AGENT_KEYS);
                if let Some(toml::Value::Table(pt)) = policy {
                    w.table(Some("[projects.agents.policy]"), &pt, POLICY_KEYS);
                }
            }
            for d in &p.targets {
                let mut tt = target_table(d);
                tt.remove("project");
                w.table(Some("[[projects.targets]]"), &tt, TARGET_KEYS);
            }
            for s in &p.def.schedules {
                let mut st = table_of(s);
                let spec = st.remove("spec");
                let action = st.remove("action");
                w.table(Some("[[projects.schedules]]"), &st, SCHEDULE_KEYS);
                if let Some(toml::Value::Table(x)) = spec {
                    w.table(Some("[projects.schedules.spec]"), &x, &["kind"]);
                }
                if let Some(toml::Value::Table(x)) = action {
                    w.table(Some("[projects.schedules.action]"), &x, &["type"]);
                }
            }
        }
        w.out
    }
}

/// Writes TOML tables with the keys in the order of the schema, not in
/// alphabetical order.
#[derive(Default)]
struct Writer {
    out: String,
}

impl Writer {
    fn table(&mut self, header: Option<&str>, t: &toml::Table, order: &[&str]) {
        if let Some(h) = header {
            if !self.out.is_empty() {
                self.out.push('\n');
            }
            self.out.push_str(h);
            self.out.push('\n');
        }
        let mut keys: Vec<&String> = t.keys().collect();
        keys.sort_by_key(|k| order.iter().position(|o| o == k).unwrap_or(usize::MAX));
        let mut nested = Vec::new();
        for k in keys {
            match &t[k] {
                toml::Value::Table(sub) => nested.push((k, sub)),
                v => {
                    let key = toml::Value::String(k.clone());
                    let key = if valid_name(k) {
                        k.clone()
                    } else {
                        key.to_string()
                    };
                    self.out.push_str(&format!("{key} = {v}\n"));
                }
            }
        }
        for (k, sub) in nested {
            let mut inline = toml::Table::new();
            inline.insert(k.clone(), toml::Value::Table(sub.clone()));
            self.out
                .push_str(&toml::to_string(&inline).unwrap_or_default());
        }
    }
}

fn table_of<T: Serialize>(v: &T) -> toml::Table {
    toml::Table::try_from(v).unwrap_or_default()
}

/// A target table without the flags that are off (their default).
fn target_table(d: &TargetDef) -> toml::Table {
    let mut t = table_of(d);
    t.retain(|_, v| v.as_bool() != Some(false));
    t
}

/// `project/name`.
pub fn agent_key(def: &AgentDef) -> String {
    format!("{}/{}", def.project, def.name)
}

fn at_of(o: &ObjectRef) -> String {
    match o.kind {
        Kind::Subscription => format!("subscriptions.{}", o.key),
        Kind::Project => format!("projects.{}", o.key),
        Kind::Target => match &o.project {
            Some(p) => format!("projects.{p}.targets.{}", o.key),
            None => format!("targets.{}", o.key),
        },
        Kind::Agent => {
            let (p, n) = o.key.split_once('/').unwrap_or(("", &o.key));
            format!("projects.{p}.agents.{n}")
        }
        Kind::Schedule => {
            let (p, n) = o.key.split_once('/').unwrap_or(("", &o.key));
            format!("projects.{p}.schedules.{n}")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::TargetKind;

    /// The example that ships in `examples/fleet/`.
    const EXAMPLE: &str = include_str!("../../../examples/fleet/terrarium.fleet.toml");

    fn errors(p: &Parsed) -> Vec<String> {
        p.issues
            .iter()
            .filter(|i| i.is_error())
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn the_example_parses_without_errors() {
        let p = parse(EXAMPLE, Some("ignored"));
        assert!(!p.has_errors(), "{:#?}", p.issues);
        let f = p.fleet.unwrap();
        assert_eq!(f.name, "terrarium");
        assert_eq!(f.version, 1);
        assert_eq!(f.projects.len(), 1);
        let proj = &f.projects[0];
        assert_eq!(proj.def.name, "terrarium");
        assert_eq!(proj.def.prefix.as_deref(), Some("TER"));
        assert!(!proj.def.repos.is_empty());
        assert!(proj.agents.len() >= 2);
        // Defaults fill the gaps; the agent's own values win.
        for a in &proj.agents {
            assert_eq!(a.def.project, "terrarium");
            assert!(!a.start, "the example must not start agents");
        }
        let lead = proj.agents.iter().find(|a| a.def.name == "lead").unwrap();
        assert_eq!(lead.def.roles, [Role::ProjectAdmin]);
        assert_eq!(lead.def.prompt.as_deref(), Some("project-lead"));
        assert!(
            f.all_targets()
                .all(|t| t.project.is_none() || t.project.as_deref() == Some("terrarium"))
        );
    }

    #[test]
    fn the_example_round_trips_through_to_toml() {
        let f = parse(EXAMPLE, None).ok().unwrap();
        let text = f.to_toml();
        let again = parse(&text, None)
            .ok()
            .unwrap_or_else(|e| panic!("{e:#?}\n{text}"));
        assert_eq!(f, again, "{text}");
    }

    #[test]
    fn schedules_parse_check_and_round_trip() {
        let text = r#"
name = "sched"

[[projects]]
name = "terra"

[[projects.agents]]
name = "lead"
harness = "claude"

[[projects.schedules]]
name = "standup"
catch_up = "once"
spec = { kind = "cron", expr = "0 9 * * MON-FRI", tz = "Europe/London" }
action = { type = "mail", to = "lead", subject = "standup", body = "post your status" }

[[projects.schedules]]
name = "tests"
enabled = false
[projects.schedules.spec]
kind = "interval"
every = "30m"
[projects.schedules.action]
type = "exec"
target = "terra-box"
cmd = ["make", "test"]
"#;
        let f = parse(text, None).ok().unwrap();
        let s = &f.projects[0].def.schedules;
        assert_eq!(s.len(), 2);
        assert!(!s[1].enabled);
        let keys: Vec<String> = f
            .objects()
            .into_iter()
            .filter(|o| o.kind == Kind::Schedule)
            .map(|o| o.key)
            .collect();
        assert_eq!(keys, ["terra/standup", "terra/tests"]);
        let out = f.to_toml();
        assert!(out.contains("[[projects.schedules]]"), "{out}");
        let again = parse(&out, None)
            .ok()
            .unwrap_or_else(|e| panic!("{e:#?}\n{out}"));
        assert_eq!(f, again, "{out}");
        // A typo and a duplicate name are errors.
        let bad = text.replace("catch_up = \"once\"", "catchup = \"once\"");
        let p = parse(&bad, None);
        assert!(
            errors(&p).iter().any(|e| e.contains("schedules.standup")),
            "{:?}",
            errors(&p)
        );
        let dup = text.replace("name = \"tests\"", "name = \"standup\"");
        let p = parse(&dup, None);
        assert!(
            errors(&p).iter().any(|e| e.contains("declared 2 times")),
            "{:?}",
            errors(&p)
        );
    }

    const SMALL: &str = r#"
name = "small"

[defaults]
harness = "pi"
subscription = "anthropic-max"
prompt = "default"
restart = "never"
start = true

[[subscriptions]]
name = "anthropic-max"
provider = "anthropic"
harnesses = ["claude", "pi"]

[[targets]]
name = "shared"
kind = "ssh"
address = "u@h"

[[projects]]
name = "alpha"

[[projects.agents]]
name = "one"

[[projects.agents]]
name = "two"
harness = "claude"
restart = "always"
start = false
[projects.agents.policy]
targets = ["shared", "box"]

[[projects.targets]]
name = "box"
kind = "docker"
host = "h1"
image = "alpine"
"#;

    #[test]
    fn defaults_apply_where_the_agent_is_silent() {
        let f = parse(SMALL, None).ok().unwrap();
        let a = &f.projects[0].agents;
        assert_eq!(a[0].def.harness, HarnessId::Pi);
        assert_eq!(a[0].def.restart, RestartPolicy::Never);
        assert_eq!(a[0].def.subscription.as_deref(), Some("anthropic-max"));
        assert!(a[0].start);
        assert_eq!(a[1].def.harness, HarnessId::Claude);
        assert_eq!(a[1].def.restart, RestartPolicy::Always);
        assert!(!a[1].start);
        assert_eq!(a[1].def.policy.targets, ["shared", "box"]);
        assert_eq!(f.projects[0].targets[0].project.as_deref(), Some("alpha"));
        assert_eq!(f.projects[0].targets[0].kind, TargetKind::Docker);
        let objects: Vec<String> = f
            .objects()
            .iter()
            .map(|o| format!("{} {}", o.kind, o.key))
            .collect();
        assert_eq!(
            objects,
            [
                "subscription anthropic-max",
                "project alpha",
                "target shared",
                "target box",
                "agent alpha/one",
                "agent alpha/two"
            ]
        );
        // Everything is defined in the file; only the prompt is outside.
        let refs = f.references();
        assert!(refs.subscriptions.is_empty() && refs.targets.is_empty());
        assert_eq!(refs.prompts["default"], ["alpha/one", "alpha/two"]);
    }

    #[test]
    fn the_file_stem_names_a_fleet_without_name() {
        let f = parse("[[projects]]\nname = \"p\"\n", Some("stem"))
            .ok()
            .unwrap();
        assert_eq!(f.name, "stem");
        assert_eq!(
            name_from_path(Path::new("/x/ops.fleet.toml")).as_deref(),
            Some("ops")
        );
        assert_eq!(
            name_from_path(Path::new("/x/fleets/ops.toml")).as_deref(),
            Some("ops")
        );
        let p = parse("[[projects]]\nname = \"p\"\n", None);
        assert!(
            errors(&p).iter().any(|e| e.contains("needs a `name`")),
            "{p:?}"
        );
    }

    #[test]
    fn bad_files_give_every_error() {
        let text = r#"
version = 2
name = "plan"
colour = "red"

[[targets]]
name = "g"
kind = "ssh"
address = "u@h"
project = "alpha"

[[projects]]
name = "alpha"
[projects.checkout]
on_agent_create = ["missing"]

[[projects.agents]]
name = "a"
harness = "claude"
roles = ["king"]

[[projects.agents]]
name = "b"

[[projects.agents]]
name = "c"
harness = "codex"
project = "beta"
polcy = {}

[[projects.agents]]
name = "c"
harness = "codex"

[[projects.agents]]
name = "Bad"
harness = "pi"
[projects.agents.policy]
targets = ["other-box"]

[[projects]]
name = "beta"
[[projects.targets]]
name = "other-box"
kind = "host"
host = "h"
"#;
        let p = parse(text, None);
        let e = errors(&p);
        let has = |s: &str| assert!(e.iter().any(|x| x.contains(s)), "no `{s}` in {e:#?}");
        has("fleet version 1, not 2");
        has("reserved word");
        has("unknown key `colour`");
        has("a global target has no `project`");
        has("names `missing`, which is not a repo");
        has("projects.alpha.agents.a: unknown variant `king`");
        has("projects.alpha.agents.b: the agent has no `harness`");
        has("remove `project`");
        has("unknown key `polcy`");
        has("agent `alpha/c` is declared 2 times");
        has("bad agent name `Bad`");
        has("a target of project `beta`");
        // The fleet is still there, without the agents that did not parse.
        let f = p.fleet.unwrap();
        assert_eq!(f.projects[0].agents.len(), 3);
    }

    #[test]
    fn bad_toml_is_one_error() {
        let p = parse("name = ", None);
        assert!(p.fleet.is_none());
        assert_eq!(p.issues.len(), 1);
        assert!(p.issues[0].message.starts_with("bad TOML"));
    }

    #[test]
    fn warnings_do_not_block() {
        let text = r#"
name = "w"
[[subscriptions]]
name = "codex-plan"
provider = "openai"
harnesses = ["codex"]
[[projects]]
name = "p"
[[projects.agents]]
name = "a"
harness = "claude"
subscription = "codex-plan"
"#;
        let p = parse(text, None);
        assert!(!p.has_errors(), "{:?}", p.issues);
        assert!(p.issues.iter().any(|i| i.message.contains("not `claude`")));
    }

    #[test]
    fn files_in_finds_both_places() {
        let dir = std::env::temp_dir().join(format!("terrarium-fleet-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("fleets")).unwrap();
        std::fs::write(dir.join("a.fleet.toml"), "").unwrap();
        std::fs::write(dir.join("b.toml"), "").unwrap();
        std::fs::write(dir.join("fleets/c.toml"), "").unwrap();
        std::fs::write(dir.join("fleets/d.txt"), "").unwrap();
        let files = files_in(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["a.fleet.toml", "fleets/c.toml"]);
        assert!(files_in(Path::new("/nonexistent/terrarium")).is_empty());
    }
}
