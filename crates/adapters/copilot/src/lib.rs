//! Adapter for the GitHub Copilot CLI (PLAN §4, §7, §8). Every fact here
//! comes from the Phase 0 spikes in `docs/compat.md` ("Copilot CLI
//! 1.0.103"); a doc comment says so where a fact is only read from the
//! binary or `--help`, or is not tested.
//!
//! - [`launch`]: the command line and env, the plugin `hooks.json`.
//! - [`config`]: the per-agent files and the overlays.
//! - [`hooks`]: hook payloads to events.
//! - [`screen`]: the prompt-ready detector.
//! - [`keys`]: turns, answers, and clearing the input box as keystrokes.
//! - [`session`]: the session file and the recorded session UUID.
//! - [`usage`]: usage from the session file, and the `/usage` screen.
//! - [`auth`]: where the token is, and the login state on the screen.

pub mod auth;
pub mod config;
pub mod error;
pub mod flags;
pub mod hooks;
pub mod keys;
pub mod launch;
pub mod screen;
pub mod session;
pub mod transcript;
pub mod usage;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use semver::{Version, VersionReq};
use terrarium_core::adapter::Screen;
use terrarium_core::{
    Adapter, Agent, Answer, Event, FileOverlay, HarnessId, HarnessSessionId, HookPayload,
    LaunchSpec, Result, ScreenHint,
};

pub use error::CopilotError;
pub use flags::{COPILOT_1_0, CopilotFlags};
pub use launch::SessionStart;

/// The Copilot CLI versions this adapter accepts. Only
/// [`CopilotFlags::verified`] (the pin in `harnesses.toml`) was measured.
pub const TESTED: &str = COPILOT_1_0.tested;

/// The env var that carries the GitHub token into an agent. The daemon
/// sets it from its credential store (see [`CopilotAdapter::required_env`]).
pub const TOKEN_ENV: &str = COPILOT_1_0.token_env[0];

/// How the rendered prompt reaches Copilot. Both go through the
/// CopilotCustomSystemPrompt extension, which needs experimental mode
/// (`"experimental": true` in the per-agent `settings.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptMode {
    /// `APPEND_SYSTEM.md`: the text goes after Copilot's default prompt
    /// (31 475 characters, with the tool rules). Verified.
    #[default]
    Append,
    /// `SYSTEM.md`: the text replaces the whole default prompt. Verified
    /// that the model sees only the text; tool use with no default prompt
    /// was not tested.
    Replace,
}

/// Settings of the adapter. The defaults suit the container with no
/// sandbox (`/auth/copilot`); the daemon sets the sandbox paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotOptions {
    /// The flag table for the installed version.
    pub flags: CopilotFlags,
    /// The program to run.
    pub program: String,
    /// `COPILOT_HOME` as the harness sees it.
    pub config_dir: PathBuf,
    /// The same directory as the daemon sees it, for session files and
    /// the auth check. `None`: the same as `config_dir`.
    pub host_config_dir: Option<PathBuf>,
    /// The cwd as the harness sees it (for example `$HOME/work` in the
    /// sandbox). `None`: the agent workspace.
    pub harness_workspace: Option<PathBuf>,
    /// The cache directory as the harness sees it, for example
    /// `/home/agent/.cache/copilot` in the sandbox. When set,
    /// `materialize_config` returns an overlay that binds the per-agent
    /// `<agent dir>/copilot/cache` there, and `COPILOT_CACHE_HOME` points
    /// at it. `None`: `COPILOT_CACHE_HOME` points at the per-agent
    /// directory itself (the sandbox binds the agent directory at the same
    /// path), and there is no overlay.
    pub cache_dir: Option<PathBuf>,
    /// A shared directory for the unpacked runtime
    /// (`COPILOT_PKG_CACHE_HOME`). `None`: each agent unpacks its own copy
    /// (about 236 MB) into its cache at the first start; a probe measured
    /// 3.1 s to the prompt with a cold cache, 0.4 s warm. Read from the
    /// binary; a shared directory is not tested.
    pub pkg_cache_dir: Option<PathBuf>,
    /// The mode for an agent with no base system prompt. See
    /// [`Self::prompt_mode_for`].
    pub prompt_mode: PromptMode,
    /// The CopilotCustomSystemPrompt package directory, loaded with
    /// `--plugin-dir` (verified). `None`: the extension must be installed
    /// in the shared home (`$COPILOT_HOME/extensions/`, verified; the image
    /// entrypoint installs it).
    pub prompt_extension_dir: Option<PathBuf>,
    /// The CLI that the hooks run.
    pub hook_program: String,
}

impl Default for CopilotOptions {
    fn default() -> Self {
        Self {
            flags: COPILOT_1_0,
            program: HarnessId::Copilot.program().to_owned(),
            config_dir: PathBuf::from(HarnessId::Copilot.auth_dir()),
            host_config_dir: None,
            harness_workspace: None,
            cache_dir: None,
            pkg_cache_dir: None,
            prompt_mode: PromptMode::Append,
            prompt_extension_dir: None,
            hook_program: "terrarium".to_owned(),
        }
    }
}

impl CopilotOptions {
    /// The prompt mode for `agent`. An agent with its own base system
    /// prompt (`Agent::system_prompt`, PLAN §8) always gets
    /// [`PromptMode::Replace`]: the rendered `prompt.md` then starts with
    /// it. Else [`Self::prompt_mode`] (default [`PromptMode::Append`]), so
    /// the harness keeps its default prompt.
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

    /// The config dir as the daemon sees it.
    pub fn host_config_dir(&self) -> &Path {
        self.host_config_dir.as_deref().unwrap_or(&self.config_dir)
    }

    /// The directory that holds the agent's `session-state/` (the daemon's
    /// view). In the sandbox (a `harness_workspace` is set) it is the
    /// agent's own `harness/copilot/`, which the sandbox binds over the
    /// shared one ([`config`]). With no sandbox the overlays are not bound,
    /// so it is the shared home.
    pub fn session_home(&self, agent: &Agent) -> PathBuf {
        if self.harness_workspace.is_some() {
            config::copilot_dir(agent)
        } else {
            self.host_config_dir().to_path_buf()
        }
    }

    /// `COPILOT_CACHE_HOME` as the harness sees it.
    pub fn harness_cache_dir(&self, agent: &Agent) -> PathBuf {
        self.cache_dir
            .clone()
            .unwrap_or_else(|| config::cache_dir(agent))
    }
}

/// The Copilot CLI adapter.
#[derive(Debug, Clone, Default)]
pub struct CopilotAdapter {
    pub options: CopilotOptions,
    mapper: Arc<hooks::HookMapper>,
}

impl CopilotAdapter {
    pub fn new(options: CopilotOptions) -> Self {
        Self {
            options,
            mapper: Arc::default(),
        }
    }

    /// The env vars that the daemon should add to an agent's `LaunchSpec`
    /// from its credential store: `COPILOT_GITHUB_TOKEN`, the GitHub token
    /// of the agent's subscription.
    ///
    /// Why: on a desktop, Copilot keeps the token in the system keychain
    /// (D-Bus), which a sandbox cannot reach; with no token Copilot starts
    /// logged out. In the container there is no keychain, and `copilot
    /// login` stores the token in plain text in the shared
    /// `config.json`; then the var is not needed, and Copilot reads the
    /// file. See [`auth`].
    ///
    /// Call site: `crates/terrariumd/src/login_session.rs`,
    /// `credential_env_for(state_dir, sub)`, for a Copilot subscription
    /// whose `env_var` is `COPILOT_GITHUB_TOKEN`; the supervisor adds its
    /// pairs to `LaunchSpec::env` after `launch` and before the sandbox
    /// wraps the spec. The value is a secret: never log it. The launch
    /// passes `--secret-env-vars` for it, so Copilot strips it from the
    /// agent's shell commands.
    pub fn required_env(&self) -> &'static [&'static str] {
        &self.options.flags.token_env[..1]
    }

    /// The launch spec with an explicit session start. Pure: no UUID is
    /// recorded.
    pub fn launch_with(&self, agent: &Agent, session: SessionStart<'_>) -> LaunchSpec {
        launch::build(&self.options, agent, session)
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

    /// Where the token comes from, for the shared home and the given env.
    pub fn auth_status_with(
        &self,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<auth::AuthStatus> {
        Ok(auth::auth_status(
            self.options.host_config_dir(),
            self.options.flags.token_env,
            env,
        )?)
    }

    /// [`Self::auth_status_with`] with the env of this process.
    pub fn auth_status(&self) -> Result<auth::AuthStatus> {
        self.auth_status_with(&|k| std::env::var(k).ok())
    }

    /// The session file of `id` (the daemon's view).
    pub fn session_file(&self, agent: &Agent, id: &HarnessSessionId) -> PathBuf {
        session::events_file(&self.options.session_home(agent), id)
    }
}

impl Adapter for CopilotAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Copilot
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(self.options.flags.tested).unwrap_or(VersionReq::STAR)
    }

    /// `copilot --version` prints "GitHub Copilot CLI 1.0.103." (with a
    /// full stop), which the core parser does not read.
    fn detect_version(&self) -> Result<Version> {
        let out = std::process::Command::new(&self.options.program)
            .arg("--version")
            .output()
            .map_err(|e| {
                terrarium_core::Error::Version(format!("cannot run {}: {e}", self.options.program))
            })?;
        parse_version(&String::from_utf8_lossy(&out.stdout))
    }

    /// A fresh start gets a new UUID (`--session-id`), recorded in
    /// `<agent dir>/copilot/session-id` for [`Adapter::find_session_id`].
    /// A failed write only loses that fallback; the `SessionStart` hook
    /// still reports the ID.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        match resume {
            Some(id) => self.launch_with(agent, SessionStart::Resume(id)),
            None => {
                let uuid = launch::new_session_uuid();
                let _ = session::record_session_id(agent, &uuid);
                self.launch_with(agent, SessionStart::Fresh(&uuid))
            }
        }
    }

    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        self.mapper.map(hook, &self.options.config_dir)
    }

    /// A `UserPromptSubmit` (or `userPromptSubmitted`) whose prompt starts
    /// with `<system_notification>`: Copilot made the turn itself, for
    /// example after a background shell command ended.
    fn is_system_prompt(&self, hook: &HookPayload) -> bool {
        let name = hook
            .str_field("hook_event_name")
            .or_else(|| hook.str_field("hookName"))
            .unwrap_or(&hook.hook);
        matches!(name, "UserPromptSubmit" | "userPromptSubmitted")
            && hook
                .str_field("prompt")
                .is_some_and(hooks::is_system_prompt)
    }

    fn screen_hint(&self, screen: &Screen) -> ScreenHint {
        screen::screen_hint(screen)
    }

    fn find_session_id(&self, agent: &Agent) -> Option<HarnessSessionId> {
        session::find_session_id(&self.options.session_home(agent), agent)
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

    /// Uses the `prompt.md` that the daemon wrote.
    fn materialize_config(&self, agent: &Agent, home: &Path) -> Result<Vec<FileOverlay>> {
        self.materialize(agent, home, None)
    }
}

/// Parse the first `x.y.z` in the output of `copilot --version`, with
/// trailing punctuation removed.
pub fn parse_version(output: &str) -> Result<Version> {
    output
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .map(|w| w.trim_start_matches('v').trim_end_matches(['.', ',', ';']))
        .find_map(|w| Version::parse(w).ok())
        .ok_or_else(|| terrarium_core::Error::Version(format!("no version in `{}`", output.trim())))
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
        "name = \"a\"\nproject = \"p\"\nharness = \"copilot\"\n{model}"
    ))
    .expect("agent def")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tested_range_covers_the_verified_version() {
        let a = CopilotAdapter::default();
        let req = a.tested_version();
        assert!(req.matches(&Version::parse(COPILOT_1_0.verified).expect("v")));
        // The pin and the range come from harnesses.toml.
        assert_eq!(
            COPILOT_1_0.verified,
            terrarium_core::harnesses::copilot::VERSION
        );
        assert_eq!(
            COPILOT_1_0.tested,
            terrarium_core::harnesses::copilot::TESTED
        );
        assert_eq!(
            req,
            VersionReq::parse(terrarium_core::harnesses::copilot::TESTED).expect("req")
        );
        assert_eq!(a.id(), HarnessId::Copilot);
    }

    #[test]
    fn version_output_parses() {
        assert_eq!(
            parse_version(
                "GitHub Copilot CLI 1.0.103.\nRun 'copilot update' to check for updates.\n"
            )
            .expect("v"),
            Version::new(1, 0, 103)
        );
        assert!(parse_version("nothing").is_err());
    }

    #[test]
    fn trait_launch_records_fresh_ids() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-copilot");
        let agent = test_agent_in(tmp.path(), None);
        let a = CopilotAdapter::new(CopilotOptions {
            host_config_dir: Some(home.clone()),
            ..CopilotOptions::default()
        });
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
        // The last one is recorded; it counts once Copilot made its dir.
        assert_eq!(a.find_session_id(&agent), None);
        let id = HarnessSessionId(sid(&two));
        let file = a.session_file(&agent, &id);
        assert!(file.starts_with(&home));
        std::fs::create_dir_all(file.parent().expect("dir")).expect("mk");
        assert_eq!(a.find_session_id(&agent), Some(id.clone()));
        // In the sandbox the session is in the agent's own directory, and
        // the one in the shared home does not count.
        let boxed = CopilotAdapter::new(CopilotOptions {
            host_config_dir: Some(home.clone()),
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            ..CopilotOptions::default()
        });
        assert_eq!(boxed.find_session_id(&agent), None);
        let own = boxed.session_file(&agent, &id);
        assert_eq!(
            own,
            config::copilot_dir(&agent)
                .join("session-state")
                .join(&id.0)
                .join("events.jsonl")
        );
        std::fs::create_dir_all(own.parent().expect("dir")).expect("mk");
        assert_eq!(boxed.find_session_id(&agent), Some(id.clone()));
        let r = a.launch(&agent, Some(&id));
        assert!(
            r.args
                .windows(2)
                .any(|w| w[0] == "--resume" && w[1] == id.0)
        );
    }

    #[test]
    fn trait_methods() {
        let a: &dyn Adapter = &CopilotAdapter::default();
        assert_eq!(a.answer("r", &Answer::Choice(1)), b"\x1b[B\r".to_vec());
        assert_eq!(a.type_turn("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
        assert!(a.clear_input_keys().starts_with(&[0x15, 0x7f]));
        assert_eq!(a.exit_keys(), b"/exit\r".to_vec());
        let ev = a.hook_event(&HookPayload {
            harness: HarnessId::Copilot,
            hook: "Stop".into(),
            body: json!({"hook_event_name": "Stop", "session_id": "s"}),
        });
        assert_eq!(ev, [Event::TurnEnd]);
        assert_eq!(
            CopilotAdapter::default().required_env(),
            ["COPILOT_GITHUB_TOKEN"]
        );
        assert_eq!(TOKEN_ENV, "COPILOT_GITHUB_TOKEN");
    }

    #[test]
    fn system_notification_prompts_are_system_prompts() {
        let a: &dyn Adapter = &CopilotAdapter::default();
        let hook = |name: &str, body: serde_json::Value| HookPayload {
            harness: HarnessId::Copilot,
            hook: name.into(),
            body,
        };
        let system = "<system_notification>Shell command completed</system_notification>";
        assert!(a.is_system_prompt(&hook(
            "UserPromptSubmit",
            json!({"hook_event_name": "UserPromptSubmit", "prompt": system}),
        )));
        // The camelCase dialect.
        assert!(a.is_system_prompt(&hook(
            "userPromptSubmitted",
            json!({"prompt": format!("  {system}")}),
        )));
        assert!(!a.is_system_prompt(&hook(
            "UserPromptSubmit",
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "fix the bug"}),
        )));
        // Only a prompt-submit hook can be a system prompt.
        assert!(!a.is_system_prompt(&hook(
            "Notification",
            json!({"hook_event_name": "Notification", "prompt": system}),
        )));
    }

    #[test]
    fn auth_status_reads_the_host_home() {
        let tmp = tempfile::tempdir().expect("tmp");
        let a = CopilotAdapter::new(CopilotOptions {
            host_config_dir: Some(tmp.path().to_path_buf()),
            ..CopilotOptions::default()
        });
        let st = a
            .auth_status_with(&|k| (k == TOKEN_ENV).then(|| "t".to_owned()))
            .expect("status");
        assert_eq!(st.source, auth::CredentialSource::Env(TOKEN_ENV.into()));
    }

    /// Spawns the real binary. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the copilot binary on PATH"]
    fn smoke_copilot_version() {
        let out = std::process::Command::new("copilot")
            .arg("--version")
            .output()
            .expect("run copilot --version");
        assert!(out.status.success());
        let v = CopilotAdapter::default().detect_version().expect("version");
        assert!(
            CopilotAdapter::default().tested_version().matches(&v),
            "{v}"
        );
        let help = std::process::Command::new("copilot")
            .arg("--help")
            .output()
            .expect("run copilot --help");
        let help = String::from_utf8_lossy(&help.stdout);
        let f = COPILOT_1_0;
        for flag in [
            f.experimental,
            f.yolo,
            f.plugin_dir,
            f.session_id,
            f.resume,
            f.model,
            f.cwd,
            f.no_auto_update,
            f.disable_telemetry,
            f.log_dir,
            f.secret_env_vars,
        ] {
            assert!(help.contains(flag), "{flag} is not in copilot --help");
        }
    }
}
