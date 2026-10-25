//! Adapter for Claude Code (PLAN §4, §7, §8). Every fact here comes from the
//! Phase 0 spikes in `docs/compat.md`; a doc comment says so where a fact
//! is only read from the binary, or not tested.
//!
//! - [`launch`]: the command line and env (`ClaudeAdapter::launch_with`).
//! - [`config`]: the per-agent `.claude.json` and the overlays.
//! - [`hooks`]: hook payloads to events.
//! - [`screen`]: the prompt-ready detector.
//! - [`keys`]: turns and answers as keystrokes.
//! - [`session`]: the session file path.
//! - [`usage`]: token usage from the session file.
//! - [`auth`]: `claude auth status` and the plan tier.

pub mod auth;
pub mod config;
pub mod error;
pub mod flags;
pub mod hooks;
pub mod keys;
pub mod launch;
pub mod screen;
pub mod session;
pub mod usage;

use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};
use terrarium_core::adapter::{Screen, run_version};
use terrarium_core::{
    Adapter, Agent, Answer, Event, FileOverlay, HarnessId, HarnessSessionId, HookPayload,
    LaunchSpec, Result, ScreenHint,
};

pub use error::ClaudeError;
pub use flags::{CLAUDE_2_1, ClaudeFlags};
pub use launch::SessionStart;

/// The Claude Code versions this adapter accepts. Only
/// [`ClaudeFlags::verified`] (the pin in `harnesses.toml`) was measured.
pub const TESTED: &str = CLAUDE_2_1.tested;

/// How the rendered prompt reaches Claude Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptMode {
    /// `--append-system-prompt`: the text goes after the default prompt.
    #[default]
    Append,
    /// `--system-prompt`: the text replaces the default prompt. Claude Code
    /// still keeps the first line, "You are Claude Code, Anthropic's
    /// official CLI for Claude." It cannot be removed. The tools stay.
    Replace,
}

/// Settings of the adapter. The defaults suit the container layout of
/// PLAN §11.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeOptions {
    /// The flag table for the installed version.
    pub flags: ClaudeFlags,
    /// The program to run.
    pub program: String,
    /// `CLAUDE_CONFIG_DIR` as the harness sees it. Always set it: without
    /// it, Claude Code writes `.claude.json` to `$HOME`, outside the home.
    pub config_dir: PathBuf,
    /// The same directory as the daemon sees it, for reading session files
    /// and credentials. `None`: the same as `config_dir`.
    pub host_config_dir: Option<PathBuf>,
    /// The cwd as the harness sees it (for example `$HOME/work` in the
    /// sandbox). `None`: the agent workspace. The trust key and the session
    /// directory name use this path.
    pub harness_workspace: Option<PathBuf>,
    pub prompt_mode: PromptMode,
    /// The CLI that the hooks run.
    pub hook_program: String,
    /// Whether the harness runs as root (then `IS_SANDBOX=1` is set).
    /// `None`: look at the uid of this process.
    pub run_as_root: Option<bool>,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        Self {
            flags: CLAUDE_2_1,
            program: HarnessId::Claude.program().to_owned(),
            config_dir: PathBuf::from(HarnessId::Claude.auth_dir()),
            host_config_dir: None,
            harness_workspace: None,
            prompt_mode: PromptMode::Append,
            hook_program: "terrarium".to_owned(),
            run_as_root: None,
        }
    }
}

impl ClaudeOptions {
    /// The cwd of the harness process.
    pub fn harness_cwd(&self, agent: &Agent) -> PathBuf {
        self.harness_workspace
            .clone()
            .unwrap_or_else(|| agent.workspace.clone())
    }

    /// The config dir as the daemon sees it.
    pub fn host_config_dir(&self) -> &Path {
        self.host_config_dir.as_deref().unwrap_or(&self.config_dir)
    }

    /// The directory that holds the agent's `projects/<cwd>/` (the
    /// daemon's view). In the sandbox (a `harness_workspace` is set) it is
    /// the agent's own `harness/claude/`, which the sandbox binds over the
    /// shared one ([`config`]). With no sandbox the overlays are not bound,
    /// so it is the shared home.
    pub fn session_home(&self, agent: &Agent) -> PathBuf {
        if self.harness_workspace.is_some() {
            config::claude_dir(agent)
        } else {
            self.host_config_dir().to_path_buf()
        }
    }
}

fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

/// The Claude Code adapter.
#[derive(Debug, Clone, Default)]
pub struct ClaudeAdapter {
    pub options: ClaudeOptions,
}

impl ClaudeAdapter {
    pub fn new(options: ClaudeOptions) -> Self {
        Self { options }
    }

    fn as_root(&self) -> bool {
        self.options.run_as_root.unwrap_or_else(running_as_root)
    }

    /// The launch spec with an explicit session start and prompt text.
    /// Use this when the caller must know the new session UUID up front.
    pub fn launch_with(
        &self,
        agent: &Agent,
        session: SessionStart<'_>,
        prompt: Option<&str>,
    ) -> LaunchSpec {
        launch::build(&self.options, agent, session, prompt, self.as_root())
    }

    /// Write the per-agent files. `prompt`: see [`config::materialize`].
    pub fn materialize(
        &self,
        agent: &Agent,
        home: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<FileOverlay>> {
        Ok(config::materialize(&self.options, agent, home, prompt)?)
    }

    /// `claude auth status` against the shared home. This blocks.
    pub fn auth_status(&self) -> Result<auth::AuthStatus> {
        Ok(auth::auth_status(
            &self.options.program,
            self.options.host_config_dir(),
        )?)
    }

    /// The plan tier from `.credentials.json` in the shared home.
    pub fn plan_tier(&self) -> Result<Option<auth::PlanTier>> {
        Ok(auth::plan_tier(self.options.host_config_dir())?)
    }

    /// The session file of `id` for this agent (the daemon's view).
    pub fn session_file(&self, agent: &Agent, id: &HarnessSessionId) -> PathBuf {
        session::session_file(
            &self.options.session_home(agent),
            &self.options.harness_cwd(agent),
            id,
        )
    }
}

impl Adapter for ClaudeAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Claude
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(self.options.flags.tested).unwrap_or(VersionReq::STAR)
    }

    fn detect_version(&self) -> Result<Version> {
        run_version(&self.options.program)
    }

    /// A fresh start gets a new UUID (`--session-id`); the `SessionStart`
    /// hook reports it. The prompt comes from `<agent dir>/prompt.md`. A
    /// prompt file that cannot be read gives a start with no prompt.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let prompt = config::read_prompt(agent).ok().flatten();
        let uuid;
        let session = match resume {
            Some(id) => SessionStart::Resume(id),
            None => {
                uuid = launch::new_session_uuid();
                SessionStart::Fresh(&uuid)
            }
        };
        self.launch_with(agent, session, prompt.as_deref())
    }

    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        hooks::hook_events(hook)
    }

    fn screen_hint(&self, screen: &Screen) -> ScreenHint {
        screen::screen_hint(screen)
    }

    fn find_session_id(&self, agent: &Agent) -> Option<HarnessSessionId> {
        session::newest_session(
            &self.options.session_home(agent),
            &self.options.harness_cwd(agent),
        )
    }

    fn type_turn(&self, text: &str) -> Vec<u8> {
        keys::type_turn(text)
    }

    fn answer(&self, _request_id: &str, answer: &Answer) -> Vec<u8> {
        keys::answer(answer)
    }

    /// `/exit` then CR (`docs/compat.md`). Without it the supervisor's
    /// stop kills the harness, and no `SessionEnd` fires.
    fn exit_keys(&self) -> Vec<u8> {
        keys::exit()
    }

    fn clear_input_keys(&self) -> Vec<u8> {
        keys::CLEAR_INPUT.to_vec()
    }

    /// Keeps an existing `prompt.md`. Use [`ClaudeAdapter::materialize`]
    /// to write a new one.
    fn materialize_config(&self, agent: &Agent, home: &Path) -> Result<Vec<FileOverlay>> {
        self.materialize(agent, home, None)
    }
}

#[cfg(test)]
pub(crate) fn test_agent_in(root: &Path, model: Option<&str>) -> Agent {
    let def: terrarium_core::config::AgentDef = toml_agent(model);
    Agent::from_def(&def, root)
}

#[cfg(test)]
pub(crate) fn test_agent(model: Option<&str>) -> Agent {
    test_agent_in(Path::new("/state"), model)
}

#[cfg(test)]
fn toml_agent(model: Option<&str>) -> terrarium_core::config::AgentDef {
    let model = model
        .map(|m| format!("model = \"{m}\"\n"))
        .unwrap_or_default();
    toml::from_str(&format!(
        "name = \"a\"\nproject = \"p\"\nharness = \"claude\"\n{model}"
    ))
    .expect("agent def")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tested_range_covers_the_verified_version() {
        let a = ClaudeAdapter::default();
        let req = a.tested_version();
        assert!(req.matches(&Version::parse(CLAUDE_2_1.verified).expect("v")));
        // The pin and the range come from harnesses.toml.
        assert_eq!(
            CLAUDE_2_1.verified,
            terrarium_core::harnesses::claude::VERSION
        );
        assert_eq!(CLAUDE_2_1.tested, terrarium_core::harnesses::claude::TESTED);
        assert_eq!(
            req,
            VersionReq::parse(terrarium_core::harnesses::claude::TESTED).expect("req")
        );
        assert_eq!(a.id(), HarnessId::Claude);
    }

    #[test]
    fn trait_launch_reads_prompt_and_makes_fresh_ids() {
        let tmp = tempfile::tempdir().expect("tmp");
        let agent = test_agent_in(tmp.path(), Some("haiku"));
        let a = ClaudeAdapter::new(ClaudeOptions {
            run_as_root: Some(false),
            ..ClaudeOptions::default()
        });
        a.materialize(&agent, tmp.path(), Some("Agent prompt."))
            .expect("materialize");
        let one = a.launch(&agent, None);
        let two = a.launch(&agent, None);
        let sid = |s: &LaunchSpec| {
            let i = s
                .args
                .iter()
                .position(|x| x == "--session-id")
                .expect("flag");
            s.args[i + 1].clone()
        };
        assert_ne!(sid(&one), sid(&two));
        assert!(one.args.iter().any(|x| x == "Agent prompt."));
    }

    #[test]
    fn find_session_id_uses_the_agent_dir_and_harness_cwd() {
        let tmp = tempfile::tempdir().expect("tmp");
        let shared = tmp.path().join("auth-claude");
        let a = ClaudeAdapter::new(ClaudeOptions {
            host_config_dir: Some(shared.clone()),
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            ..ClaudeOptions::default()
        });
        let agent = crate::test_agent_in(tmp.path(), None);
        assert_eq!(a.find_session_id(&agent), None);
        // A session of another agent in the shared home is not this one's.
        let other =
            shared.join("projects/-home-agent-work/0b5c1d2e-0000-4000-8000-000000000001.jsonl");
        std::fs::create_dir_all(other.parent().expect("parent")).expect("dir");
        std::fs::write(&other, "{}\n").expect("write");
        assert_eq!(a.find_session_id(&agent), None);

        let id = HarnessSessionId("aee36d42-3246-4e16-be9c-16dd97c85c57".into());
        let file = a.session_file(&agent, &id);
        assert_eq!(
            file,
            config::claude_dir(&agent)
                .join("projects/-home-agent-work/aee36d42-3246-4e16-be9c-16dd97c85c57.jsonl")
        );
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        std::fs::write(&file, "{}\n").expect("write");
        assert_eq!(a.find_session_id(&agent), Some(id));

        // With no sandbox the harness writes into the shared home.
        let none = ClaudeAdapter::new(ClaudeOptions {
            host_config_dir: Some(shared.clone()),
            ..ClaudeOptions::default()
        });
        assert!(
            none.session_file(&agent, &HarnessSessionId("x".into()))
                .starts_with(&shared)
        );
    }

    #[test]
    fn answer_goes_through_the_trait() {
        let a: &dyn Adapter = &ClaudeAdapter::default();
        assert_eq!(a.answer("r", &Answer::Choice(1)), b"\x1b[B\r".to_vec());
        assert_eq!(a.type_turn("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
        // Regression: the stop path needs the exit keys, else it kills.
        assert_eq!(a.exit_keys(), b"/exit\r".to_vec());
    }

    /// Spawns the real binary. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the claude binary on PATH"]
    fn smoke_claude_help() {
        let out = std::process::Command::new("claude")
            .arg("--help")
            .output()
            .expect("run claude --help");
        assert!(out.status.success());
        let help = String::from_utf8_lossy(&out.stdout);
        let f = CLAUDE_2_1;
        for flag in [
            f.yolo,
            f.settings,
            f.setting_sources,
            f.session_id,
            f.resume,
            f.model,
            f.append_system_prompt,
            f.system_prompt,
            f.system_prompt_snapshot_off[0],
        ] {
            assert!(help.contains(flag), "{flag} is not in claude --help");
        }
        let v = ClaudeAdapter::default().detect_version().expect("version");
        assert!(ClaudeAdapter::default().tested_version().matches(&v), "{v}");
    }
}
