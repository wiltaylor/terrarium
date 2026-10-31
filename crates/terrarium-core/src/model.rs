//! Runtime domain objects. On-disk definitions live in [`crate::config`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{AgentDef, CompactMaxAge, IdleStop, Policy, RestartPolicy};
use crate::harness::HarnessId;
use crate::ids::{AgentId, HarnessSessionId};
use crate::role::Role;
use crate::status::AgentStatus;

/// A durable Terrarium agent identity. See PLAN §5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub project: String,
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    /// See [`AgentDef::subscription_pool`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscription_pool: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The agent's files: `<work>/<project>/<name>` (`TERRARIUM_WORK_DIR`,
    /// default `/work`). An agent made before the work root has
    /// `/state/agents/<ulid>/workspace`; its next start moves it.
    pub workspace: PathBuf,
    /// The per-agent harness dir, `/state/agents/<ulid>/harness`. The
    /// supervisor sets it before it calls the adapter. `None`: the
    /// `harness` dir next to `workspace` (the layout before the work root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_dir: Option<PathBuf>,
    /// The prompt template name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// See [`AgentDef::system_prompt`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// See [`AgentDef::persona`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// See [`AgentDef::idle_stop`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop: Option<IdleStop>,
    /// See [`AgentDef::compact_on_idle`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_on_idle: Option<bool>,
    /// See [`AgentDef::compact_max_age`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_max_age: Option<CompactMaxAge>,
    #[serde(default)]
    pub status: AgentStatus,
    /// The current harness session, if one has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<HarnessSessionId>,
    /// See [`AgentDef::new_session_on_prompt_change`].
    #[serde(default = "yes")]
    pub new_session_on_prompt_change: bool,
    /// See [`AgentDef::skills`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// The skills that the daemon copied into `<harness dir>/skills/` for
    /// this start (PLAN §8.6), sorted. The supervisor sets it before it
    /// calls the adapter; the adapter gives the harness these skills and
    /// its skill tool, and no skill tool when the list is empty. Never
    /// stored.
    #[serde(skip)]
    pub delivered_skills: Vec<String>,
    /// The user settings of the harness from the config repo
    /// (`<config repo>/harness/<harness>/<file>`, PLAN §8.2), parsed. A
    /// TOML file (Codex) is here as the same tree in JSON. The supervisor
    /// sets it before it calls the adapter; the adapter lays its forced
    /// keys over it. Never stored.
    #[serde(skip)]
    pub harness_settings: Option<serde_json::Value>,
}

fn yes() -> bool {
    true
}

impl Agent {
    /// True when the agent has its own base system prompt: the adapter
    /// then replaces the harness's default prompt (PLAN §8), else it
    /// appends the Terrarium prompt to the default one.
    pub fn replaces_system_prompt(&self) -> bool {
        self.system_prompt
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
    }

    /// Make a new agent from a definition. The workspace lives under
    /// `state_dir/agents/<ulid>/workspace`.
    pub fn from_def(def: &AgentDef, state_dir: &std::path::Path) -> Self {
        let id = AgentId::new();
        Self {
            id,
            name: def.name.clone(),
            project: def.project.clone(),
            harness: def.harness,
            subscription: def.subscription.clone(),
            subscription_pool: def.subscription_pool.clone(),
            model: def.model.clone(),
            workspace: state_dir
                .join("agents")
                .join(id.to_string())
                .join("workspace"),
            harness_dir: None,
            prompt: def.prompt.clone(),
            system_prompt: def.system_prompt.clone(),
            persona: def.persona.clone(),
            roles: def.roles.clone(),
            policy: def.policy.clone(),
            restart: def.restart,
            idle_stop: def.idle_stop,
            compact_on_idle: def.compact_on_idle,
            compact_max_age: def.compact_max_age,
            status: AgentStatus::Stopped,
            session: None,
            new_session_on_prompt_change: def.new_session_on_prompt_change,
            skills: def.skills.clone(),
            delivered_skills: Vec::new(),
            harness_settings: None,
        }
    }
}
