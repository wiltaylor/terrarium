//! The sandbox around a harness (PLAN §11): wrap a `LaunchSpec` in `bwrap`.
//!
//! What the harness sees inside, for an agent `<ulid>`:
//!
//! | Path inside | From the host | Mode |
//! |---|---|---|
//! | `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin` | the same paths (a symlink stays a symlink) | ro |
//! | `/etc/{resolv.conf, hosts, ssl, passwd, group, localtime, alternatives, ...}` | the same paths, when they exist | ro |
//! | the tools dir (`TERRARIUM_TOOLS_DIR`, `/opt/terrarium/tools`): the harnesses, their `bin/` on `PATH`. The harness runs from the `bin/` of the version dir that was active at the launch (`AgentLayout::harness_bin`), and that dir comes first on `PATH`, so a later switch does not change a running agent | the same path | ro |
//! | the harness install directory, when it is outside `/usr` and the tools dir | the same path | ro |
//! | `/proc`, `/dev`, `/tmp` | new, private | |
//! | `$HOME` (`/home/agent`) | a new tmpfs | rw |
//! | `$HOME/work` | `<work>/<project>/<agent>` (`TERRARIUM_WORK_DIR`) | rw |
//! | `/work/<project>/<agent>` | the same workspace again, at the path a target sees it at | rw |
//! | `<state>/agents/<ulid>/harness` | the same path (`prompt.md`, the adapter's per-agent files) | ro |
//! | `$HOME/.claude` (and the other harness homes) | the shared home, for example `/auth/claude` | rw |
//! | each `FileOverlay` target | the per-agent file (also a part of `harness/` that the harness writes, such as pi's `sessions/`). The session files of Claude Code (`projects/<cwd>/`), Codex (`sessions/`), and Copilot (`session-state/`) are such overlays, so no agent sees the transcripts of another agent | rw or ro |
//! | `/run/terrarium/agent.sock` | `<run>/agents/<ulid>.sock` | rw |
//! | `/opt/terrarium/bin/terrarium` | the `terrarium` CLI | ro |
//! | each local repo of the agent's project under `TERRARIUM_REPOS_DIR` (`/repos`) | the same path | rw |
//! | the account store of each pool member with a login of the harness (Claude, pi; PLAN §16.3) | the same path, `<state>/credentials/accounts/<subscription>/store` | rw |
//! | `/run/terrarium/cred` (pi on an account) | `<state>/agents/<ulid>/harness/cred`: the agent's account pointer | ro |
//!
//! Everything else is hidden: other agents, the database, the daemon socket,
//! the rest of the host, and `<state>/agents/<ulid>/private`, which holds
//! the runner's records (`events.jsonl`, `pty.log`, `runner.json` with the
//! injected keys, `runner-exit.json`). The agent cannot read or change
//! them (`docs/security-review.md`, F1). The network stays on. The child env starts
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
/// The default of `TERRARIUM_REPOS_DIR`: the local repos that the sandbox
/// binds read-write into the agents of their project ([`local_repo_binds`]).
pub const DEFAULT_REPOS_DIR: &str = "/repos";
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
    /// The tools dir (`TERRARIUM_TOOLS_DIR`), absolute: the npm prefix of
    /// the harnesses. The sandbox binds it read-only at the same path and
    /// puts its `bin/` on `PATH`, after [`SANDBOX_BIN_DIR`].
    pub tools_dir: Option<PathBuf>,
    /// Pass `--new-session`. Off by default: with it, the harness has no
    /// controlling terminal, and a PTY resize does not reach it (Claude Code
    /// did not redraw; `docs/compat.md`, "Claude inside bwrap"). The flag
    /// guards against `TIOCSTI` input injection into a terminal that the
    /// sandbox shares with its parent. Here each sandbox has its own PTY,
    /// so an injection can only reach the agent's own input.
    pub new_session: bool,
    /// `TERRARIUM_REPOS_DIR`. A project repo whose URL is a local path
    /// under this dir is bound read-write, at the same path, into each agent
    /// of that project, so `git push` works with no credential. Nothing
    /// outside this dir is bound this way.
    pub repos_dir: PathBuf,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Bwrap,
            bwrap: PathBuf::from("bwrap"),
            home: PathBuf::from(SANDBOX_HOME),
            cli: None,
            ro_paths: Vec::new(),
            tools_dir: None,
            new_session: false,
            repos_dir: PathBuf::from(DEFAULT_REPOS_DIR),
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
    /// `<state>/agents/<ulid>`. Not bound.
    pub agent_dir: PathBuf,
    /// `<state>/agents/<ulid>/harness`: bound read-only at the same path.
    pub harness_dir: PathBuf,
    /// `<state>/agents/<ulid>/private`: the runner's records. Never bound.
    pub private_dir: PathBuf,
    /// `<work>/<project>/<agent>` (`crate::workspace`).
    pub workspace: PathBuf,
    /// The second place the sandbox shows the workspace:
    /// `/work/<project>/<agent>`, the path a target sees it at, so a path in
    /// a compiler message on the target is a path in the sandbox too.
    pub workspace_mount: Option<PathBuf>,
    /// `<run>/agents/<ulid>.sock`.
    pub socket: PathBuf,
    pub homes: Vec<HarnessHome>,
    pub overlays: Vec<FileOverlay>,
    /// The local repos of the agent's project, bound read-write
    /// ([`local_repo_binds`]).
    pub repos: Vec<RepoBind>,
    /// The account stores of the agent's pool (`crate::accounts`), each
    /// bound read-write at its own path, so the agent's pointer resolves
    /// inside. Only the pool's accounts: never another one.
    pub accounts: Vec<PathBuf>,
    /// The agent's pointer dir (`<harness dir>/cred`), bound read-only at
    /// [`crate::accounts::SANDBOX_CRED`] too: pi's shared `auth.json` link
    /// goes through it.
    pub cred_alias: Option<PathBuf>,
    /// The `bin/` of the harness version dir that this launch runs
    /// (`<tools>/versions/<name>/<version>/bin`,
    /// `crate::harness_tools::active_version`). The program comes from it,
    /// and it is first on `PATH` after the CLI dir, so a harness that runs
    /// itself again through `PATH` gets the same version. `None`: the
    /// program comes from `PATH`.
    pub harness_bin: Option<PathBuf>,
    /// The agent's ssh-agent socket dir
    /// (`<runtime>/ssh-agent/<ulid>`, `crate::secrets::agent_keys`): bound
    /// at `/run/terrarium/ssh-agent`, with `SSH_AUTH_SOCK` on its socket.
    /// `None`: the agent has no `ssh_key` grant.
    pub ssh_agent: Option<PathBuf>,
    /// Start the program through `terrarium exec-env` (the CLI that the
    /// sandbox binds): it gets the env vars of the secret grants from the
    /// daemon inside the sandbox, so no value is in the launch spec or in
    /// the env of `bwrap` (PLAN §16b.4). Needs [`SandboxConfig::cli`].
    pub exec_env: bool,
    /// `exec-env` also sets the harness credentials (the subscription's
    /// API key, the Copilot account token), so it runs with
    /// `--require-credentials`: when the daemon does not answer, it fails
    /// and does not start the harness with no credentials.
    pub exec_env_credentials: bool,
}

/// One local repo bind: the real directory on the host, and the path of
/// the repo URL, which is where the agent sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoBind {
    pub source: PathBuf,
    pub target: PathBuf,
}

/// The path of a local repo URL (`file:///repos/x.git` or `/repos/x.git`)
/// when it is under `repos_dir`, else `None`. The check is lexical: a path
/// with `.` or `..`, a `file://host/` URL, or `repos_dir` itself is `None`.
pub fn local_repo_path(url: &str, repos_dir: &Path) -> Option<PathBuf> {
    let raw = match url.strip_prefix("file://") {
        Some(rest) if rest.starts_with('/') => rest,
        Some(_) => return None,
        None if url.starts_with('/') => url,
        None => return None,
    };
    let path = Path::new(raw);
    let plain = !raw.split('/').any(|seg| seg == "." || seg == "..");
    if !plain || !repos_dir.is_absolute() || path == repos_dir || !path.starts_with(repos_dir) {
        return None;
    }
    Some(path.to_path_buf())
}

/// The binds for the repo URLs of one project. A URL makes a bind when its
/// path is under `repos_dir` ([`local_repo_path`]), and its real path (after
/// symlinks) is a directory that is still under the real `repos_dir`. Each
/// target is bound once.
pub fn local_repo_binds<'a>(
    urls: impl IntoIterator<Item = &'a str>,
    repos_dir: &Path,
) -> Vec<RepoBind> {
    let Ok(real_dir) = std::fs::canonicalize(repos_dir) else {
        return Vec::new();
    };
    let mut out: Vec<RepoBind> = Vec::new();
    for url in urls {
        let Some(target) = local_repo_path(url, repos_dir) else {
            continue;
        };
        let Ok(source) = std::fs::canonicalize(&target) else {
            continue;
        };
        if source == real_dir || !source.starts_with(&real_dir) || !source.is_dir() {
            continue;
        }
        if !out.iter().any(|b| b.target == target) {
            out.push(RepoBind { source, target });
        }
    }
    out
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
        let pinned = layout.harness_bin.clone();
        self.wrap_with(spec, layout, env, &real_symlink, &|p| {
            pinned_program(pinned.as_deref(), p, &is_executable)
                .or_else(|| which(p, std::env::var("PATH").ok().as_deref()))
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
            // The pinned version runs by its path; any other program stays
            // as it is, for `PATH`.
            let program = layout
                .harness_bin
                .as_deref()
                .and_then(|b| resolve(&spec.program).filter(|f| f.starts_with(b)))
                .map_or(spec.program.clone(), |f| f.display().to_string());
            let (program, args) = match self.cli.as_deref().filter(|_| layout.exec_env) {
                Some(cli) => {
                    let mut a = exec_env_args(program, layout.exec_env_credentials);
                    a.extend(spec.args.iter().cloned());
                    (cli.display().to_string(), a)
                }
                None => (program, spec.args.clone()),
            };
            return LaunchSpec {
                program,
                args,
                env,
                clear_env: true,
                ..spec
            };
        }
        let (program, install_dir) = locate(&spec.program, resolve, self.tools_dir.as_deref());
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
        if let Some(dir) = self.tools_dir.as_deref() {
            ro.push(dir);
        }
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
        if let Some(m) = &layout.workspace_mount {
            push3(&mut args, "--bind", &layout.workspace, m);
        }
        // The harness files, read-only. `private/` next to it stays out.
        push3(
            &mut args,
            "--ro-bind",
            &layout.harness_dir,
            &layout.harness_dir,
        );
        for h in &layout.homes {
            push3(&mut args, "--bind", &h.host, &self.home_inside(h));
        }
        for o in &layout.overlays {
            let flag = if o.writable { "--bind" } else { "--ro-bind" };
            push3(&mut args, flag, &o.source, &o.target);
        }
        for r in &layout.repos {
            push3(&mut args, "--bind", &r.source, &r.target);
        }
        for a in &layout.accounts {
            push3(&mut args, "--bind", a, a);
        }
        if let Some(c) = &layout.cred_alias {
            push3(
                &mut args,
                "--ro-bind",
                c,
                Path::new(crate::accounts::SANDBOX_CRED),
            );
        }
        push3(
            &mut args,
            "--bind",
            &layout.socket,
            Path::new(SANDBOX_SOCKET),
        );
        if let Some(dir) = &layout.ssh_agent {
            use crate::secrets::agent_keys::{SANDBOX_DIR, SANDBOX_SOCK, SOCK_NAME};
            // The dir, not the socket file: a new daemon makes a new
            // socket file there, and a running agent sees it.
            // Read-only: connect() works on a socket in a read-only bind,
            // and the agent cannot put a file or a symlink next to it.
            push3(&mut args, "--ro-bind", dir, Path::new(SANDBOX_DIR));
            push3(
                &mut args,
                "--symlink",
                &Path::new(SANDBOX_DIR).join(SOCK_NAME),
                Path::new(SANDBOX_SOCK),
            );
        }
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
        if self.cli.is_some() && layout.exec_env {
            let cli = Path::new(SANDBOX_BIN_DIR).join("terrarium");
            args.push(cli.display().to_string());
            args.extend(exec_env_args(program, layout.exec_env_credentials));
        } else {
            args.push(program);
        }
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
                let mut parts = vec![SANDBOX_BIN_DIR.to_owned()];
                parts.extend(layout.harness_bin.iter().map(|b| b.display().to_string()));
                parts.extend(
                    self.tools_dir
                        .iter()
                        .map(|t| t.join("bin").display().to_string()),
                );
                parts.push(SANDBOX_PATH.to_owned());
                env.insert("PATH".into(), parts.join(":"));
                env.insert(ENV_AGENT_SOCKET.into(), SANDBOX_SOCKET.into());
            }
            SandboxMode::None => {
                if let Some(home) = inherited("HOME") {
                    env.insert("HOME".into(), home);
                }
                let mut path = inherited("PATH").unwrap_or_else(|| SANDBOX_PATH.to_owned());
                if let Some(b) = &layout.harness_bin {
                    path = crate::harness_tools::prepend_path(b, &path);
                }
                if let Some(dir) = self.cli.as_deref().and_then(Path::parent) {
                    path = format!("{}:{path}", dir.display());
                }
                env.insert("PATH".into(), path);
                env.insert(ENV_AGENT_SOCKET.into(), layout.socket.display().to_string());
            }
        }
        if let Some(dir) = &layout.ssh_agent {
            let sock = match self.mode {
                SandboxMode::Bwrap => crate::secrets::agent_keys::SANDBOX_SOCK.to_owned(),
                SandboxMode::None => dir
                    .join(crate::secrets::agent_keys::SOCK_NAME)
                    .display()
                    .to_string(),
            };
            env.insert("SSH_AUTH_SOCK".into(), sock);
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

/// `exec-env [--require-credentials] -- <program>`: the args of
/// `terrarium` that start `program` with the env vars of the secret grants
/// and the harness credentials ([`AgentLayout::exec_env`],
/// [`AgentLayout::exec_env_credentials`]).
fn exec_env_args(program: String, credentials: bool) -> Vec<String> {
    let mut a = vec!["exec-env".to_owned()];
    if credentials {
        a.push("--require-credentials".to_owned());
    }
    a.extend(["--".to_owned(), program]);
    a
}

/// True for a var that must not pass from the daemon env.
pub fn scrubbed(key: &str) -> bool {
    SCRUB_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// `<bin>/<program>` when `bin` is given, `program` is a bare name, and
/// `exists` says the file is there.
fn pinned_program(
    bin: Option<&Path>,
    program: &str,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let found = bin?.join(program);
    (!program.contains('/') && exists(&found)).then_some(found)
}

/// The program path inside the sandbox and the host directory to bind for
/// it. A program under the system dirs or the tools dir (bound whole) runs
/// as it is. A program elsewhere
/// (for example `~/.local/bin/claude`, a symlink into
/// `~/.local/share/claude/versions/`) runs by its real path, and the
/// directory of that path is bound read-only.
fn locate(
    program: &str,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
    tools: Option<&Path>,
) -> (String, Option<PathBuf>) {
    let Some(found) = resolve(program) else {
        return (program.to_owned(), None);
    };
    if under_system_dir(&found) || tools.is_some_and(|t| found.starts_with(t)) {
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
    let mut args = system_dir_args(symlink);
    for e in ETC_ENTRIES {
        let p = Path::new("/etc").join(e);
        push3(&mut args, "--ro-bind-try", &p, &p);
    }
    args
}

/// The binds of [`SYSTEM_DIRS`] only (no `/etc`): read-only, and a
/// directory that is a symlink on the host is the same symlink inside.
/// The read sandbox of the repo browser (`crate::git::ReadGit`) uses them.
pub fn system_dir_args(symlink: &dyn Fn(&Path) -> Option<PathBuf>) -> Vec<String> {
    let mut args = Vec::new();
    for d in SYSTEM_DIRS {
        let p = Path::new(d);
        match symlink(p) {
            Some(target) => push3(&mut args, "--symlink", &target, p),
            None => push3(&mut args, "--ro-bind-try", p, p),
        }
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

/// True for an env var whose value is a secret: an API key, a token, a
/// password. [`command_line`] hides its value.
pub fn secret_var(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD"]
        .iter()
        .any(|w| k.contains(w))
}

/// The spec as one shell command line: `cd <cwd> && env -i K=V ... prog args`.
/// For `terrariumd sandbox-dry-run` and the start log. The value of each
/// [`secret_var`] shows as `<redacted>`.
pub fn command_line(spec: &LaunchSpec) -> String {
    command_line_hiding(spec, &[])
}

/// [`command_line`], and the value of each var in `hidden` (the env vars
/// of the secret grants) shows as `<redacted>` too.
pub fn command_line_hiding(spec: &LaunchSpec, hidden: &[String]) -> String {
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
        let v = if secret_var(k) || hidden.iter().any(|h| h == k) {
            "<redacted>"
        } else {
            v
        };
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
            harness_dir: dir.join("harness"),
            private_dir: dir.join("private"),
            workspace: PathBuf::from("/work/terra/a1"),
            workspace_mount: Some(PathBuf::from("/work/terra/a1")),
            socket: PathBuf::from("/run/terrarium/agents/01K8AGENT0000000000000000A.sock"),
            homes: vec![HarnessHome {
                harness: HarnessId::Claude,
                host: PathBuf::from("/auth/claude"),
            }],
            overlays: vec![
                FileOverlay {
                    source: dir.join("harness/claude/claude.json"),
                    target: PathBuf::from("/home/agent/.claude/.claude.json"),
                    writable: true,
                },
                // The agent's own transcripts over the shared project dir.
                FileOverlay {
                    source: dir.join("harness/claude/projects/-home-agent-work"),
                    target: PathBuf::from("/home/agent/.claude/projects/-home-agent-work"),
                    writable: true,
                },
                FileOverlay {
                    source: dir.join("harness/pi/sessions"),
                    target: dir.join("harness/pi/sessions"),
                    writable: true,
                },
            ],
            repos: Vec::new(),
            accounts: Vec::new(),
            cred_alias: None,
            ssh_agent: None,
            exec_env: false,
            exec_env_credentials: false,
            harness_bin: None,
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
    /// host has `/lib64` as a symlink; the probe says so. The harness comes
    /// from the tools dir, which is bound read-only, with its `bin/` on
    /// `PATH`.
    #[test]
    fn bwrap_argv_matches_the_golden_command() {
        let cfg = SandboxConfig {
            tools_dir: Some(PathBuf::from("/opt/terrarium/tools")),
            ..config()
        };
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
        let resolve =
            |p: &str| (p == "claude").then(|| PathBuf::from("/opt/terrarium/tools/bin/claude"));
        let wrapped = cfg.wrap_with(spec(), &l, env, &symlink, &resolve);
        let golden = "cd /work/terra/a1 && env -i \
CLAUDE_CONFIG_DIR=/home/agent/.claude DISABLE_TELEMETRY=1 HOME=/home/agent LANG=en_US.UTF-8 \
PATH=/opt/terrarium/bin:/opt/terrarium/tools/bin:/usr/local/bin:/usr/bin:/bin TERM=xterm-256color \
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
--ro-bind /opt/terrarium/tools /opt/terrarium/tools \
--proc /proc --dev /dev --tmpfs /tmp --tmpfs /home/agent \
--bind /work/terra/a1 /home/agent/work --bind /work/terra/a1 /work/terra/a1 \
--ro-bind /state/agents/01K8AGENT0000000000000000A/harness /state/agents/01K8AGENT0000000000000000A/harness \
--bind /auth/claude /home/agent/.claude \
--bind /state/agents/01K8AGENT0000000000000000A/harness/claude/claude.json /home/agent/.claude/.claude.json \
--bind /state/agents/01K8AGENT0000000000000000A/harness/claude/projects/-home-agent-work /home/agent/.claude/projects/-home-agent-work \
--bind /state/agents/01K8AGENT0000000000000000A/harness/pi/sessions /state/agents/01K8AGENT0000000000000000A/harness/pi/sessions \
--bind /run/terrarium/agents/01K8AGENT0000000000000000A.sock /run/terrarium/agent.sock \
--ro-bind /usr/local/bin/terrarium /opt/terrarium/bin/terrarium \
--chdir /home/agent/work -- /opt/terrarium/tools/bin/claude --dangerously-skip-permissions --model haiku";
        assert_eq!(command_line(&wrapped), golden);

        let cfg = SandboxConfig {
            new_session: true,
            ..cfg
        };
        let wrapped = cfg.wrap_with(spec(), &l, BTreeMap::new(), &symlink, &resolve);
        assert_eq!(wrapped.args[3], "--new-session");
    }

    /// F1 of `docs/security-review.md`: the runner's records stay out of
    /// the sandbox. No bind of any kind names `private/` or a directory
    /// above it, so the agent can neither read nor change them.
    /// PLAN §16b.4: the ssh-agent dir binds at `/run/terrarium/ssh-agent`,
    /// and `SSH_AUTH_SOCK` names the symlink to its socket.
    #[test]
    fn the_ssh_agent_dir_binds_with_its_socket_var() {
        let cfg = config();
        let mut l = layout();
        let resolve = |p: &str| (p == "claude").then(|| PathBuf::from("/usr/bin/claude"));
        let none = cfg.wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        assert!(!none.args.iter().any(|a| a.contains("ssh-agent")));
        assert!(
            !cfg.child_env(&l, &BTreeMap::new(), &|_| None)
                .contains_key("SSH_AUTH_SOCK")
        );
        l.ssh_agent = Some(PathBuf::from("/run/t/ssh-agent/01K8AGENT0000000000000000A"));
        let env = cfg.child_env(&l, &BTreeMap::new(), &|_| None);
        assert_eq!(env["SSH_AUTH_SOCK"], "/run/terrarium/ssh-agent.sock");
        let line = command_line(&cfg.wrap_with(spec(), &l, env, &|_| None, &resolve));
        assert!(
            line.contains(
                "--ro-bind /run/t/ssh-agent/01K8AGENT0000000000000000A /run/terrarium/ssh-agent \
                 --symlink /run/terrarium/ssh-agent/agent.sock /run/terrarium/ssh-agent.sock"
            ),
            "{line}"
        );
        let plain = SandboxConfig {
            mode: SandboxMode::None,
            ..config()
        };
        let env = plain.child_env(&l, &BTreeMap::new(), &|_| None);
        assert_eq!(
            env["SSH_AUTH_SOCK"],
            "/run/t/ssh-agent/01K8AGENT0000000000000000A/agent.sock"
        );
    }

    #[test]
    fn the_sandbox_never_binds_the_private_dir() {
        let cfg = config();
        let l = layout();
        let resolve = |p: &str| (p == "claude").then(|| PathBuf::from("/usr/bin/claude"));
        let wrapped = cfg.wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        let a = &wrapped.args;
        let mut binds = 0;
        for (i, flag) in a.iter().enumerate() {
            if !matches!(
                flag.as_str(),
                "--bind" | "--bind-try" | "--ro-bind" | "--ro-bind-try" | "--dev-bind"
            ) {
                continue;
            }
            binds += 1;
            let source = Path::new(&a[i + 1]);
            let target = Path::new(&a[i + 2]);
            for p in [source, target] {
                assert!(
                    !l.private_dir.starts_with(p) && !p.starts_with(&l.private_dir),
                    "{flag} {} {} exposes {}",
                    source.display(),
                    target.display(),
                    l.private_dir.display()
                );
            }
            if flag == "--bind" {
                assert_ne!(source, l.agent_dir, "the agent dir is not bound read-write");
            }
        }
        assert!(binds > 10, "{a:?}");
        // The harness dir is read-only; only the overlays are writable.
        let i = a
            .iter()
            .position(|x| x == &l.harness_dir.display().to_string())
            .unwrap();
        assert_eq!(a[i - 1], "--ro-bind");
    }

    /// The bind rule of the local repos: only a path under the repos dir
    /// is bound, read-write, at the path of its URL. A file URL outside the
    /// dir, a `..` escape, a symlink out of the dir, the dir itself, and a
    /// missing repo are not bound.
    #[test]
    fn local_repos_bind_only_under_the_repos_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repos");
        let outside = tmp.path().join("elsewhere");
        for d in [
            dir.join("app.git"),
            dir.join("team/lib.git"),
            outside.join("x.git"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::os::unix::fs::symlink(outside.join("x.git"), dir.join("escape.git")).unwrap();
        let d = dir.display();
        let o = outside.display();
        let urls = [
            format!("file://{d}/app.git"),
            format!("{d}/team/lib.git"),
            format!("file://{d}/app.git"),
            format!("file://{o}/x.git"),
            format!("{o}/x.git"),
            format!("file://{d}/../elsewhere/x.git"),
            format!("file://{d}/escape.git"),
            format!("file://{d}"),
            format!("file://{d}/missing.git"),
            format!("file://host{d}/app.git"),
            format!("https://example.com{d}/app.git"),
            "repos/app.git".to_owned(),
        ];
        let binds = local_repo_binds(urls.iter().map(String::as_str), &dir);
        let real = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(
            binds,
            vec![
                RepoBind {
                    source: real.join("app.git"),
                    target: dir.join("app.git"),
                },
                RepoBind {
                    source: real.join("team/lib.git"),
                    target: dir.join("team/lib.git"),
                },
            ]
        );
        assert_eq!(
            local_repo_path("file:///repos/app.git", Path::new("/repos")),
            Some(PathBuf::from("/repos/app.git"))
        );
        for url in [
            "file:///srv/app.git",
            "/repos2/app.git",
            "file:///repos/./app.git",
            "file:///repos/a/../../etc",
        ] {
            assert_eq!(local_repo_path(url, Path::new("/repos")), None, "{url}");
        }
        // No repos dir: no binds.
        assert!(
            local_repo_binds(urls.iter().map(String::as_str), &tmp.path().join("none")).is_empty()
        );

        // The binds go into the argv read-write, at the URL path.
        let mut l = layout();
        l.repos = binds;
        let resolve = |p: &str| (p == "claude").then(|| PathBuf::from("/usr/bin/claude"));
        let wrapped = config().wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        let line = command_line(&wrapped);
        let want = format!(
            "--bind {} {}",
            real.join("app.git").display(),
            dir.join("app.git").display()
        );
        assert!(line.contains(&want), "{line}");
        assert!(!line.contains(&o.to_string()), "{line}");
    }

    /// The account stores of the pool are bound read-write at their own
    /// path, and the pointer dir read-only at the fixed sandbox path. No
    /// other account and no other part of `<state>/credentials` is bound.
    #[test]
    fn only_the_pool_accounts_are_bound() {
        let mut l = layout();
        let root = PathBuf::from("/state/credentials/accounts");
        l.accounts = vec![root.join("max-a/store"), root.join("max-b/store")];
        l.cred_alias = Some(l.harness_dir.join("cred"));
        let resolve = |p: &str| (p == "claude").then(|| PathBuf::from("/usr/bin/claude"));
        let wrapped = config().wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        let line = command_line(&wrapped);
        for s in ["max-a", "max-b"] {
            let p = root.join(s).join("store");
            assert!(
                line.contains(&format!("--bind {} {}", p.display(), p.display())),
                "{line}"
            );
        }
        let alias = format!(
            "--ro-bind {}/cred {}",
            l.harness_dir.display(),
            crate::accounts::SANDBOX_CRED
        );
        assert!(line.contains(&alias), "{line}");
        let a = &wrapped.args;
        for (i, flag) in a.iter().enumerate() {
            if flag.contains("bind") && a[i + 1].starts_with("/state/credentials") {
                let src = Path::new(&a[i + 1]);
                assert!(l.accounts.iter().any(|x| x == src), "{flag} {src:?}");
            }
        }
    }

    /// A process in a real bwrap sandbox sees only its pool's account dirs
    /// under the accounts root. Skips when bwrap cannot make a sandbox on
    /// this host.
    #[test]
    fn a_sandboxed_process_sees_only_its_pool() {
        let cfg = SandboxConfig {
            bwrap: PathBuf::from("bwrap"),
            ..SandboxConfig::default()
        };
        let Ok(bwrap) = cfg.check() else {
            eprintln!("skipped: bwrap cannot make a sandbox here");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let root = state.join("credentials/accounts");
        for s in ["max-a", "max-b", "max-c"] {
            let store = root.join(s).join("store");
            std::fs::create_dir_all(store.join("claude")).unwrap();
            std::fs::write(store.join("claude/.credentials.json"), s).unwrap();
            std::fs::write(root.join(s).join("account.json"), "{}").unwrap();
        }
        let agent: AgentId = "01K8AGENT0000000000000000B".parse().unwrap();
        let dir = state.join("agents").join(agent.to_string());
        let harness_dir = dir.join("harness");
        std::fs::create_dir_all(harness_dir.join("cred")).unwrap();
        std::os::unix::fs::symlink(root.join("max-b/store"), harness_dir.join("cred/account"))
            .unwrap();
        let workspace = tmp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let socket = tmp.path().join("agent.sock");
        std::fs::write(&socket, "").unwrap();
        let l = AgentLayout {
            agent,
            agent_dir: dir.clone(),
            harness_dir: harness_dir.clone(),
            private_dir: dir.join("private"),
            workspace,
            workspace_mount: None,
            socket,
            homes: Vec::new(),
            overlays: Vec::new(),
            repos: Vec::new(),
            accounts: vec![root.join("max-a/store"), root.join("max-b/store")],
            cred_alias: Some(harness_dir.join("cred")),
            ssh_agent: None,
            exec_env: false,
            exec_env_credentials: false,
            harness_bin: None,
        };
        let script = format!(
            "ls {root}; echo ---; cat {link}/claude/.credentials.json; echo; \
             cat {alias}/account/claude/.credentials.json; echo; \
             cat {root}/max-c/store/claude/.credentials.json 2>&1 || echo hidden; \
             cat {root}/max-a/account.json 2>&1 || echo meta-hidden",
            root = root.display(),
            link = harness_dir.join("cred/account").display(),
            alias = crate::accounts::SANDBOX_CRED,
        );
        let spec = LaunchSpec::new("/bin/sh", "/home/agent/work")
            .arg("-c")
            .arg(script);
        let cfg = SandboxConfig { bwrap, ..cfg };
        let env = cfg.child_env(&l, &BTreeMap::new(), &|_| None);
        let wrapped = cfg.wrap(spec, &l, env);
        let out = std::process::Command::new(&wrapped.program)
            .args(&wrapped.args)
            .env_clear()
            .envs(&wrapped.env)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let (list, rest) = text.split_once("---").unwrap();
        let mut names: Vec<&str> = list.split_whitespace().collect();
        names.sort_unstable();
        assert_eq!(names, ["max-a", "max-b"], "{text}");
        let lines: Vec<&str> = rest.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines[0], "max-b", "the pointer resolves inside: {text}");
        assert_eq!(lines[1], "max-b", "the fixed alias resolves too: {text}");
        assert_eq!(lines.last(), Some(&"meta-hidden"), "{text}");
        assert!(text.contains("hidden"), "{text}");
        assert!(!text.contains("max-c\n"), "{text}");
    }

    /// S1 and S6 (secret manager review, 2026-10-29): with secret env vars
    /// the program starts through `terrarium exec-env` inside the sandbox,
    /// so no value is in the env or the argv of `bwrap`.
    #[test]
    fn exec_env_wraps_the_program_inside_the_sandbox() {
        let cfg = config();
        let mut l = layout();
        l.exec_env = true;
        let resolve = |p: &str| (p == "claude").then(|| PathBuf::from("/usr/bin/claude"));
        let s = spec();
        let original = s.args.clone();
        let w = cfg.wrap_with(s, &l, BTreeMap::new(), &|_| None, &resolve);
        let i = w.args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &w.args[i + 1..i + 5],
            [
                "/opt/terrarium/bin/terrarium",
                "exec-env",
                "--",
                "/usr/bin/claude"
            ]
        );
        assert_eq!(&w.args[i + 5..], original.as_slice());
        // Mode `none`: the host CLI runs the program.
        let plain = SandboxConfig {
            mode: SandboxMode::None,
            ..config()
        };
        let w = plain.wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        assert_eq!(w.program, "/usr/local/bin/terrarium");
        assert_eq!(&w.args[..3], ["exec-env", "--", "claude"]);
        assert_eq!(&w.args[3..], original.as_slice());
        // Harness credentials: exec-env must reach the daemon.
        l.exec_env_credentials = true;
        let w = plain.wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        assert_eq!(
            &w.args[..4],
            ["exec-env", "--require-credentials", "--", "claude"]
        );
        let w = cfg.wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        let i = w.args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &w.args[i + 1..i + 6],
            [
                "/opt/terrarium/bin/terrarium",
                "exec-env",
                "--require-credentials",
                "--",
                "/usr/bin/claude"
            ]
        );
        l.exec_env_credentials = false;
        // No CLI: no wrap (the daemon warns, and sets no var).
        let bare = SandboxConfig {
            cli: None,
            ..config()
        };
        let w = bare.wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        assert!(!w.args.iter().any(|a| a == "exec-env"));
    }

    /// S13 (secret manager review, 2026-10-29): the ssh-agent dir binds
    /// read-only. `ssh-add -l` in a real sandbox still connects to the
    /// socket and lists the key, and the agent cannot write into the dir.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_ssh_agent_socket_works_in_a_read_only_bind() {
        let cfg = SandboxConfig {
            bwrap: PathBuf::from("bwrap"),
            ..SandboxConfig::default()
        };
        let Ok(bwrap) = cfg.check() else {
            eprintln!("skipped: bwrap cannot make a sandbox here");
            return;
        };
        if which("ssh-add", std::env::var("PATH").ok().as_deref()).is_none() {
            eprintln!("skipped: no ssh-add");
            return;
        }
        let (dir, db, _) = crate::secrets::agent_keys::tests::fixture().await;
        let a = crate::secrets::grants::tests::AGENT;
        let run = dir.path().join("run");
        let h = crate::secrets::agent_keys::serve(db, dir.path().into(), &run, a, "p").unwrap();
        let mut l = layout();
        l.agent = a.parse().unwrap();
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        l.workspace = workspace;
        l.workspace_mount = None;
        l.harness_dir = dir.path().join("harness");
        std::fs::create_dir_all(&l.harness_dir).unwrap();
        l.socket = dir.path().join("agent.sock");
        std::fs::write(&l.socket, "").unwrap();
        l.homes = Vec::new();
        l.overlays = Vec::new();
        l.ssh_agent = Some(crate::secrets::agent_keys::socket_dir(&run, a));
        let spec = LaunchSpec::new("/bin/sh", "/home/agent/work")
            .arg("-c")
            .arg("ssh-add -l; touch /run/terrarium/ssh-agent/x 2>/dev/null && echo WROTE || echo READONLY");
        let cfg = SandboxConfig {
            bwrap,
            cli: None,
            tools_dir: None,
            ..cfg
        };
        let env = cfg.child_env(&l, &BTreeMap::new(), &|_| None);
        let wrapped = cfg.wrap(spec, &l, env);
        let out = tokio::process::Command::new(&wrapped.program)
            .args(&wrapped.args)
            .env_clear()
            .envs(&wrapped.env)
            .output()
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("global/deploy"), "{text} {out:?}");
        assert!(text.contains("READONLY"), "{text}");
        h.shutdown().await;
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
        let (program, dir) = locate("claude", &|_| Some(link.clone()), None);
        let real = std::fs::canonicalize(&real).unwrap();
        assert_eq!(program, real.display().to_string());
        assert_eq!(dir.as_deref(), real.parent());
        let (program, dir) = locate("x", &|_| Some(PathBuf::from("/usr/bin/x")), None);
        assert_eq!((program.as_str(), dir), ("/usr/bin/x", None));
        // A harness in the tools dir runs by its `bin/` path; the tools
        // dir is bound whole, so no bind of its own.
        let (program, dir) = locate(
            "codex",
            &|_| Some(PathBuf::from("/opt/terrarium/tools/bin/codex")),
            Some(Path::new("/opt/terrarium/tools")),
        );
        assert_eq!(
            (program.as_str(), dir),
            ("/opt/terrarium/tools/bin/codex", None)
        );
    }

    /// An agent launched on a harness version runs the program from the
    /// version dir, with that `bin/` first on `PATH` after the CLI dir. The
    /// tools dir is bound once, read-only; the version dir needs no bind of
    /// its own. A program that is not in the version dir comes from `PATH`.
    #[test]
    fn the_launch_runs_the_version_dir_of_the_agent() {
        let tools = PathBuf::from("/opt/terrarium/tools");
        let bin = tools.join("versions/claude/2.1.310/bin");
        let cfg = SandboxConfig {
            tools_dir: Some(tools.clone()),
            ..config()
        };
        let mut l = layout();
        l.harness_bin = Some(bin.clone());
        let env = cfg.child_env(&l, &BTreeMap::new(), &|_| None);
        assert_eq!(
            env["PATH"],
            "/opt/terrarium/bin:/opt/terrarium/tools/versions/claude/2.1.310/bin:\
             /opt/terrarium/tools/bin:/usr/local/bin:/usr/bin:/bin"
        );
        let present = |p: &Path| p == bin.join("claude");
        let resolve = |p: &str| {
            pinned_program(Some(&bin), p, &present).or_else(|| Some(tools.join("bin").join(p)))
        };
        let wrapped = cfg.wrap_with(spec(), &l, env.clone(), &|_| None, &resolve);
        let a = &wrapped.args;
        let dash = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[dash + 1], bin.join("claude").display().to_string());
        let ro: Vec<&str> = a
            .iter()
            .enumerate()
            .filter(|(_, f)| f.as_str() == "--ro-bind")
            .map(|(i, _)| a[i + 1].as_str())
            .collect();
        assert_eq!(
            ro.iter()
                .filter(|s| s.starts_with("/opt/terrarium/tools"))
                .count(),
            1,
            "{ro:?}"
        );
        assert!(ro.contains(&"/opt/terrarium/tools"), "{ro:?}");

        // Another program: from the tools `bin/`, as before.
        let other = LaunchSpec::new("codex", "/home/agent/work");
        let wrapped = cfg.wrap_with(other, &l, env, &|_| None, &resolve);
        let a = &wrapped.args;
        let dash = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[dash + 1], "/opt/terrarium/tools/bin/codex");

        // No sandbox: the same program path, and the same `PATH` order.
        let none = SandboxConfig {
            mode: SandboxMode::None,
            cli: Some(PathBuf::from("/opt/t/bin/terrarium")),
            ..cfg.clone()
        };
        let host = |k: &str| (k == "PATH").then(|| "/t/bin:/usr/bin".to_owned());
        let env = none.child_env(&l, &BTreeMap::new(), &host);
        assert_eq!(
            env["PATH"],
            format!("/opt/t/bin:{}:/t/bin:/usr/bin", bin.display())
        );
        let wrapped = none.wrap_with(spec(), &l, env, &|_| None, &resolve);
        assert_eq!(wrapped.program, bin.join("claude").display().to_string());
        // Without a version dir, the program stays a name for `PATH`.
        l.harness_bin = None;
        let wrapped = none.wrap_with(spec(), &l, BTreeMap::new(), &|_| None, &resolve);
        assert_eq!(wrapped.program, "claude");
        assert_eq!(
            pinned_program(Some(&bin), "../claude", &|_| true),
            None,
            "a path is never pinned"
        );
    }

    #[test]
    fn command_line_hides_secrets() {
        let spec = LaunchSpec::new("pi", "/w")
            .env("ANTHROPIC_API_KEY", "sk-secret")
            .env("COPILOT_GITHUB_TOKEN", "ghp_secret")
            .env("PI_MODEL", "x");
        let line = command_line(&spec);
        assert!(!line.contains("secret"), "{line}");
        assert!(line.contains("'ANTHROPIC_API_KEY=<redacted>'"), "{line}");
        assert!(line.contains("PI_MODEL=x"), "{line}");
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
