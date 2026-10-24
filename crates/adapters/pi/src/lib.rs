//! Adapter for pi. Flags from PLAN §4. The hook mapping is not known yet;
//! the Phase 0 spikes fill it in (see `docs/compat.md`).

use semver::{Version, VersionReq};
use terrarium_core::adapter::run_version;
use terrarium_core::{
    Adapter, Agent, Event, HarnessId, HarnessSessionId, HookPayload, LaunchSpec, Result,
};

/// The harness version this adapter was tested against.
pub const TESTED: &str = "=0.85.1";

#[derive(Debug, Default, Clone, Copy)]
pub struct PiAdapter;

impl Adapter for PiAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(TESTED).unwrap_or(VersionReq::STAR)
    }

    fn detect_version(&self) -> Result<Version> {
        run_version("pi")
    }

    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let mut spec = LaunchSpec::new("pi", &agent.workspace).env("PI_TELEMETRY", "0");
        if let Some(model) = &agent.model {
            spec = spec.arg("--model").arg(model.as_str());
        }
        match resume {
            Some(id) => spec.arg("--session").arg(id.0.as_str()),
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
        assert_eq!(PiAdapter.id(), HarnessId::Pi);
        assert_ne!(PiAdapter.tested_version(), VersionReq::STAR);
    }
}
