//! The one place that picks the adapter of a runner process. The daemon
//! builds the launch spec, so the runner needs the adapter only for
//! `hook_event`, `screen_hint`, `type_turn`, `clear_input_keys`, and
//! `answer`. The options set here are the ones that those methods read.

use std::sync::Arc;

use semver::{Version, VersionReq};
use terrarium_adapter_claude::{ClaudeAdapter, ClaudeOptions};
use terrarium_adapter_codex::{CodexAdapter, CodexOptions};
use terrarium_adapter_copilot::{CopilotAdapter, CopilotOptions};
use terrarium_adapter_pi::{PiAdapter, PiOptions};
use terrarium_core::{
    Adapter, Agent, Answer, Event, HarnessId, HarnessSessionId, HookPayload, LaunchSpec,
};
use terrarium_runner::{AdapterKind, RunnerSpec};

pub fn select(spec: &RunnerSpec) -> Arc<dyn Adapter> {
    if spec.adapter == AdapterKind::Fake {
        return Arc::new(FakeAdapter {
            harness: spec.harness,
        });
    }
    let o = &spec.adapter_options;
    match spec.harness {
        HarnessId::Claude => {
            let mut opts = ClaudeOptions::default();
            if let Some(d) = &o.config_dir {
                opts.config_dir = d.clone();
            }
            opts.host_config_dir = o.host_config_dir.clone();
            Arc::new(ClaudeAdapter::new(opts))
        }
        HarnessId::Codex => {
            let mut opts = CodexOptions::default();
            if let Some(d) = &o.config_dir {
                opts.codex_home = d.clone();
            }
            opts.host_codex_home = o.host_config_dir.clone();
            Arc::new(CodexAdapter::new(opts))
        }
        HarnessId::Copilot => {
            let mut opts = CopilotOptions::default();
            if let Some(d) = &o.config_dir {
                opts.config_dir = d.clone();
            }
            opts.host_config_dir = o.host_config_dir.clone();
            Arc::new(CopilotAdapter::new(opts))
        }
        HarnessId::Pi => {
            let mut opts = PiOptions::default();
            if let Some(d) = &o.config_dir {
                opts.agent_dir = d.clone();
            }
            opts.host_agent_dir = o.host_config_dir.clone();
            Arc::new(PiAdapter::new(opts))
        }
    }
}

/// `adapter: "fake"`: the hook names of Claude Code, no screen hints, and
/// `/quit` as the exit keys. The runner tests drive the fake TUI with it.
struct FakeAdapter {
    harness: HarnessId,
}

impl Adapter for FakeAdapter {
    fn id(&self) -> HarnessId {
        self.harness
    }
    fn tested_version(&self) -> VersionReq {
        VersionReq::STAR
    }
    fn detect_version(&self) -> terrarium_core::Result<Version> {
        Ok(Version::new(0, 0, 0))
    }
    fn launch(&self, agent: &Agent, _resume: Option<&HarnessSessionId>) -> LaunchSpec {
        LaunchSpec::new("true", &agent.workspace)
    }
    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        match hook.hook.as_str() {
            "SessionStart" => hook
                .str_field("session_id")
                .map(|id| Event::SessionStarted {
                    harness_session: HarnessSessionId(id.to_owned()),
                    transcript_path: None,
                    source: hook.str_field("source").map(str::to_owned),
                })
                .into_iter()
                .collect(),
            "UserPromptSubmit" => vec![Event::TurnStart {
                prompt: hook.str_field("prompt").map(str::to_owned),
            }],
            "Stop" => vec![Event::TurnEnd],
            "Notification" => vec![Event::Notification {
                message: hook.str_field("message").unwrap_or_default().to_owned(),
                question: true,
            }],
            _ => vec![],
        }
    }
    /// A `UserPromptSubmit` with a `<system_notification>` prompt, as
    /// Copilot sends it.
    fn is_system_prompt(&self, hook: &HookPayload) -> bool {
        hook.hook == "UserPromptSubmit"
            && hook
                .str_field("prompt")
                .is_some_and(|p| p.trim_start().starts_with("<system_notification>"))
    }
    /// The fake TUI exits 0 on `/quit`.
    fn exit_keys(&self) -> Vec<u8> {
        b"/quit\r".to_vec()
    }
    fn answer(&self, _request_id: &str, answer: &Answer) -> Vec<u8> {
        match answer {
            Answer::Allow => b"y\r".to_vec(),
            Answer::Deny => b"n\r".to_vec(),
            Answer::Choice(_) => Vec::new(),
        }
    }
}
