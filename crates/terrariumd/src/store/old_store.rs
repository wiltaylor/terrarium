//! The in-memory agent store. The supervisor (Phase 1) fills it; for now it
//! starts empty.

use std::collections::BTreeMap;
use std::sync::RwLock;

use terrarium_core::{Agent, AgentId};

#[derive(Debug, Default)]
pub struct AgentStore {
    agents: RwLock<BTreeMap<AgentId, Agent>>,
}

impl AgentStore {
    /// All agents, optionally in one project, ordered by ID.
    pub fn list(&self, project: Option<&str>) -> Vec<Agent> {
        let agents = match self.agents.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        agents
            .values()
            .filter(|a| project.is_none_or(|p| a.project == p))
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub fn insert(&self, agent: Agent) {
        let mut agents = match self.agents.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        agents.insert(agent.id, agent);
    }
}
