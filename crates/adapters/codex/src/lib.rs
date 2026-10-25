//! Adapter for the Codex CLI (PLAN §4, §7, §8). Every fact here comes from
//! the Phase 0 spikes in `docs/compat.md` ("Codex 0.160.0"); a doc comment
//! says so where a fact is only read from the binary, or not tested.
//!
//! - [`flags`]: the version pin and the flag table.
//! - [`launch`]: the command line and env (`CodexAdapter::launch_with`).
//! - [`config`]: the per-agent files and the overlays.
//! - [`hooks`]: hook payloads to events.
//! - [`screen`]: the prompt-ready detector.
//! - [`keys`]: turns and answers as keystrokes.
//! - [`session`]: the session files and the session ID.
//! - [`usage`]: token usage and rate limits from the session file.
//! - [`auth`]: `codex login status` and the plan.
//!
//! Two facts shape this adapter more than the others:
//!
//! - Codex has no flag that sets the session ID, and `SessionStart` fires
//!   only at the first prompt. A fresh agent has no session ID until its
//!   first turn. The runner gets it from the `SessionStart` hook.
//! - Codex freezes the system prompt (both `model_instructions_file` and
//!   `developer_instructions`) when it makes the session. `codex resume`
//!   with a changed template still sends the old prompt. A changed
//!   template needs a fresh session; see
//!   [`Adapter::prompt_frozen_on_resume`].

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

pub use error::CodexError;
pub use flags::{CODEX_0_160, CodexFlags};

/// The Codex versions this adapter accepts. Only [`CodexFlags::verified`]
/// (0.160.0) was measured. This is the one place for the version pin.
pub const TESTED: &str = CODEX_0_160.tested;

/// How the rendered prompt reaches Codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptMode {
    /// `-c model_instructions_file=<agent dir>/prompt.md`: the file replaces
    /// the whole base prompt of Codex. The tools stay. The template must
    /// then also say how to work in the workspace.
    #[default]
    Replace,
    /// `-c developer_instructions=<text>`: the text goes in a `developer`
    /// message. The default Codex prompt stays.
    Append,
}

/// Settings of the adapter. The defaults suit the container layout of
/// PLAN §11.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexOptions {
    /// The flag table for the installed version.
    pub flags: CodexFlags,
    /// The program to run.
    pub program: String,
    /// `CODEX_HOME` as the harness sees it. Always set it.
    pub codex_home: PathBuf,
    /// The same directory as the daemon sees it, for reading session files
    /// and `auth.json`. `None`: the same as `codex_home`.
    pub host_codex_home: Option<PathBuf>,
    /// The cwd as the harness sees it (for example `$HOME/work` in the
    /// sandbox). `None`: the agent workspace. `-C` and the trust key use
    /// this path.
    pub harness_workspace: Option<PathBuf>,
    pub prompt_mode: PromptMode,
    /// The CLI that the hooks run.
    pub hook_program: String,
}

impl Default for CodexOptions {
    fn default() -> Self {
        Self {
            flags: CODEX_0_160,
            program: HarnessId::Codex.program().to_owned(),
            codex_home: PathBuf::from(HarnessId::Codex.auth_dir()),
            host_codex_home: None,
            harness_workspace: None,
            prompt_mode: PromptMode::Replace,
            hook_program: "terrarium".to_owned(),
        }
    }
}

impl CodexOptions {
    /// The cwd of the harness process.
    pub fn harness_cwd(&self, agent: &Agent) -> PathBuf {
        self.harness_workspace
            .clone()
            .unwrap_or_else(|| agent.workspace.clone())
    }

    /// `CODEX_HOME` as the daemon sees it.
    pub fn host_codex_home(&self) -> &Path {
        self.host_codex_home.as_deref().unwrap_or(&self.codex_home)
    }
}

/// The Codex adapter.
#[derive(Debug, Clone, Default)]
pub struct CodexAdapter {
    pub options: CodexOptions,
}

impl CodexAdapter {
    pub fn new(options: CodexOptions) -> Self {
        Self { options }
    }

    /// The launch spec with an explicit prompt text. `prompt` is the text
    /// of `<agent dir>/prompt.md`, or `None` for no prompt. In
    /// [`PromptMode::Replace`] the spec names the file, so the file must
    /// hold the same text (see [`CodexAdapter::materialize`]).
    pub fn launch_with(
        &self,
        agent: &Agent,
        resume: Option<&HarnessSessionId>,
        prompt: Option<&str>,
    ) -> LaunchSpec {
        launch::build(&self.options, agent, resume, prompt)
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

    /// `codex login status` against the shared home. This blocks.
    pub fn auth_status(&self) -> Result<auth::AuthStatus> {
        Ok(auth::auth_status(
            &self.options.program,
            self.options.host_codex_home(),
        )?)
    }

    /// The plan and login mode from `auth.json` in the shared home.
    pub fn plan(&self) -> Result<Option<auth::Plan>> {
        Ok(auth::plan(self.options.host_codex_home())?)
    }

    /// The session file of `id` (the daemon's view), if it exists.
    pub fn session_file(&self, id: &HarnessSessionId) -> Option<PathBuf> {
        session::find_session_file(self.options.host_codex_home(), id)
    }
}

impl Adapter for CodexAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(self.options.flags.tested).unwrap_or(VersionReq::STAR)
    }

    fn detect_version(&self) -> Result<Version> {
        run_version(&self.options.program)
    }

    /// A fresh start has no session ID; the first `SessionStart` hook
    /// reports it. The prompt comes from `<agent dir>/prompt.md`. A prompt
    /// file that cannot be read gives a start with no prompt (Codex stops
    /// the start when `model_instructions_file` names a missing file).
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let prompt = config::read_prompt(agent).ok().flatten();
        self.launch_with(agent, resume, prompt.as_deref())
    }

    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        hooks::hook_events(hook)
    }

    fn screen_hint(&self, screen: &Screen) -> ScreenHint {
        screen::screen_hint(screen)
    }

    fn find_session_id(&self, agent: &Agent) -> Option<HarnessSessionId> {
        session::find_session_id(&self.options, agent)
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
        keys::clear_input()
    }

    /// Keeps an existing `prompt.md`. Use [`CodexAdapter::materialize`] to
    /// write a new one.
    fn materialize_config(&self, agent: &Agent, home: &Path) -> Result<Vec<FileOverlay>> {
        self.materialize(agent, home, None)
    }

    /// Codex keeps the prompt of the session that it made first
    /// (`docs/compat.md`, "Prompt on resume"). A changed template needs a
    /// fresh session.
    fn prompt_frozen_on_resume(&self) -> bool {
        true
    }
}

#[cfg(test)]
pub(crate) fn test_agent_in(root: &Path, model: Option<&str>) -> Agent {
    let model = model
        .map(|m| format!("model = \"{m}\"\n"))
        .unwrap_or_default();
    let def: terrarium_core::config::AgentDef = toml::from_str(&format!(
        "name = \"a\"\nproject = \"p\"\nharness = \"codex\"\n{model}"
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

    #[test]
    fn tested_range_covers_the_verified_version() {
        let a = CodexAdapter::default();
        let req = a.tested_version();
        assert!(req.matches(&Version::parse(CODEX_0_160.verified).expect("v")));
        assert!(!req.matches(&Version::new(0, 161, 0)));
        assert!(!req.matches(&Version::new(0, 159, 9)));
        assert_eq!(a.id(), HarnessId::Codex);
        assert!(a.prompt_frozen_on_resume());
    }

    #[test]
    fn trait_launch_reads_the_prompt_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let agent = test_agent_in(tmp.path(), Some("gpt-6-luna"));
        let home = tmp.path().join("auth-codex");
        std::fs::create_dir_all(&home).expect("home");
        let a = CodexAdapter::default();
        let without = a.launch(&agent, None);
        assert!(
            !without
                .args
                .iter()
                .any(|x| x.starts_with("model_instructions_file="))
        );
        a.materialize(&agent, &home, Some("Agent prompt."))
            .expect("materialize");
        let with = a.launch(&agent, None);
        let want = format!(
            "model_instructions_file={}",
            launch::toml_string(&config::prompt_path(&agent).to_string_lossy())
        );
        assert!(with.args.contains(&want), "{:?}", with.args);
    }

    #[test]
    fn keys_go_through_the_trait() {
        let a: &dyn Adapter = &CodexAdapter::default();
        assert_eq!(a.answer("r", &Answer::Choice(1)), b"\x1b[B\r".to_vec());
        assert_eq!(a.type_turn("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
        assert!(a.clear_input_keys().starts_with(&[0x15]));
        assert_eq!(a.exit_keys(), b"/exit\r".to_vec());
    }

    /// Spawns the real binary. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the codex binary on PATH"]
    fn smoke_codex_version() {
        let a = CodexAdapter::default();
        let v = a.detect_version().expect("codex --version");
        assert!(a.tested_version().matches(&v), "{v}");
        let out = std::process::Command::new("codex")
            .arg("--help")
            .output()
            .expect("run codex --help");
        let help = String::from_utf8_lossy(&out.stdout);
        let f = CODEX_0_160;
        for flag in [
            f.no_daemon,
            f.yolo,
            f.hook_trust,
            "--cd",
            "--config",
            "--model",
        ] {
            assert!(help.contains(flag), "{flag} is not in codex --help");
        }
        // `--skip-git-repo-check` is an `exec` flag; the TUI rejects it.
        assert!(!help.contains("--skip-git-repo-check"));
    }
}
