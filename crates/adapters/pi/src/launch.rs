//! The command line for a fresh start or a resume (PLAN §7, §8).

use std::path::Path;

use terrarium_core::config::SubscriptionDef;
use terrarium_core::{Agent, LaunchSpec};

use crate::flags::PiFlags;
use crate::{PiOptions, PromptMode, config, provider};

/// The provider and model flags for an agent.
///
/// - The provider comes from the subscription ([`provider::provider_for`]).
/// - The model comes from the agent. A model `<provider>/<id>` with the
///   same provider loses the prefix. Without a subscription, a model
///   `<provider>/<id>` goes to pi as it is: pi reads the provider from it.
/// - Without both, pi uses `defaultProvider` and `defaultModel` from the
///   agent's `settings.json`.
pub fn model_args(
    f: &PiFlags,
    agent: &Agent,
    subscription: Option<&SubscriptionDef>,
) -> Vec<String> {
    let provider = subscription.and_then(provider::provider_for);
    let mut model = agent
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    if let (Some(p), Some(m)) = (provider.as_deref(), model) {
        model = Some(
            m.strip_prefix(p)
                .and_then(|r| r.strip_prefix('/'))
                .unwrap_or(m),
        );
    }
    let mut out = Vec::new();
    if let Some(p) = provider {
        out.extend([f.provider.to_owned(), p]);
    }
    if let Some(m) = model {
        out.extend([f.model.to_owned(), m.to_owned()]);
    }
    out
}

/// Build the launch spec. This is pure: [`crate::PiAdapter`] finds the
/// prompt file and the session ID, and then calls this.
///
/// - `session_id`: a new UUID on a fresh start, or the ID to resume. pi
///   creates the session when the ID is new in the session directory, and
///   resumes it when it is there. The same flag does both.
/// - `prompt_file`: the rendered prompt (`prompt.md`), when it has text.
///   pi reads the file at every start, so a changed template applies on
///   the next start, also on a resume (`docs/compat.md`: the prompt is not
///   frozen).
///
/// The flags, in order:
///
/// - `--provider <p> --model <m>`: see [`model_args`].
/// - `--offline`: no update check.
/// - `--no-extensions -e <agent dir>/pi/terrarium.ts`: only the Terrarium
///   extension loads.
/// - `--no-skills --no-prompt-templates`: no global skills (pi reads
///   `~/.agents/skills` outside its home) and no `/template` expansions.
/// - `--no-context-files` (unless [`PiOptions::context_files`]): pi does
///   not load `AGENTS.md`/`CLAUDE.md`. It would load the one in the shared
///   home too, which is the operator's. The prompt templates tell the agent
///   to read `AGENTS.md` and `CLAUDE.md` in the workspace itself, as for
///   the other harnesses.
/// - `--no-approve`: ignore project-local `.pi/` files (no trust dialog),
///   as Claude Code's `--setting-sources ""` keeps repository settings out.
/// - `--session-dir <agent dir>/pi/sessions --session-id <id>`.
/// - `--append-system-prompt <file>` (default) or `--system-prompt <file>`
///   (see [`PromptMode`]).
///
/// No yolo flag: pi never asks before a tool.
pub fn build(
    opts: &PiOptions,
    agent: &Agent,
    session_id: &str,
    subscription: Option<&SubscriptionDef>,
    prompt_file: Option<&Path>,
) -> LaunchSpec {
    let f = &opts.flags;
    let mut spec = LaunchSpec::new(opts.program.as_str(), opts.harness_cwd(agent));
    for a in model_args(f, agent, subscription) {
        spec = spec.arg(a);
    }
    spec = spec
        .arg(f.offline)
        .arg(f.no_extensions)
        .arg(f.extension)
        .arg(path_str(&config::extension_path(agent)))
        .arg(f.no_skills)
        .arg(f.no_prompt_templates);
    if !opts.context_files {
        spec = spec.arg(f.no_context_files);
    }
    spec = spec
        .arg(f.no_approve)
        .arg(f.session_dir)
        .arg(path_str(&config::sessions_dir(agent)))
        .arg(f.session_id)
        .arg(session_id);
    if let Some(file) = prompt_file {
        let flag = match opts.prompt_mode {
            PromptMode::Append => f.append_system_prompt,
            PromptMode::Replace => f.system_prompt,
        };
        spec = spec.arg(flag).arg(path_str(file));
    }
    spec = spec.env(f.config_env, path_str(&opts.agent_dir));
    for (key, value) in f.quiet_env {
        spec = spec.env(*key, *value);
    }
    spec
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A new random UUID (version 4 layout) for `--session-id`.
pub fn new_session_uuid() -> String {
    // A ULID has 80 random bits. Set the version and variant bits on top.
    let mut n: u128 = terrarium_core::SessionId::new().0.0;
    n = (n & !(0xF << 76)) | (0x4 << 76);
    n = (n & !(0x3 << 62)) | (0x2 << 62);
    let h = format!("{n:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent;
    use std::path::PathBuf;

    fn value_after<'a>(spec: &'a LaunchSpec, flag: &str) -> Option<&'a str> {
        let i = spec.args.iter().position(|a| a == flag)?;
        spec.args.get(i + 1).map(String::as_str)
    }

    fn codex_sub() -> SubscriptionDef {
        SubscriptionDef {
            name: "chatgpt-pro".into(),
            provider: "openai-codex".into(),
            plan: Some("Pro".into()),
            harnesses: vec![terrarium_core::HarnessId::Pi],
            probe: None,
            alert_at: 80,
        }
    }

    #[test]
    fn fresh_start_has_every_flag() {
        let agent = test_agent(Some("gpt-5.6-luna"));
        let prompt = PathBuf::from("/state/agents/x/prompt.md");
        let spec = build(
            &PiOptions::default(),
            &agent,
            "11111111-2222-4333-8444-555555555555",
            Some(&codex_sub()),
            Some(&prompt),
        );
        assert_eq!(spec.program, "pi");
        assert!(spec.pty);
        assert_eq!(spec.cwd, agent.workspace);
        let dir = config::pi_dir(&agent);
        let want: Vec<String> = [
            "--provider",
            "openai-codex",
            "--model",
            "gpt-5.6-luna",
            "--offline",
            "--no-extensions",
            "-e",
            &dir.join("terrarium.ts").to_string_lossy(),
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
            "--no-approve",
            "--session-dir",
            &dir.join("sessions").to_string_lossy(),
            "--session-id",
            "11111111-2222-4333-8444-555555555555",
            "--append-system-prompt",
            "/state/agents/x/prompt.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(spec.args, want);
        assert_eq!(spec.env["PI_CODING_AGENT_DIR"], "/auth/pi");
        assert_eq!(spec.env["PI_TELEMETRY"], "0");
        assert_eq!(spec.env["PI_OFFLINE"], "1");
        assert_eq!(spec.env.len(), 3);
    }

    #[test]
    fn options_change_prompt_mode_context_and_dirs() {
        let opts = PiOptions {
            agent_dir: PathBuf::from("/home/agent/.pi/agent"),
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            prompt_mode: PromptMode::Replace,
            context_files: true,
            ..PiOptions::default()
        };
        let spec = build(
            &opts,
            &test_agent(None),
            "id",
            None,
            Some(Path::new("/p.md")),
        );
        assert_eq!(value_after(&spec, "--system-prompt"), Some("/p.md"));
        assert!(!spec.args.iter().any(|a| a == "--append-system-prompt"));
        assert!(!spec.args.iter().any(|a| a == "--no-context-files"));
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a == "--provider" || a == "--model")
        );
        assert_eq!(spec.cwd, Path::new("/home/agent/work"));
        assert_eq!(spec.env["PI_CODING_AGENT_DIR"], "/home/agent/.pi/agent");
    }

    #[test]
    fn model_prefix_and_provider() {
        let sub = codex_sub();
        assert_eq!(
            model_args(
                &crate::PI_0_85,
                &test_agent(Some("openai-codex/gpt-5.6-luna")),
                Some(&sub)
            ),
            ["--provider", "openai-codex", "--model", "gpt-5.6-luna"]
        );
        // OpenRouter model ids have their own slash.
        let or = SubscriptionDef {
            provider: "openrouter".into(),
            ..sub.clone()
        };
        assert_eq!(
            model_args(
                &crate::PI_0_85,
                &test_agent(Some("anthropic/claude-x:high")),
                Some(&or)
            ),
            [
                "--provider",
                "openrouter",
                "--model",
                "anthropic/claude-x:high"
            ]
        );
        // No subscription: the model goes as it is.
        assert_eq!(
            model_args(
                &crate::PI_0_85,
                &test_agent(Some("openai-codex/gpt-5.6-luna")),
                None
            ),
            ["--model", "openai-codex/gpt-5.6-luna"]
        );
        assert!(model_args(&crate::PI_0_85, &test_agent(Some(" ")), None).is_empty());
        assert_eq!(
            model_args(&crate::PI_0_85, &test_agent(None), Some(&sub)),
            ["--provider", "openai-codex"]
        );
    }

    #[test]
    fn session_uuids_are_v4_and_differ() {
        let a = new_session_uuid();
        assert_ne!(a, new_session_uuid());
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
    }
}
