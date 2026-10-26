//! Runtime domain objects. On-disk definitions live in [`crate::config`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{AgentDef, Policy, RestartPolicy};
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
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub status: AgentStatus,
    /// The current harness session, if one has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<HarnessSessionId>,
    /// See [`AgentDef::new_session_on_prompt_change`].
    #[serde(default = "yes")]
    pub new_session_on_prompt_change: bool,
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
            model: def.model.clone(),
            workspace: state_dir
                .join("agents")
                .join(id.to_string())
                .join("workspace"),
            harness_dir: None,
            prompt: def.prompt.clone(),
            system_prompt: def.system_prompt.clone(),
            roles: def.roles.clone(),
            policy: def.policy.clone(),
            restart: def.restart,
            status: AgentStatus::Stopped,
            session: None,
            new_session_on_prompt_change: def.new_session_on_prompt_change,
        }
    }
}
