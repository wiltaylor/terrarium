//! Clone and update repos for agent workspaces (PLAN §9, §21).
//!
//! The daemon runs the `git` binary itself, outside every sandbox, so the
//! secret of a repo never enters the sandbox filesystem:
//!
//! - A token goes to git through a one-shot credential helper script in a
//!   0700 run dir under `<state>/credentials/.run/`. The script answers only
//!   for the protocol and host of the repo URL. The run dir is removed after
//!   the run. The secret is never on the command line, in the env, or in
//!   the repo's `.git/config`.
//! - An SSH key goes to `ssh -i` through `GIT_SSH_COMMAND`, as a 0600 copy in
//!   the same run dir.
//! - After a clone, the daemon writes `credential.helper = !terrarium repo
//!   credential` and `credential.useHttpPath = true` into the local
//!   `.git/config`, so a push from inside the sandbox asks the daemon
//!   (`POST /api/repos/credential`).
//!
//! An update runs git in a repo that the agent controls. Its `.git/config`
//! could name hooks, filters, or proxies that would run code in the daemon,
//! or send the token to another host. So the update:
//!
//! - copies `.git/config` (a regular file, not a symlink) to a 0600 file in
//!   its own run dir, and checks **that copy** against a short allowlist
//!   ([`config_allowed`]). A change during the check cannot slip past it.
//! - checks a new copy again after the fetch, before the first command that
//!   touches the work tree (`status`, `checkout`, `merge` can run filters).
//! - runs every command with overrides that switch off hooks, fsmonitor,
//!   askpass, and dangerous transports ([`HARDEN`]), with
//!   `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1`, so the
//!   daemon user's and the system's config do not apply.
//! - fetches from the stored URL, not from `origin`.
//!
//! What remains: git reads the live `.git/config` on each command, and the
//! agent can write it at any time from its sandbox. An edit between the
//! second check and a work tree command can still add a filter driver
//! (`filter.<name>.smudge`, named by `.gitattributes`), an `include.path`,
//! or a URL-scoped key (`http.<url>.proxy`) for that one command. git has
//! no switch that ignores the local config, and `-c include.path=` is a
//! parse error, so the `-c` overrides cannot cover names that the agent
//! picks. The window is a few milliseconds per command. To close it, the
//! daemon would have to update a clone of its own and copy the files, or
//! stop the agent during the update.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::secrets::{RunDirs, Secret};

/// The value of `credential.helper` in a workspace repo.
pub const WORKSPACE_HELPER: &str = "!terrarium repo credential";

/// The longest a clone or an update may take.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// `-c` overrides on every git command. Command-line values win over the
/// repo's local config.
const HARDEN: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.untrackedCache=false",
    // An empty value resets the helper list, so no helper from the repo or
    // the daemon user's config sees (or stores) the secret.
    "credential.helper=",
    "protocol.allow=never",
    "protocol.https.allow=always",
    "protocol.http.allow=always",
    "protocol.ssh.allow=always",
    "protocol.file.allow=always",
    "protocol.git.allow=never",
    "protocol.ext.allow=never",
    "protocol.fd.allow=never",
    "protocol.ftp.allow=never",
    "protocol.ftps.allow=never",
    "fetch.recurseSubmodules=false",
    "submodule.recurse=false",
    "gc.auto=0",
    "maintenance.auto=false",
    "merge.verifySignatures=false",
    "advice.detachedHead=false",
];

/// Env variables that would point git at another repo or config.
const ENV_REMOVE: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_NAMESPACE",
    "GIT_CREDENTIAL_HELPER",
];

/// One clone or update.
#[derive(Debug, Clone)]
pub struct Checkout {
    /// The stored repo URL.
    pub url: String,
    /// `<workspace>/<path>`.
    pub dest: PathBuf,
    /// The branch to check out. `None`: the remote's default on a clone,
    /// the current branch on an update.
    pub branch: Option<String>,
    /// `--depth` for a clone or a fetch.
    pub depth: Option<u32>,
    pub secret: Option<Secret>,
    /// Where the one-shot helper files go.
    pub store: RunDirs,
    /// A local-path URL under one of these dirs is refused (the state dir:
    /// other agents' workspaces and the run dirs of the secrets).
    pub deny_local_under: Vec<PathBuf>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Cloned,
    Updated,
    UpToDate,
    /// Fetched, but not fast-forwarded: `note` says why.
    NotUpdated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub action: Action,
    /// The HEAD commit, when the repo has one.
    pub head: Option<String>,
    /// The current branch, when HEAD is on one.
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum GitError {
    /// The daemon will not run git for this request.
    #[error("{0}")]
    Refused(String),
    /// git ran and failed.
    #[error("git {command} failed ({status}): {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("git did not finish in {0:?}")]
    Timeout(Duration),
    #[error("{0}")]
    Io(String),
}

/// The parts of an `http(s)` URL that git sends to a credential helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpLocation {
    pub protocol: String,
    /// The host, with `:port` when the URL has one. Lower case.
    pub host: String,
    /// The path without slashes at the ends and without `.git`.
    pub path: String,
}

/// Normalise a path as git sends it with `credential.useHttpPath`.
pub fn normalize_path(path: &str) -> String {
    let p = path.trim_matches('/');
    p.strip_suffix(".git")
        .unwrap_or(p)
        .trim_end_matches('/')
        .to_owned()
}

/// The credential-helper view of an `http://` or `https://` URL. `None`
/// for SSH, file, and other URLs.
pub fn http_location(url: &str) -> Option<HttpLocation> {
    let (protocol, rest) = url.split_once("://")?;
    let protocol = protocol.to_ascii_lowercase();
    if protocol != "https" && protocol != "http" {
        return None;
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if host.is_empty() {
        return None;
    }
    Some(HttpLocation {
        protocol,
        host: host.to_ascii_lowercase(),
        path: normalize_path(path),
    })
}

/// Quote for `sh`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn io(e: impl std::fmt::Display) -> GitError {
    GitError::Io(e.to_string())
}

/// A branch name the daemon passes to git: no option, no ref syntax.
fn check_branch(b: &str) -> Result<(), GitError> {
    let bad = b.is_empty()
        || b.starts_with(['-', '/', '.'])
        || b.ends_with(['/', '.'])
        || b.ends_with(".lock")
        || b.contains("..")
        || b.contains("@{")
        || b.contains("//")
        || b.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        });
    if bad {
        return Err(GitError::Refused(format!("bad branch name `{b}`")));
    }
    Ok(())
}

/// True for a repo URL that names a path on the daemon's machine
/// (`file://...` or an absolute path).
pub fn is_local_url(url: &str) -> bool {
    url.starts_with("file://") || url.starts_with('/')
}

/// Refuse URLs that are options, remote-helper syntax (`ext::`), relative
/// local paths, or local paths under a denied dir.
fn check_url(url: &str, deny: &[PathBuf]) -> Result<(), GitError> {
    if url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control) {
        return Err(GitError::Refused(format!("bad repo URL `{url}`")));
    }
    let before_slash = url.split('/').next().unwrap_or("");
    if before_slash.contains("::") {
        return Err(GitError::Refused(format!(
            "repo URL `{url}` uses a remote helper; use https, ssh, or an absolute path"
        )));
    }
    let local = if let Some(p) = url.strip_prefix("file://") {
        Some(p)
    } else if url.starts_with('/') {
        Some(url)
    } else if url.contains("://") || before_slash.contains(':') {
        None
    } else {
        return Err(GitError::Refused(format!(
            "repo URL `{url}` is a relative path; use an absolute path or a URL"
        )));
    };
    if let Some(p) = local {
        let real = std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
        for d in deny {
            let d = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
            if real.starts_with(&d) {
                return Err(GitError::Refused(format!(
                    "repo URL `{url}` points into the daemon state dir"
                )));
            }
        }
    }
    Ok(())
}

/// Is `key = value` from a workspace repo's local config safe for the
/// daemon to run git with? Keys are as `git config --list` prints them:
/// section and name in lower case.
pub fn config_allowed(key: &str, value: &str) -> bool {
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let (sub, var) = match rest.rsplit_once('.') {
        Some((s, v)) => (Some(s), v),
        None => (None, rest),
    };
    match (section, sub) {
        ("core", None) => matches!(
            var,
            "repositoryformatversion"
                | "filemode"
                | "bare"
                | "logallrefupdates"
                | "ignorecase"
                | "precomposeunicode"
                | "symlinks"
                | "autocrlf"
                | "eol"
                | "safecrlf"
                | "quotepath"
        ),
        // Named remotes only: a remote named by a URL would apply to the
        // daemon's fetch of that URL.
        ("remote", Some(name)) => {
            !name.contains([':', '/'])
                && matches!(
                    var,
                    "url" | "fetch" | "pushurl" | "push" | "prune" | "tagopt"
                )
        }
        ("branch", Some(_)) => matches!(
            var,
            "remote" | "merge" | "rebase" | "pushremote" | "description"
        ),
        ("credential", None) => match var {
            "helper" => value.is_empty() || value == WORKSPACE_HELPER,
            "usehttppath" => true,
            _ => false,
        },
        ("user" | "advice" | "color", _) => true,
        ("push", None) => matches!(var, "default" | "autosetupremote" | "followtags"),
        ("pull", None) => matches!(var, "rebase" | "ff"),
        ("init", None) => var == "defaultbranch",
        ("extensions", None) => var == "objectformat",
        ("commit", None) => var == "gpgsign",
        _ => false,
    }
}

/// The env and `-c` values of one checkout, and the run dir that holds the
/// one-shot helper files (removed on drop).
struct Runner {
    config: Vec<String>,
    env: Vec<(String, String)>,
    deadline: Instant,
    timeout: Duration,
    _run: Option<crate::secrets::RunDir>,
}

fn token_helper(loc: &HttpLocation, answer: &Path) -> String {
    format!(
        r#"#!/bin/sh
# One-shot git credential helper of terrariumd. Removed after the run.
test "$1" = get || exit 0
p=
h=
while IFS= read -r line; do
  test -z "$line" && break
  case "$line" in
    protocol=*) p=${{line#protocol=}} ;;
    host=*) h=${{line#host=}} ;;
  esac
done
if test "$p" = {proto} && test "$h" = {host}; then
  cat {answer}
fi
exit 0
"#,
        proto = sh_quote(&loc.protocol),
        host = sh_quote(&loc.host),
        answer = sh_quote(&answer.to_string_lossy()),
    )
}

impl Runner {
    fn new(c: &Checkout) -> Result<Self, GitError> {
        let mut config: Vec<String> = HARDEN.iter().map(|s| (*s).to_owned()).collect();
        let mut ssh = "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new".to_owned();
        let mut run = None;
        match &c.secret {
            Some(Secret::Token { username, token }) => {
                if let Some(loc) = http_location(&c.url) {
                    let dir = c.store.run_dir().map_err(|e| io(e.message))?;
                    let answer = dir
                        .write_private(
                            "answer",
                            zeroize::Zeroizing::new(format!(
                                "username={username}\npassword={}\n",
                                token.as_str()
                            ))
                            .as_bytes(),
                        )
                        .map_err(|e| io(e.message))?;
                    let script = dir
                        .write_script("helper", &token_helper(&loc, &answer))
                        .map_err(|e| io(e.message))?;
                    config.push(format!(
                        "credential.helper=!{}",
                        sh_quote(&script.to_string_lossy())
                    ));
                    run = Some(dir);
                }
            }
            Some(Secret::SshKey(key)) => {
                let dir = c.store.run_dir().map_err(|e| io(e.message))?;
                let path = dir
                    .write_private("key", key.as_bytes())
                    .map_err(|e| io(e.message))?;
                ssh.push_str(&format!(
                    " -o IdentitiesOnly=yes -i {}",
                    sh_quote(&path.to_string_lossy())
                ));
                run = Some(dir);
            }
            Some(other @ (Secret::Login { .. } | Secret::Other(_))) => {
                return Err(io(format!(
                    "the credential is a {}; a repo needs a token, a github_token, or an ssh_key",
                    other.kind_label()
                )));
            }
            None => {}
        }
        let env = vec![
            ("LC_ALL".to_owned(), "C".to_owned()),
            // Only the repo's local config (checked) and the `-c` values.
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
            // An empty GIT_ASKPASS would fall back to core.askPass from
            // the repo config. `false` answers nothing.
            ("GIT_ASKPASS".to_owned(), "false".to_owned()),
            ("SSH_ASKPASS".to_owned(), "false".to_owned()),
            // Wins over core.sshCommand in the repo config.
            ("GIT_SSH_COMMAND".to_owned(), ssh),
        ];
        Ok(Self {
            config,
            env,
            deadline: Instant::now() + c.timeout,
            timeout: c.timeout,
            _run: run,
        })
    }

    /// Run one git command. Progress lines (stderr, split on `\r` and
    /// `\n`) go to `progress`. Returns stdout.
    fn git(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        progress: &mut dyn FnMut(&str),
    ) -> Result<String, GitError> {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("git");
        for kv in &self.config {
            cmd.arg("-c").arg(kv);
        }
        if let Some(d) = cwd {
            cmd.arg("-C").arg(d);
        }
        cmd.args(args);
        for k in ENV_REMOVE {
            cmd.env_remove(k);
        }
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a timeout kills ssh and the remote
            // helpers too.
            .process_group(0);
        let name = args.first().copied().unwrap_or("");
        let mut child = cmd
            .spawn()
            .map_err(|e| io(format!("cannot run git: {e}")))?;
        let mut stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut buf);
            }
            buf
        });
        let (tx, rx) = mpsc::channel::<String>();
        let err_thread = std::thread::spawn(move || {
            let Some(mut s) = stderr else { return };
            let mut buf = [0u8; 4096];
            let mut line = Vec::new();
            loop {
                let n = match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for &b in &buf[..n] {
                    if b == b'\n' || b == b'\r' {
                        if !line.is_empty() {
                            let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
                            line.clear();
                        }
                    } else {
                        line.push(b);
                    }
                }
            }
            if !line.is_empty() {
                let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
            }
        });
        let mut tail: VecDeque<String> = VecDeque::new();
        let mut on_line = |l: String, tail: &mut VecDeque<String>| {
            progress(&l);
            tail.push_back(l);
            if tail.len() > 12 {
                tail.pop_front();
            }
        };
        let status = loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(l) => on_line(l, &mut tail),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            if let Some(st) = child.try_wait().map_err(io)? {
                break st;
            }
            if Instant::now() >= self.deadline {
                let pid = child.id().to_string();
                let _ = Command::new("kill")
                    .arg("-KILL")
                    .arg(format!("-{pid}"))
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::Timeout(self.timeout));
            }
        };
        let _ = err_thread.join();
        while let Ok(l) = rx.try_recv() {
            on_line(l, &mut tail);
        }
        let out = out_thread.join().unwrap_or_default();
        if !status.success() {
            return Err(GitError::Failed {
                command: name.to_owned(),
                status: status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| format!("exit {c}")),
                stderr: tail.into_iter().collect::<Vec<_>>().join("\n"),
            });
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn quiet(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        self.git(Some(cwd), args, &mut |_| {})
    }

    fn head(&self, dest: &Path) -> Option<String> {
        self.quiet(dest, &["rev-parse", "-q", "--verify", "HEAD"])
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    fn branch(&self, dest: &Path) -> Option<String> {
        self.quiet(dest, &["symbolic-ref", "-q", "--short", "HEAD"])
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// The local config that sends pushes from the sandbox to the daemon.
    fn write_local_config(&self, dest: &Path) -> Result<(), GitError> {
        // The empty first value resets helpers from other config files.
        self.quiet(
            dest,
            &[
                "config",
                "--local",
                "--replace-all",
                "credential.helper",
                "",
            ],
        )?;
        self.quiet(
            dest,
            &[
                "config",
                "--local",
                "--add",
                "credential.helper",
                WORKSPACE_HELPER,
            ],
        )?;
        self.quiet(
            dest,
            &["config", "--local", "credential.useHttpPath", "true"],
        )?;
        Ok(())
    }
}

/// The keys of a git config file that are not in the allowlist. Reads the
/// file without running anything from it.
fn unsafe_config_keys(r: &Runner, file: &Path) -> Result<Vec<String>, GitError> {
    let file = file.to_string_lossy();
    let out = r.git(
        None,
        &["config", "--file", &file, "--no-includes", "-z", "--list"],
        &mut |_| {},
    )?;
    let mut bad: Vec<String> = out
        .split('\0')
        .filter(|e| !e.is_empty())
        .filter_map(|e| {
            let (k, v) = e.split_once('\n').unwrap_or((e, ""));
            (!config_allowed(k, v)).then(|| k.to_owned())
        })
        .collect();
    bad.sort();
    bad.dedup();
    Ok(bad)
}

/// Copy `<dest>/.git/config` into a private run dir and check the copy
/// against the allowlist. The copy, not the live file, is what the check
/// reads, so an edit during the check does not pass half-read.
fn check_config_snapshot(c: &Checkout, r: &Runner) -> Result<(), GitError> {
    let live = c.dest.join(".git").join("config");
    let meta = std::fs::symlink_metadata(&live).map_err(io)?;
    if !meta.is_file() {
        return Err(GitError::Refused(format!(
            "{} is not a regular file; the daemon updates only its own clones",
            live.display()
        )));
    }
    let bytes = std::fs::read(&live).map_err(io)?;
    let dir = c.store.run_dir().map_err(|e| io(e.message))?;
    let copy = dir
        .write_private("config", &bytes)
        .map_err(|e| io(e.message))?;
    let bad = unsafe_config_keys(r, &copy)?;
    if !bad.is_empty() {
        return Err(GitError::Refused(format!(
            "the repo config at {} has keys the daemon does not run git with: {}; \
             remove them, or fetch inside the sandbox",
            c.dest.display(),
            bad.join(", ")
        )));
    }
    Ok(())
}

/// The checks before the daemon runs git in a repo the agent controls.
fn check_workspace_repo(dest: &Path) -> Result<(), GitError> {
    let git = dest.join(".git");
    let meta = std::fs::symlink_metadata(&git).map_err(io)?;
    if !meta.is_dir() {
        return Err(GitError::Refused(format!(
            "{} is not a plain .git directory; the daemon updates only its own clones",
            git.display()
        )));
    }
    for f in ["commondir", "gitdir"] {
        if git.join(f).exists() {
            return Err(GitError::Refused(format!(
                "{} is a linked worktree; the daemon updates only its own clones",
                dest.display()
            )));
        }
    }
    Ok(())
}

/// A git runner for the config repo (`crate::config_repo`), with the same
/// hardening as a workspace update ([`HARDEN`], no global or system
/// config, no askpass) and the same secret handling: a token through a
/// one-shot credential helper, an SSH key through `GIT_SSH_COMMAND`. The
/// secret never goes on the command line, in the env, or in
/// `.git/config`. `url` is the remote that the token answers for. One
/// runner serves one operation: its deadline starts at [`GitRunner::new`].
pub struct GitRunner(Runner);

impl GitRunner {
    pub fn new(
        url: &str,
        secret: Option<Secret>,
        store: RunDirs,
        timeout: Duration,
    ) -> Result<Self, GitError> {
        let c = Checkout {
            url: url.to_owned(),
            dest: PathBuf::new(),
            branch: None,
            depth: None,
            secret,
            store,
            deny_local_under: Vec::new(),
            timeout,
        };
        Runner::new(&c).map(Self)
    }

    /// Run `git -C <cwd> <args>`; the answer is stdout. A failure has the
    /// last lines of stderr.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        self.0.quiet(cwd, args)
    }
}

/// Clone `c.url` into `c.dest`, or update the clone that is there.
/// Blocking: call it on the blocking pool ([`clone_or_update_async`]).
pub fn clone_or_update(c: &Checkout, progress: &mut dyn FnMut(&str)) -> Result<Outcome, GitError> {
    check_url(&c.url, &c.deny_local_under)?;
    if let Some(b) = &c.branch {
        check_branch(b)?;
    }
    let depth = c.depth.map(|d| d.max(1).to_string());
    match std::fs::symlink_metadata(&c.dest) {
        Ok(m) if m.file_type().is_symlink() => Err(GitError::Refused(format!(
            "{} is a symlink",
            c.dest.display()
        ))),
        Ok(m) if !m.is_dir() => Err(GitError::Refused(format!(
            "{} exists and is not a directory",
            c.dest.display()
        ))),
        Ok(_) if c.dest.join(".git").exists() || c.dest.join(".git").is_symlink() => {
            update(c, depth.as_deref(), progress)
        }
        Ok(_) => {
            let empty = std::fs::read_dir(&c.dest).map_err(io)?.next().is_none();
            if !empty {
                return Err(GitError::Refused(format!(
                    "{} exists and is not a git checkout",
                    c.dest.display()
                )));
            }
            clone(c, depth.as_deref(), progress)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = c.dest.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            clone(c, depth.as_deref(), progress)
        }
        Err(e) => Err(io(e)),
    }
}

fn clone(
    c: &Checkout,
    depth: Option<&str>,
    progress: &mut dyn FnMut(&str),
) -> Result<Outcome, GitError> {
    let r = Runner::new(c)?;
    let dest = c.dest.to_string_lossy().into_owned();
    // No hardlinks: the agent owns the workspace files, and must not reach
    // the objects of a local source repo through them.
    let mut args = vec!["clone", "--progress", "--no-hardlinks"];
    if let Some(b) = &c.branch {
        args.extend(["--branch", b.as_str()]);
    }
    if let Some(d) = depth {
        args.extend(["--depth", d]);
    }
    args.extend(["--", c.url.as_str(), dest.as_str()]);
    r.git(None, &args, progress)?;
    r.write_local_config(&c.dest)?;
    Ok(Outcome {
        action: Action::Cloned,
        head: r.head(&c.dest),
        branch: r.branch(&c.dest),
        note: None,
    })
}

fn update(
    c: &Checkout,
    depth: Option<&str>,
    progress: &mut dyn FnMut(&str),
) -> Result<Outcome, GitError> {
    check_workspace_repo(&c.dest)?;
    let r = Runner::new(c)?;
    check_config_snapshot(c, &r)?;
    let dest = &c.dest;
    let current = r.branch(dest);
    let Some(branch) = c.branch.clone().or_else(|| current.clone()) else {
        return Err(GitError::Refused(
            "HEAD is not on a branch; name one with `branch`".into(),
        ));
    };
    check_branch(&branch)?;
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    // The stored URL, not `origin`: the agent may have changed the remote.
    let mut args = vec![
        "fetch",
        "--progress",
        "--no-recurse-submodules",
        "--upload-pack=git-upload-pack",
    ];
    if let Some(d) = depth {
        args.extend(["--depth", d]);
    }
    args.extend(["--", c.url.as_str(), refspec.as_str()]);
    r.git(Some(dest), &args, progress)?;
    r.write_local_config(dest)?;
    // The agent may have edited the config during the fetch. Check again
    // before the commands that touch the work tree (they can run filters).
    check_config_snapshot(c, &r)?;

    let status = r.quiet(dest, &["status", "--porcelain", "--untracked-files=no"])?;
    let not_updated = |note: String| Outcome {
        action: Action::NotUpdated,
        head: r.head(dest),
        branch: r.branch(dest),
        note: Some(note),
    };
    if !status.trim().is_empty() {
        return Ok(not_updated(format!(
            "the work tree has local changes; fetched origin/{branch}, did not fast-forward"
        )));
    }
    let remote_ref = format!("refs/remotes/origin/{branch}");
    if current.as_deref() != Some(branch.as_str()) {
        let local_ref = format!("refs/heads/{branch}");
        let exists = r
            .quiet(dest, &["rev-parse", "-q", "--verify", &local_ref])
            .is_ok();
        let switched = if exists {
            r.quiet(dest, &["checkout", "-q", &branch])
        } else {
            r.quiet(
                dest,
                &["checkout", "-q", "--no-track", "-b", &branch, &remote_ref],
            )
        };
        if let Err(e) = switched {
            return Ok(not_updated(format!("cannot switch to {branch}: {e}")));
        }
    }
    let before = r.head(dest);
    if r.quiet(dest, &["merge", "--ff-only", "-q", &remote_ref])
        .is_err()
    {
        return Ok(not_updated(format!(
            "{branch} has diverged from origin/{branch}; fetched, did not fast-forward"
        )));
    }
    let head = r.head(dest);
    Ok(Outcome {
        action: if head == before {
            Action::UpToDate
        } else {
            Action::Updated
        },
        head,
        branch: r.branch(dest),
        note: None,
    })
}

/// [`clone_or_update`] on the blocking pool. Progress lines go to
/// `progress`; the sender drops when git ends.
pub async fn clone_or_update_async(
    c: Checkout,
    progress: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<Outcome, GitError> {
    tokio::task::spawn_blocking(move || {
        clone_or_update(&c, &mut |line| {
            let _ = progress.send(line.to_owned());
        })
    })
    .await
    .map_err(|e| GitError::Io(format!("the git task failed: {e}")))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let st = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .unwrap();
        assert!(st.success(), "{script}");
    }

    /// A bare repo with one commit on `main`, and a work clone to push from.
    fn bare_repo(dir: &Path) -> PathBuf {
        sh(
            dir,
            "git init -q --bare -b main origin.git && git init -q -b main src && cd src && \
             echo one > a.txt && git add a.txt && git commit -q -m one && \
             git push -q ../origin.git main",
        );
        dir.join("origin.git")
    }

    fn checkout(dir: &Path, url: &Path, dest: &Path) -> Checkout {
        Checkout {
            url: url.to_string_lossy().into_owned(),
            dest: dest.to_owned(),
            branch: None,
            depth: None,
            secret: None,
            store: RunDirs::new(&dir.join("state")),
            deny_local_under: vec![dir.join("state")],
            timeout: Duration::from_secs(60),
        }
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    }

    #[test]
    fn clone_writes_the_helper_and_update_fast_forwards() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        let ws = tmp.path().join("state/agents/A/workspace");
        let dest = ws.join("proj");
        let c = checkout(tmp.path(), &origin, &dest);
        let mut lines = Vec::new();
        let out = clone_or_update(&c, &mut |l| lines.push(l.to_owned())).unwrap();
        assert_eq!(out.action, Action::Cloned);
        assert_eq!(out.branch.as_deref(), Some("main"));
        let head = out.head.clone().unwrap();
        assert_eq!(
            head,
            git_out(&tmp.path().join("src"), &["rev-parse", "HEAD"])
        );
        assert!(dest.join("a.txt").is_file());
        assert!(!lines.is_empty(), "progress lines");
        let config = std::fs::read_to_string(dest.join(".git/config")).unwrap();
        assert!(
            config.contains("helper = !terrarium repo credential"),
            "{config}"
        );
        assert!(config.contains("useHttpPath = true"), "{config}");
        assert_eq!(
            git_out(&dest, &["config", "remote.origin.url"]),
            origin.to_string_lossy()
        );

        // Nothing new: up to date.
        let out = clone_or_update(&c, &mut |_| {}).unwrap();
        assert_eq!(out.action, Action::UpToDate);
        assert_eq!(out.head.as_deref(), Some(head.as_str()));

        // A new commit upstream: fast-forward.
        sh(
            &tmp.path().join("src"),
            "echo two > b.txt && git add b.txt && git commit -q -m two && git push -q ../origin.git main",
        );
        let out = clone_or_update(&c, &mut |_| {}).unwrap();
        assert_eq!(out.action, Action::Updated);
        assert!(dest.join("b.txt").is_file());

        // Local changes: fetch only.
        sh(
            &tmp.path().join("src"),
            "echo three > c.txt && git add c.txt && git commit -q -m three && git push -q ../origin.git main",
        );
        std::fs::write(dest.join("a.txt"), "changed").unwrap();
        let out = clone_or_update(&c, &mut |_| {}).unwrap();
        assert_eq!(out.action, Action::NotUpdated);
        assert!(out.note.unwrap().contains("local changes"));
        assert!(!dest.join("c.txt").exists());
        // The helper lines stay single.
        let config = std::fs::read_to_string(dest.join(".git/config")).unwrap();
        assert_eq!(
            config.matches("terrarium repo credential").count(),
            1,
            "{config}"
        );
    }

    #[test]
    fn update_refuses_a_dangerous_repo_config() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        let dest = tmp.path().join("ws/proj");
        let c = checkout(tmp.path(), &origin, &dest);
        clone_or_update(&c, &mut |_| {}).unwrap();
        let marker = tmp.path().join("pwned");
        sh(
            &dest,
            &format!(
                "git config filter.x.smudge 'touch {}' && git config core.hooksPath /tmp",
                marker.display()
            ),
        );
        match clone_or_update(&c, &mut |_| {}) {
            Err(GitError::Refused(m)) => {
                assert!(
                    m.contains("filter.x.smudge") && m.contains("core.hookspath"),
                    "{m}"
                )
            }
            other => panic!("{other:?}"),
        }
        assert!(!marker.exists());
    }

    /// An edit to `.git/config` while the fetch runs is caught before the
    /// work tree commands, and its filter never runs.
    #[test]
    fn update_checks_the_config_again_after_the_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        let dest = tmp.path().join("ws/proj");
        let c = checkout(tmp.path(), &origin, &dest);
        clone_or_update(&c, &mut |_| {}).unwrap();
        let marker = tmp.path().join("pwned");
        // Upstream: a file that the filter `x` would smudge.
        sh(
            &tmp.path().join("src"),
            "echo '* filter=x' > .gitattributes && echo two > b.txt && git add . && \
             git commit -q -m two && git push -q ../origin.git main",
        );
        let cfg = dest.join(".git/config");
        let mut edited = false;
        let result = clone_or_update(&c, &mut |_| {
            if !edited {
                edited = true;
                let mut text = std::fs::read_to_string(&cfg).unwrap();
                text.push_str(&format!(
                    "[filter \"x\"]\n\tsmudge = touch {}\n",
                    marker.display()
                ));
                std::fs::write(&cfg, text).unwrap();
            }
        });
        assert!(edited, "the fetch printed no progress line");
        match result {
            Err(GitError::Refused(m)) => assert!(m.contains("filter.x.smudge"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(!marker.exists());
        assert!(!dest.join("b.txt").exists());
    }

    #[test]
    fn update_refuses_a_symlinked_config() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        let dest = tmp.path().join("ws/proj");
        let c = checkout(tmp.path(), &origin, &dest);
        clone_or_update(&c, &mut |_| {}).unwrap();
        let cfg = dest.join(".git/config");
        let elsewhere = tmp.path().join("other-config");
        std::fs::rename(&cfg, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &cfg).unwrap();
        match clone_or_update(&c, &mut |_| {}) {
            Err(GitError::Refused(m)) => assert!(m.contains("not a regular file"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn clone_with_branch_and_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        sh(
            &tmp.path().join("src"),
            "git checkout -q -b dev && echo d > d.txt && git add d.txt && git commit -q -m d && \
             git push -q ../origin.git dev",
        );
        let dest = tmp.path().join("ws/proj");
        let mut c = checkout(tmp.path(), &origin, &dest);
        c.url = format!("file://{}", origin.display());
        c.branch = Some("dev".into());
        c.depth = Some(1);
        let out = clone_or_update(&c, &mut |_| {}).unwrap();
        assert_eq!(out.branch.as_deref(), Some("dev"));
        assert!(dest.join("d.txt").is_file());
        assert_eq!(git_out(&dest, &["rev-list", "--count", "HEAD"]), "1");
        // Switch an existing clone to main.
        c.branch = Some("main".into());
        let out = clone_or_update(&c, &mut |_| {}).unwrap();
        assert_eq!(out.branch.as_deref(), Some("main"));
        assert!(!dest.join("d.txt").exists());
    }

    #[test]
    fn refuses_bad_urls_and_dests() {
        let tmp = tempfile::tempdir().unwrap();
        let deny = vec![tmp.path().join("state")];
        std::fs::create_dir_all(tmp.path().join("state/agents/B/workspace/x")).unwrap();
        for url in [
            "ext::sh -c touch% /tmp/x",
            "-uhttps://x",
            "relative/path",
            &format!("{}/state/agents/B/workspace/x", tmp.path().display()),
            &format!("file://{}/state/agents/B/workspace/x", tmp.path().display()),
        ] {
            assert!(
                matches!(check_url(url, &deny), Err(GitError::Refused(_))),
                "{url}"
            );
        }
        for url in [
            "https://github.com/a/b.git",
            "git@github.com:a/b.git",
            "ssh://git@host:22/a/b",
            "/srv/git/a.git",
        ] {
            check_url(url, &deny).unwrap();
        }
        for b in ["-x", "a..b", "a b", "a:b", "x.lock", ""] {
            assert!(check_branch(b).is_err(), "{b}");
        }
        check_branch("feat/parser-2").unwrap();

        // A dest that is a symlink or a non-git dir is refused.
        let origin = bare_repo(tmp.path());
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("f"), "x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        for dest in [link, other] {
            let c = checkout(tmp.path(), &origin, &dest);
            assert!(
                matches!(clone_or_update(&c, &mut |_| {}), Err(GitError::Refused(_))),
                "{}",
                dest.display()
            );
        }
    }

    #[test]
    fn http_locations() {
        let l = http_location("https://user@GitHub.com/wiltaylor/terrarium.git").unwrap();
        assert_eq!(l.protocol, "https");
        assert_eq!(l.host, "github.com");
        assert_eq!(l.path, "wiltaylor/terrarium");
        let l = http_location("http://git.lan:8080/a/b/").unwrap();
        assert_eq!(l.host, "git.lan:8080");
        assert_eq!(l.path, "a/b");
        assert!(http_location("git@github.com:a/b.git").is_none());
        assert!(http_location("/srv/a.git").is_none());
        assert_eq!(normalize_path("/a/b.git/"), "a/b");
    }

    #[test]
    fn config_allowlist() {
        assert!(config_allowed("core.bare", "false"));
        assert!(config_allowed("remote.origin.url", "https://x"));
        assert!(config_allowed("branch.main.merge", "refs/heads/main"));
        assert!(config_allowed("credential.helper", WORKSPACE_HELPER));
        assert!(config_allowed("credential.helper", ""));
        assert!(config_allowed("user.email", "a@b"));
        assert!(!config_allowed("credential.helper", "store"));
        assert!(!config_allowed("core.hookspath", "/x"));
        assert!(!config_allowed("core.fsmonitor", "x"));
        assert!(!config_allowed("core.worktree", "/x"));
        assert!(!config_allowed("filter.lfs.smudge", "x"));
        assert!(!config_allowed("remote.origin.uploadpack", "x"));
        assert!(!config_allowed("remote.https://x.url", "x"));
        assert!(!config_allowed(
            "url.https://evil/.insteadof",
            "https://github.com/"
        ));
        assert!(!config_allowed("include.path", "x"));
        assert!(!config_allowed("http.proxy", "x"));
    }

    #[test]
    fn token_helper_answers_only_its_host() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunDirs::new(tmp.path());
        let c = Checkout {
            url: "https://github.com/a/b.git".into(),
            dest: tmp.path().join("d"),
            branch: None,
            depth: None,
            secret: Some(Secret::Token {
                username: "x-access-token".into(),
                token: zeroize::Zeroizing::new("tok'en".into()),
            }),
            store,
            deny_local_under: vec![],
            timeout: Duration::from_secs(5),
        };
        let r = Runner::new(&c).unwrap();
        let helper = r
            .config
            .iter()
            .find_map(|kv| kv.strip_prefix("credential.helper=!"))
            .unwrap()
            .to_owned();
        // The secret is not on the command line or in the env.
        assert!(!r.config.iter().any(|kv| kv.contains("tok'en")));
        assert!(!r.env.iter().any(|(_, v)| v.contains("tok'en")));
        let ask = |input: &str| {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(format!("{helper} get"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let o = child.wait_with_output().unwrap();
            String::from_utf8(o.stdout).unwrap()
        };
        assert_eq!(
            ask("protocol=https\nhost=github.com\n\n"),
            "username=x-access-token\npassword=tok'en\n"
        );
        assert_eq!(ask("protocol=https\nhost=evil.com\n\n"), "");
        assert_eq!(ask("protocol=http\nhost=github.com\n\n"), "");
        let run = r._run.as_ref().unwrap().path().to_owned();
        drop(r);
        assert!(!run.exists(), "the run dir is removed");
    }
}
