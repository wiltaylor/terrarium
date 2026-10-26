//! Adapter for pi (PLAN §4, §7, §8, §16). Every fact here comes from the
//! Phase 0 spikes in `docs/compat.md` ("pi 0.85.1"), from the adapter's
//! own live check on 0.85.1, or from the 1.4.0 check ("pi 1.4.0"); a doc
//! comment says so where a fact is only read from `pi --help` or the
//! installed source, or not tested.
//!
//! - [`launch`]: the command line and env (`PiAdapter::launch_with`).
//! - [`provider`]: subscription to `--provider`, and the API-key env var.
//! - [`config`]: the per-agent files, the shipped extension, the overlays.
//! - [`hooks`]: the extension's events to Terrarium events.
//! - [`screen`]: the prompt-ready and busy detector.
//! - [`keys`]: turns, answers, clear, and exit as keystrokes.
//! - [`session`]: the session files.
//! - [`usage`]: token usage from the session file.
//! - [`auth`]: `pi auth check`.
//!
//! pi has no hooks of its own. A Terrarium extension (`ext/terrarium.ts`,
//! loaded with `-e`) sends each pi event to the runner socket in the hook
//! message shape of `docs/runner.md`.

pub mod auth;
pub mod config;
pub mod error;
pub mod flags;
pub mod hooks;
pub mod keys;
pub mod launch;
pub mod provider;
pub mod screen;
pub mod session;
pub mod transcript;
pub mod usage;

use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};
use terrarium_core::adapter::{Screen, run_version};
use terrarium_core::config::SubscriptionDef;
use terrarium_core::{
    Adapter, Agent, Answer, Event, FileOverlay, HarnessId, HarnessSessionId, HookPayload,
    LaunchSpec, Result, ScreenHint,
};

pub use error::PiError;
pub use flags::{PI_PINNED, PiFlags};
pub use provider::{provider_for, refusal, required_env, start_refusal};

/// The pi versions this adapter accepts. Only [`PiFlags::verified`]
/// (the pin in `harnesses.toml`) was measured.
pub const TESTED: &str = PI_PINNED.tested;

/// How the rendered prompt reaches pi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptMode {
    /// `--append-system-prompt <file>`: the text goes after pi's default
    /// prompt, which holds the tool guidance.
    #[default]
    Append,
    /// `--system-prompt <file>`: the text replaces the whole default
    /// prompt. pi adds only "Current working directory: …". Its tool
    /// guidance goes too (`docs/compat.md`, "Surprises").
    Replace,
}

/// Settings of the adapter. The defaults suit the container layout of
/// PLAN §11.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiOptions {
    /// The flag table for the installed version.
    pub flags: PiFlags,
    /// The program to run.
    pub program: String,
    /// `PI_CODING_AGENT_DIR` as pi sees it: the shared pi home. In the
    /// sandbox it is `$HOME/.pi/agent`; on the host it is `/auth/pi`.
    pub agent_dir: PathBuf,
    /// The same directory as the daemon sees it, for `pi auth check`.
    /// `None`: the same as `agent_dir`.
    pub host_agent_dir: Option<PathBuf>,
    /// The cwd as pi sees it (for example `$HOME/work` in the sandbox).
    /// `None`: the agent workspace.
    pub harness_workspace: Option<PathBuf>,
    /// The mode for an agent with no base system prompt. See
    /// [`Self::prompt_mode_for`].
    pub prompt_mode: PromptMode,
    /// Let pi load `AGENTS.md` and `CLAUDE.md` (no `--no-context-files`).
    /// Off by default: see [`launch::build`].
    pub context_files: bool,
}

impl Default for PiOptions {
    fn default() -> Self {
        Self {
            flags: PI_PINNED,
            program: HarnessId::Pi.program().to_owned(),
            agent_dir: PathBuf::from(HarnessId::Pi.auth_dir()),
            host_agent_dir: None,
            harness_workspace: None,
            prompt_mode: PromptMode::Append,
            context_files: false,
        }
    }
}

impl PiOptions {
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

    /// The cwd of the pi process.
    pub fn harness_cwd(&self, agent: &Agent) -> PathBuf {
        self.harness_workspace
            .clone()
            .unwrap_or_else(|| agent.workspace.clone())
    }

    /// The pi home as the daemon sees it.
    pub fn host_agent_dir(&self) -> &Path {
        self.host_agent_dir.as_deref().unwrap_or(&self.agent_dir)
    }
}

/// The pi adapter.
#[derive(Debug, Clone, Default)]
pub struct PiAdapter {
    pub options: PiOptions,
}

impl PiAdapter {
    pub fn new(options: PiOptions) -> Self {
        Self { options }
    }

    /// The launch spec with an explicit session ID. A new UUID starts a
    /// session; a used one resumes it. Use this when the caller must know
    /// the session ID up front.
    pub fn launch_with(
        &self,
        agent: &Agent,
        session_id: &str,
        subscription: Option<&SubscriptionDef>,
    ) -> LaunchSpec {
        let prompt = config::prompt_file(agent);
        launch::build(
            &self.options,
            agent,
            session_id,
            subscription,
            prompt.as_deref(),
        )
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

    /// `pi auth check` for `provider` against the shared home. `extra_env`
    /// carries an API key (see [`required_env`]). This blocks.
    pub fn auth_check(
        &self,
        provider: &str,
        extra_env: &[(String, String)],
    ) -> Result<auth::AuthCheck> {
        Ok(auth::auth_check(
            &self.options.program,
            self.options.host_agent_dir(),
            provider,
            extra_env,
        )?)
    }

    /// The session file of `id` for this agent, if pi has written it. The
    /// path is the same for the daemon and in the sandbox.
    pub fn session_file(&self, agent: &Agent, id: &HarnessSessionId) -> Option<PathBuf> {
        session::find_session_file(&config::sessions_dir(agent), id)
    }
}

impl Adapter for PiAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn tested_version(&self) -> VersionReq {
        VersionReq::parse(self.options.flags.tested).unwrap_or(VersionReq::STAR)
    }

    fn detect_version(&self) -> Result<Version> {
        run_version(&self.options.program)
    }

    /// No subscription: pi gets only `--model` (see [`launch::model_args`]).
    /// The daemon calls [`Adapter::launch_for`] with the resolved
    /// subscription.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        self.launch_for(agent, resume, None)
    }

    /// A fresh start gets a new UUID, a resume the old ID; both go through
    /// `--session-id`. The `session_start` event reports it.
    fn launch_for(
        &self,
        agent: &Agent,
        resume: Option<&HarnessSessionId>,
        subscription: Option<&SubscriptionDef>,
    ) -> LaunchSpec {
        let id = match resume {
            Some(id) => id.0.clone(),
            None => launch::new_session_uuid(),
        };
        self.launch_with(agent, &id, subscription)
    }

    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        hooks::hook_events(hook)
    }

    fn screen_hint(&self, screen: &Screen) -> ScreenHint {
        screen::screen_hint(screen)
    }

    fn find_session_id(&self, agent: &Agent) -> Option<HarnessSessionId> {
        session::newest_session(&config::sessions_dir(agent))
    }

    fn type_turn(&self, text: &str) -> Vec<u8> {
        keys::type_turn(text)
    }

    fn answer(&self, _request_id: &str, answer: &Answer) -> Vec<u8> {
        keys::answer(answer)
    }

    fn clear_input_keys(&self) -> Vec<u8> {
        keys::CLEAR_INPUT.to_vec()
    }

    fn exit_keys(&self) -> Vec<u8> {
        keys::EXIT.to_vec()
    }

    /// Keeps an existing `prompt.md`. Use [`PiAdapter::materialize`] to
    /// write a new one.
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
        "name = \"a\"\nproject = \"p\"\nharness = \"pi\"\nsubscription = \"chatgpt-pro\"\n{model}"
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

    fn value_after<'a>(spec: &'a LaunchSpec, flag: &str) -> Option<&'a str> {
        let i = spec.args.iter().position(|a| a == flag)?;
        spec.args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn tested_range_covers_the_verified_version() {
        let a = PiAdapter::default();
        let req = a.tested_version();
        assert!(req.matches(&Version::parse(PI_PINNED.verified).expect("v")));
        // The pin and the range come from harnesses.toml.
        assert_eq!(PI_PINNED.verified, terrarium_core::harnesses::pi::VERSION);
        assert_eq!(PI_PINNED.tested, terrarium_core::harnesses::pi::TESTED);
        assert_eq!(
            req,
            VersionReq::parse(terrarium_core::harnesses::pi::TESTED).expect("req")
        );
        assert_eq!(a.id(), HarnessId::Pi);
    }

    #[test]
    fn trait_launch_makes_fresh_ids_and_resumes_with_the_same_flag() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        std::fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), Some("gpt-5.6-luna"));
        let a = PiAdapter::default();
        a.materialize(&agent, &home, Some("Agent prompt."))
            .expect("materialize");
        let one = a.launch(&agent, None);
        let two = a.launch(&agent, None);
        assert_ne!(
            value_after(&one, "--session-id"),
            value_after(&two, "--session-id")
        );
        let prompt = config::prompt_path(&agent);
        assert_eq!(
            value_after(&one, "--append-system-prompt"),
            Some(prompt.to_string_lossy().as_ref())
        );
        let id = HarnessSessionId("0aa4e42a-0404-423c-a41c-332c83e48e25".into());
        let again = a.launch(&agent, Some(&id));
        assert_eq!(value_after(&again, "--session-id"), Some(id.0.as_str()));
        // The trait launch has no subscription, so no provider flag.
        assert!(!one.args.iter().any(|x| x == "--provider"));
        let sub = SubscriptionDef {
            name: "chatgpt-pro".into(),
            provider: "openai".into(),
            plan: Some("Pro".into()),
            harnesses: vec![HarnessId::Pi],
            probe: None,
            alert_at: 80,
        };
        let with = a.launch_for(&agent, Some(&id), Some(&sub));
        assert_eq!(value_after(&with, "--provider"), Some("openai-codex"));
        assert_eq!(value_after(&with, "--model"), Some("gpt-5.6-luna"));
        // The extension that `-e` names is on disk.
        assert!(Path::new(value_after(&with, "-e").expect("ext")).is_file());
    }

    #[test]
    fn find_session_id_uses_the_agent_session_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let agent = test_agent_in(tmp.path(), None);
        let a = PiAdapter::default();
        assert_eq!(a.find_session_id(&agent), None);
        let dir = config::sessions_dir(&agent);
        std::fs::create_dir_all(&dir).expect("dir");
        let id = "a6acb784-5d88-4161-8792-90143365c46a";
        let file = dir.join(format!("2026-10-24T07-49-08-371Z_{id}.jsonl"));
        std::fs::write(&file, "{}\n").expect("write");
        let hid = HarnessSessionId(id.into());
        assert_eq!(a.find_session_id(&agent), Some(hid.clone()));
        assert_eq!(a.session_file(&agent, &hid), Some(file));
    }

    #[test]
    fn keys_go_through_the_trait() {
        let a: &dyn Adapter = &PiAdapter::default();
        assert_eq!(
            a.answer("r", &Answer::Choice(2)),
            b"\x1b[B\x1b[B\r".to_vec()
        );
        assert_eq!(a.type_turn("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
        assert_eq!(a.clear_input_keys(), b"\x03".to_vec());
        assert_eq!(a.exit_keys(), b"/quit\r".to_vec());
    }

    /// The extension must parse as TypeScript. Uses `node
    /// --experimental-strip-types --check` when a `node` binary is on
    /// `PATH`; skips otherwise.
    #[test]
    fn extension_is_valid_typescript() {
        let ok = std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("skipped: no node binary");
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("terrarium.mts");
        std::fs::write(&path, config::EXTENSION).expect("write");
        let out = std::process::Command::new("node")
            .args(["--experimental-strip-types", "--no-warnings", "--check"])
            .arg(&path)
            .output()
            .expect("node");
        assert!(
            out.status.success(),
            "node --check failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Every event that `hooks` maps is subscribed.
        for name in [
            "session_start",
            "session_shutdown",
            "session_compact",
            "input",
            "agent_settled",
            "tool_execution_start",
            "tool_execution_end",
            "ui_prompt_start",
            "ui_prompt_end",
            "message_end",
        ] {
            assert!(
                config::EXTENSION.contains(&format!("pi.on(\"{name}\"")),
                "{name}"
            );
        }
        assert!(config::EXTENSION.contains(r#"JSON.stringify({ type: "hook", hook, payload })"#));
    }

    /// Spawns the real binary. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the pi binary on PATH"]
    fn smoke_pi_version_and_help() {
        let a = PiAdapter::default();
        let v = a.detect_version().expect("pi --version");
        assert!(a.tested_version().matches(&v), "{v}");
        let out = std::process::Command::new("pi")
            .arg("--help")
            .output()
            .expect("pi --help");
        let help = String::from_utf8_lossy(&out.stdout);
        let f = PI_PINNED;
        for flag in [
            f.provider,
            f.model,
            f.no_extensions,
            f.no_mcp,
            f.tui_mode.0,
            f.no_skills,
            f.no_prompt_templates,
            f.no_context_files,
            f.no_approve,
            f.offline,
            f.session_id,
            f.session_dir,
            f.system_prompt,
            f.append_system_prompt,
            f.config_env,
            "PI_TELEMETRY",
            "PI_OFFLINE",
        ] {
            assert!(help.contains(flag), "{flag} is not in pi --help");
        }
    }
}
