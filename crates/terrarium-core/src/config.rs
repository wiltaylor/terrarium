//! On-disk TOML definitions: projects, agents, targets, and subscriptions.
//!
//! The daemon reads these from `$TERRARIUM_CONFIG_DIR` (default
//! `/etc/terrarium`), one file per object, in the `projects/`, `agents/`,
//! `targets/`, and `subscriptions/` directories. When a file has no `name`,
//! the file stem is the name.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::harness::HarnessId;
use crate::role::Role;

/// What the agent may reach. See PLAN §9 and §12.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Project targets and global targets, by name.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Mail recipients. `"*"` means every agent in the same project.
    #[serde(default = "default_mail_to")]
    pub mail_to: Vec<String>,
    /// Explicit `project/agent` names outside the project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cross_project: Vec<String>,
}

fn default_mail_to() -> Vec<String> {
    vec!["*".to_owned()]
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            mail_to: default_mail_to(),
            cross_project: Vec::new(),
        }
    }
}

/// What the supervisor does when the harness exits. See PLAN §18.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    #[default]
    OnFailure,
    Never,
}

/// An agent definition, `agents/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub project: String,
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    /// pi and codex take a model flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The prompt template name under `prompts/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Start a fresh session, not a resume, when the rendered prompt
    /// changed and the harness keeps the prompt of a session
    /// (`Adapter::prompt_frozen_on_resume`, for example Codex). The old
    /// session stays in the history. Default true.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub new_session_on_prompt_change: bool,
}

fn yes() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_yes(b: &bool) -> bool {
    *b
}

/// A git remote attached to a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDef {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// A key in `/state/credentials`. Never given to agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The checkout directory name. The default is the repo name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckoutDef {
    /// Repos cloned into `$HOME/work/<repo>` when an agent is made.
    #[serde(default)]
    pub on_agent_create: Vec<String>,
}

/// A project definition, `projects/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The card ID prefix, for example `TER` for `TER-42`. The default comes
    /// from the name. See PLAN §13.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoDef>,
    #[serde(default)]
    pub checkout: CheckoutDef,
    /// The project's schedules (docs/api.md §19): `[[schedules]]` in a
    /// project file, `[[projects.schedules]]` in a fleet file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<crate::schedule::ScheduleDef>,
}

/// The kind of a target. See PLAN §19.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Host,
    Ssh,
    Docker,
    Vmlab,
}

/// A target definition, `targets/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub kind: TargetKind,
    /// The owning project. A target without one is global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The host agent name, for `host`, `docker`, and `vmlab`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `ssh`: the address, `user@host` or `host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// `ssh`: the key name in `/state/credentials`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// `docker`: the image to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `docker`: an existing container to exec into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// `vmlab`: the lab file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab: Option<PathBuf>,
    /// `vmlab`: the machine in the lab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// `vmlab`: the name of a repo of the target's project that holds the
    /// lab definition. On `up` the host agent makes a git worktree of it at
    /// `lab_ref`, checks the lab against its allowlist, and runs it
    /// (docs/api.md §17.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_repo: Option<String>,
    /// `vmlab` with `lab_repo`: a branch, a tag, or a commit. Default: the
    /// repo's default branch, else `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_ref: Option<String>,
    /// `vmlab` with `lab_repo` or `lab_from_workspace`: the directory in the
    /// repo (or the workspace) that holds `vmlab.wcl`. Default: the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_path: Option<String>,
    /// `vmlab`: the lab definition is in the workspace of each holder
    /// (`<work>/<project>/<agent>/<lab_path>`). Each holder gets its own
    /// lab instance.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lab_from_workspace: bool,
    /// Created on agent start and destroyed on agent stop.
    #[serde(default)]
    pub ephemeral: bool,
    /// Keep an ephemeral target after the agent stops.
    #[serde(default)]
    pub keep: bool,
    #[serde(default)]
    pub snapshot_before_task: bool,
}

/// A subscription definition, `subscriptions/<name>.toml`. See PLAN §16.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The harnesses that can spend this subscription.
    #[serde(default)]
    pub harnesses: Vec<HarnessId>,
    /// How to read the remaining quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    /// Alert when any window passes this percentage.
    #[serde(default = "default_alert_at")]
    pub alert_at: u8,
}

fn default_alert_at() -> u8 {
    80
}

/// A definition that has a name, which can come from the file stem.
pub trait Named {
    fn name_mut(&mut self) -> &mut String;
}

macro_rules! named {
    ($($t:ty),*) => {$(
        impl Named for $t {
            fn name_mut(&mut self) -> &mut String {
                &mut self.name
            }
        }
    )*};
}
named!(AgentDef, ProjectDef, TargetDef, SubscriptionDef);

/// Parse one definition file. The file stem fills in a missing name.
pub fn load_file<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Named,
{
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut def: T = toml::from_str(&text).map_err(|source| Error::Toml {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    let name = def.name_mut();
    if name.is_empty()
        && let Some(stem) = path.file_stem()
    {
        *name = stem.to_string_lossy().into_owned();
    }
    Ok(def)
}

/// Every `*.toml` in `dir`, sorted. A missing directory has no files.
fn toml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: dir.to_owned(),
                source,
            });
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml") && p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

fn load_kind<T>(dir: &Path, errors: &mut Vec<Error>) -> Vec<T>
where
    T: for<'de> Deserialize<'de> + Named,
{
    let files = match toml_files(dir) {
        Ok(files) => files,
        Err(e) => {
            errors.push(e);
            return Vec::new();
        }
    };
    files
        .iter()
        .filter_map(|path| match load_file(path) {
            Ok(def) => Some(def),
            Err(e) => {
                errors.push(e);
                None
            }
        })
        .collect()
}

/// All definitions found in a config directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Definitions {
    pub projects: Vec<ProjectDef>,
    pub agents: Vec<AgentDef>,
    pub targets: Vec<TargetDef>,
    pub subscriptions: Vec<SubscriptionDef>,
}

impl Definitions {
    /// Read every definition under `dir`. A file that does not parse goes
    /// into the error list and does not stop the load.
    pub fn load(dir: &Path) -> (Self, Vec<Error>) {
        let mut errors = Vec::new();
        let defs = Self {
            projects: load_kind(&dir.join("projects"), &mut errors),
            agents: load_kind(&dir.join("agents"), &mut errors),
            targets: load_kind(&dir.join("targets"), &mut errors),
            subscriptions: load_kind(&dir.join("subscriptions"), &mut errors),
        };
        (defs, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(text: &str) -> T
    where
        T: for<'de> Deserialize<'de> + Serialize + PartialEq + std::fmt::Debug,
    {
        let parsed: T = toml::from_str(text).expect("parse example");
        let written = toml::to_string(&parsed).expect("serialise");
        let reparsed: T = toml::from_str(&written).expect("parse serialised");
        assert_eq!(parsed, reparsed);
        parsed
    }

    // PLAN §9, agent policy.
    const AGENT_POLICY: &str = r#"
project = "terrarium"
harness = "pi"
subscription = "anthropic-max"        # or "openai-codex", "copilot-pro", ...
model = "anthropic/claude-opus-5-5"   # optional; pi and codex take a model flag

[policy]
targets = ["buildbox", "lab-win11"]   # project targets and global targets by name
mail_to = ["*"]                       # "*" means every agent in the same project
# cross_project = ["ops/triage"]      # explicit project/agent names outside the project
"#;

    // PLAN §10, a leader agent.
    const AGENT_LEAD: &str = r#"
# agents/terrarium-lead.toml
name = "lead"
project = "terrarium"
harness = "claude"
prompt = "project-lead"
roles = ["project_admin"]
"#;

    // PLAN §9, a project.
    const PROJECT: &str = r#"
# projects/terrarium.toml
name = "terrarium"
description = "The Terrarium tool itself"

[[repos]]
name = "terrarium"
url = "git@github.com:wiltaylor/terrarium.git"
default_branch = "main"
credential = "github-deploy"          # key in /state/credentials, never given to agents

[[repos]]
name = "copilot-prompt"
url = "https://github.com/wiltaylor/CopilotCustomSystemPrompt.git"

[checkout]
on_agent_create = ["terrarium"]       # cloned into $HOME/work/terrarium when the agent is made
"#;

    // PLAN §19, a target.
    const TARGET: &str = r#"
# targets/lab-win11.toml
kind = "vmlab"
host = "lab1"                 # host agent name
lab  = "/home/wil/labs/win11.wcl"
machine = "win11"
ephemeral = true
snapshot_before_task = true
"#;

    // PLAN §16, a subscription.
    const SUBSCRIPTION: &str = r#"
# subscriptions/anthropic-max.toml
name = "anthropic-max"
provider = "anthropic"
plan = "Max 20x"
harnesses = ["claude", "pi"]           # which harnesses can spend it
probe = "anthropic-oauth-usage"        # how to read remaining quota
alert_at = 80                          # percent of any window
"#;

    #[test]
    fn agent_policy_example_round_trips() {
        let def: AgentDef = round_trip(AGENT_POLICY);
        assert_eq!(def.harness, HarnessId::Pi);
        assert_eq!(def.subscription.as_deref(), Some("anthropic-max"));
        assert_eq!(def.policy.targets, ["buildbox", "lab-win11"]);
        assert_eq!(def.policy.mail_to, ["*"]);
        assert!(def.policy.cross_project.is_empty());
        assert_eq!(def.restart, RestartPolicy::OnFailure);
    }

    #[test]
    fn agent_lead_example_round_trips() {
        let def: AgentDef = round_trip(AGENT_LEAD);
        assert_eq!(def.name, "lead");
        assert_eq!(def.roles, [Role::ProjectAdmin]);
        assert_eq!(def.prompt.as_deref(), Some("project-lead"));
        assert_eq!(def.policy, Policy::default());
    }

    #[test]
    fn project_example_round_trips() {
        let def: ProjectDef = round_trip(PROJECT);
        assert_eq!(def.name, "terrarium");
        assert_eq!(def.repos.len(), 2);
        assert_eq!(def.repos[0].credential.as_deref(), Some("github-deploy"));
        assert_eq!(def.repos[1].default_branch, None);
        assert_eq!(def.checkout.on_agent_create, ["terrarium"]);
    }

    #[test]
    fn target_example_round_trips() {
        let def: TargetDef = round_trip(TARGET);
        assert_eq!(def.kind, TargetKind::Vmlab);
        assert_eq!(def.host.as_deref(), Some("lab1"));
        assert!(def.ephemeral && def.snapshot_before_task && !def.keep);
    }

    #[test]
    fn subscription_example_round_trips() {
        let def: SubscriptionDef = round_trip(SUBSCRIPTION);
        assert_eq!(def.harnesses, [HarnessId::Claude, HarnessId::Pi]);
        assert_eq!(def.alert_at, 80);
    }

    #[test]
    fn restart_policy_uses_kebab_case() {
        let def: AgentDef =
            toml::from_str("project = \"p\"\nharness = \"codex\"\nrestart = \"always\"\n")
                .expect("parse");
        assert_eq!(def.restart, RestartPolicy::Always);
        assert!(
            toml::to_string(&AgentDef {
                restart: RestartPolicy::OnFailure,
                ..def
            })
            .expect("serialise")
            .contains("restart = \"on-failure\"")
        );
    }

    #[test]
    fn load_reads_a_config_dir_and_names_from_stems() {
        let dir = std::env::temp_dir().join(format!("terrarium-core-test-{}", ulid::Ulid::new()));
        for sub in ["projects", "agents", "targets", "subscriptions"] {
            std::fs::create_dir_all(dir.join(sub)).expect("mkdir");
        }
        std::fs::write(dir.join("projects/terrarium.toml"), PROJECT).expect("write");
        std::fs::write(dir.join("agents/worker.toml"), AGENT_POLICY).expect("write");
        std::fs::write(dir.join("agents/terrarium-lead.toml"), AGENT_LEAD).expect("write");
        std::fs::write(dir.join("agents/broken.toml"), "harness = 7").expect("write");
        std::fs::write(dir.join("agents/notes.txt"), "ignored").expect("write");
        std::fs::write(dir.join("targets/lab-win11.toml"), TARGET).expect("write");
        // subscriptions/ stays empty.

        let (defs, errors) = Definitions::load(&dir);
        std::fs::remove_dir_all(&dir).expect("cleanup");

        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(defs.projects.len(), 1);
        let names: Vec<_> = defs.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["lead", "worker"]);
        assert_eq!(defs.targets[0].name, "lab-win11");
        assert!(defs.subscriptions.is_empty());
    }

    #[test]
    fn load_of_a_missing_dir_is_empty() {
        let (defs, errors) = Definitions::load(Path::new("/nonexistent/terrarium"));
        assert!(errors.is_empty());
        assert_eq!(defs, Definitions::default());
    }
}
