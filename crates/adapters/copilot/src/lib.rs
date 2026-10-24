//! Adapter for copilot. Flags from PLAN §4. The hook mapping is not known yet;
//! the Phase 0 spikes fill it in (see `docs/compat.md`).

use semver::{Version, VersionReq};
use terrarium_core::adapter::run_version;
use terrarium_core::{
    Adapter, Agent, Event, HarnessId, HarnessSessionId, HookPayload, LaunchSpec, Result,
};

/// The harness version this adapter was tested against.
pub const TESTED: &str = "=1.0.103";

#[derive(Debug, Default, Clone, Copy)]
pub struct CopilotAdapter;

impl Adapter for CopilotAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Copilot
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(TESTED).unwrap_or(VersionReq::STAR)
    }

    fn detect_version(&self) -> Result<Version> {
        run_version("copilot")
    }

    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let spec = LaunchSpec::new("copilot", &agent.workspace)
            .arg("--experimental")
            .arg("--yolo")
            .arg("--disable-client-telemetry")
            .env("COPILOT_ALLOW_ALL", "true")
            .env("COPILOT_AUTO_UPDATE", "false");
        match resume {
            Some(id) => spec.arg("--resume").arg(id.0.as_str()),
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
        assert_eq!(CopilotAdapter.id(), HarnessId::Copilot);
        assert_ne!(CopilotAdapter.tested_version(), VersionReq::STAR);
    }
}
