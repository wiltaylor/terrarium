//! Tool proxies (GitHub #21, the wiki page `design/tool-proxies`): a named
//! command that the operator defines, `tools/<name>.toml` in the config
//! repo or `POST /api/tools`. A granted agent runs it as a CLI in its
//! sandbox, and the daemon runs it on the target. The agent gets one tool,
//! not the target.
//!
//! This module has the definition and the checks that need no store
//! ([`ToolDef::check`]): the name, the argv, the interpreter rule, and the
//! limits. The daemon adds the checks against its store: the target and
//! its scope, the secrets, and the names of the sandbox `PATH`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The longest tool name.
pub const NAME_MAX: usize = 32;
/// The longest `timeout`.
pub const TIMEOUT_MAX: Duration = Duration::from_secs(24 * 3600);
/// The largest `max_output`.
pub const OUTPUT_MAX: usize = 64 * 1024 * 1024;
/// The most agent arguments that `args.max` may allow.
pub const ARGS_MAX: u32 = 256;
/// The longest agent argument that `args.max_len` may allow.
pub const ARG_LEN_MAX: u32 = 64 * 1024;

/// A tool definition. `deny_unknown_fields`: there is no `shell` field, and
/// a typo is an error, not a silent default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The target that runs the command.
    pub target: String,
    /// The program and its fixed arguments. The agent's arguments are
    /// appended as more argv entries. No shell parses any of it.
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// A duration such as `120s` or `5m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Env var name to secret name. The daemon sets them on the target side.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_secrets: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "ToolArgs::is_empty")]
    pub args: ToolArgs,
    /// `false` (default), `true` (each run asks the operator), or
    /// `{ first = [...] }` (a run whose first argument is in the list asks).
    #[serde(default, skip_serializing_if = "Approval::is_off")]
    pub approval: Approval,
    /// How long a run waits for the operator's approval, such as `1h`
    /// (default [`DEFAULT_APPROVAL_TIMEOUT`], at most 24h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_timeout: Option<String>,
    /// Pass the shim's stdin to the command.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stdin: bool,
    /// A size such as `1MiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<String>,
    /// Runs per agent: `<n>/s`, `<n>/m`, or `<n>/h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
    /// The owning project. A tool without one is global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// The rules for the agent's arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolArgs {
    /// The first argument must be one of these (subcommands). Empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// No argument may be one of these flags, or start with `<flag>=`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_flags: Vec<String>,
    /// The most agent arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    /// The longest agent argument in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_len: Option<u32>,
}

impl ToolArgs {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// When a run asks the operator first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Approval {
    Always(bool),
    First { first: Vec<String> },
}

impl Default for Approval {
    fn default() -> Self {
        Self::Always(false)
    }
}

impl Approval {
    pub fn is_off(&self) -> bool {
        self == &Self::Always(false)
    }

    /// Does a run with the agent arguments `argv` ask the operator first?
    pub fn asks(&self, argv: &[String]) -> bool {
        match self {
            Self::Always(on) => *on,
            Self::First { first } => argv.first().is_some_and(|a| first.contains(a)),
        }
    }
}

/// How long a run waits for the operator's approval when the tool sets no
/// `approval_timeout` (design Q5).
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(3600);

/// Programs that run code that their arguments give: a tool on one of them
/// needs a fixed script and `args.allow` (design D1).
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "mksh",
    "fish",
    "csh",
    "tcsh",
    "pwsh",
    "powershell",
    "cmd",
    "python",
    "python2",
    "python3",
    "pypy",
    "pypy3",
    "node",
    "nodejs",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "lua",
    "luajit",
    "tclsh",
    "wish",
    "rscript",
    "julia",
    "osascript",
    "awk",
    "gawk",
    "busybox",
];

/// Programs that start another program or a package: the next word is what
/// runs. `(program, subcommand that it needs before the target, if any)`.
const RUNNERS: &[(&str, Option<&str>)] = &[
    ("env", None),
    ("uv", Some("run")),
    ("uvx", None),
    ("pipx", Some("run")),
    ("poetry", Some("run")),
    ("npx", None),
    ("bunx", None),
    ("pnpx", None),
];

/// Flags of an interpreter that run code from the argv itself.
const INLINE_CODE: &[&str] = &[
    "-c",
    "-e",
    "--eval",
    "-r",
    "-Command",
    "-command",
    "-EncodedCommand",
    "-",
];

/// The lower-case program name of a path, without a version suffix
/// (`/usr/bin/python3.12` gives `python3`).
fn base(word: &str) -> String {
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name).to_owned();
    // python3.12 -> python3, node22 stays node22 (not in the list).
    match name.split_once('.') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) => head.to_owned(),
        _ => name,
    }
}

fn is_interpreter(word: &str) -> bool {
    INTERPRETERS.contains(&base(word).as_str())
}

/// The interpreter rule of design D1. `command` runs code that the agent's
/// arguments could choose when it is an interpreter (or a runner of one,
/// or of a script or package) with no fixed script, with inline code, or
/// with no `args.allow`.
pub fn interpreter_problem(command: &[String], allow: &[String]) -> Option<String> {
    let mut i = 0;
    loop {
        let word = command.get(i)?;
        let name = base(word);
        if let Some((_, sub)) = RUNNERS.iter().find(|(r, _)| *r == name) {
            i += 1;
            // Skip `VAR=value` (env), the flags, and the subcommand.
            let mut sub_seen = sub.is_none();
            while let Some(w) = command.get(i) {
                if w.starts_with('-') || (name == "env" && w.contains('=')) {
                    i += 1;
                } else if !sub_seen && Some(w.as_str()) == *sub {
                    sub_seen = true;
                    i += 1;
                } else {
                    break;
                }
            }
            let Some(next) = command.get(i) else {
                return Some(format!(
                    "`{word}` runs whatever the agent passes: name the program or script it runs"
                ));
            };
            if is_interpreter(next) {
                continue;
            }
            if name == "env" {
                // `env` with a plain program: check that program.
                continue;
            }
            // A runner of a script or a package acts as an interpreter with a script.
            return allow.is_empty().then(|| {
                format!("`{word}` runs a script or package: a tool on it needs `args.allow`")
            });
        }
        if !is_interpreter(word) {
            return None;
        }
        let rest = &command[i + 1..];
        if let Some(flag) = rest.iter().find(|w| INLINE_CODE.contains(&w.as_str())) {
            return Some(format!(
                "`{word} {flag}` runs code from its arguments: point the tool at a script"
            ));
        }
        if !rest.iter().any(|w| !w.starts_with('-')) {
            return Some(format!(
                "`{word}` with no script runs whatever the agent passes: point the tool at a script"
            ));
        }
        if allow.is_empty() {
            return Some(format!(
                "`{word}` is an interpreter: a tool on it needs `args.allow` (the subcommands the agent may run)"
            ));
        }
        return None;
    }
}

/// `30/m` as (30, one minute).
pub fn parse_rate(s: &str) -> Result<(u32, Duration), String> {
    let bad = || format!("bad rate `{s}`: use <n>/s, <n>/m, or <n>/h, such as 30/m");
    let (n, unit) = s.trim().split_once('/').ok_or_else(bad)?;
    let n: u32 = n.trim().parse().map_err(|_| bad())?;
    let per = match unit.trim() {
        "s" => Duration::from_secs(1),
        "m" => Duration::from_secs(60),
        "h" => Duration::from_secs(3600),
        _ => return Err(bad()),
    };
    if n == 0 {
        return Err(bad());
    }
    Ok((n, per))
}

/// The most agent arguments of a run when the tool sets no `args.max`.
pub const DEFAULT_ARGS_MAX: u32 = 64;
/// The longest agent argument when the tool sets no `args.max_len`.
pub const DEFAULT_ARG_LEN_MAX: u32 = 4096;
/// The output cap of a run when the tool sets no `max_output`.
pub const DEFAULT_OUTPUT_MAX: usize = 1024 * 1024;
/// The rate of a tool with no `rate`: runs per agent.
pub const DEFAULT_RATE: (u32, Duration) = (60, Duration::from_secs(60));

/// Check the agent's arguments of a run against `args` (design D4 and lead
/// review L1). Each argument is passed as its own argv entry with no
/// shell, so the rules only guard the flags that the script would read:
///
/// - at most `max` arguments (default [`DEFAULT_ARGS_MAX`]), each at most
///   `max_len` bytes (default [`DEFAULT_ARG_LEN_MAX`]), no NUL;
/// - with `allow`, the first argument is one of it (a run with no argument
///   is refused too);
/// - a deny flag `--name` refuses `--name`, `--name=...`, and each prefix
///   `--n`, `--na`, ... of it, because `getopt_long` takes an unambiguous
///   abbreviation; a single-dash form `-name` and `-name=...` too (the Go
///   style);
/// - a short deny flag `-o` refuses each argument that starts with one dash
///   and has `o` after it: `-o`, `-oVALUE`, and a bundle such as `-xo`.
///
/// Each argument is checked, also after a literal `--`. The error names the
/// argument and the rule.
pub fn check_args(args: &ToolArgs, argv: &[String]) -> Result<(), String> {
    let max = args.max.unwrap_or(DEFAULT_ARGS_MAX) as usize;
    let max_len = args.max_len.unwrap_or(DEFAULT_ARG_LEN_MAX) as usize;
    if argv.len() > max {
        return Err(format!(
            "{} arguments; this tool takes at most {max}",
            argv.len()
        ));
    }
    for (i, a) in argv.iter().enumerate() {
        if a.len() > max_len {
            return Err(format!(
                "argument {} has {} bytes; this tool takes at most {max_len}",
                i + 1,
                a.len()
            ));
        }
        if a.contains('\0') {
            return Err(format!("argument {} has a NUL byte", i + 1));
        }
    }
    if !args.allow.is_empty() {
        match argv.first() {
            None => {
                return Err(format!(
                    "this tool needs a first argument: one of {}",
                    args.allow.join(", ")
                ));
            }
            Some(first) if !args.allow.contains(first) => {
                return Err(format!(
                    "`{first}` is not allowed as the first argument: use one of {}",
                    args.allow.join(", ")
                ));
            }
            Some(_) => {}
        }
    }
    for a in argv {
        for flag in &args.deny_flags {
            if denied(flag, a) {
                return Err(format!(
                    "`{a}` is refused: this tool denies the flag {flag}"
                ));
            }
        }
    }
    Ok(())
}

/// Does deny flag `flag` refuse argument `a` ([`check_args`])?
fn denied(flag: &str, a: &str) -> bool {
    let name = flag.trim_start_matches('-');
    if name.is_empty() {
        return false;
    }
    let short = !flag.starts_with("--") && name.chars().count() == 1;
    if short {
        // One dash, not two, and the letter anywhere after it: a glued
        // value or a bundle.
        return a.starts_with('-') && !a.starts_with("--") && a[1..].contains(name);
    }
    if let Some(rest) = a.strip_prefix("--") {
        // `--name`, `--name=...`, or an abbreviation `--na`.
        let given = rest.split_once('=').map_or(rest, |(n, _)| n);
        return !given.is_empty() && name.starts_with(given);
    }
    if let Some(rest) = a.strip_prefix('-') {
        // The single-dash long form: `-name` or `-name=...`.
        let given = rest.split_once('=').map_or(rest, |(n, _)| n);
        return given == name;
    }
    false
}

/// A valid tool name: 1-32 of `a-z`, `0-9`, `-`, starting with a letter,
/// and not `terrarium`.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = name.len() <= NAME_MAX
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        return Err(format!(
            "bad tool name `{name}`: use 1-{NAME_MAX} of a-z, 0-9, and `-`, starting with a letter"
        ));
    }
    // `terrarium-*` stays the CLI's: the tool link dispatch skips it.
    if name == "terrarium" || name.starts_with("terrarium-") {
        return Err(format!(
            "a tool may not be named `{name}`: terrarium and terrarium-* are the CLI's"
        ));
    }
    Ok(())
}

/// A valid env var name for `env_secrets`, not one that changes how the
/// command is found or loaded.
pub fn check_env(name: &str) -> Result<(), String> {
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(format!(
            "bad env name `{name}` in env_secrets: use A-Z, 0-9, and `_`"
        ));
    }
    if name == "PATH"
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("TERRARIUM_")
    {
        return Err(format!("env_secrets may not set `{name}`"));
    }
    Ok(())
}

impl ToolDef {
    /// The checks that need no store (design D1).
    pub fn check(&self) -> Result<(), String> {
        check_name(&self.name)?;
        if self.target.trim().is_empty() {
            return Err("`target` is empty".into());
        }
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err("`command` must name a program: a non-empty argv such as [\"/usr/local/bin/tracker\"]".into());
        }
        if self.command.iter().any(|w| w.contains('\0')) {
            return Err("`command` has a NUL byte".into());
        }
        if let Some(p) = interpreter_problem(&self.command, &self.args.allow) {
            return Err(p);
        }
        if let Some(t) = &self.timeout {
            let d = crate::eta::parse_duration(t).map_err(|e| format!("timeout: {e}"))?;
            if u64::try_from(d.num_seconds()).unwrap_or(u64::MAX) > TIMEOUT_MAX.as_secs() {
                return Err(format!("timeout `{t}` is over 24h"));
            }
        }
        if let Some(t) = &self.approval_timeout {
            let d = crate::eta::parse_duration(t).map_err(|e| format!("approval_timeout: {e}"))?;
            if d.num_seconds() < 1
                || u64::try_from(d.num_seconds()).unwrap_or(u64::MAX) > TIMEOUT_MAX.as_secs()
            {
                return Err(format!("approval_timeout `{t}` must be 1s to 24h"));
            }
        }
        if let Some(m) = &self.max_output {
            let n = crate::skills::parse_size(m)
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("bad max_output `{m}`: use a size such as 1MiB or 512K"))?;
            if n > OUTPUT_MAX {
                return Err(format!("max_output `{m}` is over 64MiB"));
            }
        }
        if let Some(r) = &self.rate {
            parse_rate(r)?;
        }
        for (env, secret) in &self.env_secrets {
            check_env(env)?;
            if secret.trim().is_empty() {
                return Err(format!("env_secrets `{env}` names no secret"));
            }
        }
        let a = &self.args;
        if a.max.is_some_and(|m| m > ARGS_MAX) {
            return Err(format!("args.max is over {ARGS_MAX}"));
        }
        if a.max_len.is_some_and(|m| m == 0 || m > ARG_LEN_MAX) {
            return Err(format!("args.max_len must be 1 to {ARG_LEN_MAX}"));
        }
        if a.allow.iter().chain(&a.deny_flags).any(|w| w.is_empty()) {
            return Err("args.allow and args.deny_flags may not have an empty entry".into());
        }
        if let Some(f) = a.deny_flags.iter().find(|f| !f.starts_with('-')) {
            return Err(format!(
                "args.deny_flags entry `{f}` is not a flag (it must start with `-`)"
            ));
        }
        if let Approval::First { first } = &self.approval
            && first.is_empty()
        {
            return Err(
                "approval.first is empty: use approval = true or a list of first arguments".into(),
            );
        }
        Ok(())
    }

    /// `None` when the tool is global.
    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    /// The approval timeout of a run ([`DEFAULT_APPROVAL_TIMEOUT`] when the
    /// tool sets none; [`Self::check`] has checked the value).
    pub fn approval_wait(&self) -> Duration {
        self.approval_timeout
            .as_deref()
            .and_then(|t| crate::eta::parse_duration(t).ok())
            .and_then(|d| u64::try_from(d.num_seconds()).ok())
            .map_or(DEFAULT_APPROVAL_TIMEOUT, Duration::from_secs)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tool(toml_text: &str) -> ToolDef {
        let mut d: ToolDef = toml::from_str(toml_text).unwrap();
        if d.name.is_empty() {
            d.name = "tracker-read".into();
        }
        d
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    const BASE: &str = "target = \"wsl\"\ncommand = [\"/home/u/bin/tracker-read\"]\n";

    #[test]
    fn a_plain_definition_parses_and_checks() {
        let d = tool(
            "name = \"tracker-read\"\ntarget = \"wsl\"\ncommand = [\"/home/u/.local/bin/uv\", \"run\", \"/home/u/scripts/tracker.py\"]\n\
             cwd = \"/home/u/scripts\"\ntimeout = \"120s\"\nenv_secrets = { TRACKER_TOKEN = \"tracker-token\" }\n\
             args = { allow = [\"show\", \"search\"], deny_flags = [\"--config\"], max = 16, max_len = 4096 }\n\
             approval = { first = [\"comment\"] }\nmax_output = \"1MiB\"\nrate = \"30/m\"\n",
        );
        d.check().unwrap();
        assert_eq!(
            d.approval,
            Approval::First {
                first: vec!["comment".into()]
            }
        );
        assert_eq!(tool(BASE).approval, Approval::Always(false));
        assert_eq!(
            tool(&format!("{BASE}approval = true\n")).approval,
            Approval::Always(true)
        );
    }

    #[test]
    fn there_is_no_shell_field_and_no_unknown_field() {
        for extra in [
            "shell = true\n",
            "sheel = 1\n",
            "args = { allow = [\"a\"], extra = 1 }\n",
        ] {
            let r: Result<ToolDef, _> = toml::from_str(&format!("{BASE}{extra}"));
            assert!(r.is_err(), "{extra}");
        }
    }

    #[test]
    fn the_name_rules() {
        for good in ["t", "tracker-read", "a1-b2", &"a".repeat(NAME_MAX)] {
            check_name(good).unwrap();
        }
        for bad in [
            "",
            "Tracker",
            "1tool",
            "-x",
            "a_b",
            "a.b",
            "a/b",
            &"a".repeat(NAME_MAX + 1),
            "terrarium",
            "terrarium-x",
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_command_must_be_a_program() {
        let mut d = tool(BASE);
        d.command = vec![];
        assert!(d.check().unwrap_err().contains("non-empty argv"));
        d.command = argv(&[" "]);
        assert!(d.check().is_err());
        d.command = argv(&["/bin/x", "a\0b"]);
        assert!(d.check().unwrap_err().contains("NUL"));
    }

    #[test]
    fn the_interpreter_rule_refuses_a_shell_in_disguise() {
        let allow = vec!["show".to_owned()];
        // Refused.
        for (cmd, allow, says) in [
            (argv(&["bash"]), allow.clone(), "no script"),
            (argv(&["/bin/sh", "-c", "tracker"]), allow.clone(), "-c"),
            (argv(&["python3", "-c", "print(1)"]), allow.clone(), "-c"),
            (
                argv(&["/usr/bin/python3.12", "/s/t.py"]),
                vec![],
                "args.allow",
            ),
            (argv(&["node", "-e", "x"]), allow.clone(), "-e"),
            (argv(&["pwsh", "-Command", "x"]), allow.clone(), "-Command"),
            (argv(&["env", "FOO=1", "bash"]), allow.clone(), "no script"),
            (argv(&["uv", "run"]), allow.clone(), "runs whatever"),
            (argv(&["uv", "run", "python"]), allow.clone(), "no script"),
            (argv(&["uv", "run", "/s/t.py"]), vec![], "args.allow"),
            (argv(&["npx", "some-pkg"]), vec![], "args.allow"),
            (argv(&["env"]), allow.clone(), "runs whatever"),
        ] {
            let p = interpreter_problem(&cmd, &allow).unwrap_or_else(|| panic!("{cmd:?} passed"));
            assert!(p.contains(says), "{cmd:?}: {p}");
        }
        // Accepted.
        for (cmd, allow) in [
            (argv(&["/home/u/bin/tracker"]), vec![]),
            (argv(&["python3", "/s/t.py"]), allow.clone()),
            (argv(&["python3", "-u", "/s/t.py"]), allow.clone()),
            (
                argv(&["/home/u/.local/bin/uv", "run", "/s/t.py"]),
                allow.clone(),
            ),
            (
                argv(&["uv", "run", "--quiet", "python", "/s/t.py"]),
                allow.clone(),
            ),
            (argv(&["env", "LANG=C", "/usr/bin/jq"]), vec![]),
            (argv(&["/usr/bin/gh"]), vec![]),
        ] {
            assert_eq!(interpreter_problem(&cmd, &allow), None, "{cmd:?}");
        }
    }

    #[test]
    fn the_limits_and_the_env_names() {
        let check = |extra: &str| tool(&format!("{BASE}{extra}")).check();
        check("timeout = \"5m\"\nmax_output = \"512K\"\nrate = \"10/s\"\n").unwrap();
        for (extra, says) in [
            ("timeout = \"soon\"\n", "timeout"),
            ("timeout = \"25h\"\n", "over 24h"),
            ("max_output = \"lots\"\n", "max_output"),
            ("max_output = \"65MiB\"\n", "over 64MiB"),
            ("rate = \"30/d\"\n", "bad rate"),
            ("rate = \"0/m\"\n", "bad rate"),
            ("env_secrets = { token = \"t\" }\n", "bad env name"),
            ("env_secrets = { PATH = \"t\" }\n", "may not set"),
            ("env_secrets = { LD_PRELOAD = \"t\" }\n", "may not set"),
            ("env_secrets = { TERRARIUM_X = \"t\" }\n", "may not set"),
            ("env_secrets = { TOKEN = \" \" }\n", "names no secret"),
            ("args = { max = 1000 }\n", "args.max"),
            ("args = { max_len = 0 }\n", "max_len"),
            ("args = { deny_flags = [\"config\"] }\n", "not a flag"),
            ("args = { allow = [\"\"] }\n", "empty entry"),
            ("approval = { first = [] }\n", "approval.first"),
        ] {
            let e = check(extra).unwrap_err();
            assert!(e.contains(says), "{extra}: {e}");
        }
        assert_eq!(parse_rate("30/m").unwrap(), (30, Duration::from_secs(60)));
    }

    fn targs(allow: &[&str], deny: &[&str]) -> ToolArgs {
        ToolArgs {
            allow: allow.iter().map(|s| (*s).to_owned()).collect(),
            deny_flags: deny.iter().map(|s| (*s).to_owned()).collect(),
            max: None,
            max_len: None,
        }
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    /// TER-30 (design D4, lead review L1): the argument rules of a run.
    #[test]
    fn the_argument_rules_refuse_deny_flags_in_each_form() {
        let a = targs(&["show", "search"], &["--config", "-o"]);
        assert!(check_args(&a, &v(&["show", "T-1"])).is_ok());
        assert!(check_args(&a, &v(&["search", "--limit", "5", "x y; rm -rf /"])).is_ok());
        // allow: the first argument, and no empty run.
        assert!(
            check_args(&a, &v(&["delete", "T-1"]))
                .unwrap_err()
                .contains("not allowed")
        );
        assert!(
            check_args(&a, &[])
                .unwrap_err()
                .contains("needs a first argument")
        );
        // A long deny flag: exact, with a value, abbreviated, single dash.
        for bad in [
            "--config",
            "--config=/etc/x",
            "--conf",
            "--c",
            "--co=x",
            "-config",
            "-config=x",
        ] {
            let e = check_args(&a, &v(&["show", bad])).unwrap_err();
            assert!(e.contains("--config"), "{bad}: {e}");
        }
        // Not a prefix of the deny flag: allowed.
        for ok in ["--configure", "--count", "--", "-c", "--limit=2"] {
            assert!(check_args(&a, &v(&["show", ok])).is_ok(), "{ok}");
        }
        // A short deny flag: alone, glued, in a bundle; not in a long flag.
        for bad in ["-o", "-o/etc/passwd", "-xo", "-vxo", "-ofile"] {
            assert!(check_args(&a, &v(&["show", bad])).is_err(), "{bad}");
        }
        for ok in ["-x", "--output-format", "foo", "-v"] {
            assert!(check_args(&a, &v(&["show", ok])).is_ok(), "{ok}");
        }
        // A literal `--` does not end the checks.
        assert!(check_args(&a, &v(&["show", "--", "--config=x"])).is_err());
        assert!(check_args(&a, &v(&["show", "--", "-o"])).is_err());
    }

    #[test]
    fn the_argument_limits_have_defaults() {
        let a = targs(&[], &[]);
        assert!(check_args(&a, &[]).is_ok());
        let many = vec!["x".to_owned(); DEFAULT_ARGS_MAX as usize + 1];
        assert!(check_args(&a, &many).unwrap_err().contains("at most 64"));
        let long = vec!["x".repeat(DEFAULT_ARG_LEN_MAX as usize + 1)];
        assert!(check_args(&a, &long).unwrap_err().contains("at most 4096"));
        assert!(check_args(&a, &v(&["a\0b"])).unwrap_err().contains("NUL"));
        let mut b = targs(&[], &[]);
        b.max = Some(2);
        b.max_len = Some(3);
        assert!(check_args(&b, &v(&["abc", "de"])).is_ok());
        assert!(check_args(&b, &v(&["a", "b", "c"])).is_err());
        assert!(check_args(&b, &v(&["abcd"])).is_err());
    }

    /// TER-31: when a run asks the operator, and the approval timeout.
    #[test]
    fn approval_rules_and_timeout() {
        assert!(!Approval::Always(false).asks(&v(&["show"])));
        assert!(Approval::Always(true).asks(&[]));
        let first = Approval::First {
            first: v(&["comment", "close"]),
        };
        assert!(first.asks(&v(&["comment", "T-1"])));
        assert!(!first.asks(&v(&["show", "comment"])));
        assert!(!first.asks(&[]));
        let mut t = tool("name = \"x\"\ntarget = \"box\"\ncommand = [\"/bin/x\"]\n");
        assert_eq!(t.approval_wait(), DEFAULT_APPROVAL_TIMEOUT);
        t.approval_timeout = Some("90s".into());
        assert!(t.check().is_ok());
        assert_eq!(t.approval_wait(), Duration::from_secs(90));
        for bad in ["0s", "25h", "soon"] {
            t.approval_timeout = Some(bad.into());
            assert!(t.check().is_err(), "{bad}");
        }
    }
}
