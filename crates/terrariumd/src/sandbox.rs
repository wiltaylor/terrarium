//! The sandbox around a harness (PLAN §11): wrap a `LaunchSpec` in `bwrap`.
//!
//! What the harness sees inside, for an agent `<ulid>`:
//!
//! | Path inside | From the host | Mode |
//! |---|---|---|
//! | `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin` | the same paths (a symlink stays a symlink) | ro |
//! | `/etc/{resolv.conf, hosts, ssl, passwd, group, localtime, alternatives, ...}` | the same paths, when they exist | ro |
//! | the harness install directory, when it is outside `/usr` | the same path | ro |
//! | `/proc`, `/dev`, `/tmp` | new, private | |
//! | `$HOME` (`/home/agent`) | a new tmpfs | rw |
//! | `$HOME/work` | `<state>/agents/<ulid>/workspace` | rw |
//! | `<state>/agents/<ulid>` | the same path | rw |
//! | `$HOME/.claude` (and the other harness homes) | the shared home, for example `/auth/claude` | rw |
//! | each `FileOverlay` target | the per-agent file | rw or ro |
//! | `/run/terrarium/agent.sock` | `<run>/agents/<ulid>.sock` | rw |
//! | `/opt/terrarium/bin/terrarium` | the `terrarium` CLI | ro |
//!
//! Everything else is hidden: other agents, the database, the daemon socket,
//! and the rest of the host. The network stays on. The child env starts
//! empty (see [`child_env`]).
//!
//! Mode `none` runs the harness with no sandbox. It is for development
//! outside Docker and for tests. It still cleans the env.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use terrarium_core::paths::{ENV_AGENT, ENV_AGENT_SOCKET};
use terrarium_core::{AgentId, FileOverlay, HarnessId, LaunchSpec};

/// The runner socket inside the sandbox.
pub const SANDBOX_SOCKET: &str = "/run/terrarium/agent.sock";
/// The directory of the `terrarium` CLI inside the sandbox.
pub const SANDBOX_BIN_DIR: &str = "/opt/terrarium/bin";
/// `$HOME` inside the sandbox.
pub const SANDBOX_HOME: &str = "/home/agent";
/// The `PATH` inside the sandbox, after [`SANDBOX_BIN_DIR`].
const SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The top-level system directories. A directory that is a symlink on the
/// host (merged `/usr`) becomes the same symlink inside.
pub const SYSTEM_DIRS: &[&str] = &["/usr", "/lib", "/lib64", "/bin", "/sbin"];

/// The part of `/etc` that a harness needs: name lookup, TLS roots, the
/// user database, the time zone, and the dynamic linker cache. Each one is
/// bound only when it exists.
pub const ETC_ENTRIES: &[&str] = &[
    "resolv.conf",
    "hosts",
    "nsswitch.conf",
    "host.conf",
    "gai.conf",
    "ssl",
    "ca-certificates",
    "pki",
    "passwd",
    "group",
    "localtime",
    "alternatives",
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
];

/// Inherited env vars that the child may keep. Everything else is dropped.
const INHERIT: &[&str] = &["LANG", "LC_ALL", "LC_CTYPE", "TZ", "USER", "LOGNAME"];

/// Env vars that never pass from the daemon to a harness, unless the
/// launch spec sets them itself. They carry other logins and other config.
pub const SCRUB_PREFIXES: &[&str] = &[
    "CLAUDE",
    "ANTHROPIC",
    "OPENAI",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "COPILOT",
    "PI_",
];

/// How a harness runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// In `bwrap` (the default).
    #[default]
    Bwrap,
    /// With no sandbox. For development and tests only.
    None,
}

impl fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bwrap => "bwrap",
            Self::None => "none",
        })
    }
}

impl FromStr for SandboxMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bwrap" => Ok(Self::Bwrap),
            "none" => Ok(Self::None),
            _ => Err(format!("unknown sandbox mode `{s}`; use `bwrap` or `none`")),
        }
    }
}

/// The sandbox settings of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    /// The `bwrap` program.
    pub bwrap: PathBuf,
    /// `$HOME` inside the sandbox.
    pub home: PathBuf,
    /// The `terrarium` CLI on the host. The sandbox binds it read-only; mode
    /// `none` puts its directory first on `PATH`.
    pub cli: Option<PathBuf>,
    /// More host paths to bind read-only at the same path.
    pub ro_paths: Vec<PathBuf>,
    /// Pass `--new-session`. Off by default: with it, the harness has no
    /// controlling terminal, and a PTY resize does not reach it (Claude Code
    /// did not redraw; `docs/compat.md`, "Claude inside bwrap"). The flag
    /// guards against `TIOCSTI` input injection into a terminal that the
    /// sandbox shares with its parent. Here each sandbox has its own PTY,
    /// so an injection can only reach the agent's own input.
    pub new_session: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Bwrap,
            bwrap: PathBuf::from("bwrap"),
            home: PathBuf::from(SANDBOX_HOME),
            cli: None,
            ro_paths: Vec::new(),
            new_session: false,
        }
    }
}

/// A shared harness home and where the harness sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessHome {
    pub harness: HarnessId,
    /// The directory on the host, for example `/auth/claude`.
    pub host: PathBuf,
}

/// The paths of one agent on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLayout {
    pub agent: AgentId,
    /// `<state>/agents/<ulid>`.
    pub agent_dir: PathBuf,
    /// `<state>/agents/<ulid>/workspace`.
    pub workspace: PathBuf,
    /// `<run>/agents/<ulid>.sock`.
    pub socket: PathBuf,
    pub homes: Vec<HarnessHome>,
    pub overlays: Vec<FileOverlay>,
}

/// The home of `harness` relative to `$HOME`, as the harness expects it.
pub fn home_rel(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Claude => ".claude",
        HarnessId::Codex => ".codex",
        HarnessId::Copilot => ".copilot",
        HarnessId::Pi => ".pi/agent",
    }
}

/// Find `program` on `path` (a `PATH` value). A program with a `/` is
/// returned as it is.
pub fn which(program: &str, path: Option<&str>) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    std::env::split_paths(path?)
        .map(|d| d.join(program))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn under_system_dir(p: &Path) -> bool {
    SYSTEM_DIRS.iter().any(|d| p.starts_with(d))
}

/// The symlink target of `p` on the host, if `p` is a symlink.
pub fn real_symlink(p: &Path) -> Option<PathBuf> {
    std::fs::symlink_metadata(p)
        .ok()
        .filter(|m| m.file_type().is_symlink())
        .and_then(|_| std::fs::read_link(p).ok())
}

impl SandboxConfig {
    /// Where the harness sees its home.
    pub fn home_inside(&self, home: &HarnessHome) -> PathBuf {
        match self.mode {
            SandboxMode::Bwrap => self.home.join(home_rel(home.harness)),
            SandboxMode::None => home.host.clone(),
        }
    }

    /// Where the harness sees the agent workspace (its cwd).
    pub fn workspace_inside(&self, layout: &AgentLayout) -> PathBuf {
        match self.mode {
            SandboxMode::Bwrap => self.home.join("work"),
            SandboxMode::None => layout.workspace.clone(),
        }
    }

    /// The `terrarium` program as the harness hooks call it.
    pub fn hook_program(&self) -> String {
        match (self.mode, &self.cli) {
            (SandboxMode::Bwrap, _) => format!("{SANDBOX_BIN_DIR}/terrarium"),
            (SandboxMode::None, Some(cli)) => cli.display().to_string(),
            (SandboxMode::None, None) => "terrarium".into(),
        }
    }

    /// Check that the sandbox can run, before any agent starts. In mode
    /// `bwrap`, run `bwrap --version`, then a tiny sandbox with the system
    /// binds. The error says what to fix.
    pub fn check(&self) -> Result<PathBuf, String> {
        if self.mode == SandboxMode::None {
            return Ok(PathBuf::new());
        }
        let path = std::env::var("PATH").ok();
        let bwrap = which(&self.bwrap.to_string_lossy(), path.as_deref()).ok_or_else(|| {
            format!(
                "the sandbox needs `{}`, but it is not on PATH; install bubblewrap, \
                 or start the daemon with --sandbox none (development only)",
                self.bwrap.display()
            )
        })?;
        let version = std::process::Command::new(&bwrap)
            .arg("--version")
            .output()
            .map_err(|e| format!("cannot run {}: {e}", bwrap.display()))?;
        if !version.status.success() {
            return Err(format!(
                "{} --version failed: {}",
                bwrap.display(),
                String::from_utf8_lossy(&version.stderr).trim()
            ));
        }
        let mut probe = vec!["--unshare-all".to_owned(), "--share-net".to_owned()];
        probe.extend(system_args(&real_symlink));
        probe.extend(["--proc", "/proc", "--dev", "/dev", "--", "true"].map(String::from));
        let out = std::process::Command::new(&bwrap)
            .args(&probe)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", bwrap.display()))?;
        if !out.status.success() {
            return Err(format!(
                "bwrap cannot make a sandbox here: {}. It needs user namespaces; in Docker, \
                 see docs/compat.md (\"bwrap inside Docker\") for the seccomp profile",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(bwrap)
    }

    /// Wrap `spec` for `layout`. `env` is the child env from [`child_env`].
    /// Mode `none` keeps the command and sets the env only.
    ///
    /// The result always has `clear_env`, so nothing leaks from the daemon.
    pub fn wrap(
        &self,
        spec: LaunchSpec,
        layout: &AgentLayout,
        env: BTreeMap<String, String>,
    ) -> LaunchSpec {
        self.wrap_with(spec, layout, env, &real_symlink, &|p| {
            which(p, std::env::var("PATH").ok().as_deref())
        })
    }

    /// [`Self::wrap`] with the file system probes given, for tests.
    pub fn wrap_with(
        &self,
        spec: LaunchSpec,
        layout: &AgentLayout,
        env: BTreeMap<String, String>,
        symlink: &dyn Fn(&Path) -> Option<PathBuf>,
        resolve: &dyn Fn(&str) -> Option<PathBuf>,
    ) -> LaunchSpec {
        if self.mode == SandboxMode::None {
            return LaunchSpec {
                env,
                clear_env: true,
                ..spec
            };
        }
        let (program, install_dir) = locate(&spec.program, resolve);
        let mut args = vec![
            "--unshare-all".to_owned(),
            "--share-net".to_owned(),
            "--die-with-parent".to_owned(),
        ];
        if self.new_session {
            args.push("--new-session".into());
        }
        args.extend(system_args(symlink));
        let mut ro: Vec<&Path> = self.ro_paths.iter().map(PathBuf::as_path).collect();
        if let Some(dir) = install_dir.as_deref() {
            ro.push(dir);
        }
        for p in ro {
            push3(&mut args, "--ro-bind", p, p);
        }
        args.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"].map(String::from));
        push2(&mut args, "--tmpfs", &self.home);
        push3(
            &mut args,
            "--bind",
            &layout.workspace,
            &self.home.join("work"),
        );
        push3(&mut args, "--bind", &layout.agent_dir, &layout.agent_dir);
        for h in &layout.homes {
            push3(&mut args, "--bind", &h.host, &self.home_inside(h));
        }
        for o in &layout.overlays {
            let flag = if o.writable { "--bind" } else { "--ro-bind" };
            push3(&mut args, flag, &o.source, &o.target);
        }
        push3(
            &mut args,
            "--bind",
            &layout.socket,
            Path::new(SANDBOX_SOCKET),
        );
        if let Some(cli) = &self.cli {
            push3(
                &mut args,
                "--ro-bind",
                cli,
                &Path::new(SANDBOX_BIN_DIR).join("terrarium"),
            );
        }
        push2(&mut args, "--chdir", &spec.cwd);
        args.push("--".into());
        args.push(program);
        args.extend(spec.args);
        LaunchSpec {
            program: self.bwrap.display().to_string(),
            args,
            env,
            cwd: layout.workspace.clone(),
            pty: spec.pty,
            clear_env: true,
        }
    }

    /// The env of the child, from a clean base. `inherited` reads the
    /// daemon's env. `spec_env` (the adapter's) goes on top.
    ///
    /// Base: `PATH`, `HOME`, `TERM=xterm-256color`, `LANG` (and a few
    /// locale and user names), the config-dir var of each harness home, and
    /// `TERRARIUM_SOCKET` and `TERRARIUM_AGENT`. Nothing with a
    /// [`SCRUB_PREFIXES`] prefix passes from `inherited`.
    pub fn child_env(
        &self,
        layout: &AgentLayout,
        spec_env: &BTreeMap<String, String>,
        inherited: &dyn Fn(&str) -> Option<String>,
    ) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for key in INHERIT {
            if let Some(v) = inherited(key).filter(|_| !scrubbed(key)) {
                env.insert((*key).to_owned(), v);
            }
        }
        env.entry("LANG".to_owned())
            .or_insert_with(|| "C.UTF-8".to_owned());
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert(ENV_AGENT.into(), layout.agent.to_string());
        match self.mode {
            SandboxMode::Bwrap => {
                env.insert("HOME".into(), self.home.display().to_string());
                env.insert("PATH".into(), format!("{SANDBOX_BIN_DIR}:{SANDBOX_PATH}"));
                env.insert(ENV_AGENT_SOCKET.into(), SANDBOX_SOCKET.into());
            }
            SandboxMode::None => {
                if let Some(home) = inherited("HOME") {
                    env.insert("HOME".into(), home);
                }
                let base = inherited("PATH").unwrap_or_else(|| SANDBOX_PATH.to_owned());
                let path = match self.cli.as_deref().and_then(Path::parent) {
                    Some(dir) => format!("{}:{base}", dir.display()),
                    None => base,
                };
                env.insert("PATH".into(), path);
                env.insert(ENV_AGENT_SOCKET.into(), layout.socket.display().to_string());
            }
        }
        for h in &layout.homes {
            env.insert(
                h.harness.home_env().to_owned(),
                self.home_inside(h).display().to_string(),
            );
        }
        for (k, v) in spec_env {
            env.insert(k.clone(), v.clone());
        }
        env
    }
}

/// True for a var that must not pass from the daemon env.
pub fn scrubbed(key: &str) -> bool {
    SCRUB_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// The program path inside the sandbox and the host directory to bind for
/// it. A program under the system dirs runs as it is. A program elsewhere
/// (for example `~/.local/bin/claude`, a symlink into
/// `~/.local/share/claude/versions/`) runs by its real path, and the
/// directory of that path is bound read-only.
fn locate(program: &str, resolve: &dyn Fn(&str) -> Option<PathBuf>) -> (String, Option<PathBuf>) {
    let Some(found) = resolve(program) else {
        return (program.to_owned(), None);
    };
    if under_system_dir(&found) {
        return (found.display().to_string(), None);
    }
    let real = std::fs::canonicalize(&found).unwrap_or(found);
    if under_system_dir(&real) {
        return (real.display().to_string(), None);
    }
    let dir = real.parent().map(Path::to_path_buf);
    (real.display().to_string(), dir)
}

fn system_args(symlink: &dyn Fn(&Path) -> Option<PathBuf>) -> Vec<String> {
    let mut args = Vec::new();
    for d in SYSTEM_DIRS {
        let p = Path::new(d);
        match symlink(p) {
            Some(target) => push3(&mut args, "--symlink", &target, p),
            None => push3(&mut args, "--ro-bind-try", p, p),
        }
    }
    for e in ETC_ENTRIES {
        let p = Path::new("/etc").join(e);
        push3(&mut args, "--ro-bind-try", &p, &p);
    }
    args
}

fn push2(args: &mut Vec<String>, flag: &str, a: &Path) {
    args.push(flag.to_owned());
    args.push(a.display().to_string());
}

fn push3(args: &mut Vec<String>, flag: &str, a: &Path, b: &Path) {
    args.push(flag.to_owned());
    args.push(a.display().to_string());
    args.push(b.display().to_string());
}

/// Quote one word for a POSIX shell.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-=:,+@%".contains(&b));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The spec as one shell command line: `cd <cwd> && env -i K=V ... prog args`.
/// For `terrariumd sandbox-dry-run` and the start log.
pub fn command_line(spec: &LaunchSpec) -> String {
    let mut words = vec![
        "cd".to_owned(),
        shell_quote(&spec.cwd.display().to_string()),
        "&&".to_owned(),
        "env".to_owned(),
    ];
    if spec.clear_env {
        words.push("-i".into());
    }
    for (k, v) in &spec.env {
        words.push(shell_quote(&format!("{k}={v}")));
    }
    words.push(shell_quote(&spec.program));
    words.extend(spec.args.iter().map(|a| shell_quote(a)));
    words.join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn layout() -> AgentLayout {
        let agent: AgentId = "01K8AGENT0000000000000000A".parse().unwrap();
        let dir = PathBuf::from("/state/agents/01K8AGENT0000000000000000A");
        AgentLayout {
            agent,
            agent_dir: dir.clone(),
            workspace: dir.join("workspace"),
            socket: PathBuf::from("/run/terrarium/agents/01K8AGENT0000000000000000A.sock"),
            homes: vec![HarnessHome {
                harness: HarnessId::Claude,
                host: PathBuf::from("/auth/claude"),
            }],
            overlays: vec![FileOverlay {
                source: dir.join("claude/claude.json"),
                target: PathBuf::from("/home/agent/.claude/.claude.json"),
                writable: true,
            }],
        }
    }

    fn config() -> SandboxConfig {
        SandboxConfig {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            cli: Some(PathBuf::from("/usr/local/bin/terrarium")),
            ..SandboxConfig::default()
        }
    }

    fn spec() -> LaunchSpec {
        LaunchSpec::new("claude", "/home/agent/work")
            .arg("--dangerously-skip-permissions")
            .arg("--model")
            .arg("haiku")
            .env("CLAUDE_CONFIG_DIR", "/home/agent/.claude")
            .env("DISABLE_TELEMETRY", "1")
    }

    /// The bwrap command for a sample agent, as in PLAN §11. A merged-`/usr`
    /// host has `/lib64` as a symlink; the probe says so.
    #[test]
    fn bwrap_argv_matches_the_golden_command() {
        let cfg = config();
        let l = layout();
        let host_env = |k: &str| match k {
            "LANG" => Some("en_US.UTF-8".to_owned()),
            "HOME" => Some("/home/wil".to_owned()),
            "PATH" => Some("/home/wil/bin:/usr/bin".to_owned()),
            "CLAUDE_CODE_OAUTH_TOKEN" | "ANTHROPIC_API_KEY" | "GH_TOKEN" => Some("secret".into()),
            _ => None,
        };
        let env = cfg.child_env(&l, &spec().env, &host_env);
        let symlink = |p: &Path| (p == Path::new("/lib64")).then(|| PathBuf::from("usr/lib"));
        let resolve = |p: &str| (p == "claude").then(|| PathBuf::from("/usr/bin/claude"));
        let wrapped = cfg.wrap_with(spec(), &l, env, &symlink, &resolve);
        let golden = "cd /state/agents/01K8AGENT0000000000000000A/workspace && env -i \
CLAUDE_CONFIG_DIR=/home/agent/.claude DISABLE_TELEMETRY=1 HOME=/home/agent LANG=en_US.UTF-8 \
PATH=/opt/terrarium/bin:/usr/local/bin:/usr/bin:/bin TERM=xterm-256color \
TERRARIUM_AGENT=01K8AGENT0000000000000000A TERRARIUM_SOCKET=/run/terrarium/agent.sock \
/usr/bin/bwrap --unshare-all --share-net --die-with-parent \
--ro-bind-try /usr /usr --ro-bind-try /lib /lib --symlink usr/lib /lib64 \
--ro-bind-try /bin /bin --ro-bind-try /sbin /sbin \
--ro-bind-try /etc/resolv.conf /etc/resolv.conf --ro-bind-try /etc/hosts /etc/hosts \
--ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf --ro-bind-try /etc/host.conf /etc/host.conf \
--ro-bind-try /etc/gai.conf /etc/gai.conf --ro-bind-try /etc/ssl /etc/ssl \
--ro-bind-try /etc/ca-certificates /etc/ca-certificates --ro-bind-try /etc/pki /etc/pki \
--ro-bind-try /etc/passwd /etc/passwd --ro-bind-try /etc/group /etc/group \
--ro-bind-try /etc/localtime /etc/localtime --ro-bind-try /etc/alternatives /etc/alternatives \
--ro-bind-try /etc/ld.so.cache /etc/ld.so.cache --ro-bind-try /etc/ld.so.conf /etc/ld.so.conf \
--ro-bind-try /etc/ld.so.conf.d /etc/ld.so.conf.d \
--proc /proc --dev /dev --tmpfs /tmp --tmpfs /home/agent \
--bind /state/agents/01K8AGENT0000000000000000A/workspace /home/agent/work \
--bind /state/agents/01K8AGENT0000000000000000A /state/agents/01K8AGENT0000000000000000A \
--bind /auth/claude /home/agent/.claude \
--bind /state/agents/01K8AGENT0000000000000000A/claude/claude.json /home/agent/.claude/.claude.json \
--bind /run/terrarium/agents/01K8AGENT0000000000000000A.sock /run/terrarium/agent.sock \
--ro-bind /usr/local/bin/terrarium /opt/terrarium/bin/terrarium \
--chdir /home/agent/work -- /usr/bin/claude --dangerously-skip-permissions --model haiku";
        assert_eq!(command_line(&wrapped), golden);

        let cfg = SandboxConfig {
            new_session: true,
            ..config()
        };
        let wrapped = cfg.wrap_with(spec(), &l, BTreeMap::new(), &symlink, &resolve);
        assert_eq!(wrapped.args[3], "--new-session");
    }

    #[test]
    fn env_is_clean_and_spec_wins() {
        let cfg = SandboxConfig {
            mode: SandboxMode::None,
            cli: Some(PathBuf::from("/opt/t/bin/terrarium")),
            ..SandboxConfig::default()
        };
        let l = layout();
        let host = |k: &str| match k {
            "PATH" => Some("/usr/bin".to_owned()),
            "HOME" => Some("/home/wil".to_owned()),
            "ANTHROPIC_API_KEY" | "OPENAI_API_KEY" | "COPILOT_X" | "PI_KEY" => Some("x".into()),
            _ => None,
        };
        let mut spec_env = BTreeMap::new();
        spec_env.insert("ANTHROPIC_MODEL".to_owned(), "set-by-spec".to_owned());
        let env = cfg.child_env(&l, &spec_env, &host);
        assert_eq!(env["PATH"], "/opt/t/bin:/usr/bin");
        assert_eq!(env["HOME"], "/home/wil");
        assert_eq!(env["CLAUDE_CONFIG_DIR"], "/auth/claude");
        assert_eq!(env["TERRARIUM_SOCKET"], l.socket.display().to_string());
        assert_eq!(env["ANTHROPIC_MODEL"], "set-by-spec");
        for k in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "COPILOT_X", "PI_KEY"] {
            assert!(!env.contains_key(k), "{k} leaked");
        }
        let wrapped = cfg.wrap(spec(), &l, env);
        assert_eq!(wrapped.program, "claude");
        assert!(wrapped.clear_env);
    }

    #[test]
    fn program_outside_usr_binds_its_real_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let versions = tmp.path().join("share/claude/versions");
        std::fs::create_dir_all(&versions).unwrap();
        let real = versions.join("2.1.310");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&real, bin.join("claude")).unwrap();
        let link = bin.join("claude");
        let (program, dir) = locate("claude", &|_| Some(link.clone()));
        let real = std::fs::canonicalize(&real).unwrap();
        assert_eq!(program, real.display().to_string());
        assert_eq!(dir.as_deref(), real.parent());
        let (program, dir) = locate("x", &|_| Some(PathBuf::from("/usr/bin/x")));
        assert_eq!((program.as_str(), dir), ("/usr/bin/x", None));
    }

    #[test]
    fn modes_parse() {
        assert_eq!("bwrap".parse::<SandboxMode>().unwrap(), SandboxMode::Bwrap);
        assert_eq!("none".parse::<SandboxMode>().unwrap(), SandboxMode::None);
        assert!("docker".parse::<SandboxMode>().is_err());
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
    }
}
