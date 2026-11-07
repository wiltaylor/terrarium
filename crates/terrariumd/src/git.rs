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
//! - ssh records the host keys (trust on first use) in
//!   `<state>/ssh/sandbox/ssh_known_hosts` ([`known_hosts_dir`]). Each
//!   sandbox shows that dir read-only at `/etc/ssh`, so git over ssh in the
//!   sandbox trusts the same keys.
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
//!
//! The repo browser (`crate::repo_browse`) reads checkouts with
//! [`ReadGit`]: the same hardening, more overrides ([`READ_HARDEN`]), and
//! only commands that run no filter, textconv, or external diff, so the
//! window above runs nothing there. git reads an agent checkout only in a
//! `bwrap` sandbox that shows the checkout (bound by its fd) and `/usr`,
//! and nothing else of the host ([`ReadGit::run`]). For `main` it reads a
//! bare mirror that the daemon owns ([`refresh_mirror`]), on the host.

use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, SFlag, fstatat};
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
    /// The hooks that the checkout gets ([`install_hooks`]).
    pub hooks: Hooks,
    /// The `origin` URL that a workspace checkout gets ([`Origin`]).
    pub origin: Origin,
    /// An update fetches from this local repo of the daemon (the mirror of
    /// a proxied repo), with no credential, not from `url` with the
    /// secret. So no git command with the token runs in a repo that the
    /// agent controls (review P4). A clone still uses `url` and the secret.
    pub fetch_from: Option<PathBuf>,
}

/// The `origin` URL of a workspace checkout (issue #11, PLAN §9 "The git
/// proxy"). The daemon itself always fetches from the stored URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Origin {
    /// Leave it: the clone URL after a clone, the agent's value after an
    /// update.
    #[default]
    Keep,
    /// The repo goes through the daemon's git proxy: `origin` (fetch and
    /// push URL) is this proxy URL.
    Proxy(String),
    /// The repo goes straight to the forge: an `origin` that starts with
    /// this proxy prefix (the repo was proxied before) goes back to the
    /// stored URL.
    Direct { proxy_prefix: String },
}

/// The git hooks of a repo in an agent checkout (PLAN §9, "Git hooks in
/// agent checkouts"). git never copies hooks on a clone, so the daemon
/// writes them into `.git/hooks` after each clone and update. They run
/// for the agent's own git commands in the sandbox. The daemon's own git
/// commands never run them ([`HARDEN`]: `core.hooksPath=/dev/null`).
///
/// A client-side hook is a guard against mistakes, not a hard control: the
/// agent can push with `--no-verify`, or edit `.git/hooks`. The hard rule
/// is the daemon's git proxy (`crate::git_proxy`, [`Origin::Proxy`]): it
/// reads the pushed refs, and the agent has no token to push past it.
/// A `direct` repo (ssh, a local repo, or `push_via = direct`) has only
/// the hook; there, a hard rule is a branch protection rule on the git
/// server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hooks {
    /// A tracked dir of the repo: each regular file in it at `HEAD` goes
    /// to `.git/hooks/<name>`, mode 0755.
    pub dir: Option<String>,
    /// Branch globs (`op/*`): the daemon writes a `pre-push` hook that
    /// refuses a push whose local or remote branch matches one. The
    /// repo's own `pre-push` from `dir` becomes `pre-push.repo` and runs
    /// after the check.
    pub push_deny: Vec<String>,
}

impl Hooks {
    fn is_empty(&self) -> bool {
        self.dir.is_none() && self.push_deny.is_empty()
    }
}

/// The name of the repo's own `pre-push` when the daemon writes the
/// `push_deny` hook in its place.
pub const REPO_PRE_PUSH: &str = "pre-push.repo";
/// The largest hook file that the daemon copies.
const MAX_HOOK: u64 = 1 << 20;
/// The most hook files that the daemon copies from one `hooks_dir`.
const MAX_HOOKS: usize = 64;

/// The `pre-push` hook of a `push_deny` list. The globs passed
/// `terrarium_core::config::check_push_deny_glob`, so they are safe as
/// shell `case` patterns.
pub fn push_deny_hook(push_deny: &[String]) -> String {
    let patterns = push_deny.join("|");
    format!(
        r#"#!/bin/sh
# Written by terrariumd from the repo's push_deny list. Each checkout by
# the daemon writes it again: change push_deny on the repo, not this file.
# The repo's own pre-push (hooks_dir) is {REPO_PRE_PUSH} and runs after it.
input=$(cat)
refused=
while read -r local_ref _ remote_ref _; do
  for ref in "$local_ref" "$remote_ref"; do
    branch=${{ref#refs/heads/}}
    test "$branch" = "$ref" && continue
    case "$branch" in
      {patterns})
        echo "pre-push: the branch $branch matches push_deny of this repo; Terrarium refuses the push" >&2
        refused=1
        ;;
    esac
  done
done <<TERRARIUM_REFS
$input
TERRARIUM_REFS
test -n "$refused" && exit 1
repo_hook="${{0%/*}}/{REPO_PRE_PUSH}"
if test -x "$repo_hook"; then
  if test -n "$input"; then printf '%s\n' "$input"; fi | "$repo_hook" "$@"
  exit $?
fi
exit 0
"#
    )
}

/// The checks of a hook file name from `hooks_dir`: a plain name that git
/// could run (`pre-commit`, `commit-msg`, ...).
fn hook_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Write the hooks of `c.hooks` into `.git/hooks` of `c.dest`, and answer
/// the names written. The files of `hooks_dir` come from the commit at
/// `HEAD` (`git ls-tree`, `git cat-file`), not from the work tree, so a
/// symlink in the work tree cannot make the daemon copy a file of the
/// host. Each file goes in by a new temp file and a rename, opened from
/// the fd of `.git/hooks` with no symlink followed. A `hooks_dir` that
/// `HEAD` does not have copies nothing. A hook that is there and not in
/// `hooks_dir` stays.
fn install_hooks(c: &Checkout, r: &Runner) -> Result<Vec<String>, GitError> {
    if c.hooks.is_empty() {
        return Ok(Vec::new());
    }
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(dir) = &c.hooks.dir {
        terrarium_core::config::check_hooks_dir(dir).map_err(GitError::Refused)?;
        files = hook_files(c, r, dir)?;
    }
    if !c.hooks.push_deny.is_empty() {
        terrarium_core::config::check_push_deny(&c.hooks.push_deny).map_err(GitError::Refused)?;
        if let Some(f) = files.iter_mut().find(|(n, _)| n == "pre-push") {
            REPO_PRE_PUSH.clone_into(&mut f.0);
        }
        files.push((
            "pre-push".to_owned(),
            push_deny_hook(&c.hooks.push_deny).into_bytes(),
        ));
    }
    let git_dir = open_git_dir(&c.dest, false)?;
    let shown = Path::new(".git");
    let hooks = match check_entry(&git_dir, "hooks", Want::OptDir, shown)? {
        Some(fd) => fd,
        None => {
            nix::sys::stat::mkdirat(&git_dir, "hooks", Mode::from_bits_truncate(0o755))
                .map_err(|e| io(format!(".git/hooks: {e}")))?;
            check_entry(&git_dir, "hooks", Want::Dir, shown)?
                .ok_or_else(|| refused_layout(&shown.join("hooks"), "is not there"))?
        }
    };
    let mut names = Vec::new();
    for (name, bytes) in files {
        write_hook(&hooks, &name, &bytes)?;
        names.push(name);
    }
    Ok(names)
}

/// The regular files directly in `dir` at `HEAD`: name and bytes. No
/// `HEAD`, or no such dir: none.
fn hook_files(c: &Checkout, r: &Runner, dir: &str) -> Result<Vec<(String, Vec<u8>)>, GitError> {
    let spec = format!("{dir}/");
    let Ok((list, _)) = r.exec_named(
        Some(&c.dest),
        &["ls-tree", "-z", "--full-tree", "HEAD", "--", &spec],
        "ls-tree",
        Some(1 << 20),
        r.deadline,
    ) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in list.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        // `<mode> <type> <object>\t<path>`
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut meta = meta.split(' ');
        let (Some(mode), Some(kind), Some(oid)) = (meta.next(), meta.next(), meta.next()) else {
            continue;
        };
        // A symlink (120000), a dir, or a submodule is not a hook file.
        if kind != "blob" || !matches!(mode, "100755" | "100644") {
            continue;
        }
        let Some(name) = path.strip_prefix(&spec).filter(|n| hook_name_ok(n)) else {
            continue;
        };
        if out.len() >= MAX_HOOKS {
            return Err(GitError::Refused(format!(
                "{dir} has more than {MAX_HOOKS} files; the daemon copies at most {MAX_HOOKS} hooks"
            )));
        }
        if !oid.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let (bytes, cut) = r.exec_named(
            Some(&c.dest),
            &["cat-file", "blob", oid],
            "cat-file",
            Some(usize::try_from(MAX_HOOK).unwrap_or(usize::MAX)),
            r.deadline,
        )?;
        if cut {
            return Err(GitError::Refused(format!(
                "{dir}/{name} is larger than {MAX_HOOK} bytes; the daemon does not copy it"
            )));
        }
        out.push((name.to_owned(), bytes));
    }
    Ok(out)
}

/// Write `bytes` to `<hooks>/<name>` with mode 0755: a new temp file
/// (`O_EXCL`, `O_NOFOLLOW`), then a rename over the old entry. A rename
/// replaces a symlink, never follows it.
fn write_hook(hooks: &OwnedFd, name: &str, bytes: &[u8]) -> Result<(), GitError> {
    use std::io::Write as _;
    let tmp = format!(".{name}.terrarium-tmp");
    let shown = |n: &str| format!(".git/hooks/{n}");
    match nix::unistd::unlinkat(hooks, tmp.as_str(), nix::unistd::UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => {}
        Err(e) => return Err(io(format!("{}: {e}", shown(&tmp)))),
    }
    let fd = openat(
        hooks,
        tmp.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o755),
    )
    .map_err(|e| io(format!("{}: {e}", shown(&tmp))))?;
    let mut f = std::fs::File::from(fd);
    f.write_all(bytes)
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt as _;
            f.set_permissions(std::fs::Permissions::from_mode(0o755))
        })
        .map_err(|e| io(format!("{}: {e}", shown(&tmp))))?;
    drop(f);
    nix::fcntl::renameat(hooks, tmp.as_str(), hooks, name)
        .map_err(|e| io(format!("{}: {e}", shown(name))))
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
    /// The hooks that the daemon wrote into `.git/hooks` ([`Hooks`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<String>,
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
    /// The daemon cannot run the command here: the repo browser has no
    /// read sandbox for an agent checkout ([`ReadGit`]).
    #[error("{0}")]
    Unavailable(String),
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

/// The file name of the repo host keys in [`known_hosts_dir`]: the name of
/// ssh's global known hosts file, so ssh in a sandbox reads it with no
/// config.
pub const KNOWN_HOSTS_FILE: &str = "ssh_known_hosts";

/// `<state>/ssh/sandbox`: the dir of the repo host keys. The daemon's git
/// over ssh records each new host key there (trust on first use, as
/// before), and each `bwrap` sandbox binds the dir read-only at
/// `/etc/ssh`, so `git fetch` and `git push` over ssh in the sandbox trust
/// the same keys as the daemon. The dir, not the file, is bound: a file
/// that is written again (a new inode) still shows in a running sandbox.
pub fn known_hosts_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("ssh").join("sandbox")
}

/// Make [`known_hosts_dir`] (0700, in the 0700 `<state>/ssh`) and a
/// [`KNOWN_HOSTS_FILE`] in it when it has none. The new file gets the
/// lines of the daemon user's `$HOME/.ssh/known_hosts`, the file that the
/// daemon's git used before, so an upgrade keeps the hosts that the daemon
/// trusts. Returns the dir.
pub fn ensure_known_hosts_dir(state_dir: &Path) -> std::io::Result<PathBuf> {
    let seed = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ssh/known_hosts"));
    ensure_known_hosts_dir_from(state_dir, seed.as_deref())
}

/// [`ensure_known_hosts_dir`] with the seed file given. Only a run that
/// makes the file copies `seed` (as it is: hashed lines stay hashed); a
/// later run leaves the file alone. A missing or unreadable seed leaves
/// the new file empty.
fn ensure_known_hosts_dir_from(state_dir: &Path, seed: Option<&Path>) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let ssh = state_dir.join("ssh");
    let dir = known_hosts_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    for d in [&ssh, &dir] {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    let file = dir.join(KNOWN_HOSTS_FILE);
    let mut new = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            add_published_keys(&file)?;
            return Ok(dir);
        }
        Err(e) => return Err(e),
    };
    let lines = match seed.map(std::fs::read) {
        Some(Ok(b)) => b,
        Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
            let f = file.display();
            tracing::warn!("cannot read the daemon's known hosts to seed {f}: {e}");
            Vec::new()
        }
        _ => Vec::new(),
    };
    if !lines.is_empty() {
        new.write_all(&lines)?;
        if !lines.ends_with(b"\n") {
            new.write_all(b"\n")?;
        }
    }
    drop(new);
    add_published_keys(&file)?;
    Ok(dir)
}

/// GitHub's published SSH host keys (TER-38), so git over ssh to
/// github.com works in a sandbox from the first start, before the daemon's
/// own git has met the host. Source: `ssh_keys` of
/// https://api.github.com/meta, read on 2026-11-05; the fingerprints
/// (SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU ED25519,
/// SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM ECDSA,
/// SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s RSA) match
/// `ssh_key_fingerprints` there and a key scan of github.com. Update them
/// here if GitHub rotates a key.
pub const PUBLISHED_HOST_KEYS: &[&str] = &[
    "github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
    "github.com ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=",
    "github.com ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCj7ndNxQowgcQnjshcLrqPEiiphnt+VTTvDP6mHBL9j1aNUkY4Ue1gvwnGLVlOhGeYrnZaMgRK6+PKCUXaDbC7qtbW8gIkhL7aGCsOr/C56SJMy/BCZfxd1nWzAOxSDPgVsmerOBYfNqltV9/hWCqBywINIR+5dIg6JTJ72pcEpEjcYgXkE2YEFXV1JHnsKgbLWNlhScqb2UmyRkQyytRLtL+38TGxkxCflmO+5Z8CSSNY7GidjMIZ7Q4zMjA2n1nGrlTDkzwDCsw+wqFPGQA179cnfGWOWRVruj16z6XyvxvjJwbz0wQZ75XK5tKSb7FNyeIEs4TT4jk+S4dhPeAUC5y+bDYirYgM4GC7uEnztnZyaVWQ7B381AK4Qdrwt51ZqExKbQpTUNn+EjqoTwvqNj4kqx5QUCI0ThS/YkOxJCXmPUWZbhjpCg56i+2aB6CmK2JGhn57K5mj0MNdBXA4/WnwH6XoPWJzK5Nyu2zB3nAZp+S5hpQs+p1vN1/wsjk=",
];

/// Add each line of [`PUBLISHED_HOST_KEYS`] whose key is not in `file` yet
/// (the same key type and key, on any line). A recorded line is never
/// changed or removed: a host keeps every key the file has.
fn add_published_keys(file: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let have = std::fs::read_to_string(file)?;
    let key_of = |line: &str| -> Option<(String, String)> {
        let mut w = line.split_whitespace();
        let first = w.next()?;
        // A line may start with a marker such as `@cert-authority`.
        let (kind, key) = if first.starts_with('@') {
            (w.nth(1)?, w.next()?)
        } else {
            (w.next()?, w.next()?)
        };
        Some((kind.to_owned(), key.to_owned()))
    };
    let known: std::collections::HashSet<(String, String)> = have
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(key_of)
        .collect();
    let missing: Vec<&str> = PUBLISHED_HOST_KEYS
        .iter()
        .copied()
        .filter(|l| key_of(l).is_some_and(|k| !known.contains(&k)))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new().append(true).open(file)?;
    if !have.is_empty() && !have.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    for l in missing {
        writeln!(f, "{l}")?;
    }
    Ok(())
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
/// The end of one command of [`Runner::spawn_wait_status`].
struct Waited {
    stdout: Vec<u8>,
    /// The stdout cap was full: git was stopped.
    over: bool,
    /// `None` when git was stopped at the cap.
    status: Option<std::process::ExitStatus>,
    /// The last lines of stderr.
    stderr: String,
}

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
        // The host keys go to the file that the sandboxes read
        // ([`known_hosts_dir`]). When the dir cannot be made, ssh still
        // runs: it only cannot record a new key.
        let state = c.store.state_dir();
        if let Err(e) = ensure_known_hosts_dir(state) {
            let dir = known_hosts_dir(state);
            tracing::warn!(dir = %dir.display(), "cannot make the repo known hosts dir: {e}");
        }
        let known = known_hosts_dir(state).join(KNOWN_HOSTS_FILE);
        ssh.push_str(&format!(
            " -o {}",
            sh_quote(&format!("UserKnownHostsFile={}", known.display()))
        ));
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
        let (out, _) = self.exec(cwd, args, progress, None, self.deadline)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Run one git command until `deadline`. With `cap`, keep at most
    /// `cap` bytes of stdout: at more, git is stopped and the answer is
    /// the first `cap` bytes and `true`.
    fn exec(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        progress: &mut dyn FnMut(&str),
        cap: Option<usize>,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), GitError> {
        let name = args.first().copied().unwrap_or("");
        self.exec_inner(cwd, args, name, progress, cap, deadline)
    }

    /// [`Runner::exec`] with no progress, and `name` for the error.
    fn exec_named(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        name: &str,
        cap: Option<usize>,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), GitError> {
        self.exec_inner(cwd, args, name, &mut |_| {}, cap, deadline)
    }

    fn exec_inner(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        name: &str,
        progress: &mut dyn FnMut(&str),
        cap: Option<usize>,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), GitError> {
        let cmd = self.command(cwd, args);
        self.spawn_wait(cmd, name, progress, cap, deadline)
    }

    /// The git command of [`Runner::exec`]: the overrides, the env, no
    /// stdin.
    fn command(&self, cwd: Option<&Path>, args: &[&str]) -> Command {
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
            .stdin(Stdio::null());
        cmd
    }

    /// Run `cmd` (git, or `bwrap` around git) until `deadline`, as
    /// [`Runner::exec`] says. The caller sets the program, the args, the
    /// env, and stdin.
    fn spawn_wait(
        &self,
        cmd: Command,
        name: &str,
        progress: &mut dyn FnMut(&str),
        cap: Option<usize>,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), GitError> {
        let r = self.spawn_wait_status(cmd, progress, cap, deadline)?;
        if let Some(status) = r.status
            && !status.success()
        {
            return Err(GitError::Failed {
                command: name.to_owned(),
                status: status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| format!("exit {c}")),
                stderr: r.stderr,
            });
        }
        Ok((r.stdout, r.over))
    }

    /// [`Runner::spawn_wait`] that does not fail on an exit code: the
    /// caller reads the status. `status` is `None` when the cap was full.
    fn spawn_wait_status(
        &self,
        mut cmd: Command,
        progress: &mut dyn FnMut(&str),
        cap: Option<usize>,
        deadline: Instant,
    ) -> Result<Waited, GitError> {
        use std::os::unix::process::CommandExt;
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a timeout kills ssh and the remote
            // helpers too.
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| io(format!("cannot run git: {e}")))?;
        let mut stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (full_tx, full_rx) = mpsc::channel::<()>();
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let Some(s) = stdout.as_mut() else {
                return (buf, false);
            };
            match cap {
                None => {
                    let _ = s.read_to_end(&mut buf);
                    (buf, false)
                }
                Some(cap) => {
                    // One byte more than the cap tells "more" from "exactly".
                    let _ = s.take(cap as u64 + 1).read_to_end(&mut buf);
                    let over = buf.len() > cap;
                    if over {
                        buf.truncate(cap);
                        let _ = full_tx.send(());
                    }
                    (buf, over)
                }
            }
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
        let kill = |child: &mut std::process::Child| {
            let pid = child.id().to_string();
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{pid}"))
                .status();
            let _ = child.kill();
            let _ = child.wait();
        };
        let mut over = false;
        let status = loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(l) => on_line(l, &mut tail),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            if full_rx.try_recv().is_ok() {
                // The cap is full: the rest of the output is not needed.
                over = true;
                kill(&mut child);
                break None;
            }
            if let Some(st) = child.try_wait().map_err(io)? {
                break Some(st);
            }
            if Instant::now() >= deadline {
                kill(&mut child);
                return Err(GitError::Timeout(self.timeout));
            }
        };
        let _ = err_thread.join();
        while let Ok(l) = rx.try_recv() {
            on_line(l, &mut tail);
        }
        let (out, full) = out_thread.join().unwrap_or_default();
        let stderr = tail.into_iter().collect::<Vec<_>>().join("\n");
        if over || full {
            return Ok(Waited {
                stdout: out,
                over: true,
                status: None,
                stderr,
            });
        }
        Ok(Waited {
            stdout: out,
            over: false,
            status,
            stderr,
        })
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

    /// The `origin` of a workspace checkout ([`Origin`]). The agent's
    /// `pushurl` of origin goes: a push would skip the proxy with it.
    fn write_origin(&self, c: &Checkout) -> Result<(), GitError> {
        let dest = &c.dest;
        let set = |url: &str| -> Result<(), GitError> {
            self.quiet(dest, &["config", "--local", "remote.origin.url", url])?;
            // No pushurl is fine: `--unset-all` then exits 5.
            let _ = self.quiet(
                dest,
                &["config", "--local", "--unset-all", "remote.origin.pushurl"],
            );
            Ok(())
        };
        match &c.origin {
            Origin::Keep => Ok(()),
            Origin::Proxy(url) => set(url),
            Origin::Direct { proxy_prefix } => {
                let now = self
                    .quiet(dest, &["config", "--local", "--get", "remote.origin.url"])
                    .unwrap_or_default();
                if now.trim().starts_with(proxy_prefix.as_str()) {
                    set(&c.url)
                } else {
                    Ok(())
                }
            }
        }
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
    let fd = open_git_dir(&c.dest, false)?;
    check_config_copy(&fd, &c.store, r)
}

/// The largest `.git/config` that the daemon copies and checks.
const MAX_CONFIG: u64 = 1 << 20;

/// Copy the `config` of the git dir `git_dir` (opened with no symlink
/// followed, [`open_git_dir`]) and check the copy. The file is opened
/// from the fd with `O_NOFOLLOW`, not by a path. No file is no config.
fn check_config_copy(git_dir: &OwnedFd, store: &RunDirs, r: &Runner) -> Result<(), GitError> {
    let not_file = || {
        GitError::Refused(
            ".git/config is not a regular file; the daemon updates only its own clones".into(),
        )
    };
    let fd = match openat(
        git_dir,
        "config",
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(()),
        Err(Errno::ELOOP) => return Err(not_file()),
        Err(e) => return Err(io(format!(".git/config: {e}"))),
    };
    let st = nix::sys::stat::fstat(&fd).map_err(io)?;
    if SFlag::from_bits_truncate(st.st_mode & SFlag::S_IFMT.bits()) != SFlag::S_IFREG {
        return Err(not_file());
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(MAX_CONFIG + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > MAX_CONFIG {
        return Err(GitError::Refused(format!(
            ".git/config is larger than {MAX_CONFIG} bytes"
        )));
    }
    let dir = store.run_dir().map_err(|e| io(e.message))?;
    let copy = dir
        .write_private("config", &bytes)
        .map_err(|e| io(e.message))?;
    let bad = unsafe_config_keys(r, &copy)?;
    if !bad.is_empty() {
        return Err(GitError::Refused(format!(
            "the repo config (.git/config) has keys the daemon does not run git with: {}; \
             remove them, or fetch inside the sandbox",
            bad.join(", ")
        )));
    }
    Ok(())
}

/// The checks before the daemon runs git in a repo the agent controls:
/// a plain `.git` directory, not a linked worktree, the layout of
/// [`check_git_dir`], and no symlink or special file anywhere under
/// `.git` ([`check_no_links`]).
///
/// The daemon's own fetch and merge into a workspace (`clone_or_update`)
/// run on the host, not in the read sandbox of [`ReadGit`], so this walk
/// stays the check for them. The messages name paths relative to the
/// checkout, never a path of the daemon.
fn check_workspace_repo(dest: &Path) -> Result<(), GitError> {
    let fd = open_git_dir(dest, false)?;
    let shown = Path::new(".git");
    check_git_dir(&fd, shown)?;
    check_no_links(&fd, shown)
}

const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);

/// The most entries under one `.git` that [`check_no_links`] reads.
const MAX_GIT_ENTRIES: usize = 1_000_000;
/// The most directory levels under `.git` that [`check_no_links`] goes
/// down. git itself makes few (`refs/heads/<a>/<b>`, `objects/<xx>`).
const MAX_GIT_DEPTH: usize = 64;
/// The longest that [`check_no_links`] takes for one `.git`.
const LINK_CHECK_TIME: Duration = Duration::from_secs(2);

fn refused_layout(shown: &Path, what: &str) -> GitError {
    GitError::Refused(format!(
        "{}: {what}; the daemon reads only a plain git dir",
        shown.display()
    ))
}

/// Open the git dir of `dir` with no symlink in the last components:
/// `<dir>/.git` for a checkout, `<dir>` itself when `bare`.
fn open_git_dir(dir: &Path, bare: bool) -> Result<OwnedFd, GitError> {
    let parent = match dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = dir
        .file_name()
        .ok_or_else(|| io("the repo path has no name"))?;
    let p = openat(
        nix::fcntl::AT_FDCWD,
        parent,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io(format!("{}: {e}", parent.display())))?;
    let d = openat(&p, name, DIR_FLAGS, Mode::empty()).map_err(|e| match e {
        Errno::ELOOP | Errno::ENOTDIR => GitError::Refused(
            "the repo dir is not a plain directory; the daemon reads only a plain git dir".into(),
        ),
        e => io(format!("{}: {e}", dir.display())),
    })?;
    if bare {
        return Ok(d);
    }
    open_dot_git(&d)
}

/// Open `.git` under the checkout dir `dir`, with no symlink followed.
fn open_dot_git(dir: &OwnedFd) -> Result<OwnedFd, GitError> {
    openat(dir, ".git", DIR_FLAGS, Mode::empty()).map_err(|e| match e {
        Errno::ELOOP | Errno::ENOTDIR => {
            refused_layout(Path::new(".git"), "is not a plain directory")
        }
        Errno::ENOENT => refused_layout(Path::new(".git"), "is not there"),
        e => io(format!(".git: {e}")),
    })
}

/// What one name in a git dir must be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    File,
    Dir,
    /// A regular file or nothing.
    OptFile,
    /// A directory or nothing.
    OptDir,
    /// Nothing, or an empty regular file.
    Empty,
    Absent,
}

/// Check `name` under `at` (no symlink followed). The answer is the dir,
/// opened, for a [`Want::Dir`] or a [`Want::OptDir`] that is there.
fn check_entry(
    at: &OwnedFd,
    name: &str,
    want: Want,
    shown: &Path,
) -> Result<Option<OwnedFd>, GitError> {
    let path = shown.join(name);
    let st = match fstatat(at, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(st) => Some(st),
        Err(Errno::ENOENT) => None,
        Err(e) => return Err(io(format!("{}: {e}", path.display()))),
    };
    let kind = st.map(|s| SFlag::from_bits_truncate(s.st_mode & SFlag::S_IFMT.bits()));
    let ok = match (want, kind) {
        (Want::File | Want::OptFile, Some(SFlag::S_IFREG)) => true,
        (Want::Dir | Want::OptDir, Some(SFlag::S_IFDIR)) => true,
        (Want::OptFile | Want::OptDir | Want::Empty | Want::Absent, None) => true,
        (Want::Empty, Some(SFlag::S_IFREG)) => st.is_some_and(|s| s.st_size == 0),
        _ => false,
    };
    if !ok {
        let what = match want {
            Want::File | Want::OptFile => "is not a regular file",
            Want::Dir | Want::OptDir => "is not a plain directory",
            Want::Empty | Want::Absent if name.contains("alternates") => {
                "is there; the daemon does not read the objects of another repo"
            }
            Want::Empty | Want::Absent => "is there: a linked worktree",
        };
        return Err(refused_layout(&path, what));
    }
    if kind == Some(SFlag::S_IFDIR) && matches!(want, Want::Dir | Want::OptDir) {
        return openat(at, name, DIR_FLAGS, Mode::empty())
            .map(Some)
            .map_err(|e| match e {
                Errno::ELOOP | Errno::ENOTDIR => refused_layout(&path, "is not a plain directory"),
                e => io(format!("{}: {e}", path.display())),
            });
    }
    Ok(None)
}

/// The layout of a git dir that the daemon reads (`.git` of a checkout,
/// or a bare repo), checked with no symlink followed: `HEAD`, `config`,
/// `index`, `packed-refs`, `shallow`, and `info/grafts` are regular files
/// (or are not there); `objects`, `objects/info`, `objects/pack`, `refs`,
/// and `info` are plain directories; there is no `commondir` or `gitdir`
/// (a linked worktree); and there is no `objects/info/alternates` (an
/// empty file is allowed) and no `objects/info/http-alternates`. An
/// alternates file, or a symlink in place of one of these names, would
/// let git read the objects or the files of another repo, or a file of
/// the daemon. Cheap: it runs before each read command ([`ReadGit::run`]).
fn check_git_dir(fd: &OwnedFd, shown: &Path) -> Result<(), GitError> {
    for (name, want) in [
        ("HEAD", Want::File),
        ("config", Want::OptFile),
        ("index", Want::OptFile),
        ("packed-refs", Want::OptFile),
        ("shallow", Want::OptFile),
        ("commondir", Want::Absent),
        ("gitdir", Want::Absent),
        ("refs", Want::Dir),
    ] {
        check_entry(fd, name, want, shown)?;
    }
    if let Some(info) = check_entry(fd, "info", Want::OptDir, shown)? {
        check_entry(&info, "grafts", Want::OptFile, &shown.join("info"))?;
    }
    let objects = check_entry(fd, "objects", Want::Dir, shown)?
        .ok_or_else(|| refused_layout(&shown.join("objects"), "is not there"))?;
    let shown_o = shown.join("objects");
    check_entry(&objects, "pack", Want::OptDir, &shown_o)?;
    if let Some(info) = check_entry(&objects, "info", Want::OptDir, &shown_o)? {
        let shown_i = shown_o.join("info");
        check_entry(&info, "alternates", Want::Empty, &shown_i)?;
        check_entry(&info, "http-alternates", Want::Absent, &shown_i)?;
    }
    Ok(())
}

/// Walk the whole git dir: each entry is a plain directory or a regular
/// file. A symlink anywhere (a loose object, a pack, a ref, `info/exclude`)
/// could make git read a file of another repo or of the daemon, and show
/// its bytes as an object or in an error message. The walk opens each
/// directory from the fd of its parent with `O_NOFOLLOW` (one fd for each
/// level, never a path from the top), so it is linear in the entries. It
/// refuses a git dir with more than [`MAX_GIT_ENTRIES`] entries, more than
/// [`MAX_GIT_DEPTH`] levels, or a walk longer than [`LINK_CHECK_TIME`].
fn check_no_links(fd: &OwnedFd, shown: &Path) -> Result<(), GitError> {
    let mut w = LinkWalk {
        seen: 0,
        until: Instant::now() + LINK_CHECK_TIME,
        shown,
    };
    w.dir(fd, &mut Vec::new())
}

/// The state of one [`check_no_links`] walk.
struct LinkWalk<'a> {
    seen: usize,
    until: Instant,
    shown: &'a Path,
}

impl LinkWalk<'_> {
    fn path(&self, rel: &[std::ffi::OsString]) -> PathBuf {
        rel.iter().fold(self.shown.to_path_buf(), |p, x| p.join(x))
    }

    /// Check the directory `fd` (at `rel` under the git dir), then each
    /// directory in it.
    fn dir(&mut self, fd: &OwnedFd, rel: &mut Vec<std::ffi::OsString>) -> Result<(), GitError> {
        if rel.len() > MAX_GIT_DEPTH {
            return Err(GitError::Refused(format!(
                "{} has more than {MAX_GIT_DEPTH} levels of directories; \
                 the daemon reads only a plain git dir",
                self.shown.display()
            )));
        }
        let mut subdirs = Vec::new();
        let mut d = nix::dir::Dir::from_fd(fd.try_clone().map_err(io)?).map_err(io)?;
        for e in d.iter() {
            let e = e.map_err(io)?;
            let name = e.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            self.seen += 1;
            if self.seen > MAX_GIT_ENTRIES {
                return Err(GitError::Refused(format!(
                    "{} has more than {MAX_GIT_ENTRIES} entries; run `git gc` in it",
                    self.shown.display()
                )));
            }
            if self.seen.is_multiple_of(256) && Instant::now() >= self.until {
                return Err(self.slow());
            }
            let os = std::ffi::OsStr::from_bytes(name).to_owned();
            let kind = match e.file_type() {
                Some(t) => t,
                None => {
                    let st =
                        fstatat(fd, os.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io)?;
                    match SFlag::from_bits_truncate(st.st_mode & SFlag::S_IFMT.bits()) {
                        SFlag::S_IFDIR => nix::dir::Type::Directory,
                        SFlag::S_IFREG => nix::dir::Type::File,
                        _ => nix::dir::Type::Symlink,
                    }
                }
            };
            match kind {
                nix::dir::Type::File => {}
                nix::dir::Type::Directory => subdirs.push(os),
                _ => {
                    return Err(refused_layout(
                        &self.path(rel).join(&os),
                        "is a symlink or a special file",
                    ));
                }
            }
        }
        drop(d);
        for name in subdirs {
            if Instant::now() >= self.until {
                return Err(self.slow());
            }
            let child = openat(fd, name.as_os_str(), DIR_FLAGS, Mode::empty()).map_err(|e| {
                let p = self.path(rel).join(&name);
                match e {
                    Errno::ELOOP | Errno::ENOTDIR => refused_layout(&p, "is not a plain directory"),
                    e => io(format!("{}: {e}", p.display())),
                }
            })?;
            rel.push(name);
            self.dir(&child, rel)?;
            rel.pop();
        }
        Ok(())
    }

    fn slow(&self) -> GitError {
        GitError::Refused(format!(
            "the check of {} took more than {}s; run `git gc` in it",
            self.shown.display(),
            LINK_CHECK_TIME.as_secs()
        ))
    }
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
            hooks: Hooks::default(),
            origin: Origin::default(),
            fetch_from: None,
        };
        Runner::new(&c).map(Self)
    }

    /// Run `git -C <cwd> <args>`; the answer is stdout. A failure has the
    /// last lines of stderr.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        self.0.quiet(cwd, args)
    }
}

/// More `-c` overrides for the read commands of the repo browser
/// (`crate::repo_browse`). They switch off each git feature that runs a
/// program named by config or by `.gitattributes` (external diff,
/// textconv, signature checks with `gpg.program`), and they fix the
/// output format, so that a key in the live repo config cannot change
/// what the parser reads.
const READ_HARDEN: &[&str] = &[
    "core.attributesFile=/dev/null",
    "diff.external=",
    "diff.noprefix=false",
    "diff.mnemonicPrefix=false",
    "diff.relative=false",
    "diff.renames=false",
    "diff.ignoreSubmodules=all",
    "core.quotePath=true",
    "color.ui=false",
    "log.showSignature=false",
    "log.decorate=false",
    "log.follow=false",
    "log.showRoot=true",
    "log.mailmap=false",
    "mailmap.file=/dev/null",
    "mailmap.blob=",
    "core.excludesFile=/dev/null",
    "i18n.logOutputEncoding=UTF-8",
    // A read command fetches nothing: no transport at all (a promisor
    // remote would fetch missing objects from another repo).
    "protocol.allow=never",
    "protocol.file.allow=never",
    "protocol.http.allow=never",
    "protocol.https.allow=never",
    "protocol.ssh.allow=never",
];

/// A runner for the read commands of the repo browser (PLAN §9a). It has
/// the hardening of a workspace update ([`HARDEN`], no global or system
/// config, no askpass), [`READ_HARDEN`], `GIT_ATTR_NOSYSTEM=1`,
/// `GIT_OPTIONAL_LOCKS=0` (git writes no index), and a deadline for each
/// command. It has no secret: it does not fetch.
///
/// git reads an agent checkout ([`CheckoutDir`]) only inside a `bwrap`
/// sandbox ([`ReadGit::run`]). With no usable `bwrap`, each read of an
/// agent checkout is [`GitError::Unavailable`]: there is no fallback to
/// git on the host. The daemon's own mirror is read on the host.
pub struct ReadGit {
    runner: Runner,
    per_command: Duration,
    jail: Jail,
}

/// Where git reads an agent checkout.
#[derive(Debug, Clone)]
enum Jail {
    /// In a sandbox of this `bwrap`.
    Bwrap(PathBuf),
    /// Nowhere: there is no usable `bwrap` (the reason).
    Missing(String),
    /// Tests only: git on the host, in the checkout. The hostile-config
    /// tests use it to show that the read commands themselves run no
    /// driver (a driver that runs inside the sandbox leaves no mark).
    #[cfg(test)]
    Host,
}

/// The path of the checkout inside the read sandbox.
const REPO_INSIDE: &str = "/repo";

/// The message of [`GitError::Unavailable`] for an agent checkout.
pub const NO_READ_SANDBOX: &str = "the repo browser needs bwrap for agent trees; \
     install bubblewrap (the Docker image has it), or read the main tree only";

/// The args of the read sandbox, before the bind of the checkout: no
/// namespace shared with the host (no network), a new session, an empty
/// env except `env`, and only `/usr` (and the `/lib`, `/lib64`, `/bin`,
/// `/sbin` symlinks or binds of [`crate::sandbox::SYSTEM_DIRS`]), a new
/// `/proc`, a minimal `/dev`, and an empty `/tmp`. No `/etc`, no `/home`,
/// no state dir, no work dir, no other checkout.
///
/// `/usr` is visible: it holds the programs and the libraries of the
/// image, and no secret (the secrets are under the state dir and in the
/// daemon's env, which the sandbox does not get). A symlink in the
/// checkout that points out of [`REPO_INSIDE`] finds nothing, or a
/// file of `/usr`.
fn jail_args(env: &[(String, String)]) -> Vec<String> {
    let mut a: Vec<String> = [
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ]
    .map(String::from)
    .into();
    a.extend(crate::sandbox::system_dir_args(
        &crate::sandbox::real_symlink,
    ));
    a.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"].map(String::from));
    let fixed = [("PATH", "/usr/bin:/bin"), ("HOME", "/nonexistent")];
    for (k, v) in fixed
        .iter()
        .map(|(k, v)| (*k, *v))
        .chain(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    {
        a.extend(["--setenv".to_owned(), k.to_owned(), v.to_owned()]);
    }
    a
}

/// After a failed probe of the read sandbox, the time until the next
/// probe. A short failure (no free user namespace or PID at that time)
/// does not stop the agent trees until a restart.
const JAIL_RETRY: Duration = Duration::from_secs(30);

/// After a failed probe, the pause before the one more probe that comes
/// before the failure is kept: a failure for a moment under load does not
/// close the agent trees for [`JAIL_RETRY`].
const JAIL_REPROBE: Duration = Duration::from_millis(250);

/// The last probe of the read sandbox and its time.
type JailCache = std::sync::Mutex<Option<(Jail, Instant)>>;

/// The read sandbox of this machine: `bwrap` on `PATH`, and a sandbox
/// with [`jail_args`] that runs `git --version`. A good probe is kept. A
/// failed probe is tried once more after [`JAIL_REPROBE`]; a second
/// failure is kept for [`JAIL_RETRY`], then the next call probes again.
fn read_jail() -> Jail {
    static JAIL: JailCache = std::sync::Mutex::new(None);
    cached_jail(&JAIL, Instant::now(), probe_jail, || {
        std::thread::sleep(JAIL_REPROBE);
    })
}

/// [`read_jail`] with the cache, the time, the probe, and the pause before
/// the second probe given.
fn cached_jail(
    cache: &JailCache,
    now: Instant,
    mut probe: impl FnMut() -> Jail,
    pause: impl FnOnce(),
) -> Jail {
    let mut c = match cache.lock() {
        Ok(c) => c,
        Err(p) => p.into_inner(),
    };
    match &*c {
        Some((j @ Jail::Bwrap(_), _)) => return j.clone(),
        Some((j, at)) if now.saturating_duration_since(*at) < JAIL_RETRY => return j.clone(),
        _ => {}
    }
    let mut j = probe();
    if matches!(j, Jail::Missing(_)) {
        pause();
        j = probe();
    }
    if let Jail::Missing(why) = &j {
        tracing::warn!("the repo browser cannot read agent trees: {why}");
    }
    *c = Some((j.clone(), now));
    j
}

fn probe_jail() -> Jail {
    let path = std::env::var("PATH").ok();
    let Some(bwrap) = crate::sandbox::which("bwrap", path.as_deref()) else {
        return Jail::Missing("bwrap is not on PATH".into());
    };
    let out = Command::new(&bwrap)
        .args(jail_args(&[]))
        .args(["--", "git", "--version"])
        .env_clear()
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Jail::Bwrap(bwrap),
        Ok(o) => Jail::Missing(format!(
            "bwrap cannot make the read sandbox here: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Jail::Missing(format!("cannot run {}: {e}", bwrap.display())),
    }
}

/// True when this machine has a usable read sandbox for agent checkouts.
pub fn read_sandbox_ready() -> bool {
    matches!(read_jail(), Jail::Bwrap(_))
}

/// The stdout of one read command.
#[derive(Debug, Clone)]
pub struct ReadOut {
    pub bytes: Vec<u8>,
    /// The output was over the cap: `bytes` is the first part only.
    pub truncated: bool,
}

impl ReadOut {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// An agent checkout, opened for the reads of one request: the fd of its
/// directory. [`CheckoutDir::open`] opens it once, with no symlink
/// followed under the workspace. Every later read goes through this fd,
/// never through a path that is resolved again: git binds it into the
/// read sandbox ([`ReadGit::run`]), and `crate::repo_browse` reads the
/// work tree from it. An agent that swaps the checkout dir or `.git` for
/// a symlink after the open changes nothing that the daemon reads.
#[derive(Debug)]
pub struct CheckoutDir {
    fd: OwnedFd,
    /// The name in logs (for example `agent:<ULID>`).
    label: String,
    /// The end of the request: [`READ_REQUEST_TIME`] after the open.
    until: Instant,
}

/// The longest that all the reads of one agent checkout take for one
/// request (the git commands and the work of the daemon), from
/// [`CheckoutDir::open`]. Each git command also has its own deadline, but
/// many slow commands do not add up past this one.
pub const READ_REQUEST_TIME: Duration = Duration::from_secs(20);

impl CheckoutDir {
    /// Open `<workspace>/<path>`. The workspace is the daemon's own dir
    /// (`<work>/<project>/<agent>`: no agent can replace it, it is a mount
    /// point in the sandbox); each part of `path` is opened from its parent
    /// with `O_NOFOLLOW`. The messages do not name the workspace.
    pub fn open(workspace: &Path, path: &str, label: impl Into<String>) -> Result<Self, GitError> {
        let mut fd = openat(
            nix::fcntl::AT_FDCWD,
            workspace,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| match e {
            Errno::ENOENT => GitError::Refused("the agent has no workspace".into()),
            e => io(format!("the workspace: {e}")),
        })?;
        let parts: Vec<&str> = path.split('/').collect();
        if path.is_empty()
            || path.starts_with('/')
            || parts
                .iter()
                .any(|p| matches!(*p, "" | "." | "..") || p.contains('\0'))
        {
            return Err(GitError::Refused(format!("bad checkout path `{path}`")));
        }
        for p in parts {
            fd = openat(&fd, p, DIR_FLAGS, Mode::empty()).map_err(|e| match e {
                Errno::ENOENT => GitError::Refused(format!("there is no checkout `{path}`")),
                Errno::ELOOP | Errno::ENOTDIR => {
                    GitError::Refused(format!("the checkout `{path}` is not a plain directory"))
                }
                e => io(format!("the checkout `{path}`: {e}")),
            })?;
        }
        Ok(Self {
            fd,
            label: label.into(),
            until: Instant::now() + READ_REQUEST_TIME,
        })
    }

    /// `Timeout` when the time of the request ([`READ_REQUEST_TIME`]) is
    /// over.
    pub fn in_time(&self) -> Result<(), GitError> {
        if Instant::now() >= self.until {
            return Err(GitError::Timeout(READ_REQUEST_TIME));
        }
        Ok(())
    }

    /// Tests only: the time of the request is over.
    #[cfg(test)]
    pub(crate) fn expire(&mut self) {
        self.until = Instant::now();
    }

    /// The fd of the checkout dir.
    pub fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    fn git_dir(&self) -> Result<OwnedFd, GitError> {
        open_dot_git(&self.fd)
    }
}

/// The repo of one read command.
#[derive(Debug, Clone, Copy)]
pub enum ReadRepo<'a> {
    /// The daemon's own bare mirror (`refresh_mirror`): git runs on the
    /// host.
    Mirror(&'a Path),
    /// An agent checkout: git runs in the read sandbox.
    Checkout(&'a CheckoutDir),
}

impl<'a> From<&'a Path> for ReadRepo<'a> {
    fn from(p: &'a Path) -> Self {
        Self::Mirror(p)
    }
}

impl<'a> From<&'a PathBuf> for ReadRepo<'a> {
    fn from(p: &'a PathBuf) -> Self {
        Self::Mirror(p)
    }
}

impl<'a> From<&'a CheckoutDir> for ReadRepo<'a> {
    fn from(c: &'a CheckoutDir) -> Self {
        Self::Checkout(c)
    }
}

impl ReadGit {
    pub fn new(per_command: Duration) -> Self {
        Self::with_jail(per_command, read_jail())
    }

    fn with_jail(per_command: Duration, jail: Jail) -> Self {
        let mut runner = Runner {
            config: HARDEN.iter().map(|s| (*s).to_owned()).collect(),
            env: Vec::new(),
            deadline: Instant::now() + per_command,
            timeout: per_command,
            _run: None,
        };
        runner
            .config
            .extend(READ_HARDEN.iter().map(|s| (*s).to_owned()));
        runner.env = vec![
            ("LC_ALL".to_owned(), "C".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_ATTR_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_OPTIONAL_LOCKS".to_owned(), "0".to_owned()),
            ("GIT_NO_REPLACE_OBJECTS".to_owned(), "1".to_owned()),
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
            ("GIT_ASKPASS".to_owned(), "false".to_owned()),
            ("SSH_ASKPASS".to_owned(), "false".to_owned()),
            ("GIT_PAGER".to_owned(), "cat".to_owned()),
            ("PAGER".to_owned(), "cat".to_owned()),
            // No network from a read command; `false` fails any ssh.
            ("GIT_SSH_COMMAND".to_owned(), "false".to_owned()),
            // No lazy fetch of a missing object from a promisor remote.
            ("GIT_NO_LAZY_FETCH".to_owned(), "1".to_owned()),
            // Empty: no alternate object store from the env.
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES".to_owned(), String::new()),
        ];
        Self {
            runner,
            per_command,
            jail,
        }
    }

    /// Tests only: the read commands of an agent checkout run on the host.
    #[cfg(test)]
    pub(crate) fn on_host(per_command: Duration) -> Self {
        Self::with_jail(per_command, Jail::Host)
    }

    /// Tests only: a machine with no usable `bwrap`.
    #[cfg(test)]
    pub(crate) fn without_sandbox(per_command: Duration) -> Self {
        Self::with_jail(per_command, Jail::Missing("no bwrap in this test".into()))
    }

    /// Tests only: a runner with the read sandbox, or `None` (with a
    /// note on stderr) when this machine cannot make one.
    #[cfg(test)]
    pub(crate) fn sandboxed(per_command: Duration) -> Option<Self> {
        match read_jail() {
            Jail::Bwrap(_) => Some(Self::new(per_command)),
            j => {
                eprintln!("skipped: no read sandbox here ({j:?})");
                None
            }
        }
    }

    /// True when this runner can read an agent checkout: it has the read
    /// sandbox (or, in tests, runs on the host).
    pub fn reads_checkouts(&self) -> bool {
        !matches!(self.jail, Jail::Missing(_))
    }

    /// `git --no-pager <args>` in `repo`, at most `cap` bytes of stdout.
    ///
    /// git gets the git dir and the work tree on the command line (no
    /// search in the parent dirs, no `core.worktree`). Just before git
    /// starts, [`check_git_dir`] checks the layout of the git dir again,
    /// for a clear refusal of an alternates file or a symlink.
    ///
    /// For an agent checkout, that check is not the boundary. git runs in
    /// `bwrap` ([`jail_args`]) with the checkout dir bound read-only at
    /// [`REPO_INSIDE`] by its fd: the fd of the [`CheckoutDir`] is the
    /// stdin of `bwrap`, and `--ro-bind-fd 0` binds the directory of that
    /// fd. (`bwrap` mounts the dir that the fd names, compares the device
    /// and inode of the mount with the fd, stops on a difference, and
    /// closes the fd before git starts.) So a swap of the checkout path
    /// has no effect, and inside the sandbox each symlink in `.git` that
    /// points to another repo, to the state dir, or to any file of the
    /// daemon finds nothing.
    ///
    /// The stderr of git never leaves this function: a failure is logged
    /// at debug level and is [`GitError::Failed`] with an empty `stderr`
    /// (with an `include.path` added in a race, stderr could show if a
    /// file of the daemon exists, or a line of it).
    pub fn run<'a>(
        &self,
        repo: impl Into<ReadRepo<'a>>,
        args: &[&str],
        cap: usize,
    ) -> Result<ReadOut, GitError> {
        let repo = repo.into();
        let name = args.first().copied().unwrap_or("");
        let mut deadline = Instant::now() + self.per_command;
        if let ReadRepo::Checkout(c) = repo {
            c.in_time()?;
            deadline = deadline.min(c.until);
        }
        let r = match repo {
            ReadRepo::Mirror(dir) => {
                let fd = open_git_dir(dir, true)?;
                check_git_dir(&fd, Path::new(""))?;
                drop(fd);
                let mut full = vec!["--no-pager", "--git-dir=.", "--work-tree=."];
                full.extend_from_slice(args);
                // `--no-pager` goes before the subcommand, so `exec` names
                // the subcommand from args.
                self.runner
                    .exec_named(Some(dir), &full, name, Some(cap), deadline)
            }
            ReadRepo::Checkout(c) => {
                check_git_dir(&c.git_dir()?, Path::new(".git"))?;
                self.run_checkout(c, args, name, cap, deadline)
            }
        };
        let label = match repo {
            ReadRepo::Mirror(dir) => dir.display().to_string(),
            ReadRepo::Checkout(c) => c.label.clone(),
        };
        match r {
            Ok((bytes, truncated)) => Ok(ReadOut { bytes, truncated }),
            Err(GitError::Failed {
                command,
                status,
                stderr,
            }) => {
                tracing::debug!(tree = %label, %command, %status, %stderr, "a repo browser read failed");
                Err(GitError::Failed {
                    command,
                    status,
                    stderr: String::new(),
                })
            }
            Err(e) => Err(e),
        }
    }

    fn run_checkout(
        &self,
        c: &CheckoutDir,
        args: &[&str],
        name: &str,
        cap: usize,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), GitError> {
        let bwrap = match &self.jail {
            Jail::Bwrap(b) => b,
            Jail::Missing(why) => {
                tracing::debug!(tree = %c.label, "no read sandbox: {why}");
                return Err(GitError::Unavailable(NO_READ_SANDBOX.into()));
            }
            #[cfg(test)]
            Jail::Host => {
                use std::os::fd::AsRawFd as _;
                let p = std::fs::read_link(format!("/proc/self/fd/{}", c.fd.as_raw_fd()))
                    .map_err(io)?;
                let mut full = vec!["--no-pager", "--git-dir=.git", "--work-tree=."];
                full.extend_from_slice(args);
                return self
                    .runner
                    .exec_named(Some(&p), &full, name, Some(cap), deadline);
            }
        };
        let mut cmd = Command::new(bwrap);
        cmd.env_clear()
            .args(jail_args(&self.runner.env))
            .args(["--ro-bind-fd", "0", REPO_INSIDE, "--chdir", REPO_INSIDE])
            .args(["--", "git"]);
        for kv in &self.runner.config {
            cmd.arg("-c").arg(kv);
        }
        cmd.arg("--no-pager")
            .arg(format!("--git-dir={REPO_INSIDE}/.git"))
            .arg(format!("--work-tree={REPO_INSIDE}"))
            .args(args)
            // The child gets the fd as its stdin (fd 0, no close-on-exec);
            // no other process of the daemon gets it.
            .stdin(Stdio::from(c.fd.try_clone().map_err(io)?));
        // At the deadline or the cap, `spawn_wait` kills the process group
        // of `bwrap`; `--die-with-parent` then kills git in the sandbox.
        self.runner
            .spawn_wait(cmd, name, &mut |_| {}, Some(cap), deadline)
    }

    /// [`ReadGit::run`] as text, with a cap of 4 MiB.
    pub fn text<'a>(
        &self,
        repo: impl Into<ReadRepo<'a>>,
        args: &[&str],
    ) -> Result<String, GitError> {
        Ok(self.run(repo, args, 4 << 20)?.text())
    }

    /// The checks before the reads of a repo that an agent controls:
    /// `.git` is a plain directory (not a symlink, not a linked worktree),
    /// the layout of [`check_git_dir`] (no alternates file), and a copy of
    /// `.git/config` has only keys of the allowlist ([`config_allowed`]).
    ///
    /// It also walks all of `.git` once ([`check_no_links`], bounded):
    /// a FIFO or a device in `.git` (a ref, the HEAD ref) would hold each
    /// git command until its deadline. The walk is not the boundary for a
    /// symlink (the read sandbox is), but it gives a clear `409` for one.
    pub fn check_checkout(&self, c: &CheckoutDir, store: &RunDirs) -> Result<(), GitError> {
        let gd = c.git_dir()?;
        let shown = Path::new(".git");
        check_git_dir(&gd, shown)?;
        check_no_links(&gd, shown)?;
        check_config_copy(&gd, store, &self.runner)
    }
}

/// The alternates files of a bare repo. With one of them, git reads the
/// objects of another repo.
fn alternates_files(git_dir: &Path) -> [PathBuf; 2] {
    let info = git_dir.join("objects").join("info");
    [info.join("alternates"), info.join("http-alternates")]
}

/// Clone `c.url` into the bare mirror `c.dest`, or fetch every branch into
/// the mirror that is there (PLAN §9a). The mirror is the daemon's own:
/// no agent can write it, so its config is the one that `git clone`
/// wrote. A first clone goes to a temp dir beside `c.dest`, then a rename,
/// so a failed clone leaves no half mirror. Blocking.
///
/// The clone uses `--no-local`: a local source is read through
/// `git-upload-pack`, as a remote one is. A plain local clone copies the
/// source's `objects/info/alternates`, and the browser would then serve
/// the objects of the repo that the file names. A mirror with an
/// alternates file (from an older daemon) is removed and cloned again;
/// after each clone and fetch, the daemon makes sure that there is no
/// alternates file.
pub fn refresh_mirror(c: &Checkout) -> Result<(), GitError> {
    check_url(&c.url, &c.deny_local_under)?;
    let r = Runner::new(c)?;
    let parent = c
        .dest
        .parent()
        .ok_or_else(|| io("the mirror path has no parent"))?;
    std::fs::create_dir_all(parent).map_err(io)?;
    if alternates_files(&c.dest)
        .iter()
        .any(|f| std::fs::symlink_metadata(f).is_ok())
    {
        // Objects can be missing from the mirror's own store: a fetch
        // would not get them again. Clone it again.
        tracing::warn!(mirror = %c.dest.display(), "the mirror has an alternates file; cloning it again");
        std::fs::remove_dir_all(&c.dest).map_err(io)?;
    }
    if std::fs::symlink_metadata(c.dest.join("HEAD")).is_ok() {
        let args = [
            "fetch",
            "--quiet",
            "--prune",
            "--no-tags",
            "--no-recurse-submodules",
            "--upload-pack=git-upload-pack",
            "--",
            c.url.as_str(),
            "+refs/heads/*:refs/heads/*",
        ];
        r.git(Some(&c.dest), &args, &mut |_| {})?;
        return remove_alternates(&c.dest);
    }
    let name = c
        .dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mirror".into());
    let tmp = parent.join(format!(".{name}.{}", ulid::Ulid::new()));
    let tmp_s = tmp.to_string_lossy().into_owned();
    let cloned = r.git(
        None,
        &[
            "clone",
            "--quiet",
            "--bare",
            "--no-local",
            "--no-tags",
            "--",
            c.url.as_str(),
            tmp_s.as_str(),
        ],
        &mut |_| {},
    );
    if let Err(e) = cloned.and_then(|_| remove_alternates(&tmp)) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &c.dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(io(e));
    }
    Ok(())
}

/// Remove the alternates files of the bare repo `git_dir`, if any.
fn remove_alternates(git_dir: &Path) -> Result<(), GitError> {
    for f in alternates_files(git_dir) {
        match std::fs::remove_file(&f) {
            Ok(()) => tracing::warn!(file = %f.display(), "removed an alternates file"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(format!("{}: {e}", f.display()))),
        }
    }
    Ok(())
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
    // the objects of a local source repo through them. `--no-local`: a
    // local clone copies the source's `objects/info/alternates`, and the
    // daemon refuses a checkout with one ([`check_git_dir`]).
    let mut args = vec!["clone", "--progress", "--no-hardlinks", "--no-local"];
    if let Some(b) = &c.branch {
        args.extend(["--branch", b.as_str()]);
    }
    if let Some(d) = depth {
        args.extend(["--depth", d]);
    }
    args.extend(["--", c.url.as_str(), dest.as_str()]);
    r.git(None, &args, progress)?;
    r.write_local_config(&c.dest)?;
    r.write_origin(c)?;
    let hooks = install_hooks(c, &r)?;
    Ok(Outcome {
        action: Action::Cloned,
        head: r.head(&c.dest),
        branch: r.branch(&c.dest),
        note: None,
        hooks,
    })
}

fn update(
    c: &Checkout,
    depth: Option<&str>,
    progress: &mut dyn FnMut(&str),
) -> Result<Outcome, GitError> {
    check_workspace_repo(&c.dest)?;
    // From the daemon's mirror: no secret in this run at all.
    let no_secret;
    let (r, from) = match &c.fetch_from {
        Some(m) => {
            no_secret = Checkout {
                secret: None,
                ..c.clone()
            };
            (Runner::new(&no_secret)?, m.to_string_lossy().into_owned())
        }
        None => (Runner::new(c)?, c.url.clone()),
    };
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
    args.extend(["--", from.as_str(), refspec.as_str()]);
    r.git(Some(dest), &args, progress)?;
    r.write_local_config(dest)?;
    r.write_origin(c)?;
    // The agent may have edited the config during the fetch. Check again
    // before the commands that touch the work tree (they can run filters).
    check_config_snapshot(c, &r)?;
    let mut out = fast_forward(&r, dest, &branch, current.as_deref())?;
    // The hooks of the new HEAD.
    out.hooks = install_hooks(c, &r)?;
    Ok(out)
}

/// The work tree part of [`update`]: switch to `branch` and fast-forward
/// it to `origin/<branch>`, when the work tree is clean.
fn fast_forward(
    r: &Runner,
    dest: &Path,
    branch: &str,
    current: Option<&str>,
) -> Result<Outcome, GitError> {
    let status = r.quiet(dest, &["status", "--porcelain", "--untracked-files=no"])?;
    let not_updated = |note: String| Outcome {
        action: Action::NotUpdated,
        head: r.head(dest),
        branch: r.branch(dest),
        note: Some(note),
        hooks: Vec::new(),
    };
    if !status.trim().is_empty() {
        return Ok(not_updated(format!(
            "the work tree has local changes; fetched origin/{branch}, did not fast-forward"
        )));
    }
    let remote_ref = format!("refs/remotes/origin/{branch}");
    if current != Some(branch) {
        let local_ref = format!("refs/heads/{branch}");
        let exists = r
            .quiet(dest, &["rev-parse", "-q", "--verify", &local_ref])
            .is_ok();
        let switched = if exists {
            r.quiet(dest, &["checkout", "-q", branch])
        } else {
            r.quiet(
                dest,
                &["checkout", "-q", "--no-track", "-b", branch, &remote_ref],
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
        hooks: Vec::new(),
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

/// The head of `branch` on the remote `c.url`, read with `git ls-remote`
/// and the secret of `c` (TER-103: in the daemon, not on a target).
/// `None`: the remote has no such branch. Blocking.
pub fn ls_remote(c: &Checkout, branch: &str) -> Result<Option<String>, GitError> {
    check_url(&c.url, &c.deny_local_under)?;
    check_branch(branch)?;
    let r = Runner::new(c)?;
    let refname = format!("refs/heads/{branch}");
    let out = r.git(
        None,
        &[
            "ls-remote",
            "--upload-pack=git-upload-pack",
            "--",
            c.url.as_str(),
            refname.as_str(),
        ],
        &mut |_| {},
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .find(|(_, r)| *r == refname)
        .map(|(sha, _)| sha.to_owned()))
}

/// What a merge onto the head of a [`JobRepo`] gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStep {
    /// The new head: a merge commit, or the old head when the branch was
    /// in it already.
    Merged(String),
    /// The branch conflicts; the files with a conflict.
    Conflict(Vec<String>),
    /// The merge failed without a conflict (for example unrelated
    /// histories); git's message.
    Failed(String),
}

/// The repo of the daemon for the steps of one queue job that hold the
/// repo credential (TER-103, docs/api.md §35): a bare repo at `c.dest` in
/// the daemon's state dir, which no target, sandbox, or recipe can reach.
/// It fetches with the secret, merges without a work tree (`git
/// merge-tree`, so no checkout, filter, or hook runs), writes the bundle
/// that the target clones from, and pushes the recorded commit. Only
/// [`JobRepo::fetch`] and [`JobRepo::push`] get the secret. Blocking.
pub struct JobRepo {
    c: Checkout,
}

impl JobRepo {
    /// A new, empty bare repo at `c.dest` (an old one there is removed),
    /// in a 0700 parent dir.
    pub fn create(c: Checkout) -> Result<Self, GitError> {
        use std::os::unix::fs::PermissionsExt;
        check_url(&c.url, &c.deny_local_under)?;
        let parent = c
            .dest
            .parent()
            .ok_or_else(|| io("the job repo path has no parent"))?;
        std::fs::create_dir_all(parent).map_err(io)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        if std::fs::symlink_metadata(&c.dest).is_ok() {
            std::fs::remove_dir_all(&c.dest).map_err(io)?;
        }
        let me = Self { c };
        let dest = me.c.dest.to_string_lossy().into_owned();
        me.runner(false)?.git(
            None,
            &["init", "-q", "--bare", "--template=", dest.as_str()],
            &mut |_| {},
        )?;
        Ok(me)
    }

    /// The path of the bare repo.
    pub fn path(&self) -> &Path {
        &self.c.dest
    }

    /// A runner with a new deadline; with the secret only for `secret`.
    fn runner(&self, secret: bool) -> Result<Runner, GitError> {
        if secret {
            return Runner::new(&self.c);
        }
        Runner::new(&Checkout {
            secret: None,
            ..self.c.clone()
        })
    }

    fn local(&self, args: &[&str]) -> Result<String, GitError> {
        self.runner(false)?.quiet(&self.c.dest, args)
    }

    /// Fetch `specs` (`+refs/heads/x:refs/y`) from `c.url`, with the secret.
    pub fn fetch(&self, specs: &[String]) -> Result<(), GitError> {
        let mut args = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "--upload-pack=git-upload-pack",
            "--",
            self.c.url.as_str(),
        ];
        args.extend(specs.iter().map(String::as_str));
        self.runner(true)?
            .git(Some(&self.c.dest), &args, &mut |_| {})
            .map(|_| ())
    }

    /// The commit that `rev` names, if any.
    pub fn commit_of(&self, rev: &str) -> Result<Option<String>, GitError> {
        let spec = format!("{rev}^{{commit}}");
        match self.local(&["rev-parse", "--verify", "-q", "--end-of-options", &spec]) {
            Ok(s) => Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
            Err(GitError::Failed { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Is `a` an ancestor of `b` (or `b` itself)?
    pub fn is_ancestor(&self, a: &str, b: &str) -> Result<bool, GitError> {
        match self.local(&["merge-base", "--is-ancestor", a, b]) {
            Ok(_) => Ok(true),
            Err(GitError::Failed { status, .. }) if status == "exit 1" => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The merge-base of `a` and `b`, if they have one.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>, GitError> {
        match self.local(&["merge-base", "--end-of-options", a, b]) {
            Ok(s) => Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
            Err(GitError::Failed { status, .. }) if status == "exit 1" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Point `refname` at `sha`.
    pub fn set_ref(&self, refname: &str, sha: &str) -> Result<(), GitError> {
        self.local(&["update-ref", refname, sha]).map(|_| ())
    }

    /// Merge `other` onto `head` with a merge commit (as `merge --no-ff`
    /// would) with `message`. No work tree: `git merge-tree --write-tree`
    /// makes the tree, `git commit-tree` the commit.
    pub fn merge(&self, head: &str, other: &str, message: &str) -> Result<MergeStep, GitError> {
        if self.is_ancestor(other, head)? {
            return Ok(MergeStep::Merged(head.to_owned()));
        }
        let r = self.runner(false)?;
        let cmd = r.command(
            Some(&self.c.dest),
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                head,
                other,
            ],
        );
        let w = r.spawn_wait_status(cmd, &mut |_| {}, None, r.deadline)?;
        let out = String::from_utf8_lossy(&w.stdout).into_owned();
        let mut lines = out.lines();
        let tree = lines.next().unwrap_or("").trim().to_owned();
        match w.status.and_then(|s| s.code()) {
            Some(0) if !tree.is_empty() => {}
            Some(1) => {
                let mut files: Vec<String> = lines
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect();
                files.dedup();
                return Ok(MergeStep::Conflict(files));
            }
            _ => {
                let msg = if w.stderr.trim().is_empty() {
                    out.trim().to_owned()
                } else {
                    w.stderr.trim().to_owned()
                };
                return Ok(MergeStep::Failed(msg));
            }
        }
        let sha = self.local(&[
            "-c",
            "user.name=terrarium",
            "-c",
            "user.email=terrarium@localhost",
            "commit-tree",
            &tree,
            "-p",
            head,
            "-p",
            other,
            "-m",
            message,
        ])?;
        Ok(MergeStep::Merged(sha.trim().to_owned()))
    }

    /// Write a bundle of `refname` (with its whole history) to `out`.
    pub fn bundle(&self, refname: &str, out: &Path) -> Result<(), GitError> {
        let o = out.to_string_lossy().into_owned();
        self.local(&["bundle", "create", "-q", o.as_str(), refname])
            .map(|_| ())
    }

    /// Push the commit `sha` to `refs/heads/<branch>` of `c.url`, with the
    /// secret. Not forced: a remote branch that moved fails the push.
    pub fn push(&self, sha: &str, branch: &str) -> Result<(), GitError> {
        check_branch(branch)?;
        let spec = format!("{sha}:refs/heads/{branch}");
        self.runner(true)?
            .git(
                Some(&self.c.dest),
                &[
                    "push",
                    "--quiet",
                    "--no-verify",
                    "--receive-pack=git-receive-pack",
                    "--",
                    self.c.url.as_str(),
                    spec.as_str(),
                ],
                &mut |_| {},
            )
            .map(|_| ())
    }
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
            hooks: Hooks::default(),
            origin: Origin::default(),
            fetch_from: None,
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

    /// `git` in `dir` with no global or system config (the test machine's
    /// `core.hooksPath` must not apply). The exit status and stderr.
    fn git_clean(dir: &Path, args: &[&str]) -> (bool, String) {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    }

    /// Issue #1: a clone gets the repo's tracked hooks and the
    /// `push_deny` guard; both run for a push from the checkout. An
    /// update writes the hooks of the new `HEAD`.
    #[test]
    fn a_checkout_gets_the_repo_hooks_and_the_push_deny_guard() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        sh(
            &tmp.path().join("src"),
            "mkdir -p .githooks/sub && \
             printf '#!/bin/sh\nwhile read -r l _ _ _; do case \"$l\" in refs/heads/op/*) echo \"repo hook: op/ stays local\" >&2; exit 1;; esac; done\nexit 0\n' > .githooks/pre-push && \
             printf '#!/bin/sh\nexit 0\n' > .githooks/commit-msg && \
             echo x > .githooks/sub/inner && ln -s /etc/hostname .githooks/evil && \
             git add .githooks && git commit -q -m hooks && git push -q ../origin.git main",
        );
        let dest = tmp.path().join("state/agents/A/workspace/proj");
        let mut c = checkout(tmp.path(), &origin, &dest);
        c.deny_local_under = vec![];
        c.hooks = Hooks {
            dir: Some(".githooks".into()),
            push_deny: vec!["wip/*".into()],
        };
        let out = clone_or_update(&c, &mut |_| {}).unwrap();
        let mut names = out.hooks.clone();
        names.sort();
        assert_eq!(names, ["commit-msg", "pre-push", "pre-push.repo"]);
        let hooks = dest.join(".git/hooks");
        assert!(!hooks.join("evil").exists(), "a symlink is not a hook file");
        assert!(!hooks.join("inner").exists() && !hooks.join("sub").exists());
        for n in &names {
            let mode = std::fs::metadata(hooks.join(n))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "{n}");
        }
        assert!(
            std::fs::read_to_string(hooks.join("pre-push"))
                .unwrap()
                .contains("wip/*")
        );

        // A push from the checkout: the guard, then the repo's own hook.
        let (ok, err) = git_clean(&dest, &["checkout", "-q", "-b", "wip/x"]);
        assert!(ok, "{err}");
        let (ok, err) = git_clean(&dest, &["push", "-q", "origin", "wip/x"]);
        assert!(!ok, "push_deny refuses wip/x");
        assert!(err.contains("matches push_deny"), "{err}");
        // `wip/x` on the remote side refuses too.
        let (ok, _) = git_clean(&dest, &["push", "-q", "origin", "HEAD:refs/heads/wip/y"]);
        assert!(!ok);
        let (ok, err) = git_clean(&dest, &["checkout", "-q", "-b", "op/x"]);
        assert!(ok, "{err}");
        let (ok, err) = git_clean(&dest, &["push", "-q", "origin", "op/x"]);
        assert!(!ok, "the repo hook refuses op/x");
        assert!(err.contains("repo hook: op/ stays local"), "{err}");
        let (ok, err) = git_clean(&dest, &["checkout", "-q", "-b", "feature"]);
        assert!(ok, "{err}");
        let (ok, err) = git_clean(&dest, &["push", "-q", "origin", "feature"]);
        assert!(ok, "{err}");
        // `--no-verify` skips a client hook: it is a guard, not a control.
        let (ok, err) = git_clean(&dest, &["push", "-q", "--no-verify", "origin", "wip/x"]);
        assert!(ok, "{err}");

        // An update writes the hooks of the new HEAD.
        sh(
            &tmp.path().join("src"),
            "printf '#!/bin/sh\n# v2\nexit 0\n' > .githooks/commit-msg && \
             git commit -q -am v2 && git push -q ../origin.git main",
        );
        let (ok, err) = git_clean(&dest, &["checkout", "-q", "main"]);
        assert!(ok, "{err}");
        let out = clone_or_update(&c, &mut |_| {}).unwrap();
        assert_eq!(out.action, Action::Updated);
        assert!(
            std::fs::read_to_string(hooks.join("commit-msg"))
                .unwrap()
                .contains("v2")
        );
        // No hooks: nothing written, nothing in the answer.
        c.hooks = Hooks::default();
        assert!(clone_or_update(&c, &mut |_| {}).unwrap().hooks.is_empty());
        // A `hooks_dir` that HEAD does not have copies nothing.
        c.hooks.dir = Some("no-such-dir".into());
        assert!(clone_or_update(&c, &mut |_| {}).unwrap().hooks.is_empty());
    }

    /// A hook goes in by a rename: a symlink in its place is replaced,
    /// and its target stays as it was.
    #[test]
    fn write_hook_replaces_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks = tmp.path().join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, hooks.join("pre-commit")).unwrap();
        let fd = openat(nix::fcntl::AT_FDCWD, &hooks, DIR_FLAGS, Mode::empty()).unwrap();
        write_hook(&fd, "pre-commit", b"#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        let m = std::fs::symlink_metadata(hooks.join("pre-commit")).unwrap();
        assert!(m.is_file());
        assert!(!hooks.join(".pre-commit.terrarium-tmp").exists());
    }

    #[test]
    fn push_deny_hook_text() {
        let h = push_deny_hook(&["op/*".into(), "wip".into()]);
        assert!(h.starts_with("#!/bin/sh\n"));
        assert!(h.contains("      op/*|wip)\n"), "{h}");
        assert!(h.contains(REPO_PRE_PUSH));
        assert!(hook_name_ok("pre-push") && hook_name_ok("commit-msg"));
        assert!(!hook_name_ok(".hidden") && !hook_name_ok("a/b") && !hook_name_ok(""));
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

    /// N2 (docs/security-review.md §15): the walk of `.git` before an
    /// update opens each dir from its parent's fd, so a deep chain costs
    /// one open for each level; it stops at [`MAX_GIT_DEPTH`] levels. A
    /// loose object as a symlink is refused. The messages name no path of
    /// the daemon.
    #[test]
    fn the_link_walk_is_linear_and_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        let dest = tmp.path().join("ws/proj");
        let c = checkout(tmp.path(), &origin, &dest);
        clone_or_update(&c, &mut |_| {}).unwrap();
        check_workspace_repo(&dest).unwrap();

        // A chain of 2000 dirs under `.git` (too long for one path).
        let mut fd = open_git_dir(&dest, false).unwrap();
        for _ in 0..2000 {
            nix::sys::stat::mkdirat(&fd, "d", Mode::from_bits_truncate(0o755)).unwrap();
            fd = openat(&fd, "d", DIR_FLAGS, Mode::empty()).unwrap();
        }
        drop(fd);
        let start = Instant::now();
        match check_workspace_repo(&dest) {
            Err(GitError::Refused(m)) => {
                assert!(m.contains("levels"), "{m}");
                assert!(!m.contains(tmp.path().to_str().unwrap()), "{m}");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert!(matches!(
            clone_or_update(&c, &mut |_| {}),
            Err(GitError::Refused(m)) if m.contains("levels")
        ));
        sh(&dest, "rm -rf .git/d");

        // A deep chain under the cap is fine: each level is one open.
        let mut fd = open_git_dir(&dest, false).unwrap();
        for _ in 0..MAX_GIT_DEPTH - 1 {
            nix::sys::stat::mkdirat(&fd, "d", Mode::from_bits_truncate(0o755)).unwrap();
            fd = openat(&fd, "d", DIR_FLAGS, Mode::empty()).unwrap();
        }
        drop(fd);
        check_workspace_repo(&dest).unwrap();
        sh(&dest, "rm -rf .git/d");

        // A loose object as a symlink.
        let id = git_out(&dest, &["rev-parse", "HEAD"]);
        let obj = dest.join(".git/objects").join(&id[..2]).join(&id[2..]);
        if obj.exists() {
            std::fs::remove_file(&obj).unwrap();
        } else {
            std::fs::create_dir_all(obj.parent().unwrap()).unwrap();
        }
        std::os::unix::fs::symlink("/etc/hostname", &obj).unwrap();
        match check_workspace_repo(&dest) {
            Err(GitError::Refused(m)) => {
                assert!(m.contains("symlink"), "{m}");
                assert!(m.starts_with(".git/objects/"), "{m}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// M2 (docs/security-review.md §15): a good probe of the read sandbox
    /// is kept; a failed probe (failed twice: the probe and the one more
    /// try) is kept for [`JAIL_RETRY`] only.
    #[test]
    fn a_failed_sandbox_probe_is_tried_again() {
        let cache: JailCache = std::sync::Mutex::new(None);
        let t0 = Instant::now();
        let probes = std::cell::Cell::new(0);
        let pauses = std::cell::Cell::new(0);
        let pause = || pauses.set(pauses.get() + 1);
        let fail = || {
            probes.set(probes.get() + 1);
            Jail::Missing("no userns now".into())
        };
        let good = || {
            probes.set(probes.get() + 1);
            Jail::Bwrap(PathBuf::from("/usr/bin/bwrap"))
        };
        assert!(matches!(
            cached_jail(&cache, t0, fail, pause),
            Jail::Missing(_)
        ));
        assert_eq!((probes.get(), pauses.get()), (2, 1));
        // Within the retry time: no new probe.
        let soon = t0 + JAIL_RETRY / 2;
        assert!(matches!(
            cached_jail(&cache, soon, good, pause),
            Jail::Missing(_)
        ));
        assert_eq!(probes.get(), 2);
        // After it: a new probe, and a good one is kept.
        let later = t0 + JAIL_RETRY;
        assert!(matches!(
            cached_jail(&cache, later, good, pause),
            Jail::Bwrap(_)
        ));
        assert_eq!((probes.get(), pauses.get()), (3, 1));
        let much_later = later + JAIL_RETRY * 10;
        assert!(matches!(
            cached_jail(&cache, much_later, fail, pause),
            Jail::Bwrap(_)
        ));
        assert_eq!(probes.get(), 3);
    }

    /// A probe that fails for a moment (under load: no free user namespace
    /// or PID) is tried once more after a pause. The good second probe is
    /// the answer, and it is kept: no 30 s of `503` for the agent trees.
    #[test]
    fn a_probe_that_fails_once_is_not_kept() {
        let cache: JailCache = std::sync::Mutex::new(None);
        let t0 = Instant::now();
        let probes = std::cell::Cell::new(0);
        let pauses = std::cell::Cell::new(0);
        let once = || {
            probes.set(probes.get() + 1);
            if probes.get() == 1 {
                Jail::Missing("no free user namespace".into())
            } else {
                Jail::Bwrap(PathBuf::from("/usr/bin/bwrap"))
            }
        };
        let j = cached_jail(&cache, t0, once, || {
            // The pause comes after the first probe, before the second.
            assert_eq!(probes.get(), 1);
            pauses.set(pauses.get() + 1);
        });
        assert!(matches!(j, Jail::Bwrap(_)), "{j:?}");
        assert_eq!((probes.get(), pauses.get()), (2, 1));
        // The good answer is kept: a later call does not probe.
        let soon = t0 + JAIL_RETRY / 2;
        let j = cached_jail(
            &cache,
            soon,
            || panic!("no probe while a good one is kept"),
            || panic!("no pause"),
        );
        assert!(matches!(j, Jail::Bwrap(_)));
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

    /// R2 and R5 (docs/security-review.md §15): a local source with an
    /// alternates file gives a mirror with no alternates file, and the
    /// objects that only the alternate store has are not in the mirror.
    /// A mirror with an alternates file is cloned again.
    #[test]
    fn a_mirror_has_no_alternates() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = bare_repo(tmp.path());
        // `shared.git` has no objects of its own: an alternates file names
        // the store of `origin.git`.
        sh(
            tmp.path(),
            "git clone -q --bare --shared origin.git shared.git",
        );
        assert!(
            tmp.path()
                .join("shared.git/objects/info/alternates")
                .is_file()
        );
        // A commit that only `origin.git` has.
        sh(
            &tmp.path().join("src"),
            "git checkout -q -b secret && echo FOREIGN > f.txt && git add f.txt && \
             git commit -q -m foreign && git push -q ../origin.git secret",
        );
        let foreign = git_out(&tmp.path().join("src"), &["rev-parse", "HEAD"]);
        let main = git_out(&tmp.path().join("src"), &["rev-parse", "main"]);
        let dest = tmp.path().join("state/repo-mirrors/R.git");
        let mut c = checkout(tmp.path(), &tmp.path().join("shared.git"), &dest);
        c.deny_local_under = Vec::new();
        refresh_mirror(&c).unwrap();
        let alt = dest.join("objects/info/alternates");
        assert!(!alt.exists());
        let g = ReadGit::new(Duration::from_secs(10));
        let head = g.text(&dest, &["rev-parse", "refs/heads/main"]).unwrap();
        assert_eq!(head.trim(), main);
        assert!(g.run(&dest, &["cat-file", "-e", &foreign], 64).is_err());
        assert!(
            g.run(&dest, &["cat-file", "-e", &foreign[..7]], 64)
                .is_err()
        );
        // A fetch: still no alternates file.
        refresh_mirror(&c).unwrap();
        assert!(!alt.exists());

        // An old mirror with an alternates file: the read refuses it, and
        // the next refresh clones it again.
        std::fs::write(&alt, format!("{}/objects\n", origin.display())).unwrap();
        match g.run(&dest, &["cat-file", "-e", &foreign], 64) {
            Err(GitError::Refused(m)) => assert!(m.contains("alternates"), "{m}"),
            other => panic!("{other:?}"),
        }
        refresh_mirror(&c).unwrap();
        assert!(!alt.exists());
        assert!(g.run(&dest, &["cat-file", "-e", &foreign], 64).is_err());
        assert_eq!(
            g.text(&dest, &["rev-parse", "refs/heads/main"])
                .unwrap()
                .trim(),
            main
        );
    }

    /// A workspace clone of a local source with an alternates file has no
    /// alternates file (`--no-local`), so the daemon can update it.
    #[test]
    fn a_workspace_clone_has_no_alternates() {
        let tmp = tempfile::tempdir().unwrap();
        bare_repo(tmp.path());
        sh(
            tmp.path(),
            "git clone -q --bare --shared origin.git shared.git",
        );
        let dest = tmp.path().join("ws/proj");
        let c = checkout(tmp.path(), &tmp.path().join("shared.git"), &dest);
        clone_or_update(&c, &mut |_| {}).unwrap();
        assert!(!dest.join(".git/objects/info/alternates").exists());
        assert_eq!(
            clone_or_update(&c, &mut |_| {}).unwrap().action,
            Action::UpToDate
        );
        // A planted alternates file stops the update.
        std::fs::write(dest.join(".git/objects/info/alternates"), "/x\n").unwrap();
        assert!(matches!(
            clone_or_update(&c, &mut |_| {}),
            Err(GitError::Refused(m)) if m.contains("alternates")
        ));
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

    /// git over ssh records the host keys in the file that the sandboxes
    /// show at `/etc/ssh/ssh_known_hosts`, and the run makes that file.
    #[test]
    fn ssh_records_host_keys_where_the_sandboxes_read_them() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let c = checkout(tmp.path(), Path::new("/nowhere.git"), &tmp.path().join("d"));
        let r = Runner::new(&c).unwrap();
        let ssh = r
            .env
            .iter()
            .find_map(|(k, v)| (k == "GIT_SSH_COMMAND").then_some(v))
            .unwrap();
        let file = state.join("ssh/sandbox/ssh_known_hosts");
        assert!(
            ssh.contains(&format!("'UserKnownHostsFile={}'", file.display())),
            "{ssh}"
        );
        assert!(ssh.contains("StrictHostKeyChecking=accept-new"), "{ssh}");
        assert!(file.is_file());
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&state.join("ssh")), 0o700);
        assert_eq!(mode(&known_hosts_dir(&state)), 0o700);
        assert_eq!(mode(&file), 0o600);
        // A second run keeps the recorded keys (and adds the published
        // keys that the file lacks after them).
        std::fs::write(&file, "github.com ssh-ed25519 AAAA\n").unwrap();
        Runner::new(&c).unwrap();
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .starts_with("github.com ssh-ed25519 AAAA\n")
        );
    }

    /// The new file gets the lines of the daemon user's known hosts once:
    /// a later run does not append them again, and keeps the keys that ssh
    /// recorded. A missing seed makes an empty file.
    #[test]
    fn the_known_hosts_file_is_seeded_once_from_the_daemon_user() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let seed = home.join(".ssh/known_hosts");
        let lines = "github.com ssh-ed25519 AAAA\n|1|aGFzaA==|c2FsdA== ssh-ed25519 BBBB";
        std::fs::write(&seed, lines).unwrap();
        let state = tmp.path().join("state");
        let file = known_hosts_dir(&state).join(KNOWN_HOSTS_FILE);
        ensure_known_hosts_dir_from(&state, Some(&seed)).unwrap();
        // The seed, then the published keys (TER-38).
        let want = format!("{lines}\n{}\n", PUBLISHED_HOST_KEYS.join("\n"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), want);
        ensure_known_hosts_dir_from(&state, Some(&seed)).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), want);
        std::fs::write(&file, "recorded ssh-ed25519 CCCC\n").unwrap();
        ensure_known_hosts_dir_from(&state, Some(&seed)).unwrap();
        let got = std::fs::read_to_string(&file).unwrap();
        assert!(got.starts_with("recorded ssh-ed25519 CCCC\n"), "{got}");
        assert!(!got.contains("BBBB"), "the seed goes in once: {got}");
        let bare = tmp.path().join("bare");
        ensure_known_hosts_dir_from(&bare, Some(&tmp.path().join("none"))).unwrap();
        let fresh = known_hosts_dir(&bare).join(KNOWN_HOSTS_FILE);
        assert_eq!(
            std::fs::read_to_string(fresh).unwrap(),
            format!("{}\n", PUBLISHED_HOST_KEYS.join("\n"))
        );
    }

    /// TER-38: a fresh state dir's file holds GitHub's published keys, a
    /// second start adds no copy, a key that the file has already (from the
    /// seed or recorded by ssh) is not added again, and a recorded line of
    /// github.com with another key stays.
    #[test]
    fn the_published_github_keys_are_in_the_file_once() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let file = known_hosts_dir(&state).join(KNOWN_HOSTS_FILE);
        ensure_known_hosts_dir_from(&state, None).unwrap();
        ensure_known_hosts_dir_from(&state, None).unwrap();
        let got = std::fs::read_to_string(&file).unwrap();
        for k in PUBLISHED_HOST_KEYS {
            assert_eq!(got.matches(k).count(), 1, "{k}: {got}");
        }
        assert_eq!(got.lines().count(), PUBLISHED_HOST_KEYS.len());
        // The ed25519 key with another host pattern, a comment, and an old key.
        let ed = PUBLISHED_HOST_KEYS[0].trim_start_matches("github.com ");
        let mine = format!(
            "# notes\ngithub.com,140.82.112.3 {ed}\ngithub.com ssh-ed25519 OLDKEY no newline"
        );
        std::fs::write(&file, &mine).unwrap();
        ensure_known_hosts_dir_from(&state, None).unwrap();
        let got = std::fs::read_to_string(&file).unwrap();
        assert!(got.starts_with(&format!("{mine}\n")), "{got}");
        assert!(
            !got.contains(PUBLISHED_HOST_KEYS[0]),
            "the ed25519 key is there already: {got}"
        );
        assert!(
            got.contains(PUBLISHED_HOST_KEYS[1]) && got.contains(PUBLISHED_HOST_KEYS[2]),
            "{got}"
        );
        // Every published line parses as an OpenSSH known_hosts entry.
        for k in PUBLISHED_HOST_KEYS {
            let mut w = k.split_whitespace();
            assert_eq!(w.next(), Some("github.com"));
            ssh_key::PublicKey::from_openssh(&format!(
                "{} {}",
                w.next().unwrap(),
                w.next().unwrap()
            ))
            .unwrap();
        }
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
            hooks: Hooks::default(),
            origin: Origin::default(),
            fetch_from: None,
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

    /// TER-103: the daemon's job repo fetches, merges without a work tree
    /// (clean, conflict with the files, already merged, unrelated
    /// histories), bundles the job ref, pushes a commit, and reads a
    /// remote head.
    #[test]
    fn a_job_repo_merges_bundles_and_pushes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let origin = bare_repo(dir);
        sh(
            &dir.join("src"),
            "git push -q ../origin.git main:next && \
             git checkout -q -b clean && echo b > b.txt && git add b.txt && git commit -q -m b && \
             git push -q ../origin.git clean && \
             git checkout -q main && git checkout -q -b c1 && echo x > a.txt && git commit -qam x && \
             git push -q ../origin.git c1 && \
             git checkout -q main && git checkout -q -b c2 && echo y > a.txt && git commit -qam y && \
             git push -q ../origin.git c2 && \
             git checkout -q --orphan lone && git rm -rq --cached . && echo z > z.txt && \
             git add z.txt && git commit -q -m lone && git push -q ../origin.git lone",
        );
        let c = checkout(dir, &origin, &dir.join("daemon/jobs/p/J1.git"));
        assert_eq!(
            ls_remote(&c, "clean").unwrap(),
            Some(git_out(&origin, &["rev-parse", "clean"]))
        );
        assert_eq!(ls_remote(&c, "nope").unwrap(), None);

        let r = JobRepo::create(c).unwrap();
        let specs: Vec<String> = ["next", "clean", "c1", "c2", "lone"]
            .iter()
            .map(|b| format!("+refs/heads/{b}:refs/remotes/job/{b}"))
            .collect();
        r.fetch(&specs).unwrap();
        let next = r.commit_of("refs/remotes/job/next").unwrap().unwrap();
        assert_eq!(r.commit_of("refs/remotes/job/nope").unwrap(), None);

        let MergeStep::Merged(m1) = r
            .merge(&next, "refs/remotes/job/clean", "Merge clean onto next")
            .unwrap()
        else {
            panic!("clean did not merge")
        };
        assert_ne!(m1, next);
        let parents = git_out(r.path(), &["rev-list", "--parents", "-n", "1", &m1]);
        assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
        assert_eq!(
            git_out(r.path(), &["log", "-1", "--format=%s %an", &m1]),
            "Merge clean onto next terrarium"
        );
        let MergeStep::Merged(m2) = r
            .merge(&m1, "refs/remotes/job/c1", "Merge c1 onto next")
            .unwrap()
        else {
            panic!("c1 did not merge")
        };
        // c2 changes the line that c1 changed.
        assert_eq!(
            r.merge(&m2, "refs/remotes/job/c2", "Merge c2 onto next")
                .unwrap(),
            MergeStep::Conflict(vec!["a.txt".into()])
        );
        // A branch that is in the head already gives the head.
        assert_eq!(
            r.merge(&m2, "refs/remotes/job/clean", "again").unwrap(),
            MergeStep::Merged(m2.clone())
        );
        match r.merge(&m2, "refs/remotes/job/lone", "lone").unwrap() {
            MergeStep::Failed(msg) => assert!(msg.contains("unrelated"), "{msg}"),
            other => panic!("unrelated histories merged: {other:?}"),
        }

        r.set_ref("refs/job/merge", &m2).unwrap();
        let bundle = dir.join("daemon/jobs/p/J1.bundle");
        r.bundle("refs/job/merge", &bundle).unwrap();
        let heads = Command::new("git")
            .args(["bundle", "list-heads"])
            .arg(&bundle)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&heads.stdout).trim(),
            format!("{m2} refs/job/merge")
        );

        r.push(&m2, "next").unwrap();
        assert_eq!(git_out(&origin, &["rev-parse", "next"]), m2);
        // Not forced: a commit that is not ahead of `next` is refused.
        assert!(r.push(&next, "next").is_err());
        assert_eq!(git_out(&origin, &["rev-parse", "next"]), m2);
        // The job repo is in a 0700 dir.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("daemon/jobs/p"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
