//! The command line for a fresh start or a resume.

use std::path::Path;

use terrarium_core::{Agent, HarnessSessionId, LaunchSpec};

use crate::{GrokOptions, PromptMode};

/// How the session starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStart<'a> {
    /// A new conversation with this UUID (`--session-id`). Use a new UUID
    /// for each fresh start.
    Fresh(&'a str),
    /// Resume this session (`--resume <id>`). Not `--session-id`: 1.0.77
    /// help says that starts a new conversation (res-1190 F5).
    Resume(&'a HarnessSessionId),
}

/// Build the launch spec. Pure: [`crate::GrokAdapter::launch`] picks the
/// session UUID and records it.
///
/// The program is [`GrokOptions::program`]: the daemon sets the absolute
/// `bin/grok` of the active version dir (`crates/terrariumd`), not the
/// `agent` alias and not a `~/.grok/bin` copy.
///
/// The flags, in order:
///
/// - `--cwd <cwd>`.
/// - `--session-id <uuid>` or `--resume <id>`: see [`SessionStart`].
/// - `--model <m>` when the agent sets one.
/// - `--append-system-prompt <text>` (default) or `--system-prompt <text>`
///   (an agent with its own base prompt) when the rendered prompt has
///   text. Both are the documented Claude Code aliases.
///
/// No permission flag: no `--always-approve`, `--yolo`, `--trust`,
/// `--permission-mode`, or `--allow` rule ([`crate::GrokFlags::forbidden`]).
/// Grok asks before a tool call in its default mode; the question shows on
/// the screen. The env: `GROK_HOME`, the Grok home as the harness sees it.
pub fn build(
    opts: &GrokOptions,
    agent: &Agent,
    session: SessionStart<'_>,
    prompt: Option<&str>,
) -> LaunchSpec {
    let f = &opts.flags;
    let cwd = opts.harness_cwd(agent);
    let mut spec = LaunchSpec::new(opts.program.as_str(), &cwd)
        .arg(f.cwd)
        .arg(path_str(&cwd));
    spec = match session {
        SessionStart::Fresh(uuid) => spec.arg(f.session_id).arg(uuid),
        SessionStart::Resume(id) => spec.arg(f.resume).arg(id.0.as_str()),
    };
    if let Some(model) = agent.model.as_deref().filter(|m| !m.is_empty()) {
        spec = spec.arg(f.model).arg(model);
    }
    if let Some(text) = prompt.filter(|t| !t.trim().is_empty()) {
        let flag = match opts.prompt_mode_for(agent) {
            PromptMode::Append => f.append_system_prompt,
            PromptMode::Replace => f.system_prompt,
        };
        spec = spec.arg(flag).arg(text);
    }
    spec.env(f.home_env, path_str(&opts.home_dir))
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

    fn args(spec: &LaunchSpec) -> Vec<&str> {
        spec.args.iter().map(String::as_str).collect()
    }

    fn value_after<'a>(spec: &'a LaunchSpec, flag: &str) -> Option<&'a str> {
        let i = spec.args.iter().position(|a| a == flag)?;
        spec.args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn a_fresh_start() {
        let opts = GrokOptions {
            program: "/opt/terrarium/tools/versions/grok/1.0.77/bin/grok".into(),
            home_dir: PathBuf::from("/home/agent/.grok"),
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            ..GrokOptions::default()
        };
        let agent = test_agent(Some("grok-build"));
        let uuid = "11111111-2222-4333-8444-555555555555";
        let spec = build(&opts, &agent, SessionStart::Fresh(uuid), Some("be brief"));
        assert_eq!(
            spec.program,
            "/opt/terrarium/tools/versions/grok/1.0.77/bin/grok"
        );
        assert!(spec.pty);
        assert_eq!(spec.cwd, PathBuf::from("/home/agent/work"));
        assert_eq!(
            args(&spec),
            [
                "--cwd",
                "/home/agent/work",
                "--session-id",
                uuid,
                "--model",
                "grok-build",
                "--append-system-prompt",
                "be brief",
            ]
        );
        assert_eq!(spec.env["GROK_HOME"], "/home/agent/.grok");
        assert_eq!(spec.env.len(), 1);
    }

    #[test]
    fn a_resume_uses_resume_not_session_id() {
        let opts = GrokOptions::default();
        let agent = test_agent(None);
        let id = HarnessSessionId("0f8e4c2a-7b1d-4e3f-9a6c-5d2b8e1f4a7c".into());
        let spec = build(&opts, &agent, SessionStart::Resume(&id), None);
        assert_eq!(value_after(&spec, "--resume"), Some(id.0.as_str()));
        assert!(!args(&spec).contains(&"--session-id"));
        assert!(!args(&spec).contains(&"--model"));
        assert!(!args(&spec).contains(&"--append-system-prompt"));
        // Outside the sandbox: the workspace and the host home.
        assert_eq!(spec.cwd, agent.workspace);
        assert_eq!(spec.env["GROK_HOME"], "/auth/grok");
    }

    #[test]
    fn a_base_prompt_replaces_the_default() {
        let opts = GrokOptions::default();
        let mut agent = test_agent(None);
        agent.system_prompt = Some("You review code.".into());
        let spec = build(&opts, &agent, SessionStart::Fresh("u"), Some("text"));
        assert_eq!(value_after(&spec, "--system-prompt"), Some("text"));
        assert!(!args(&spec).contains(&"--append-system-prompt"));
        // A blank prompt adds no flag.
        let spec = build(&opts, &agent, SessionStart::Fresh("u"), Some(" \n"));
        assert!(!args(&spec).contains(&"--system-prompt"));
    }

    /// No launch widens the permissions (card TER-1192).
    #[test]
    fn no_launch_has_a_permission_flag() {
        let opts = GrokOptions::default();
        let agent = test_agent(Some("m"));
        let id = HarnessSessionId("x".into());
        for spec in [
            build(&opts, &agent, SessionStart::Fresh("u"), Some("p")),
            build(&opts, &agent, SessionStart::Resume(&id), Some("p")),
        ] {
            for bad in opts.flags.forbidden {
                assert!(
                    !spec.args.iter().any(|a| a == bad || a.starts_with(&format!("{bad}="))),
                    "{bad} in {:?}",
                    spec.args
                );
            }
        }
    }

    #[test]
    fn uuids_are_v4_and_differ() {
        let a = new_session_uuid();
        let b = new_session_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }
}
