//! One profile per harness: how the suite builds the adapter for sandbox
//! mode `none` on the real home, and what each scenario expects from it.
//! The expectations come from `docs/compat.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use terrarium_adapter_claude::{ClaudeAdapter, ClaudeOptions};
use terrarium_adapter_codex::{CodexAdapter, CodexOptions};
use terrarium_adapter_copilot::auth::CredentialSource;
use terrarium_adapter_copilot::{CopilotAdapter, CopilotOptions};
use terrarium_adapter_pi::{PiAdapter, PiOptions};
use terrarium_core::config::SubscriptionDef;
use terrarium_core::{Adapter, Agent, FileOverlay, HarnessId, HarnessSessionId, LaunchSpec};

use crate::Settings;

/// What one harness needs from the suite.
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    /// The adapter, set up for sandbox mode `none` on [`Harness::home`].
    fn adapter(&self) -> Arc<dyn Adapter>;
    /// The shared harness home (the operator's real home in a real run).
    fn home(&self) -> &Path;
    fn model(&self) -> &str;
    /// `Ok(evidence)` when logged in, `Err(reason)` when not.
    fn auth(&self) -> Result<String, String>;
    /// The adapter's `materialize` with an explicit prompt text.
    fn materialize(
        &self,
        agent: &Agent,
        home: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<FileOverlay>, String>;
    /// The launch spec, with the suite's cost settings on top.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec;
    /// Keys for a clean exit from the idle prompt.
    fn exit_keys(&self) -> Vec<u8>;
    /// Extra env for the child that the daemon's clean env does not carry
    /// in sandbox mode `none`. Each entry is a deviation; the report says so.
    fn extra_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// `Err(reason)` when the rendered prompt cannot reach the harness in
    /// this suite (sandbox mode `none`).
    fn prompt_reaches_harness(&self) -> Result<(), String> {
        Ok(())
    }
    /// The hook names of "prompt submitted" and "turn ended".
    fn turn_hooks(&self) -> [&'static str; 2];
    /// The fields that `docs/compat.md` says every turn hook payload has.
    fn payload_fields(&self) -> &'static [&'static str];
    /// The `SessionStarted.source` of a resume, when the harness reports one.
    fn resume_source(&self) -> Option<&'static str> {
        Some("resume")
    }
    /// The session file of `sid`.
    fn transcript(&self, agent: &Agent, sid: &HarnessSessionId) -> Option<PathBuf>;
    /// The usage samples in a session file: (count, first model name).
    fn usage(&self, path: &Path) -> Result<(usize, String), String>;
    /// The operator's own hooks in the shared home that fire in an agent
    /// when the sandbox does not bind the per-agent overlay over them (mode
    /// `none`). Each entry names the file and the events.
    fn user_hooks(&self) -> Vec<String> {
        Vec::new()
    }
    /// True when the suite must copy the per-agent state file over the
    /// shared one for the run (see `docs/compat-suite.md`).
    fn state_overlay(&self) -> Option<&'static str> {
        None
    }
}

/// The event names of a `{"hooks": {"<Event>": [...]}}` file with at least
/// one entry, or `None` when the file is missing, broken, or empty.
fn hook_events_in(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let names: Vec<&str> = v
        .get("hooks")?
        .as_object()?
        .iter()
        .filter(|(_, e)| e.as_array().is_some_and(|a| !a.is_empty()))
        .map(|(k, _)| k.as_str())
        .collect();
    (!names.is_empty()).then(|| format!("{} ({})", path.display(), names.join(", ")))
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

// ---------------------------------------------------------------- Claude

pub struct Claude {
    home: PathBuf,
    model: String,
    adapter: Arc<ClaudeAdapter>,
}

impl Claude {
    pub fn new(settings: &Settings, home: PathBuf) -> Self {
        let adapter = ClaudeAdapter::new(ClaudeOptions {
            config_dir: home.clone(),
            host_config_dir: None,
            harness_workspace: None,
            hook_program: settings.cli.display().to_string(),
            ..ClaudeOptions::default()
        });
        Self {
            home,
            model: settings.model(HarnessId::Claude),
            adapter: Arc::new(adapter),
        }
    }
}

impl Harness for Claude {
    fn id(&self) -> HarnessId {
        HarnessId::Claude
    }
    fn adapter(&self) -> Arc<dyn Adapter> {
        self.adapter.clone()
    }
    fn home(&self) -> &Path {
        &self.home
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn auth(&self) -> Result<String, String> {
        let a = self.adapter.auth_status().map_err(|e| e.to_string())?;
        let what = format!(
            "`claude auth status`: logged_in={}, method={}, subscription={}",
            a.logged_in,
            a.auth_method.as_deref().unwrap_or("?"),
            a.subscription_type.as_deref().unwrap_or("?")
        );
        if a.logged_in { Ok(what) } else { Err(what) }
    }
    fn materialize(
        &self,
        agent: &Agent,
        home: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<FileOverlay>, String> {
        self.adapter
            .materialize(agent, home, prompt)
            .map_err(|e| e.to_string())
    }
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        self.adapter.launch(agent, resume)
    }
    fn exit_keys(&self) -> Vec<u8> {
        terrarium_adapter_claude::keys::exit()
    }
    fn turn_hooks(&self) -> [&'static str; 2] {
        ["UserPromptSubmit", "Stop"]
    }
    fn payload_fields(&self) -> &'static [&'static str] {
        &["session_id", "transcript_path", "cwd", "hook_event_name"]
    }
    fn transcript(&self, agent: &Agent, sid: &HarnessSessionId) -> Option<PathBuf> {
        Some(self.adapter.session_file(agent, sid)).filter(|p| p.is_file())
    }
    fn usage(&self, path: &Path) -> Result<(usize, String), String> {
        let u =
            terrarium_adapter_claude::usage::read_transcript(path).map_err(|e| e.to_string())?;
        let model = u
            .samples
            .first()
            .map(|s| s.model.clone())
            .unwrap_or_default();
        Ok((u.samples.len(), model))
    }
    fn state_overlay(&self) -> Option<&'static str> {
        Some(terrarium_adapter_claude::config::STATE_FILE)
    }
}

// ----------------------------------------------------------------- Codex

pub struct Codex {
    home: PathBuf,
    model: String,
    adapter: Arc<CodexAdapter>,
}

impl Codex {
    pub fn new(settings: &Settings, home: PathBuf) -> Self {
        let adapter = CodexAdapter::new(CodexOptions {
            codex_home: home.clone(),
            host_codex_home: None,
            harness_workspace: None,
            hook_program: settings.cli.display().to_string(),
            ..CodexOptions::default()
        });
        Self {
            home,
            model: settings.model(HarnessId::Codex),
            adapter: Arc::new(adapter),
        }
    }

    /// The same adapter on another home (the turn-free hooks check).
    pub fn with_home(&self, settings: &Settings, home: PathBuf) -> Self {
        Self::new(settings, home)
    }
}

/// The reasoning effort of every Codex turn in the suite (the spikes used
/// `low`). The adapter has no option for it, so the suite adds the `-c`.
pub const CODEX_EFFORT: &str = "model_reasoning_effort=\"low\"";

impl Harness for Codex {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }
    fn adapter(&self) -> Arc<dyn Adapter> {
        self.adapter.clone()
    }
    fn home(&self) -> &Path {
        &self.home
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn auth(&self) -> Result<String, String> {
        let a = self.adapter.auth_status().map_err(|e| e.to_string())?;
        let what = format!(
            "`codex login status`: logged_in={}, method={}",
            a.logged_in,
            a.method.as_deref().unwrap_or("?")
        );
        if a.logged_in { Ok(what) } else { Err(what) }
    }
    fn materialize(
        &self,
        agent: &Agent,
        home: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<FileOverlay>, String> {
        self.adapter
            .materialize(agent, home, prompt)
            .map_err(|e| e.to_string())
    }
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        self.adapter
            .launch(agent, resume)
            .arg("-c")
            .arg(CODEX_EFFORT)
    }
    fn exit_keys(&self) -> Vec<u8> {
        terrarium_adapter_codex::keys::exit()
    }
    fn turn_hooks(&self) -> [&'static str; 2] {
        ["UserPromptSubmit", "Stop"]
    }
    fn payload_fields(&self) -> &'static [&'static str] {
        &[
            "session_id",
            "transcript_path",
            "cwd",
            "hook_event_name",
            "model",
        ]
    }
    fn transcript(&self, agent: &Agent, sid: &HarnessSessionId) -> Option<PathBuf> {
        self.adapter.session_file(agent, sid)
    }
    /// The operator's `hooks.json`; the per-agent empty file covers it only
    /// under bwrap.
    fn user_hooks(&self) -> Vec<String> {
        hook_events_in(&self.home.join(terrarium_adapter_codex::config::HOOKS_FILE))
            .into_iter()
            .collect()
    }
    fn usage(&self, path: &Path) -> Result<(usize, String), String> {
        let u = terrarium_adapter_codex::usage::read_transcript(path).map_err(|e| e.to_string())?;
        let model = u
            .samples
            .first()
            .map(|s| s.model.clone())
            .unwrap_or_default();
        Ok((u.samples.len(), model))
    }
}

// --------------------------------------------------------------- Copilot

pub struct Copilot {
    home: PathBuf,
    model: String,
    adapter: Arc<CopilotAdapter>,
    prompt_ext: Option<PathBuf>,
}

impl Copilot {
    pub fn new(settings: &Settings, home: PathBuf) -> Self {
        let adapter = CopilotAdapter::new(CopilotOptions {
            config_dir: home.clone(),
            host_config_dir: None,
            harness_workspace: None,
            prompt_extension_dir: settings.copilot_prompt_ext.clone(),
            hook_program: settings.cli.display().to_string(),
            ..CopilotOptions::default()
        });
        Self {
            home,
            model: settings.model(HarnessId::Copilot),
            adapter: Arc::new(adapter),
            prompt_ext: settings.copilot_prompt_ext.clone(),
        }
    }

    /// The system-prompt extension directory, when the run has one.
    pub fn prompt_ext(&self) -> Option<&Path> {
        self.prompt_ext.as_deref()
    }
}

/// The env that reaches the system keychain (D-Bus), where Copilot keeps
/// its token on a desktop. The daemon's clean env does not pass these.
pub const COPILOT_KEYCHAIN_ENV: &[&str] = &["DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR"];

impl Harness for Copilot {
    fn id(&self) -> HarnessId {
        HarnessId::Copilot
    }
    fn adapter(&self) -> Arc<dyn Adapter> {
        self.adapter.clone()
    }
    fn home(&self) -> &Path {
        &self.home
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn auth(&self) -> Result<String, String> {
        let a = self.adapter.auth_status().map_err(|e| e.to_string())?;
        let source = match &a.source {
            CredentialSource::Env(v) => format!("env {v}"),
            CredentialSource::PlainTextConfig => "plain-text config.json".to_owned(),
            CredentialSource::Keychain => "system keychain".to_owned(),
            CredentialSource::None => "none".to_owned(),
        };
        let what = format!("token source: {source} (from config.json and env)");
        if a.source == CredentialSource::None {
            Err(what)
        } else {
            Ok(what)
        }
    }
    fn materialize(
        &self,
        agent: &Agent,
        home: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<FileOverlay>, String> {
        self.adapter
            .materialize(agent, home, prompt)
            .map_err(|e| e.to_string())
    }
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        self.adapter.launch(agent, resume)
    }
    fn exit_keys(&self) -> Vec<u8> {
        terrarium_adapter_copilot::keys::exit()
    }
    fn extra_env(&self) -> Vec<(String, String)> {
        COPILOT_KEYCHAIN_ENV
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v)))
            .collect()
    }
    fn prompt_reaches_harness(&self) -> Result<(), String> {
        Err(
            "the prompt reaches Copilot through `SYSTEM.md` / `APPEND_SYSTEM.md`, which the \
             bwrap sandbox binds over the shared home; sandbox mode `none` cannot bind them, \
             and the suite does not write the operator's own prompt files"
                .to_owned(),
        )
    }
    fn turn_hooks(&self) -> [&'static str; 2] {
        ["UserPromptSubmit", "Stop"]
    }
    fn payload_fields(&self) -> &'static [&'static str] {
        &["session_id", "cwd", "hook_event_name"]
    }
    fn transcript(&self, agent: &Agent, sid: &HarnessSessionId) -> Option<PathBuf> {
        Some(self.adapter.session_file(agent, sid)).filter(|p| p.is_file())
    }
    /// `hooks/*.json` and the `hooks` of `settings.json`: under bwrap an
    /// empty directory and a copy without hooks cover them.
    fn user_hooks(&self) -> Vec<String> {
        let mut out = Vec::new();
        let dir = self.home.join(terrarium_adapter_copilot::config::HOOKS_DIR);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            files.sort();
            out.extend(files.iter().filter_map(|f| hook_events_in(f)));
        }
        out.extend(hook_events_in(
            &self
                .home
                .join(terrarium_adapter_copilot::config::SETTINGS_FILE),
        ));
        out
    }
    fn usage(&self, path: &Path) -> Result<(usize, String), String> {
        let u =
            terrarium_adapter_copilot::usage::read_transcript(path).map_err(|e| e.to_string())?;
        let model = u
            .samples
            .first()
            .map(|s| s.model.clone())
            .unwrap_or_default();
        Ok((u.samples.len(), model))
    }
}

// -------------------------------------------------------------------- pi

/// The API-key subscription of a pi run (`TERRARIUM_COMPAT_PI_*`).
#[derive(Debug, Clone)]
pub struct PiSettings {
    /// Terrarium's provider word, for example `openai` or `anthropic`.
    pub provider: String,
    pub model: String,
}

pub struct Pi {
    home: PathBuf,
    model: String,
    adapter: Arc<PiAdapter>,
    sub: SubscriptionDef,
}

impl Pi {
    pub fn new(settings: &Settings, home: PathBuf) -> Self {
        let adapter = PiAdapter::new(PiOptions {
            agent_dir: home.clone(),
            host_agent_dir: None,
            harness_workspace: None,
            ..PiOptions::default()
        });
        let pi = settings.pi.clone().unwrap_or(PiSettings {
            provider: "openai".to_owned(),
            model: String::new(),
        });
        let sub = SubscriptionDef {
            name: "compat".to_owned(),
            provider: pi.provider.clone(),
            plan: Some(terrarium_adapter_pi::provider::API_KEY_PLAN.to_owned()),
            harnesses: vec![HarnessId::Pi],
            probe: None,
            alert_at: 80,
        };
        Self {
            home,
            model: pi.model,
            adapter: Arc::new(adapter),
            sub,
        }
    }

    /// The subscription of the run.
    pub fn subscription(&self) -> &SubscriptionDef {
        &self.sub
    }

    /// The env var of the API key, and its value from this process.
    fn key(&self) -> Option<(String, String)> {
        let var = terrarium_adapter_pi::provider::required_env(&self.sub)?;
        std::env::var(var).ok().map(|v| (var.to_owned(), v))
    }

    /// `Err(reason)` when this subscription must not run.
    pub fn refusal(&self) -> Option<String> {
        if let Some(r) = terrarium_adapter_pi::provider::refusal(&self.sub) {
            return Some(r.to_owned());
        }
        match terrarium_adapter_pi::provider::required_env(&self.sub) {
            None => Some(format!(
                "provider `{}` has no API-key env var in the adapter",
                self.sub.provider
            )),
            Some(var) if std::env::var(var).is_err() => {
                Some(format!("set {var} to the API key for the pi run"))
            }
            Some(_) => None,
        }
    }
}

impl Harness for Pi {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }
    fn adapter(&self) -> Arc<dyn Adapter> {
        self.adapter.clone()
    }
    fn home(&self) -> &Path {
        &self.home
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn auth(&self) -> Result<String, String> {
        let provider = terrarium_adapter_pi::provider::provider_for(&self.sub)
            .ok_or_else(|| format!("no pi provider for `{}`", self.sub.provider))?;
        let extra: Vec<(String, String)> = self.key().into_iter().collect();
        let a = self
            .adapter
            .auth_check(&provider, &extra)
            .map_err(|e| e.to_string())?;
        let what = format!(
            "`pi auth check --provider {provider}`: credential_present={}, auth_type={}",
            a.credential_present,
            a.auth_type.as_deref().unwrap_or("?")
        );
        if a.credential_present {
            Ok(what)
        } else {
            Err(what)
        }
    }
    fn materialize(
        &self,
        agent: &Agent,
        home: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<FileOverlay>, String> {
        self.adapter
            .materialize(agent, home, prompt)
            .map_err(|e| e.to_string())
    }
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let mut spec = self.adapter.launch_for(agent, resume, Some(&self.sub));
        if let Some((k, v)) = self.key() {
            spec.env.insert(k, v);
        }
        spec
    }
    fn exit_keys(&self) -> Vec<u8> {
        terrarium_adapter_pi::keys::EXIT.to_vec()
    }
    fn turn_hooks(&self) -> [&'static str; 2] {
        ["input", "agent_settled"]
    }
    fn payload_fields(&self) -> &'static [&'static str] {
        &["session_id", "cwd", "session_file"]
    }
    /// pi resumes through `--session-id`, and `session_start.reason` stays
    /// `startup` (`docs/compat.md`).
    fn resume_source(&self) -> Option<&'static str> {
        None
    }
    fn transcript(&self, agent: &Agent, sid: &HarnessSessionId) -> Option<PathBuf> {
        self.adapter.session_file(agent, sid)
    }
    fn usage(&self, path: &Path) -> Result<(usize, String), String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let samples = terrarium_adapter_pi::usage::parse_session(&text);
        let model = samples.first().map(|s| s.model.clone()).unwrap_or_default();
        Ok((samples.len(), model))
    }
}

/// The text of the assistant's last reply, from the hook payloads of one
/// turn: Claude and Codex put it in `Stop.last_assistant_message`, pi in
/// `message_end`. Copilot has no reply text in its hooks.
pub fn reply_from_hooks(hooks: &[crate::recording::Captured]) -> Option<String> {
    hooks
        .iter()
        .rev()
        .find_map(|c| s(&c.body, "last_assistant_message"))
        .filter(|t| !t.trim().is_empty())
}
