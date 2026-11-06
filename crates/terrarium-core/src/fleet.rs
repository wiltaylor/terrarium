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
//!
//! [[projects.templates]]     # an agent template of the project
//! name = "worker"
//! prompt = "worker"
//!
//! [[projects.agents]]        # an agent made from a template
//! name = "w1"
//! template = "worker"
//! ```
//!
//! This module parses and checks a file without the daemon. The daemon
//! resolves the references to objects outside the file (subscriptions,
//! global targets, prompt templates) and diffs the fleet against its store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent_template::{self, Lookup, Spec, TemplateDef};
use crate::config::{
    AgentDef, Policy, ProjectDef, RestartPolicy, SYSTEM_PROMPT_FILE_KEY, SubscriptionDef,
    TargetDef, clean_system_prompt, inline_system_prompt_file,
};
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
    "templates",
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
    "templates",
];
/// The project key of the GitHub author gate (TER-85). A fleet file
/// cannot set it.
const GITHUB_AUTHORS_KEY: &str = "github_authors";
const AGENT_KEYS: &[&str] = &[
    "name",
    "template",
    "harness",
    "subscription",
    "subscription_pool",
    "model",
    "prompt",
    "system_prompt",
    "persona",
    "roles",
    "policy",
    "restart",
    "idle_stop",
    "compact_on_idle",
    "compact_max_age",
    "card_reminder",
    "skills",
    "skills_off",
    "default_skills",
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
    "limits",
];
const SUBSCRIPTION_KEYS: &[&str] = &["name", "provider", "plan", "harnesses", "probe", "alert_at"];
const SCHEDULE_KEYS: &[&str] = &["name", "enabled", "catch_up", "spec", "action"];
const TEMPLATE_KEYS: &[&str] = &[
    "name",
    "description",
    "harness",
    "subscription",
    "subscription_pool",
    "model",
    "prompt",
    "system_prompt",
    "persona",
    "roles",
    "policy",
    "restart",
    "idle_stop",
    "compact_on_idle",
    "compact_max_age",
    "card_reminder",
    "skills",
    "skills_off",
    "default_skills",
    "new_session_on_prompt_change",
    "tools",
];

/// Looks up a template that the fleet file does not declare: the daemon
/// gives its stored templates. The arguments are the project (`None`:
/// global) and the template name.
pub type Resolver<'a> = dyn Fn(Option<&str>, &str) -> Option<Spec> + 'a;

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
    pub system_prompt: Option<String>,
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
    /// `project` is the enclosing project; the defaults (and the
    /// template) are applied.
    pub def: AgentDef,
    /// Start the agent on `fleet apply --start`.
    pub start: bool,
    /// The template that the entry names (`template = "worker"` or
    /// `"global:worker"`), if any.
    pub template: Option<String>,
    /// With `template`: the keys that the entry sets itself, over the
    /// template. Empty without `template`.
    pub overrides: Spec,
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
    /// Global agent templates (`[[templates]]`).
    pub templates: Vec<TemplateDef>,
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
    /// An agent template: `project/name` (`[[projects.templates]]`), or
    /// `_/name` for a global one (`[[templates]]`).
    Template,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Project => "project",
            Self::Target => "target",
            Self::Agent => "agent",
            Self::Schedule => "schedule",
            Self::Template => "template",
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
/// does not parse is left out of the fleet, with an error. An agent whose
/// template is not in the file is left out with a warning: only the
/// daemon knows its stored templates ([`parse_with`]).
pub fn parse(text: &str, fallback_name: Option<&str>) -> Parsed {
    parse_with(text, fallback_name, None)
}

/// [`parse`], with the stored templates of the daemon. An agent whose
/// template is neither in the file nor in `stored` is an error.
pub fn parse_with(
    text: &str,
    fallback_name: Option<&str>,
    stored: Option<&Resolver<'_>>,
) -> Parsed {
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
    let templates: Vec<TemplateDef> = tables(&mut top, "templates", "", &mut issues)
        .into_iter()
        .filter_map(|(at, t)| {
            if t.contains_key("project") {
                issues.push(Issue::error(
                    &at,
                    "a global template has no `project`; put a project template under its [[projects]] as [[projects.templates]]",
                ));
                return None;
            }
            template(t, &at, &mut issues)
        })
        .collect();
    let context = Context {
        defaults: &defaults_table,
        globals: &templates,
        stored,
    };
    let projects = tables(&mut top, "projects", "", &mut issues)
        .into_iter()
        .filter_map(|(at, t)| project(t, &at, &context, &mut issues))
        .collect();

    let fleet = Fleet {
        version,
        name,
        description,
        defaults,
        subscriptions,
        targets,
        templates,
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
        if k == SYSTEM_PROMPT_FILE_KEY && known.contains(&"system_prompt") {
            issues.push(Issue::error(
                at,
                format!(
                    "`{k}` needs the path of the fleet file: the CLI and the config dir read it \
                     (`inline_prompt_files`); send `system_prompt` with the text instead"
                ),
            ));
            continue;
        }
        issues.push(Issue::error(
            at,
            format!("unknown key `{k}`; known keys: {}", known.join(", ")),
        ));
    }
}

/// Read each `system_prompt_file` of a fleet file into `system_prompt`
/// (in `[defaults]`, `[[templates]]`, `[[projects.templates]]`, and
/// `[[projects.agents]]`). A relative path is relative to `base`, the
/// directory of the fleet file. A text with no `system_prompt_file`
/// comes back as it is; else the answer is the TOML written again, with
/// no comments.
pub fn inline_prompt_files(text: &str, base: &Path) -> Result<String, String> {
    if !text.contains(SYSTEM_PROMPT_FILE_KEY) {
        return Ok(text.to_owned());
    }
    let Ok(mut top) = toml::from_str::<toml::Table>(text) else {
        // `parse` reports the bad TOML.
        return Ok(text.to_owned());
    };
    let inline = |t: &mut toml::Table, at: &str| {
        inline_system_prompt_file(t, base).map_err(|e| format!("{at}: {e}"))
    };
    let each = |v: Option<&mut toml::Value>, at: &str| -> Result<(), String> {
        if let Some(toml::Value::Array(list)) = v {
            for t in list.iter_mut().filter_map(toml::Value::as_table_mut) {
                let name = t
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("?")
                    .to_owned();
                inline(t, &format!("{at}.{name}"))?;
            }
        }
        Ok(())
    };
    if let Some(toml::Value::Table(d)) = top.get_mut("defaults") {
        inline(d, "defaults")?;
    }
    each(top.get_mut("templates"), "templates")?;
    if let Some(toml::Value::Array(projects)) = top.get_mut("projects") {
        for p in projects.iter_mut().filter_map(toml::Value::as_table_mut) {
            let name = p
                .get("name")
                .and_then(toml::Value::as_str)
                .unwrap_or("?")
                .to_owned();
            each(
                p.get_mut("templates"),
                &format!("projects.{name}.templates"),
            )?;
            each(p.get_mut("agents"), &format!("projects.{name}.agents"))?;
        }
    }
    toml::to_string(&top).map_err(|e| e.to_string())
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

/// One template table: its keys, then its spec as an agent definition.
fn template(t: toml::Table, at: &str, issues: &mut Vec<Issue>) -> Option<TemplateDef> {
    unknown_keys(&t, TEMPLATE_KEYS, at, issues);
    if let Some(toml::Value::Table(p)) = t.get("policy") {
        unknown_keys(p, POLICY_KEYS, &format!("{at}.policy"), issues);
    }
    let d: TemplateDef = def(t, at, issues)?;
    check_template(&d, at, issues).then_some(d)
}

/// Is the spec of `d` a valid agent definition? An error when not.
fn check_template(d: &TemplateDef, at: &str, issues: &mut Vec<Issue>) -> bool {
    match agent_template::to_def(&d.spec, &d.name, "x") {
        Ok(_) => true,
        Err(e) => {
            issues.push(Issue::error(at, e));
            false
        }
    }
}

/// What the agents of a project read from the rest of the file.
struct Context<'a, 'r> {
    defaults: &'a toml::Table,
    /// The global templates of the file.
    globals: &'a [TemplateDef],
    stored: Option<&'a Resolver<'r>>,
}

/// The spec of the template `key` for an agent of `project`: the
/// project's template, else the global one (`global:` asks for the
/// global one only); the file first, then the stored ones. When it is not
/// found: `Err(true)` when the daemon was asked, `Err(false)` when only
/// the daemon can tell.
fn find_template(
    key: &str,
    project: &str,
    local: &[TemplateDef],
    cx: &Context<'_, '_>,
) -> Result<Spec, bool> {
    let (name, global_only) = match agent_template::lookup(key) {
        Lookup::Global(n) => (n, true),
        Lookup::Nearest(n) => (n, false),
    };
    let stored = |p: Option<&str>| cx.stored.and_then(|f| f(p, name));
    if !global_only {
        if let Some(t) = local.iter().find(|t| t.name == name) {
            return Ok(t.spec.clone());
        }
        if let Some(s) = stored(Some(project)) {
            return Ok(s);
        }
    }
    if let Some(t) = cx.globals.iter().find(|t| t.name == name) {
        return Ok(t.spec.clone());
    }
    stored(None).ok_or(cx.stored.is_some())
}

fn json_of(t: &toml::Table) -> Spec {
    match serde_json::to_value(t) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => Spec::new(),
    }
}

fn project(
    mut t: toml::Table,
    at: &str,
    cx: &Context<'_, '_>,
    issues: &mut Vec<Issue>,
) -> Option<FleetProject> {
    let defaults = cx.defaults;
    // The GitHub author gate (TER-85): the allow list is set only by the
    // operator or by `projects/<name>.toml`, never by a fleet file. Say so,
    // and drop the key so that no fleet def carries a list.
    if t.remove(GITHUB_AUTHORS_KEY).is_some() {
        issues.push(Issue::error(
            at,
            format!(
                "`{GITHUB_AUTHORS_KEY}` (the GitHub author gate) is not a fleet key: set it in \
                 projects/<name>.toml, or as the operator with PATCH /api/projects/<name>"
            ),
        ));
    }
    unknown_keys(&t, PROJECT_KEYS, at, issues);
    let agent_tables = tables(&mut t, "agents", at, issues);
    let target_tables = tables(&mut t, "targets", at, issues);
    if let Some(toml::Value::Table(s)) = t.get("checkout") {
        unknown_keys(s, CHECKOUT_KEYS, &format!("{at}.checkout"), issues);
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
    if let Some(toml::Value::Array(list)) = t.get("templates") {
        for (i, r) in list.iter().enumerate() {
            if let toml::Value::Table(r) = r {
                let n = r.get("name").and_then(toml::Value::as_str);
                let at = match n {
                    Some(n) => format!("{at}.templates.{n}"),
                    None => format!("{at}.templates[{i}]"),
                };
                unknown_keys(r, TEMPLATE_KEYS, &at, issues);
                if let Some(toml::Value::Table(p)) = r.get("policy") {
                    unknown_keys(p, POLICY_KEYS, &format!("{at}.policy"), issues);
                }
            }
        }
    }
    let mut def: ProjectDef = def(t, at, issues)?;
    def.templates
        .retain(|d| check_template(d, &format!("{at}.templates.{}", d.name), issues));
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
            let start_key = a.remove("start");
            let template = match a.remove("template") {
                None => None,
                Some(toml::Value::String(s)) => Some(s),
                Some(_) => {
                    issues.push(Issue::error(&at, "`template` must be a string"));
                    return None;
                }
            };
            let mut overrides = Spec::new();
            if let Some(key) = &template {
                // The defaults, then the template, then the entry's own
                // keys (`agent_template::merge`: `policy` key by key).
                overrides = json_of(&a);
                overrides.remove("name");
                let spec = match find_template(key, &def.name, &def.templates, cx) {
                    Ok(s) => s,
                    Err(true) => {
                        issues.push(Issue::error(
                            &at,
                            format!("template `{key}` is neither in the fleet nor in the daemon"),
                        ));
                        return None;
                    }
                    Err(false) => {
                        issues.push(Issue::warning(
                            &at,
                            format!(
                                "template `{key}` is not in this file; the daemon looks it up at validate and apply"
                            ),
                        ));
                        return None;
                    }
                };
                let mut base = json_of(defaults);
                base.remove("start");
                let merged = agent_template::merge(
                    &agent_template::merge(&base, &spec),
                    &overrides,
                );
                let name = a.get("name").cloned();
                a = match toml::Table::try_from(serde_json::Value::Object(merged)) {
                    Ok(t) => t,
                    Err(e) => {
                        issues.push(Issue::error(&at, e.to_string()));
                        return None;
                    }
                };
                if let Some(n) = name {
                    a.insert("name".into(), n);
                }
            } else {
                for (k, v) in defaults {
                    if !a.contains_key(k) {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
            a.remove("start");
            let start = match start_key.or_else(|| defaults.get("start").cloned()) {
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
            let mut def: AgentDef = self::def(a, &at, issues)?;
            // An empty text is no system prompt, as in the store.
            def.system_prompt = match clean_system_prompt(def.system_prompt) {
                Ok(t) => t,
                Err(e) => {
                    issues.push(Issue::error(&at, e));
                    return None;
                }
            };
            def.persona = match crate::config::clean_persona(def.persona) {
                Ok(p) => p,
                Err(e) => {
                    issues.push(Issue::error(&at, e));
                    return None;
                }
            };
            def.skills = match crate::skills::clean_list(&def.skills) {
                Ok(l) => l,
                Err(e) => {
                    issues.push(Issue::error(&at, e));
                    return None;
                }
            };
            def.skills_off = match crate::skills::clean_off_list(&def.skills_off) {
                Ok(l) => l,
                Err(e) => {
                    issues.push(Issue::error(&at, e));
                    return None;
                }
            };
            Some(FleetAgent {
                def,
                start,
                template,
                overrides,
            })
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

    /// Every template: the global ones, then each project's, with the
    /// project (`None`: global).
    pub fn all_templates(&self) -> impl Iterator<Item = (Option<&str>, &TemplateDef)> {
        self.templates
            .iter()
            .map(|t| (None, t))
            .chain(self.projects.iter().flat_map(|p| {
                p.def
                    .templates
                    .iter()
                    .map(|t| (Some(p.def.name.as_str()), t))
            }))
    }

    /// Every object, in apply order: subscriptions, projects, targets,
    /// templates, agents, schedules.
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
        out.extend(self.all_templates().map(|(p, t)| ObjectRef {
            kind: Kind::Template,
            key: template_key(p, &t.name),
            project: p.map(str::to_owned),
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
                    Kind::Template => {
                        "template names are unique in a project, and among the global templates"
                    }
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
        let write_template = |w: &mut Writer, d: &TemplateDef, header: &str| {
            let mut t = table_of(d);
            let policy = t.remove("policy");
            w.table(Some(&format!("[[{header}]]")), &t, TEMPLATE_KEYS);
            if let Some(toml::Value::Table(pt)) = policy {
                w.table(Some(&format!("[{header}.policy]")), &pt, POLICY_KEYS);
            }
        };
        if !self.defaults.is_empty() {
            w.table(Some("[defaults]"), &table_of(&self.defaults), AGENT_KEYS);
        }
        for s in &self.subscriptions {
            w.table(Some("[[subscriptions]]"), &table_of(s), SUBSCRIPTION_KEYS);
        }
        for t in &self.targets {
            write_target(&mut w, target_table(t), "targets");
        }
        for d in &self.templates {
            write_template(&mut w, d, "templates");
        }
        for p in &self.projects {
            let mut t = table_of(&p.def);
            let repos = t.remove("repos");
            let checkout = t.remove("checkout");
            t.remove("schedules");
            t.remove("templates");
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
            for d in &p.def.templates {
                write_template(&mut w, d, "projects.templates");
            }
            for a in &p.agents {
                // An agent of a template: the name, the template, and the
                // keys that the entry sets itself.
                let mut at = match &a.template {
                    Some(key) => {
                        let mut at =
                            toml::Table::try_from(serde_json::Value::Object(a.overrides.clone()))
                                .unwrap_or_default();
                        at.insert("name".into(), toml::Value::String(a.def.name.clone()));
                        at.insert("template".into(), toml::Value::String(key.clone()));
                        at
                    }
                    None => {
                        let mut at = table_of(&a.def);
                        at.remove("project");
                        if a.def.policy == Policy::default() {
                            at.remove("policy");
                        }
                        at
                    }
                };
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
                write_target(&mut w, tt, "projects.targets");
            }
            for s in &p.def.schedules {
                let mut st = table_of(s);
                let spec = st.remove("spec");
                let action = st.remove("action");
                w.table(Some("[[projects.schedules]]"), &st, SCHEDULE_KEYS);
                if let Some(toml::Value::Table(mut x)) = spec {
                    // A trigger's `dedupe` and `escalate` are tables:
                    // they go under the spec as sub-tables.
                    let nested: Vec<(String, toml::Table)> = x
                        .iter()
                        .filter_map(|(k, v)| v.as_table().map(|t| (k.clone(), t.clone())))
                        .collect();
                    for (k, _) in &nested {
                        x.remove(k);
                    }
                    w.table(Some("[projects.schedules.spec]"), &x, &["kind"]);
                    for (k, t) in &nested {
                        w.table(Some(&format!("[projects.schedules.spec.{k}]")), t, &["key"]);
                    }
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

/// One target under `[[<header>]]`, with its `limits` as the sub-table
/// `[<header>.limits]`.
fn write_target(w: &mut Writer, mut t: toml::Table, header: &str) {
    let limits = t.remove("limits");
    w.table(Some(&format!("[[{header}]]")), &t, TARGET_KEYS);
    if let Some(toml::Value::Table(lt)) = limits {
        w.table(
            Some(&format!("[{header}.limits]")),
            &lt,
            &["cpu", "memory", "swap", "pids", "io_weight"],
        );
    }
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

/// The key of a template: `project/name`, or `_/name` for a global one.
pub fn template_key(project: Option<&str>, name: &str) -> String {
    format!("{}/{name}", project.unwrap_or("_"))
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
        Kind::Template => match o.key.split_once('/') {
            Some(("_", n)) => format!("templates.{n}"),
            Some((p, n)) => format!("projects.{p}.templates.{n}"),
            None => format!("templates.{}", o.key),
        },
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
    fn target_limits_parse_and_round_trip() {
        let text = r#"
[[targets]]
name = "shared"
kind = "host"
host = "h1"
[targets.limits]
cpu = 0.5
memory = "1G"

[[projects]]
name = "alpha"

[[projects.targets]]
name = "box"
kind = "host"
host = "h1"
[projects.targets.limits]
cpu = 2
memory = "4G"
pids = 512
"#;
        let f = parse(text, Some("lim"))
            .ok()
            .unwrap_or_else(|e| panic!("{e:#?}"));
        let limits: Vec<String> = f
            .all_targets()
            .filter_map(|t| t.limits.as_ref()?.resolve().ok())
            .map(|l| l.summary())
            .collect();
        assert_eq!(limits, ["0.5 CPU · 1G", "2 CPU · 4G · 512 pids"]);
        let out = f.to_toml();
        let again = parse(&out, Some("lim"))
            .ok()
            .unwrap_or_else(|e| panic!("{e:#?}\n{out}"));
        assert_eq!(f, again, "{out}");
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
        // A trigger with nested tables in its spec round-trips too.
        let trig = format!(
            "{text}{}",
            r#"
[[projects.schedules]]
name = "ci"
catch_up = "once"
[projects.schedules.spec]
kind = "on"
kinds = ["ci.run.failed"]
match = [{ path = "data.branch", op = "glob", value = "main" }]
dedupe = { key = "{{event.data.repo}}", window = "10m" }
escalate = { key = "{{event.data.repo}}", after = 3, reset_on = ["ci.run.succeeded"] }
[projects.schedules.action]
type = "card"
op = "open"
key = "{{event.data.repo}}"
"#
        );
        let f = parse(&trig, None).ok().unwrap();
        assert!(f.projects[0].def.schedules[2].spec.is_trigger());
        let out = f.to_toml();
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

    /// PLAN §8.6: `skills` on a fleet agent and a fleet template; a bad
    /// name is an error.
    #[test]
    fn skills_on_agents_and_templates() {
        let text = r#"
name = "skills"

[[templates]]
name = "reviewer"
harness = "claude"
skills = ["code-review", "code-review", "*"]

[[projects]]
name = "alpha"

[[projects.agents]]
name = "one"
harness = "codex"
skills = ["fern-check"]
skills_off = ["terrarium-db"]
default_skills = false
"#;
        let f = parse(text, None).ok().unwrap();
        let one = &f.projects[0].agents[0].def;
        assert_eq!(one.skills, ["fern-check"]);
        assert_eq!(one.skills_off, ["terrarium-db"]);
        assert!(!one.default_skills);
        let spec = &f.templates[0].spec;
        let def = crate::agent_template::to_def(spec, "x", "alpha").unwrap();
        assert_eq!(def.skills, ["code-review", "*"]);
        assert!(def.default_skills, "default true");
        assert!(def.skills_off.is_empty());
        let star = text.replace("skills_off = [\"terrarium-db\"]", "skills_off = [\"*\"]");
        let p = parse(&star, None);
        assert!(
            errors(&p).iter().any(|e| e.contains("default_skills")),
            "{:?}",
            errors(&p)
        );
        let bad = text.replace("[\"fern-check\"]", "[\"Fern Check\"]");
        let p = parse(&bad, None);
        assert!(
            errors(&p).iter().any(|e| e.contains("skill name")),
            "{:?}",
            errors(&p)
        );
    }

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
    fn a_fleet_file_cannot_set_github_authors() {
        let text = "[[projects]]\nname = \"p\"\ngithub_authors = [\"someone\"]\n";
        let p = parse(text, Some("f"));
        let errs = errors(&p);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("`github_authors` (the GitHub author gate) is not a fleet key"),
            "{errs:?}"
        );
        // The parsed def carries no list, so an apply that ignored the
        // error still sets none.
        assert!(
            p.fleet
                .as_ref()
                .is_none_or(|f| f.projects[0].def.github_authors.is_empty())
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

    #[test]
    fn templates_resolve_merge_and_round_trip() {
        let text = r#"
name = "tpl"

[defaults]
restart = "never"
model = "default-model"

[[templates]]
name = "worker"
description = "A global worker"
harness = "codex"
prompt = "global-worker"

[[templates]]
name = "reviewer"
harness = "pi"

[[projects]]
name = "terra"

[[projects.templates]]
name = "worker"
harness = "claude"
prompt = "worker"
roles = ["project_admin"]
[projects.templates.policy]
targets = ["terra-box"]
mail_to = ["lead"]

[[projects.agents]]
name = "w1"
template = "worker"

[[projects.agents]]
name = "w2"
template = "global:worker"
start = true

[[projects.agents]]
name = "w3"
template = "worker"
model = "m3"
roles = []
[projects.agents.policy]
targets = ["other"]

[[projects.agents]]
name = "r1"
template = "reviewer"
"#;
        let f = parse(text, Some("lim"))
            .ok()
            .unwrap_or_else(|e| panic!("{e:#?}"));
        let agent = |n: &str| f.agents().find(|a| a.def.name == n).unwrap().clone();
        // The project's template wins over the global one of that name.
        let w1 = agent("w1");
        assert_eq!(w1.template.as_deref(), Some("worker"));
        assert_eq!(w1.def.harness, crate::HarnessId::Claude);
        assert_eq!(w1.def.prompt.as_deref(), Some("worker"));
        assert_eq!(w1.def.roles, [Role::ProjectAdmin]);
        // [defaults] fill what the template does not set.
        assert_eq!(w1.def.restart, RestartPolicy::Never);
        assert_eq!(w1.def.model.as_deref(), Some("default-model"));
        // `global:` asks for the global one.
        let w2 = agent("w2");
        assert_eq!(w2.def.harness, crate::HarnessId::Codex);
        assert_eq!(w2.def.prompt.as_deref(), Some("global-worker"));
        assert!(w2.start);
        // The entry's keys win; `policy` merges key by key.
        let w3 = agent("w3");
        assert_eq!(w3.def.model.as_deref(), Some("m3"));
        assert!(w3.def.roles.is_empty());
        assert_eq!(w3.def.policy.targets, ["other"]);
        assert_eq!(w3.def.policy.mail_to, ["lead"]);
        // A project with no template of that name uses the global one.
        assert_eq!(agent("r1").def.harness, crate::HarnessId::Pi);
        let keys: Vec<String> = f
            .objects()
            .into_iter()
            .filter(|o| o.kind == Kind::Template)
            .map(|o| o.key)
            .collect();
        assert_eq!(keys, ["_/worker", "_/reviewer", "terra/worker"]);
        // The file round-trips with the template references.
        let out = f.to_toml();
        assert!(out.contains("template = \"global:worker\""), "{out}");
        assert!(out.contains("[[projects.templates]]"), "{out}");
        let again = parse(&out, None)
            .ok()
            .unwrap_or_else(|e| panic!("{e:#?}\n{out}"));
        assert_eq!(f, again, "{out}");

        // A template that is not in the file: a warning offline, the
        // stored one with the daemon, an error when the daemon has none.
        let other = text.replace("template = \"reviewer\"", "template = \"stored\"");
        let p = parse(&other, None);
        assert!(!p.has_errors(), "{:#?}", p.issues);
        assert!(
            p.issues
                .iter()
                .any(|i| i.message.contains("template `stored`"))
        );
        let stored = |p: Option<&str>, n: &str| {
            (p == Some("terra") && n == "stored").then(|| {
                let mut m = Spec::new();
                m.insert("harness".into(), serde_json::json!("copilot"));
                m
            })
        };
        let p = parse_with(&other, None, Some(&stored));
        let f2 = p.ok().unwrap();
        let r1 = f2.agents().find(|a| a.def.name == "r1").unwrap();
        assert_eq!(r1.def.harness, crate::HarnessId::Copilot);
        let none = |_: Option<&str>, _: &str| None;
        let p = parse_with(&other, None, Some(&none));
        assert!(
            errors(&p).iter().any(|e| e.contains("template `stored`")),
            "{:?}",
            errors(&p)
        );

        // A bad template spec, a key the spec does not have, a duplicate.
        let bad = text.replace("harness = \"pi\"", "model = \"x\"");
        assert!(
            errors(&parse(&bad, None))
                .iter()
                .any(|e| e.contains("templates.reviewer"))
        );
        let bad = text.replace("description = \"A global worker\"", "env = 1");
        assert!(
            errors(&parse(&bad, None))
                .iter()
                .any(|e| e.contains("unknown key `env`"))
        );
        let dup = text.replace("name = \"reviewer\"", "name = \"worker\"");
        assert!(
            errors(&parse(&dup, None))
                .iter()
                .any(|e| e.contains("template `_/worker`"))
        );
    }

    #[test]
    fn system_prompt_in_agents_templates_and_files() {
        let text = r#"
name = "sp"
[defaults]
harness = "claude"
[[projects]]
name = "sp"
[[projects.templates]]
name = "worker"
harness = "pi"
system_prompt_file = "worker.md"
[[projects.agents]]
name = "lead"
system_prompt = "Lead."
[[projects.agents]]
name = "quiet"
system_prompt = ""
[[projects.agents]]
name = "w1"
template = "worker"
"#;
        // The API gets no path: `system_prompt_file` is an error there.
        let p = parse(text, None);
        assert!(p.has_errors());
        assert!(
            p.issues
                .iter()
                .any(|i| i.message.contains("system_prompt_file")),
            "{:?}",
            p.issues
        );
        let dir = std::env::temp_dir().join(format!("terrarium-fleet-sp-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("worker.md"), "Work {{agent.name}}.\n").unwrap();
        let inlined = inline_prompt_files(text, &dir).unwrap();
        let missing = inline_prompt_files(text, Path::new("/nonexistent"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            missing
                .unwrap_err()
                .contains("projects.sp.templates.worker")
        );
        let f = parse(&inlined, None).ok().unwrap();
        let by = |n: &str| f.agents().find(|a| a.def.name == n).unwrap().def.clone();
        assert_eq!(by("lead").system_prompt.as_deref(), Some("Lead."));
        // An empty text is no system prompt.
        assert_eq!(by("quiet").system_prompt, None);
        assert_eq!(
            by("w1").system_prompt.as_deref(),
            Some("Work {{agent.name}}.\n")
        );
        // A text with no `system_prompt_file` stays as it is.
        assert_eq!(
            inline_prompt_files("# c\nname = \"x\"\n", Path::new("/")).unwrap(),
            "# c\nname = \"x\"\n"
        );
        // Over the limit: an error.
        let long = format!(
            "name = \"sp\"\n[[projects]]\nname = \"sp\"\n[[projects.agents]]\nname = \"a\"\nharness = \"pi\"\nsystem_prompt = \"{}\"\n",
            "x".repeat(crate::config::system_prompt_max() + 1)
        );
        assert!(parse(&long, None).has_errors());
    }
}
