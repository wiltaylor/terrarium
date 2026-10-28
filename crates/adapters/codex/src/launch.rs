//! The command line for a fresh start or a resume (PLAN §7, §8).

use std::path::Path;

use serde_json::Value;
use terrarium_core::{Agent, HarnessSessionId, LaunchSpec};

use crate::{CodexOptions, PromptMode, config};

/// A string as a TOML basic string. `-c` parses its value as TOML.
///
/// JSON string escapes are valid TOML escapes, with one gap: TOML does not
/// allow a raw DEL (U+007F), which JSON keeps as it is.
pub fn toml_string(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".to_owned())
        .replace('\u{7f}', "\\u007F")
}

/// One `-c` value that registers `<hook_program> hook <event>` for `event`,
/// in the Claude Code hook format that Codex reads:
/// `hooks.<Event>=[{hooks=[{type="command",command="..."}]}]`.
pub fn hook_override(hook_program: &str, event: &str) -> String {
    let command = toml_string(&format!("{hook_program} hook {event}"));
    format!("hooks.{event}=[{{hooks=[{{type=\"command\",command={command}}}]}}]")
}

/// The `-c` value that trusts `cwd`, so no "Folder access" dialog shows (it
/// shows only in a git repository).
///
/// Verified form: `projects.<cwd>.trust_level="trusted"`, with no quotes
/// round the path. Codex splits the key on dots, so this form works only
/// for a path with no dot. For a path with a dot, this gives an inline
/// table, `projects={"<cwd>"={trust_level="trusted"}}`. That form is not
/// tested; it can also hide the `projects` table of a shared
/// `config.toml`.
pub fn trust_override(cwd: &Path) -> String {
    let path = cwd.to_string_lossy();
    if path.contains('.') {
        format!(
            "projects={{{}={{trust_level=\"trusted\"}}}}",
            toml_string(&path)
        )
    } else {
        format!("projects.{path}.trust_level=\"trusted\"")
    }
}

/// A key part as TOML writes it: bare when it can be, else quoted.
fn toml_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_owned()
    } else {
        toml_string(k)
    }
}

/// A JSON value (from the TOML user file) as an inline TOML value.
pub fn toml_inline(v: &Value) -> String {
    match v {
        Value::Null => "\"\"".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => toml_string(s),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(toml_inline).collect::<Vec<_>>().join(",")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}={}", toml_key(k), toml_inline(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

/// Is the dotted key `path` (or a key under it) one of `forced`?
fn is_forced(path: &str, forced: &[&str]) -> bool {
    forced.iter().any(|f| {
        path == *f || path.starts_with(&format!("{f}.")) || f.starts_with(&format!("{path}."))
    })
}

fn flatten(prefix: &str, v: &Value, forced: &[&str], out: &mut Vec<String>) {
    if let Value::Object(m) = v
        && !m.is_empty()
        && m.keys().all(|k| toml_key(k) == *k)
    {
        for (k, child) in m {
            let path = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            flatten(&path, child, forced, out);
        }
        return;
    }
    if prefix.is_empty() || is_forced(prefix, forced) {
        return;
    }
    out.push(format!("{prefix}={}", toml_inline(v)));
}

/// The `-c` values of the user settings file (`Agent::harness_settings`,
/// from `<config repo>/harness/codex/config.toml`, PLAN §8.2). Each table
/// is flattened to dotted keys down to the leaves, so a key of the shared
/// `config.toml` next to it stays. A table with a key that is not bare
/// (a path, a dot) goes as one inline table: Codex splits a `-c` key on
/// the dots. The keys of `forced` and the per-agent options (`model` when
/// the agent sets one) are dropped: Terrarium's value wins.
pub fn user_overrides(agent: &Agent, forced: &[&str]) -> Vec<String> {
    let Some(settings) = &agent.harness_settings else {
        return Vec::new();
    };
    let mut forced: Vec<&str> = forced.to_vec();
    if agent.model.as_deref().is_some_and(|m| !m.is_empty()) {
        forced.push("model");
    }
    let mut out = Vec::new();
    if let Value::Object(m) = settings {
        for (k, v) in m {
            let mut one = serde_json::Map::new();
            one.insert(k.clone(), v.clone());
            if toml_key(k) == *k {
                flatten("", &Value::Object(one), &forced, &mut out);
            } else if !is_forced(k, &forced) {
                out.push(format!("{}={}", toml_key(k), toml_inline(v)));
            }
        }
    }
    out
}

/// Build the launch spec. `prompt` is the rendered system prompt text, if
/// the agent has one. This is pure: [`crate::CodexAdapter`] reads the
/// prompt file, and then calls this.
///
/// The arguments, in order:
///
/// - `resume <id>` on a resume. A fresh start has no session flag: Codex
///   makes the ID at the first prompt.
/// - `--no-daemon`: no shared app-server daemon (see
///   [`crate::CodexFlags::no_daemon`]).
/// - `--dangerously-bypass-approvals-and-sandbox`: yolo; the Terrarium
///   sandbox holds the agent.
/// - `--dangerously-bypass-hook-trust`: the `-c` hooks run with no review
///   dialog.
/// - `-C <cwd>`, then `-m <model>` when the agent sets one.
/// - `-c <key>=<value>` for each key of the user settings file, without
///   the forced keys ([`user_overrides`]). The forced `-c` values come
///   after them.
/// - `-c projects.<cwd>.trust_level="trusted"`: see [`trust_override`].
/// - `-c check_for_update_on_startup=false`, `-c analytics.enabled=false`,
///   `-c feedback.enabled=false`.
/// - `-c hooks.<Event>=[...]` for each event of
///   [`crate::CodexFlags::hook_events`]. No file in `CODEX_HOME` changes.
/// - In the sandbox (a `harness_workspace` is set):
///   `-c sqlite_home="<agent dir>/codex/sqlite"`, the agent's own SQLite
///   files ([`config`]). With no sandbox, Codex keeps them in `CODEX_HOME`.
/// - On a resume, `-c tui.resume_cwd="session"`.
/// - The prompt: `-c developer_instructions="<text>"` (default) or, for an
///   agent with its own base system prompt,
///   `-c model_instructions_file="<agent dir>/prompt.md"` (see
///   [`CodexOptions::prompt_mode_for`]). Codex reads both only when it
///   makes the session.
///
/// `--skip-git-repo-check` is not passed: it is an `exec` flag, and the
/// TUI exits with "unexpected argument" when it gets it.
///
/// The env has `CODEX_HOME` only. The daemon builds the rest of the env
/// from a clean base (`sandbox::child_env`).
pub fn build(
    opts: &CodexOptions,
    agent: &Agent,
    resume: Option<&HarnessSessionId>,
    prompt: Option<&str>,
) -> LaunchSpec {
    let f = &opts.flags;
    let cwd = opts.harness_cwd(agent);
    let mut spec = LaunchSpec::new(opts.program.as_str(), &cwd);
    if let Some(id) = resume {
        spec = spec.arg(f.resume).arg(id.0.as_str());
    }
    spec = spec
        .arg(f.no_daemon)
        .arg(f.yolo)
        .arg(f.hook_trust)
        .arg(f.cd)
        .arg(path_str(&cwd));
    if let Some(model) = agent.model.as_deref().filter(|m| !m.is_empty()) {
        spec = spec.arg(f.model).arg(model);
    }
    for o in user_overrides(agent, &f.all_forced_keys()) {
        spec = spec.arg(f.config).arg(o);
    }
    spec = spec.arg(f.config).arg(trust_override(&cwd));
    for (key, value) in f.quiet_config.iter().chain(f.hardening_config) {
        spec = spec.arg(f.config).arg(format!("{key}={value}"));
    }
    for event in f.hook_events {
        spec = spec
            .arg(f.config)
            .arg(hook_override(&opts.hook_program, event));
    }
    if opts.harness_workspace.is_some() {
        spec = spec.arg(f.config).arg(format!(
            "{}={}",
            f.sqlite_home,
            toml_string(&path_str(&config::sqlite_dir(agent)))
        ));
    }
    if resume.is_some() {
        for (key, value) in f.resume_config {
            spec = spec.arg(f.config).arg(format!("{key}={value}"));
        }
    }
    if let Some(text) = prompt.map(str::trim).filter(|t| !t.is_empty()) {
        let value = match opts.prompt_mode_for(agent) {
            PromptMode::Replace => format!(
                "{}={}",
                f.model_instructions_file,
                toml_string(&path_str(&config::prompt_path(agent)))
            ),
            PromptMode::Append => {
                format!("{}={}", f.developer_instructions, toml_string(text))
            }
        };
        spec = spec.arg(f.config).arg(value);
    }
    spec.env(f.home_env, path_str(&opts.codex_home))
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
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

    /// Every `-c` value.
    fn overrides(spec: &LaunchSpec) -> Vec<&str> {
        spec.args
            .windows(2)
            .filter(|w| w[0] == "-c")
            .map(|w| w[1].as_str())
            .collect()
    }

    fn sandbox_opts() -> CodexOptions {
        CodexOptions {
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            hook_program: "/opt/terrarium/bin/terrarium".into(),
            ..CodexOptions::default()
        }
    }

    #[test]
    fn fresh_start_has_every_flag() {
        let agent = test_agent(Some("gpt-6-luna"));
        let spec = build(&sandbox_opts(), &agent, None, Some("Be brief."));
        assert_eq!(spec.program, "codex");
        assert!(spec.pty);
        assert_eq!(spec.cwd, Path::new("/home/agent/work"));
        let a = args(&spec);
        assert_eq!(
            &a[..3],
            [
                "--no-daemon",
                "--dangerously-bypass-approvals-and-sandbox",
                "--dangerously-bypass-hook-trust"
            ]
        );
        assert!(!a.contains(&"resume"));
        assert!(!a.contains(&"--skip-git-repo-check"));
        assert_eq!(value_after(&spec, "-C"), Some("/home/agent/work"));
        assert_eq!(value_after(&spec, "-m"), Some("gpt-6-luna"));
        let c = overrides(&spec);
        assert!(c.contains(&"projects./home/agent/work.trust_level=\"trusted\""));
        assert!(c.contains(&"check_for_update_on_startup=false"));
        assert!(c.contains(&"analytics.enabled=false"));
        assert!(c.contains(&"feedback.enabled=false"));
        assert!(!c.iter().any(|o| o.starts_with("tui.resume_cwd")));
        // No base system prompt: Codex keeps its own prompt.
        assert!(c.contains(&"developer_instructions=\"Be brief.\""), "{c:?}");
        assert!(!c.iter().any(|o| o.starts_with("model_instructions_file")));
        let sqlite = format!(
            "sqlite_home=\"{}\"",
            config::codex_dir(&agent).join("sqlite").display()
        );
        assert!(c.contains(&sqlite.as_str()), "{c:?}");
        assert_eq!(spec.env["CODEX_HOME"], "/auth/codex");
        assert_eq!(spec.env.len(), 1);
    }

    /// The user file goes as `-c` values before the forced ones; the
    /// forced keys and the agent's `model` are dropped from it.
    #[test]
    fn user_settings_become_overrides_without_the_forced_keys() {
        let mut agent = test_agent(Some("gpt-6-luna"));
        let toml = r#"
model = "other"
model_reasoning_effort = "high"
check_for_update_on_startup = true
approval_policy = "on-request"
[tui]
notifications = false
resume_cwd = "ask"
[analytics]
enabled = true
[hooks]
Stop = [{ hooks = [{ type = "command", command = "evil" }] }]
[projects."/home/agent/work"]
trust_level = "untrusted"
[mcp_servers.docs]
command = "docs-mcp"
args = ["--port", "1"]
env = { "A.B" = "x" }
"#;
        agent.harness_settings = Some(
            terrarium_core::harness_settings::parse(toml, terrarium_core::HarnessId::Codex)
                .expect("toml"),
        );
        let spec = build(&sandbox_opts(), &agent, None, None);
        let c = overrides(&spec);
        for want in [
            "model_reasoning_effort=\"high\"",
            "tui.notifications=false",
            "mcp_servers.docs.command=\"docs-mcp\"",
            "mcp_servers.docs.args=[\"--port\",\"1\"]",
            "mcp_servers.docs.env={\"A.B\"=\"x\"}",
        ] {
            assert!(c.contains(&want), "{want}: {c:?}");
        }
        for gone in [
            "model=",
            "check_for_update_on_startup=true",
            "approval_policy",
            "tui.resume_cwd=\"ask\"",
            "analytics.enabled=true",
            "evil",
            "untrusted",
        ] {
            assert!(!c.iter().any(|o| o.contains(gone)), "{gone}: {c:?}");
        }
        // Each override parses as TOML.
        for o in &c {
            let (k, v) = o.split_once('=').expect("kv");
            let _: toml::Table =
                toml::from_str(&format!("x = {v}")).unwrap_or_else(|e| panic!("{k}: {e}"));
        }
        // The user values come before the forced ones.
        let first_forced = c
            .iter()
            .position(|o| o.starts_with("projects"))
            .expect("trust");
        let user = c
            .iter()
            .position(|o| o.starts_with("model_reasoning_effort"))
            .expect("user");
        assert!(user < first_forced);
        // With no model on the agent, the file's model goes through.
        agent.model = None;
        let spec = build(&sandbox_opts(), &agent, None, None);
        assert!(overrides(&spec).contains(&"model=\"other\""));
    }

    /// The packaged default parses.
    #[test]
    fn the_packaged_default_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../defaults/harness/codex/config.toml");
        let v = terrarium_core::harness_settings::load(&path, terrarium_core::HarnessId::Codex)
            .expect("parses")
            .expect("exists");
        let mut agent = test_agent(None);
        agent.harness_settings = Some(v.clone());
        let flags = CodexOptions::default().flags;
        // No key of the file is forced: each one goes through.
        let c = user_overrides(&agent, &flags.all_forced_keys());
        let all = user_overrides(&agent, &[]);
        assert_eq!(c, all);
        for want in [
            "web_search=\"disabled\"",
            "features.image_generation=true",
            "skills.bundled.enabled=false",
        ] {
            assert!(c.iter().any(|o| o == want), "{want}: {c:?}");
        }
    }

    /// D5 and the start-up dialogs: the forced keys go to every launch,
    /// and a user file cannot turn the ask-user tools on again.
    #[test]
    fn a_user_file_cannot_turn_the_asking_tools_on() {
        let mut agent = test_agent(None);
        let toml = r#"
include_collaboration_mode_instructions = true
[tools.experimental_request_user_input]
enabled = true
[features]
default_mode_request_user_input = true
goals = true
apps = true
image_generation = false
[notice]
hide_rate_limit_model_nudge = false
model_migrations = { "gpt-5.6-luna" = "gpt-5.6-luna" }
"#;
        agent.harness_settings = Some(
            terrarium_core::harness_settings::parse(toml, terrarium_core::HarnessId::Codex)
                .expect("toml"),
        );
        let spec = build(&sandbox_opts(), &agent, None, None);
        let c = overrides(&spec);
        for want in [
            "tools.experimental_request_user_input.enabled=false",
            "features.default_mode_request_user_input=false",
            "include_collaboration_mode_instructions=false",
            "features.goals=false",
            "features.apps=false",
            "notice.hide_rate_limit_model_nudge=true",
            "tui.show_tooltips=false",
            "features.hooks=true",
        ] {
            assert_eq!(c.iter().filter(|o| **o == want).count(), 1, "{want}: {c:?}");
        }
        for gone in [
            "tools.experimental_request_user_input.enabled=true",
            "features.default_mode_request_user_input=true",
            "features.goals=true",
            "include_collaboration_mode_instructions=true",
            "hide_rate_limit_model_nudge=false",
            "\"gpt-5.6-luna\"=\"gpt-5.6-luna\"",
        ] {
            assert!(!c.iter().any(|o| o.contains(gone)), "{gone}: {c:?}");
        }
        // A tweakable key passes: the operator may turn image generation off.
        assert!(c.contains(&"features.image_generation=false"));
        // Each override parses as TOML, the migration table too.
        let m = c
            .iter()
            .find(|o| o.starts_with("notice.model_migrations="))
            .expect("migrations");
        let (_, v) = m.split_once('=').expect("kv");
        let t: toml::Table = toml::from_str(&format!("x = {v}")).expect("inline table");
        assert_eq!(t["x"]["gpt-5.6-luna"].as_str(), Some("gpt-6-luna"));
        for o in &c {
            let (k, v) = o.split_once('=').expect("kv");
            let _: toml::Table =
                toml::from_str(&format!("x = {v}")).unwrap_or_else(|e| panic!("{k}: {e}"));
        }
    }

    /// Sub-agents, the sleep tool, and background terminals stay off
    /// whatever the user file says. The image tool, `view_image`, and
    /// `apply_patch` are not touched.
    #[test]
    fn a_user_file_cannot_turn_sub_agents_or_background_work_on() {
        let mut agent = test_agent(None);
        let toml = r#"
experimental_use_unified_exec_tool = true
[agents]
enabled = true
max_threads = 4
[features]
multi_agent = true
sleep_tool = true
unified_exec = true
[features.multi_agent_v2]
enabled = true
[tools.update_plan]
enabled = true
"#;
        agent.harness_settings = Some(
            terrarium_core::harness_settings::parse(toml, terrarium_core::HarnessId::Codex)
                .expect("toml"),
        );
        let spec = build(&sandbox_opts(), &agent, None, None);
        let c = overrides(&spec);
        for want in [
            "agents.enabled=false",
            "features.multi_agent=false",
            "features.multi_agent_v2=false",
            "features.sleep_tool=false",
            "tools.update_plan.enabled=false",
        ] {
            assert_eq!(c.iter().filter(|o| **o == want).count(), 1, "{want}: {c:?}");
        }
        for gone in [
            "agents.enabled=true",
            "features.multi_agent=true",
            "features.multi_agent_v2.enabled",
            "features.sleep_tool=true",
            "unified_exec",
            "tools.update_plan.enabled=true",
        ] {
            assert!(!c.iter().any(|o| o.contains(gone)), "{gone}: {c:?}");
        }
        // A key next to a forced one passes.
        assert!(c.contains(&"agents.max_threads=4"), "{c:?}");
        // No key turns off the tools that stay.
        for keep in [
            "image_generation",
            "view_image",
            "apply_patch",
            "shell_tool",
        ] {
            assert!(
                !c.iter().any(|o| o.contains(keep) && o.ends_with("false")),
                "{keep}: {c:?}"
            );
        }
    }

    /// With no sandbox the overlays are not bound, so the SQLite files stay
    /// next to the shared `sessions/` in `CODEX_HOME`.
    #[test]
    fn no_sandbox_keeps_the_sqlite_home() {
        let spec = build(&CodexOptions::default(), &test_agent(None), None, None);
        assert!(
            !overrides(&spec)
                .iter()
                .any(|o| o.starts_with("sqlite_home"))
        );
    }

    #[test]
    fn hooks_cover_every_event_and_parse_as_toml() {
        let spec = build(&sandbox_opts(), &test_agent(None), None, None);
        let hooks: Vec<&str> = overrides(&spec)
            .into_iter()
            .filter(|o| o.starts_with("hooks."))
            .collect();
        assert_eq!(hooks.len(), 11);
        for (event, o) in crate::CODEX_PINNED.hook_events.iter().zip(&hooks) {
            // The key is dotted; the value is TOML. Parse both as a document.
            let doc: toml::Table = toml::from_str(o).expect("toml");
            let cmd = &doc["hooks"][*event][0]["hooks"][0];
            assert_eq!(cmd["type"].as_str(), Some("command"));
            assert_eq!(
                cmd["command"].as_str(),
                Some(format!("/opt/terrarium/bin/terrarium hook {event}").as_str())
            );
        }
        assert_eq!(
            hook_override("terrarium", "Stop"),
            r#"hooks.Stop=[{hooks=[{type="command",command="terrarium hook Stop"}]}]"#
        );
    }

    #[test]
    fn resume_puts_the_subcommand_first() {
        let id = HarnessSessionId("01a16cb2-0d95-7b40-894d-e19fc1b66c24".into());
        let spec = build(&sandbox_opts(), &test_agent(None), Some(&id), None);
        let a = args(&spec);
        assert_eq!(&a[..3], ["resume", id.0.as_str(), "--no-daemon"]);
        assert!(overrides(&spec).contains(&"tui.resume_cwd=\"session\""));
        assert!(!a.contains(&"-m"));
        assert!(
            !overrides(&spec)
                .iter()
                .any(|o| o.starts_with("model_instructions_file"))
        );
    }

    #[test]
    fn append_mode_quotes_the_text() {
        let opts = CodexOptions {
            prompt_mode: PromptMode::Append,
            ..sandbox_opts()
        };
        let text = "  You are \"a\".\nLine two\\ \u{7f}end  \n";
        let spec = build(&opts, &test_agent(None), None, Some(text));
        let o = overrides(&spec)
            .into_iter()
            .find(|o| o.starts_with("developer_instructions="))
            .expect("developer_instructions");
        let doc: toml::Table = toml::from_str(o).expect("toml");
        assert_eq!(
            doc["developer_instructions"].as_str(),
            Some("You are \"a\".\nLine two\\ \u{7f}end")
        );
        assert!(
            !overrides(&spec)
                .iter()
                .any(|o| o.starts_with("model_instructions_file"))
        );
    }

    /// An agent with its own base system prompt replaces Codex's prompt
    /// with `prompt.md`, whatever the option says (PLAN §8).
    #[test]
    fn a_base_system_prompt_replaces_the_codex_prompt() {
        let mut agent = test_agent(None);
        agent.system_prompt = Some("You are a reviewer.".into());
        for mode in [PromptMode::Append, PromptMode::Replace] {
            let opts = CodexOptions {
                prompt_mode: mode,
                ..sandbox_opts()
            };
            let text = "You are a reviewer.\n\n# Terrarium agent a";
            let spec = build(&opts, &agent, None, Some(text));
            let c = overrides(&spec);
            let want = format!(
                "model_instructions_file=\"{}\"",
                config::prompt_path(&agent).display()
            );
            assert!(c.contains(&want.as_str()), "{c:?}");
            assert!(!c.iter().any(|o| o.starts_with("developer_instructions")));
        }
        // An empty base prompt counts as none.
        agent.system_prompt = Some(" \n".into());
        let spec = build(&sandbox_opts(), &agent, None, Some("P"));
        assert!(overrides(&spec).contains(&"developer_instructions=\"P\""));
    }

    #[test]
    fn empty_prompt_adds_nothing() {
        let spec = build(&sandbox_opts(), &test_agent(None), None, Some(" \n"));
        assert!(!overrides(&spec).iter().any(
            |o| o.starts_with("model_instructions") || o.starts_with("developer_instructions")
        ));
    }

    #[test]
    fn trust_key_for_a_path_with_a_dot() {
        assert_eq!(
            trust_override(Path::new("/home/agent/work")),
            "projects./home/agent/work.trust_level=\"trusted\""
        );
        let o = trust_override(Path::new("/home/me/.local/state/w"));
        let doc: toml::Table = toml::from_str(&o).expect("toml");
        assert_eq!(
            doc["projects"]["/home/me/.local/state/w"]["trust_level"].as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn no_sandbox_uses_the_agent_workspace() {
        let agent = test_agent(None);
        let spec = build(&CodexOptions::default(), &agent, None, None);
        assert_eq!(spec.cwd, agent.workspace);
        assert_eq!(
            value_after(&spec, "-C"),
            Some(agent.workspace.to_string_lossy().as_ref())
        );
    }
}
