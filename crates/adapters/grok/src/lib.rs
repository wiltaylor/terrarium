//! Adapter for Grok Build (`grok`), the harness of an xAI plan
//! (`terrarium_core::xai`, TER-1190 chose it, TER-1192 builds it).
//!
//! The facts come from `grok --help` of 1.0.77 and the public docs
//! (res-1190 on TER-1190). Nothing was checked in a signed-in TUI yet.
//! These are release gates, and need the operator's OK through the lead:
//! the PTY lifecycle (prompt readiness, submit, cancel with Ctrl+C,
//! quit), the hook payload fields, a refresh of `auth.json` in place, the
//! model IDs, and the update suppression. Until then:
//!
//! - the runner reads readiness from the hooks and its generic detector:
//!   [`Adapter::screen_hint`] is `Unknown`;
//! - usage is not read: `grok usage SESSION_ID` waits for its schema, and
//!   the status-line context percentage is not pool usage;
//! - `/compact` is off ([`HarnessId::compact_command`]).
//!
//! - [`launch`]: the command line and env.
//! - [`config`]: the per-agent files (the personal hooks) and the overlays.
//! - [`hooks`]: hook payloads to events.
//! - [`keys`]: turns, answers, interrupt and exit as keystrokes.
//! - [`session`]: the recorded session UUID.

pub mod config;
pub mod flags;
pub mod hooks;
pub mod keys;
pub mod launch;
pub mod session;

use std::path::{Path, PathBuf};
use std::time::Duration;

use semver::Version;
use terrarium_core::{
    Adapter, Agent, Answer, Event, FileOverlay, HarnessId, HarnessSessionId, HookPayload,
    LaunchSpec, Result,
};

pub use flags::{GROK_1_0, GrokFlags};
pub use launch::SessionStart;

/// How the rendered prompt reaches grok.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptMode {
    /// `--append-system-prompt`: after grok's default prompt.
    #[default]
    Append,
    /// `--system-prompt`: instead of grok's default prompt.
    Replace,
}

/// Settings of the adapter. The defaults suit a run with no sandbox
/// (`/auth/grok`); the daemon sets the sandbox paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokOptions {
    /// The flag table for the installed version.
    pub flags: GrokFlags,
    /// The program to run. The daemon sets the absolute `bin/grok` of the
    /// active version dir.
    pub program: String,
    /// `GROK_HOME` as the harness sees it.
    pub home_dir: PathBuf,
    /// The same directory as the daemon sees it. `None`: `home_dir`.
    pub host_home_dir: Option<PathBuf>,
    /// The cwd as the harness sees it (`$HOME/work` in the sandbox).
    /// `None`: the agent workspace.
    pub harness_workspace: Option<PathBuf>,
    /// The mode for an agent with no base system prompt.
    pub prompt_mode: PromptMode,
    /// The CLI that the hooks run.
    pub hook_program: String,
}

impl Default for GrokOptions {
    fn default() -> Self {
        Self {
            flags: GROK_1_0,
            program: HarnessId::Grok.program().to_owned(),
            home_dir: PathBuf::from(HarnessId::Grok.auth_dir()),
            host_home_dir: None,
            harness_workspace: None,
            prompt_mode: PromptMode::Append,
            hook_program: "terrarium".to_owned(),
        }
    }
}

impl GrokOptions {
    /// [`PromptMode::Replace`] for an agent with its own base system
    /// prompt, else [`Self::prompt_mode`].
    pub fn prompt_mode_for(&self, agent: &Agent) -> PromptMode {
        if agent.replaces_system_prompt() {
            PromptMode::Replace
        } else {
            self.prompt_mode
        }
    }

    /// The cwd of the harness process.
    pub fn harness_cwd(&self, agent: &Agent) -> PathBuf {
        self.harness_workspace
            .clone()
            .unwrap_or_else(|| agent.workspace.clone())
    }

    /// The Grok home as the daemon sees it.
    pub fn host_home_dir(&self) -> &Path {
        self.host_home_dir.as_deref().unwrap_or(&self.home_dir)
    }

    /// The sessions directory of `agent` (the daemon's view). In the
    /// sandbox (a `harness_workspace` is set) it is the agent's own
    /// overlay; with no sandbox the overlays are not bound, so it is the
    /// shared home's.
    pub fn sessions_dir(&self, agent: &Agent) -> PathBuf {
        if self.harness_workspace.is_some() {
            config::sessions_dir(agent)
        } else {
            self.host_home_dir().join(config::SESSIONS_DIR)
        }
    }
}

/// The Grok Build adapter.
#[derive(Debug, Clone, Default)]
pub struct GrokAdapter {
    pub options: GrokOptions,
}

impl GrokAdapter {
    pub fn new(options: GrokOptions) -> Self {
        Self { options }
    }

    /// The launch spec with an explicit session start and prompt. Pure.
    pub fn launch_with(
        &self,
        agent: &Agent,
        session: SessionStart<'_>,
        prompt: Option<&str>,
    ) -> LaunchSpec {
        launch::build(&self.options, agent, session, prompt)
    }

    /// Write the per-agent files. `prompt`: see [`config::materialize`].
    pub fn materialize(
        &self,
        agent: &Agent,
        home: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<FileOverlay>> {
        config::materialize(&self.options, agent, home, prompt)
    }
}

impl Adapter for GrokAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Grok
    }

    /// `grok --version` prints "grok 1.0.77 (50ca579fa996)".
    fn detect_version(&self) -> Result<Version> {
        terrarium_core::adapter::run_version(&self.options.program)
    }

    /// A fresh start gets a new UUID (`--session-id`), recorded in
    /// `<agent dir>/grok/session-id` for [`Adapter::find_session_id`]. A
    /// failed write only loses that fallback.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let prompt = config::read_prompt(agent).ok().flatten();
        match resume {
            Some(id) => self.launch_with(agent, SessionStart::Resume(id), prompt.as_deref()),
            None => {
                let uuid = launch::new_session_uuid();
                let _ = session::record_session_id(agent, &uuid);
                self.launch_with(agent, SessionStart::Fresh(&uuid), prompt.as_deref())
            }
        }
    }

    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        hooks::hook_events(hook)
    }

    fn find_session_id(&self, agent: &Agent) -> Option<HarnessSessionId> {
        session::find_session_id(&self.options.sessions_dir(agent), agent)
    }

    fn type_turn(&self, text: &str) -> Vec<u8> {
        keys::type_turn(text)
    }

    fn turn_submit_pause(&self) -> Duration {
        keys::SUBMIT_PAUSE
    }

    fn answer(&self, _request_id: &str, answer: &Answer) -> Vec<u8> {
        keys::answer(answer)
    }

    fn interrupt_keys(&self) -> Vec<Vec<u8>> {
        keys::interrupt()
    }

    fn exit_keys(&self) -> Vec<u8> {
        keys::exit()
    }

    fn exit_key_pause(&self) -> Duration {
        keys::SUBMIT_PAUSE
    }

    /// Not captured whether a resume takes a changed prompt: the
    /// supervisor starts a fresh session when the prompt changed.
    fn prompt_frozen_on_resume(&self) -> bool {
        true
    }

    /// Uses the `prompt.md` that the daemon wrote.
    fn materialize_config(&self, agent: &Agent, home: &Path) -> Result<Vec<FileOverlay>> {
        self.materialize(agent, home, None)
    }
}

#[cfg(test)]
pub(crate) fn test_agent_in(root: &Path, model: Option<&str>) -> Agent {
    let model = model
        .map(|m| format!("model = \"{m}\"\n"))
        .unwrap_or_default();
    let def: terrarium_core::config::AgentDef = toml::from_str(&format!(
        "name = \"a\"\nproject = \"p\"\nharness = \"grok\"\n{model}"
    ))
    .expect("agent def");
    Agent::from_def(&def, root)
}

#[cfg(test)]
pub(crate) fn test_agent(model: Option<&str>) -> Agent {
    test_agent_in(Path::new("/state"), model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_verified_version_is_the_pin() {
        assert_eq!(GROK_1_0.verified, terrarium_core::harnesses::grok::VERSION);
        assert_eq!(terrarium_core::harnesses::grok::PACKAGE, "@xai-official/grok");
        assert_eq!(terrarium_core::harnesses::grok::BINARY, "grok");
        assert!(Version::parse(GROK_1_0.verified).is_ok());
        assert_eq!(GrokAdapter::default().id(), HarnessId::Grok);
        assert_eq!(GROK_1_0.home_env, HarnessId::Grok.home_env());
    }

    #[test]
    fn the_version_output_parses() {
        assert_eq!(
            terrarium_core::adapter::parse_version_output("grok 1.0.77 (50ca579fa996)\n")
                .expect("v"),
            Version::new(1, 0, 77)
        );
    }

    #[test]
    fn trait_launch_records_fresh_ids() {
        let tmp = tempfile::tempdir().expect("tmp");
        let agent = test_agent_in(tmp.path(), None);
        let a = GrokAdapter::new(GrokOptions {
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            home_dir: PathBuf::from("/home/agent/.grok"),
            ..GrokOptions::default()
        });
        std::fs::create_dir_all(config::agent_dir(&agent)).expect("dir");
        std::fs::write(config::prompt_path(&agent), "the prompt").expect("prompt");
        let sid = |s: &LaunchSpec| {
            let i = s
                .args
                .iter()
                .position(|x| x == "--session-id")
                .expect("flag");
            s.args[i + 1].clone()
        };
        let one = a.launch(&agent, None);
        let two = a.launch(&agent, None);
        assert_ne!(sid(&one), sid(&two));
        assert!(
            one.args
                .windows(2)
                .any(|w| w[0] == "--append-system-prompt" && w[1] == "the prompt")
        );
        // The last one is recorded; it counts once grok wrote it.
        assert_eq!(a.find_session_id(&agent), None);
        let sessions = config::sessions_dir(&agent);
        std::fs::create_dir_all(&sessions).expect("dir");
        std::fs::write(sessions.join(format!("{}.jsonl", sid(&two))), "").expect("w");
        let id = HarnessSessionId(sid(&two));
        assert_eq!(a.find_session_id(&agent), Some(id.clone()));
        let r = a.launch(&agent, Some(&id));
        assert!(
            r.args
                .windows(2)
                .any(|w| w[0] == "--resume" && w[1] == id.0)
        );
    }

    #[test]
    fn trait_methods() {
        let a: &dyn Adapter = &GrokAdapter::default();
        assert_eq!(a.type_turn("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
        assert!(!a.turn_submit_pause().is_zero());
        assert_eq!(a.interrupt_keys(), vec![vec![0x03]]);
        assert_eq!(a.exit_keys(), b"/quit\r".to_vec());
        assert!(!a.exit_key_pause().is_zero());
        assert_eq!(a.answer("r", &Answer::Choice(1)), b"\x1b[B\r".to_vec());
        assert!(a.prompt_frozen_on_resume());
        assert!(!a.waits_for_startup_hook());
        let ev = a.hook_event(&HookPayload {
            harness: HarnessId::Grok,
            hook: "Stop".into(),
            body: json!({"hookEventName": "Stop", "sessionId": "s"}),
        });
        assert_eq!(ev, [Event::TurnEnd]);
    }

    /// Spawns the real binary, with no login. Run with
    /// `cargo test -- --ignored` where grok is on PATH.
    #[test]
    #[ignore = "needs the grok binary on PATH"]
    fn smoke_grok_version_and_help() {
        let v = GrokAdapter::default().detect_version().expect("version");
        assert!(!v.to_string().is_empty());
        let help = std::process::Command::new("grok")
            .arg("--help")
            .output()
            .expect("run grok --help");
        let help = String::from_utf8_lossy(&help.stdout);
        let f = GROK_1_0;
        for flag in [f.session_id, f.resume, f.model, f.cwd] {
            assert!(help.contains(flag), "{flag} is not in grok --help");
        }
    }
}
