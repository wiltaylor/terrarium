//! The command line for a fresh start or a resume (PLAN §7, §8).

use std::path::Path;

use terrarium_core::config::SubscriptionDef;
use terrarium_core::{Agent, LaunchSpec};

use crate::flags::{self, PiFlags};
use crate::{PiOptions, PromptMode, config, provider};

/// The provider and model flags for an agent.
///
/// - The provider comes from the subscription ([`provider::provider_for`]).
/// - The model comes from the agent. A model `<provider>/<id>` with the
///   same provider loses the prefix. Without a subscription, a model
///   `<provider>/<id>` goes to pi as it is: pi reads the provider from it.
/// - A subscription and no model: the provider's cheap default from
///   [`flags::default_model`]. pi 1.4 refuses `--provider` without
///   `--model`, and a start with neither flag can spend another
///   subscription (see [`flags::DEFAULT_MODELS`]). The agent's `model`
///   overrides the default.
/// - A subscription whose provider has no default (a custom provider) and
///   no model: no flags. The daemon refuses that start first
///   ([`provider::start_refusal`]).
/// - No subscription and no model: no flags. pi uses `defaultProvider` and
///   `defaultModel` from the agent's `settings.json`.
///
/// `--provider` never goes alone.
pub fn model_args(
    f: &PiFlags,
    agent: &Agent,
    subscription: Option<&SubscriptionDef>,
) -> Vec<String> {
    let provider = subscription.and_then(provider::provider_for);
    let model = agent
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let mut out = Vec::new();
    match (provider.as_deref(), model) {
        (Some(p), Some(m)) => {
            let m = m
                .strip_prefix(p)
                .and_then(|r| r.strip_prefix('/'))
                .unwrap_or(m);
            out.extend([f.provider.to_owned(), p.to_owned()]);
            out.extend([f.model.to_owned(), m.to_owned()]);
        }
        (Some(p), None) => {
            if let Some(m) = flags::default_model(p) {
                out.extend([f.provider.to_owned(), p.to_owned()]);
                out.extend([f.model.to_owned(), m.to_owned()]);
            }
        }
        (None, Some(m)) => out.extend([f.model.to_owned(), m.to_owned()]),
        (None, None) => {}
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
/// - `--tui-mode regular`: pi 1.4 starts in `fullscreen` by default
///   (alternate screen, mouse tracking). `regular` keeps the screen that
///   the detector ([`crate::screen`]) was built on.
/// - `--offline`: no update check.
/// - `--no-extensions -e <agent dir>/pi/terrarium.ts`: only the Terrarium
///   extension loads. Since 1.4 the built-in extensions stay off too.
/// - `--no-mcp`: no MCP servers. pi 1.4 would read `mcp.json` in the
///   shared home, which is the operator's.
/// - `--no-skills --no-prompt-templates`: no global skills (pi reads
///   `~/.agents/skills` outside its home) and no `/template` expansions.
/// - `--skill <agent dir>/skills/<name>` for each delivered skill (PLAN
///   §8.6). pi lists them in the `<skills>` prompt section, and the model
///   reads the `SKILL.md` with its `read` tool. pi has no skill tool.
/// - `--no-context-files` (unless [`PiOptions::context_files`]): pi does
///   not load `AGENTS.md`/`CLAUDE.md`. It would load the one in the shared
///   home too, which is the operator's. The prompt templates tell the agent
///   to read `AGENTS.md` and `CLAUDE.md` in the workspace itself, as for
///   the other harnesses.
/// - `--no-approve`: ignore project-local `.pi/` files (no trust dialog),
///   as Claude Code's `--setting-sources ""` keeps repository settings out.
/// - `--session-dir <agent dir>/pi/sessions --session-id <id>`.
/// - `--system-prompt <file | "">` and `--append-system-prompt <file | "">`:
///   the prompt file goes to the flag of its [`PromptMode`] (append by
///   default), and the other flag gets an empty value, so pi does not
///   discover the operator's `SYSTEM.md` or `APPEND_SYSTEM.md` (B1).
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
        .arg(f.tui_mode.0)
        .arg(f.tui_mode.1)
        .arg(f.offline)
        .arg(f.no_extensions)
        .arg(f.extension)
        .arg(path_str(&config::extension_path(agent)))
        .arg(f.no_mcp)
        .arg(f.no_skills);
    let skills = terrarium_core::skills::delivered_dir(&config::agent_dir(agent));
    for name in &agent.delivered_skills {
        spec = spec.arg(f.skill).arg(path_str(&skills.join(name)));
    }
    spec = spec.arg(f.no_prompt_templates);
    if !opts.context_files {
        spec = spec.arg(f.no_context_files);
    }
    spec = spec
        .arg(f.no_approve)
        .arg(f.session_dir)
        .arg(path_str(&config::sessions_dir(agent)))
        .arg(f.session_id)
        .arg(session_id);
    // B1: each prompt flag replaces only its own discovered file
    // (`SYSTEM.md` or `APPEND_SYSTEM.md` in the pi home). The other flag
    // gets an empty value: pi then discovers no file for it, and an empty
    // value gives no text (`resource-loader.js` `resolvePromptInput`). With
    // no prompt, both are empty. The empty files that `config` binds over
    // both are the second fix.
    let (system, append) = match prompt_file {
        Some(file) => match opts.prompt_mode_for(agent) {
            PromptMode::Append => (String::new(), path_str(file)),
            PromptMode::Replace => (path_str(file), String::new()),
        },
        None => (String::new(), String::new()),
    };
    spec = spec
        .arg(f.system_prompt)
        .arg(system)
        .arg(f.append_system_prompt)
        .arg(append);
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
            "--tui-mode",
            "regular",
            "--offline",
            "--no-extensions",
            "-e",
            &dir.join("terrarium.ts").to_string_lossy(),
            "--no-mcp",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
            "--no-approve",
            "--session-dir",
            &dir.join("sessions").to_string_lossy(),
            "--session-id",
            "11111111-2222-4333-8444-555555555555",
            "--system-prompt",
            "",
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

    /// PLAN §8.6: one `--skill` for each delivered skill, after
    /// `--no-skills` (which stops only discovery).
    #[test]
    fn delivered_skills_load_with_no_skills() {
        let mut agent = test_agent(None);
        let spec = build(&PiOptions::default(), &agent, "s", None, None);
        assert!(!spec.args.iter().any(|a| a == "--skill"));
        agent.delivered_skills = vec!["a".into(), "fern-check".into()];
        let spec = build(&PiOptions::default(), &agent, "s", None, None);
        let dir = terrarium_core::skills::delivered_dir(&config::agent_dir(&agent));
        let i = spec
            .args
            .iter()
            .position(|a| a == "--no-skills")
            .expect("no-skills");
        assert_eq!(
            spec.args[i + 1..i + 5],
            [
                "--skill".to_owned(),
                dir.join("a").to_string_lossy().into_owned(),
                "--skill".to_owned(),
                dir.join("fern-check").to_string_lossy().into_owned(),
            ]
        );
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
        assert_eq!(value_after(&spec, "--append-system-prompt"), Some(""));
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

    /// A base system prompt selects `--system-prompt <file>`; with none,
    /// the file is appended to pi's own prompt (PLAN §8).
    #[test]
    fn a_base_system_prompt_selects_replace() {
        let mut agent = test_agent(None);
        let file = Some(Path::new("/p.md"));
        let spec = build(&PiOptions::default(), &agent, "id", None, file);
        assert_eq!(value_after(&spec, "--append-system-prompt"), Some("/p.md"));
        // B1: the other flag is empty, so pi discovers no SYSTEM.md.
        assert_eq!(value_after(&spec, "--system-prompt"), Some(""));
        agent.system_prompt = Some("You review code.".into());
        let spec = build(&PiOptions::default(), &agent, "id", None, file);
        assert_eq!(value_after(&spec, "--system-prompt"), Some("/p.md"));
        assert_eq!(value_after(&spec, "--append-system-prompt"), Some(""));
        // No prompt: both empty, so neither file of the home loads.
        let spec = build(&PiOptions::default(), &agent, "id", None, None);
        assert_eq!(value_after(&spec, "--system-prompt"), Some(""));
        assert_eq!(value_after(&spec, "--append-system-prompt"), Some(""));
    }

    #[test]
    fn model_prefix_and_provider() {
        let sub = codex_sub();
        assert_eq!(
            model_args(
                &crate::PI_PINNED,
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
                &crate::PI_PINNED,
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
                &crate::PI_PINNED,
                &test_agent(Some("openai-codex/gpt-5.6-luna")),
                None
            ),
            ["--model", "openai-codex/gpt-5.6-luna"]
        );
        assert!(model_args(&crate::PI_PINNED, &test_agent(Some(" ")), None).is_empty());
        assert!(model_args(&crate::PI_PINNED, &test_agent(None), None).is_empty());
    }

    /// pi 1.4 refuses `--provider` without `--model`. A subscription with
    /// no model gets the provider's default, so the provider stays in force.
    #[test]
    fn subscription_without_model_gets_the_default_model() {
        let f = &crate::PI_PINNED;
        assert_eq!(
            model_args(f, &test_agent(None), Some(&codex_sub())),
            ["--provider", "openai-codex", "--model", "gpt-6-luna"]
        );
        let key = SubscriptionDef {
            provider: "anthropic".into(),
            plan: Some("API key".into()),
            ..codex_sub()
        };
        assert_eq!(
            model_args(f, &test_agent(Some("  ")), Some(&key)),
            ["--provider", "anthropic", "--model", "claude-haiku-5-5"]
        );
        // A custom provider has no default: no flags at all, never a lone
        // `--provider` (the daemon refuses this start first).
        let custom = SubscriptionDef {
            provider: "glm53-exl3".into(),
            ..codex_sub()
        };
        assert!(model_args(f, &test_agent(None), Some(&custom)).is_empty());
    }

    #[test]
    fn zai_key_gets_the_glm_provider_and_model() {
        let f = &crate::PI_PINNED;
        let zai = |plan: &str| SubscriptionDef {
            provider: "zai".into(),
            plan: Some(plan.into()),
            ..codex_sub()
        };
        assert_eq!(
            model_args(f, &test_agent(None), Some(&zai("coding"))),
            ["--provider", "zai", "--model", "glm-5.3-flash"]
        );
        assert_eq!(
            model_args(f, &test_agent(None), Some(&zai("api"))),
            ["--provider", "zai-api", "--model", "glm-5.3-flash"]
        );
        // The agent's model overrides the default; a `zai/` prefix goes.
        assert_eq!(
            model_args(f, &test_agent(Some("zai/glm-5.3")), Some(&zai("coding"))),
            ["--provider", "zai", "--model", "glm-5.3"]
        );
        assert_eq!(crate::required_env(&zai("coding")), Some("ZAI_API_KEY"));
        assert_eq!(crate::start_refusal(&zai("coding"), None), None);
    }

    #[test]
    fn subscription_and_model_use_the_agent_model() {
        let f = &crate::PI_PINNED;
        assert_eq!(
            model_args(f, &test_agent(Some("gpt-6-sol")), Some(&codex_sub())),
            ["--provider", "openai-codex", "--model", "gpt-6-sol"]
        );
        let custom = SubscriptionDef {
            provider: "glm53-exl3".into(),
            ..codex_sub()
        };
        assert_eq!(
            model_args(f, &test_agent(Some("glm-5.3")), Some(&custom)),
            ["--provider", "glm53-exl3", "--model", "glm-5.3"]
        );
    }

    /// Whatever the inputs, `--provider` comes with `--model`.
    #[test]
    fn provider_never_goes_alone() {
        let subs = [
            None,
            Some(codex_sub()),
            Some(SubscriptionDef {
                provider: "glm53-exl3".into(),
                ..codex_sub()
            }),
        ];
        for sub in &subs {
            for model in [None, Some(""), Some("x"), Some("openai-codex/y")] {
                let a = model_args(&crate::PI_PINNED, &test_agent(model), sub.as_ref());
                if a.iter().any(|x| x == "--provider") {
                    assert!(a.iter().any(|x| x == "--model"), "{a:?}");
                }
            }
        }
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
