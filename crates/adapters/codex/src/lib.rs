//! Adapter for codex. Flags from PLAN §4. The hook mapping is not known yet;
//! the Phase 0 spikes fill it in (see `docs/compat.md`).

use semver::{Version, VersionReq};
use terrarium_core::adapter::run_version;
use terrarium_core::{
    Adapter, Agent, Event, HarnessId, HarnessSessionId, HookPayload, LaunchSpec, Result,
};

/// The harness version this adapter was tested against.
pub const TESTED: &str = "=0.160.0";

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(TESTED).unwrap_or(VersionReq::STAR)
    }

    fn detect_version(&self) -> Result<Version> {
        run_version("codex")
    }

    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let mut spec = LaunchSpec::new("codex", &agent.workspace);
        if let Some(id) = resume {
            spec = spec.arg("resume").arg(id.0.as_str());
        }
        spec = spec
            .arg("--dangerously-bypass-approvals-and-sandbox")
            .arg("--skip-git-repo-check");
        match &agent.model {
            Some(model) => spec.arg("--model").arg(model.as_str()),
            None => spec,
        }
    }

    fn hook_event(&self, _hook: &HookPayload) -> Vec<Event> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tested_version_parses() {
        assert_eq!(CodexAdapter.id(), HarnessId::Codex);
        assert_ne!(CodexAdapter.tested_version(), VersionReq::STAR);
    }
}
