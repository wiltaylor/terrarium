//! Adapter for Claude Code. Flags from PLAN §4; the Phase 0 spikes confirm
//! them in `docs/compat.md`.

use semver::{Version, VersionReq};
use terrarium_core::adapter::run_version;
use terrarium_core::{
    Adapter, Agent, Event, HarnessId, HarnessSessionId, HookPayload, LaunchSpec, Result,
};

/// The Claude Code version this adapter was tested against.
pub const TESTED: &str = "=2.1.310";

#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeAdapter;

impl Adapter for ClaudeAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Claude
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(TESTED).unwrap_or(VersionReq::STAR)
    }

    fn detect_version(&self) -> Result<Version> {
        run_version("claude")
    }

    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let mut spec = LaunchSpec::new("claude", &agent.workspace)
            .arg("--dangerously-skip-permissions")
            .env("DISABLE_AUTOUPDATER", "1")
            .env("DISABLE_TELEMETRY", "1");
        if let Some(model) = &agent.model {
            spec = spec.arg("--model").arg(model.as_str());
        }
        match resume {
            Some(id) => spec.arg("--resume").arg(id.0.as_str()),
            None => spec,
        }
    }

    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        let mut events = Vec::new();
        if hook.hook == "SessionStart"
            && let Some(id) = hook.str_field("session_id")
        {
            events.push(Event::SessionStarted {
                harness_session: HarnessSessionId(id.to_owned()),
            });
        }
        match hook.hook.as_str() {
            "UserPromptSubmit" => events.push(Event::TurnStart {
                prompt: hook.str_field("prompt").map(str::to_owned),
            }),
            "Stop" => events.push(Event::TurnEnd),
            "PreToolUse" => events.push(Event::ToolUse {
                tool: hook.str_field("tool_name").unwrap_or("unknown").to_owned(),
                input: hook.body.get("tool_input").cloned().unwrap_or_default(),
            }),
            "Notification" => events.push(Event::Notification {
                message: hook.str_field("message").unwrap_or_default().to_owned(),
                question: true,
            }),
            _ => {}
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(name: &str, body: serde_json::Value) -> HookPayload {
        HookPayload {
            harness: HarnessId::Claude,
            hook: name.into(),
            body,
        }
    }

    #[test]
    fn hooks_map_to_events() {
        let a = ClaudeAdapter;
        assert_eq!(
            a.hook_event(&hook("Stop", serde_json::json!({}))),
            [Event::TurnEnd]
        );
        assert_eq!(
            a.hook_event(&hook(
                "UserPromptSubmit",
                serde_json::json!({"prompt": "hi"})
            )),
            [Event::TurnStart {
                prompt: Some("hi".into())
            }]
        );
        assert!(
            a.hook_event(&hook("Unknown", serde_json::json!({})))
                .is_empty()
        );
        assert!(a.tested_version().matches(&Version::new(2, 1, 310)));
    }
}
