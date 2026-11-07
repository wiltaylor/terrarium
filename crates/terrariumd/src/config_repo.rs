//! The config repo (PLAN §8.3): the operator's prompts and harness
//! settings, and an export of the declarative objects, in a git repo that
//! Terrarium instances share.
//!
//! The repo is the directory `TERRARIUM_CONFIG_REPO` (`/config/terrarium`
//! in the container). The daemon makes it a git repo on first use
//! ([`ConfigRepo::ensure`]). What goes in:
//!
//! - `prompts/`: the protocol templates and the harness sections (`*.md`).
//! - `system-prompts/`: the system prompt library (PLAN §8.1,
//!   `crate::system_prompts`), one `<name>.md` for each entry, with the
//!   personas `leader` and `worker`.
//! - `harness/<harness>/`: the user settings of each harness.
//! - `templates/`: the agent templates, one WCL file each
//!   (`crate::template_files`).
//! - `export/`: the schedules, and the projects with their agents,
//!   targets, and repos, as fleet files ([`export_files`]); and the
//!   secrets by name in `export/secrets/` (`crate::secrets::export`: the
//!   names, kinds, plain fields, and grants, never a value); and the
//!   board rules of the API in `export/rules/<project>.rules.wcl`. No runtime
//!   state, and no ID of this instance.
//! - `skills/`: the skill library (PLAN §8.6, `crate::skills`), one dir
//!   for each skill, with the file types of
//!   `terrarium_core::skills::EXTENSIONS`.
//! - `README.md`, `.gitignore`.
//!
//! What never goes in: the harness homes (`config/<harness>`: sessions,
//! history), the state dir (credentials, keys, the database), the `.env`,
//! an API key, a login, a token, a password, a host enrol token, a TLS key,
//! artifacts, or a transcript. Three controls keep secrets out:
//!
//! 1. [`GITIGNORE`]: it ignores every path, then allows only the known
//!    dirs and file types, then ignores the credential file names again.
//! 2. The secret scan (`terrarium_core::secret_scan`) of the staged files
//!    before each commit that the daemon makes ([`ConfigRepo::commit`]),
//!    and the `pre-commit` hook ([`HOOK`]) for a commit by hand. A hit
//!    blocks the commit; the answer names the file, the line, and the
//!    rule, never the value.
//! 3. The export names a credential or a subscription by its name only,
//!    and drops the user part of a repo URL ([`redact_url`]).
//!
//! The remote URL and its credential stay in the state dir
//! (`<state>/config-repo.json`, the credential in the secret store),
//! never in the repo or in `.git/config`: each fetch and push names the
//! URL on the command line, and the secret goes to git as for a workspace
//! clone (`crate::git::GitRunner`). A sync never discards a local commit:
//! no `reset --hard`, no force push. A conflict stops the merge and leaves
//! the repo in the merge, with the files listed.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::secret_scan::{self, Finding};

use crate::error::{ApiError, ApiResult, Code};
use crate::git::{GitError, GitRunner};
use crate::secrets::RunDirs;

/// The file in the state dir with the remote and the sync state.
pub const STATE_FILE: &str = "config-repo.json";
/// The default of `TERRARIUM_CONFIG_REPO`.
pub const DEFAULT_DIR: &str = "/config/terrarium";
/// The branch of the repo.
pub const DEFAULT_BRANCH: &str = "main";
/// The ref that holds the remote branch after a fetch. No remote is in
/// `.git/config`, so this is not `refs/remotes/origin/...`.
pub const TRACKING_PREFIX: &str = "refs/remotes/terrarium/";
/// The export dir in the repo.
pub const EXPORT_DIR: &str = "export";
/// The longest one git operation may take.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// `TERRARIUM_CONFIG_SYNC`: `off` (default), `pull`, or `sync`.
pub const ENV_SYNC: &str = "TERRARIUM_CONFIG_SYNC";
/// `TERRARIUM_CONFIG_SYNC_INTERVAL`: the time between two automatic runs.
pub const ENV_SYNC_INTERVAL: &str = "TERRARIUM_CONFIG_SYNC_INTERVAL";
/// The default of [`ENV_SYNC_INTERVAL`].
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// The `.gitignore` of the repo: an allowlist. The daemon writes it at
/// each start and before each commit; an edit does not stay.
pub const GITIGNORE: &str = "\
# Terrarium config repo. terrariumd writes this file; an edit does not stay.
# Everything is ignored, then the known paths and file types come back.
/*
!/.gitignore
!/README.md
!/prompts/
!/harness/
!/export/
!/skills/
!/templates/
!/system-prompts/
/prompts/**
!/prompts/**/
!/prompts/**/*.md
/system-prompts/**
!/system-prompts/*.md
/harness/**
!/harness/**/
!/harness/**/*.json
!/harness/**/*.toml
!/harness/**/*.yaml
/export/**
!/export/**/
!/export/**/*.toml
!/export/**/*.json
!/export/**/*.yaml
!/export/**/*.md
!/export/**/*.wcl
/templates/**
!/templates/**/
!/templates/**/*.wcl
/skills/**
!/skills/**/
!/skills/**/*.md
!/skills/**/*.txt
!/skills/**/*.json
!/skills/**/*.yaml
!/skills/**/*.yml
!/skills/**/*.toml
!/skills/**/*.py
!/skills/**/*.sh
!/skills/**/*.js
!/skills/**/*.ts
!/skills/**/*.jsx
!/skills/**/*.tsx
!/skills/**/*.css
!/skills/**/*.html
!/skills/**/*.ps1
!/skills/**/*.psm1
!/skills/**/*.sql
!/skills/**/*.png
!/skills/**/*.jpg
!/skills/**/*.jpeg
!/skills/**/*.gif
!/skills/**/*.svg
!/skills/**/*.webp
# Credential files never go in, whatever their type.
.env*
.credentials.json
credentials.json
auth.json
hosts.yml
*.pem
*.key
id_rsa*
id_ecdsa*
id_ed25519*
*.new
";

/// The `pre-commit` hook of the repo: the secret scan of a commit by hand.
/// It fails closed: with no Terrarium binary on `PATH`, no commit.
pub const HOOK: &str = r#"#!/bin/sh
# Terrarium config repo: the secret scan of each commit. terrariumd writes
# this hook. A hit blocks the commit, and names the file, the line, and the
# rule (docs/security-review.md, "Config repo").
if command -v terrarium >/dev/null 2>&1; then
	exec terrarium admin config scan --staged
fi
if command -v terrariumd >/dev/null 2>&1; then
	exec terrariumd config scan --staged
fi
echo "terrarium pre-commit: no terrarium or terrariumd binary on PATH, so the secret scan cannot run." >&2
echo "Commit with 'terrarium admin config commit', or put the terrarium CLI on PATH." >&2
exit 1
"#;

/// The config repo of the daemon, once it is configured.
#[derive(Debug, Default)]
pub struct ConfigRepoRuntime {
    repo: RwLock<Option<ConfigRepo>>,
    /// One git operation at a time.
    pub busy: tokio::sync::Mutex<()>,
    /// The loads of the template files (`crate::template_files`).
    pub templates: crate::template_files::Runtime,
}

impl ConfigRepoRuntime {
    pub fn configure(&self, repo: ConfigRepo) {
        match self.repo.write() {
            Ok(mut r) => *r = Some(repo),
            Err(p) => *p.into_inner() = Some(repo),
        }
    }

    /// The repo, or `501 not_available` when the daemon has none.
    pub fn get(&self) -> ApiResult<ConfigRepo> {
        let r = match self.repo.read() {
            Ok(r) => r.clone(),
            Err(p) => p.into_inner().clone(),
        };
        r.ok_or_else(|| {
            ApiError::new(
                Code::NotAvailable,
                "this daemon has no config repo (TERRARIUM_CONFIG_REPO)",
            )
        })
    }
}

/// The remote of the repo, as the state dir keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub url: String,
    /// The name of a global `token`, `github_token`, or `ssh_key` secret
    /// (PLAN §16b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    pub branch: String,
}

/// `<state>/config-repo.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    /// A pull brought `export/` changes that are not applied: a commit
    /// does not write `export/` from this instance until they are
    /// (`pull --apply`) or the operator writes it again (`commit
    /// --export`).
    #[serde(default)]
    pub export_pending: bool,
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    /// `added`, `modified`, `deleted`, `renamed`, `untracked`, or
    /// `conflict`.
    pub status: String,
}

/// The remote as the API shows it: the URL without credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteView {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    pub branch: String,
}

/// `GET /api/config/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub dir: String,
    pub initialized: bool,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteView>,
    /// Commits that the remote does not have, after the last fetch.
    pub ahead: u32,
    /// Commits of the remote that this repo does not have, after the last
    /// fetch.
    pub behind: u32,
    pub changes: Vec<FileChange>,
    /// A merge stopped on a conflict, and waits.
    pub merging: bool,
    pub conflicts: Vec<String>,
    /// The new defaults that wait for the operator (`<file>.new`).
    pub pending_defaults: Vec<String>,
    pub export_pending: bool,
    /// `off`, `pull`, or `sync` (`TERRARIUM_CONFIG_SYNC`).
    pub auto_sync: String,
}

/// One commit of `log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub commit: String,
    pub author: String,
    pub at: String,
    pub subject: String,
}

/// What a commit did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committed {
    /// The new commit. `None`: nothing to commit.
    pub commit: Option<String>,
    pub files: Vec<String>,
}

/// What a merge of the remote did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Merged {
    /// The remote has a branch to merge.
    pub fetched: bool,
    /// HEAD moved.
    pub updated: bool,
    /// The files that the merge changed.
    pub changed: Vec<String>,
    /// The merge stopped on these files. The repo stays in the merge.
    pub conflicts: Vec<String>,
}

/// The block of a commit by the secret scan: `422 invalid` with the
/// findings in `details.findings`.
pub fn scan_error(findings: &[Finding]) -> ApiError {
    let list: Vec<String> = findings.iter().map(ToString::to_string).collect();
    ApiError::invalid(format!(
        "the secret scan blocked the commit ({} finding(s)): {}. Remove the secret, \
         or for a false hit add `{}` to the line",
        findings.len(),
        list.join("; "),
        secret_scan::ALLOW_MARKER
    ))
    .with_details(json!({ "findings": findings }))
}

fn git_error(e: GitError) -> ApiError {
    match e {
        GitError::Refused(m) => ApiError::invalid(m),
        other => ApiError::new(Code::Invalid, other.to_string()),
    }
}

fn io_error(what: &Path, e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(format!("{}: {e}", what.display()))
}

/// The URL as the API shows it: no user part for `http(s)` (it can hold
/// a token), and no password for any scheme.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let Some((user, host)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    let lower = scheme.to_ascii_lowercase();
    if lower == "http" || lower == "https" {
        return format!("{scheme}://{host}{path}");
    }
    let user = user.split(':').next().unwrap_or_default();
    format!("{scheme}://{user}@{host}{path}")
}

/// May the repo push to `url`? A URL with a secret in it is refused: the
/// token goes in a credential.
pub fn check_url(url: &str) -> ApiResult<()> {
    let u = url.trim();
    if u.is_empty() || u != url || u.starts_with('-') || u.contains(char::is_whitespace) {
        return Err(ApiError::invalid(format!("`{url}` is not a git URL")));
    }
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("ext::") || lower.starts_with("fd::") {
        return Err(ApiError::invalid(
            "the ext:: and fd:: transports are refused",
        ));
    }
    if let Some((scheme, rest)) = u.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !["https", "http", "ssh", "git", "file"].contains(&scheme.as_str()) {
            return Err(ApiError::invalid(format!(
                "the URL scheme `{scheme}` is not supported"
            )));
        }
        let authority = rest.split('/').next().unwrap_or_default();
        if let Some((user, _)) = authority.rsplit_once('@')
            && (scheme.starts_with("http") || user.contains(':'))
        {
            return Err(ApiError::invalid(
                "the URL holds a user or a password: put the token in a credential \
                 (`--credential <name>`), not in the URL",
            ));
        }
        return Ok(());
    }
    // `git@host:path` (scp form) or a local path.
    if u.starts_with('/') || u.contains(':') {
        return Ok(());
    }
    Err(ApiError::invalid(format!("`{url}` is not a git URL")))
}

/// The daemon's config repo.
#[derive(Debug, Clone)]
pub struct ConfigRepo {
    pub dir: PathBuf,
    pub state_dir: PathBuf,
    /// The value of `TERRARIUM_CONFIG_SYNC`, for the status.
    pub auto_sync: String,
}

impl ConfigRepo {
    pub fn new(dir: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            dir,
            state_dir,
            auto_sync: "off".into(),
        }
    }

    fn state_path(&self) -> PathBuf {
        self.state_dir.join(STATE_FILE)
    }

    pub fn state(&self) -> RepoState {
        std::fs::read_to_string(self.state_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, s: &RepoState) -> ApiResult<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.state_path();
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(s)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| io_error(&tmp, e))?;
        f.write_all(text.as_bytes())
            .map_err(|e| io_error(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_error(&path, e))
    }

    fn branch(&self) -> String {
        self.state()
            .remote
            .map_or_else(|| DEFAULT_BRANCH.to_owned(), |r| r.branch)
    }

    fn tracking(&self) -> String {
        format!("{TRACKING_PREFIX}{}", self.branch())
    }

    /// A runner for local work: no remote, no secret.
    fn local(&self) -> ApiResult<GitRunner> {
        GitRunner::new("", None, RunDirs::new(&self.state_dir), TIMEOUT).map_err(git_error)
    }

    /// A runner for the remote, with its credential.
    fn remote(&self) -> ApiResult<(GitRunner, Remote)> {
        let remote = self.state().remote.ok_or_else(|| {
            ApiError::invalid(
                "the config repo has no remote: run `terrarium admin config remote set <url>`",
            )
        })?;
        // A global secret (PLAN §16b.2). The config repo has no `Db`
        // handle (`terrariumd config` runs with no daemon): it reads the
        // database on its own connection.
        let secret = match &remote.credential {
            Some(name) => {
                let conn = crate::secrets::open_db(&self.state_dir)?;
                Some(
                    crate::secrets::require(&conn, &self.state_dir, None, name).map_err(|e| {
                        ApiError::invalid(format!(
                            "the config repo remote's secret `{name}`: {}",
                            e.message
                        ))
                    })?,
                )
            }
            None => None,
        };
        let g = GitRunner::new(&remote.url, secret, RunDirs::new(&self.state_dir), TIMEOUT)
            .map_err(git_error)?;
        Ok((g, remote))
    }

    fn git(&self, g: &GitRunner, args: &[&str]) -> ApiResult<String> {
        g.run(&self.dir, args).map_err(git_error)
    }

    fn ok(&self, g: &GitRunner, args: &[&str]) -> bool {
        g.run(&self.dir, args).is_ok()
    }

    fn head(&self, g: &GitRunner) -> Option<String> {
        g.run(&self.dir, &["rev-parse", "-q", "--verify", "HEAD"])
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    pub fn initialized(&self) -> bool {
        self.dir.join(".git").is_dir()
    }

    /// Make the repo when it is not one, and write `.gitignore` and the
    /// `pre-commit` hook. Nothing else changes.
    pub fn ensure(&self) -> ApiResult<()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io_error(&self.dir, e))?;
        let g = self.local()?;
        if !self.initialized() {
            self.git(&g, &["init", "-q", "-b", DEFAULT_BRANCH])?;
        }
        let ignore = self.dir.join(".gitignore");
        if std::fs::read_to_string(&ignore).ok().as_deref() != Some(GITIGNORE) {
            std::fs::write(&ignore, GITIGNORE).map_err(|e| io_error(&ignore, e))?;
        }
        let hooks = self.dir.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).map_err(|e| io_error(&hooks, e))?;
        let hook = hooks.join("pre-commit");
        if std::fs::read_to_string(&hook).ok().as_deref() != Some(HOOK) {
            std::fs::write(&hook, HOOK).map_err(|e| io_error(&hook, e))?;
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io_error(&hook, e))?;
        Ok(())
    }

    fn unmerged(&self, g: &GitRunner) -> Vec<String> {
        g.run(&self.dir, &["diff", "--name-only", "--diff-filter=U", "-z"])
            .map(|s| split_z(&s))
            .unwrap_or_default()
    }

    fn merging(&self) -> bool {
        self.dir.join(".git").join("MERGE_HEAD").exists()
    }

    fn changes(&self, g: &GitRunner) -> ApiResult<Vec<FileChange>> {
        let out = self.git(
            g,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;
        let mut items = out.split('\0').filter(|s| !s.is_empty());
        let mut changes = Vec::new();
        while let Some(entry) = items.next() {
            if entry.len() < 4 {
                continue;
            }
            let (xy, path) = entry.split_at(3);
            let xy = xy.trim_end();
            let status = match xy {
                "??" => "untracked",
                "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU" => "conflict",
                x if x.contains('R') => {
                    // A rename carries the old path as the next item.
                    items.next();
                    "renamed"
                }
                x if x.contains('D') => "deleted",
                x if x.contains('A') => "added",
                _ => "modified",
            };
            changes.push(FileChange {
                path: path.to_owned(),
                status: status.into(),
            });
        }
        Ok(changes)
    }

    /// The state of the repo. It does not fetch: `ahead` and `behind`
    /// count against the last fetch.
    pub fn status(&self) -> ApiResult<Status> {
        let st = self.state();
        let remote = st.remote.as_ref().map(|r| RemoteView {
            url: redact_url(&r.url),
            credential: r.credential.clone(),
            branch: r.branch.clone(),
        });
        let pending_defaults = crate::defaults::pending(&self.dir)
            .into_iter()
            .map(|p| p.path)
            .collect();
        let mut s = Status {
            dir: self.dir.display().to_string(),
            initialized: self.initialized(),
            branch: self.branch(),
            head: None,
            remote,
            ahead: 0,
            behind: 0,
            changes: Vec::new(),
            merging: false,
            conflicts: Vec::new(),
            pending_defaults,
            export_pending: st.export_pending,
            auto_sync: self.auto_sync.clone(),
        };
        if !s.initialized {
            return Ok(s);
        }
        let g = self.local()?;
        s.head = self.head(&g);
        s.changes = self.changes(&g)?;
        s.merging = self.merging();
        s.conflicts = self.unmerged(&g);
        let tracking = self.tracking();
        if s.head.is_some() && self.ok(&g, &["rev-parse", "-q", "--verify", &tracking]) {
            let range = format!("HEAD...{tracking}");
            if let Ok(out) = g.run(&self.dir, &["rev-list", "--left-right", "--count", &range]) {
                let mut n = out.split_whitespace().map(|v| v.parse().unwrap_or(0));
                s.ahead = n.next().unwrap_or(0);
                s.behind = n.next().unwrap_or(0);
            }
        }
        Ok(s)
    }

    /// The diff of the work tree against HEAD, with the new files in full.
    /// `path` limits it to one file.
    pub fn diff(&self, path: Option<&str>) -> ApiResult<String> {
        if !self.initialized() {
            return Ok(String::new());
        }
        let g = self.local()?;
        let mut out = String::new();
        if self.head(&g).is_some() {
            let mut args = vec!["diff", "--no-color", "--no-ext-diff", "HEAD", "--"];
            if let Some(p) = path {
                args.push(p);
            }
            out.push_str(&self.git(&g, &args)?);
        }
        for c in self.changes(&g)? {
            let new_file = c.status == "untracked" || (c.status == "added" && out.is_empty());
            if !new_file || path.is_some_and(|p| p != c.path) {
                continue;
            }
            let text = std::fs::read_to_string(self.dir.join(&c.path)).unwrap_or_default();
            out.push_str(&crate::store::wiki::unified_diff(
                "",
                &text,
                "/dev/null",
                &format!("b/{}", c.path),
            ));
        }
        Ok(out)
    }

    /// Stage every change (the `.gitignore` decides), scan the staged
    /// files, and commit. A finding blocks the commit, unstages, and is an
    /// error ([`scan_error`]). During a merge, a file that still has
    /// conflict markers blocks the commit too.
    pub fn commit(&self, message: &str) -> ApiResult<Committed> {
        let message = message.trim();
        if message.is_empty() {
            return Err(ApiError::invalid("a commit needs a message"));
        }
        self.ensure()?;
        let g = self.local()?;
        let merging = self.merging();
        if merging {
            let mut marked = Vec::new();
            for p in self.unmerged(&g) {
                let text = std::fs::read_to_string(self.dir.join(&p)).unwrap_or_default();
                if text
                    .lines()
                    .any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> "))
                {
                    marked.push(p);
                }
            }
            if !marked.is_empty() {
                return Err(ApiError::conflict(format!(
                    "resolve the conflicts first: {} still have conflict markers",
                    marked.join(", ")
                ))
                .with_details(json!({ "conflicts": marked })));
            }
        }
        self.git(&g, &["add", "-A"])?;
        let staged = split_z(&self.git(
            &g,
            &[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--diff-filter=ACMR",
            ],
        )?);
        let mut findings = Vec::new();
        for p in &staged {
            let blob = self.git(&g, &["show", &format!(":{p}")])?;
            findings.extend(secret_scan::scan(p, blob.as_bytes()));
        }
        if !findings.is_empty() {
            if !merging {
                if self.head(&g).is_some() {
                    let _ = g.run(&self.dir, &["reset", "-q"]);
                } else {
                    let _ = g.run(
                        &self.dir,
                        &["rm", "-r", "-q", "--cached", "--ignore-unmatch", "."],
                    );
                }
            }
            return Err(scan_error(&findings));
        }
        let all = split_z(&self.git(&g, &["diff", "--cached", "--name-only", "-z"])?);
        if all.is_empty() && !merging {
            return Ok(Committed {
                commit: None,
                files: Vec::new(),
            });
        }
        self.git(
            &g,
            &[
                "-c",
                "user.name=Terrarium",
                "-c",
                "user.email=terrarium@localhost",
                "commit",
                "-q",
                "--no-verify",
                "-m",
                message,
            ],
        )?;
        Ok(Committed {
            commit: self.head(&g),
            files: all,
        })
    }

    /// The last `limit` commits.
    pub fn log(&self, limit: u32) -> ApiResult<Vec<LogEntry>> {
        if !self.initialized() {
            return Ok(Vec::new());
        }
        let g = self.local()?;
        if self.head(&g).is_none() {
            return Ok(Vec::new());
        }
        let n = format!("-n{}", limit.clamp(1, 500));
        let out = self.git(&g, &["log", &n, "--format=%H%x1f%an%x1f%aI%x1f%s%x1e"])?;
        Ok(out
            .split('\u{1e}')
            .filter_map(|rec| {
                let mut f = rec.trim_start_matches('\n').split('\u{1f}');
                Some(LogEntry {
                    commit: f.next().filter(|c| !c.is_empty())?.to_owned(),
                    author: f.next()?.to_owned(),
                    at: f.next()?.to_owned(),
                    subject: f.next()?.trim_end().to_owned(),
                })
            })
            .collect())
    }

    /// Fetch the remote branch into the tracking ref. False: the remote
    /// has no such branch yet (an empty repo).
    pub fn fetch(&self) -> ApiResult<bool> {
        self.ensure()?;
        let (g, remote) = self.remote()?;
        let refspec = format!("+refs/heads/{}:{}", remote.branch, self.tracking());
        match g.run(
            &self.dir,
            &["fetch", "-q", "--no-tags", &remote.url, &refspec],
        ) {
            Ok(_) => Ok(true),
            Err(GitError::Failed { stderr, .. })
                if stderr.contains("couldn't find remote ref")
                    || stderr.contains("could not find remote ref") =>
            {
                Ok(false)
            }
            Err(e) => Err(git_error(e)),
        }
    }

    /// Merge the tracking ref into the branch. A fast-forward when it can;
    /// else a merge commit. A conflict stops the merge with the files in
    /// [`Merged::conflicts`], and the repo stays in the merge (`abort`
    /// leaves it). Uncommitted changes stop it before it starts.
    pub fn merge(&self) -> ApiResult<Merged> {
        let g = self.local()?;
        let tracking = self.tracking();
        if !self.ok(&g, &["rev-parse", "-q", "--verify", &tracking]) {
            return Ok(Merged::default());
        }
        let mut m = Merged {
            fetched: true,
            ..Merged::default()
        };
        if self.merging() {
            return Err(ApiError::conflict(
                "a merge waits for its conflicts: resolve them and commit, or abort",
            ));
        }
        // A new instance that joins a shared repo: its own files (the
        // seeded defaults) become its first commit, so the merge keeps
        // them and a file that differs shows as a conflict.
        if self.head(&g).is_none() && !self.changes(&g)?.is_empty() {
            self.commit("Terrarium config: the local files before the first pull")?;
        }
        let dirty: Vec<String> = self.changes(&g)?.into_iter().map(|c| c.path).collect();
        if !dirty.is_empty() {
            return Err(ApiError::conflict(format!(
                "commit the local changes first: {}",
                dirty.join(", ")
            )));
        }
        let before = self.head(&g);
        let result = match &before {
            Some(_) if self.ok(&g, &["merge-base", "--is-ancestor", &tracking, "HEAD"]) => {
                return Ok(m);
            }
            Some(_) if self.ok(&g, &["merge-base", "--is-ancestor", "HEAD", &tracking]) => {
                g.run(&self.dir, &["merge", "-q", "--ff-only", &tracking])
            }
            Some(_) => g.run(
                &self.dir,
                &[
                    "-c",
                    "user.name=Terrarium",
                    "-c",
                    "user.email=terrarium@localhost",
                    "merge",
                    "-q",
                    "--no-edit",
                    "--no-ff",
                    // Two instances that made their repos on their own
                    // have no common commit. Files that are the same (the
                    // seeded defaults) merge; others conflict.
                    "--allow-unrelated-histories",
                    "-m",
                    "Merge the config repo remote",
                    &tracking,
                ],
            ),
            None => g.run(&self.dir, &["merge", "-q", "--ff-only", &tracking]),
        };
        if let Err(e) = result {
            let conflicts = self.unmerged(&g);
            if !conflicts.is_empty() {
                m.conflicts = conflicts;
                return Ok(m);
            }
            if self.merging() {
                let _ = g.run(&self.dir, &["merge", "--abort"]);
            }
            return Err(git_error(e));
        }
        let after = self.head(&g);
        m.updated = after != before;
        if m.updated {
            let range = match &before {
                Some(b) => vec!["diff", "--name-only", "-z", b.as_str(), "HEAD"],
                None => vec!["ls-tree", "-r", "--name-only", "-z", "HEAD"],
            };
            m.changed = split_z(&self.git(&g, &range)?);
        }
        Ok(m)
    }

    /// Leave a merge that stopped on a conflict: back to the commit before
    /// it. Local commits stay.
    pub fn abort(&self) -> ApiResult<()> {
        if !self.merging() {
            return Err(ApiError::conflict("no merge waits"));
        }
        let g = self.local()?;
        self.git(&g, &["merge", "--abort"]).map(|_| ())
    }

    /// Push the branch. Never forced: a remote with new commits refuses
    /// it, and `sync` merges them first.
    pub fn push(&self) -> ApiResult<bool> {
        self.ensure()?;
        let (g, remote) = self.remote()?;
        let local = self.local()?;
        if self.head(&local).is_none() {
            return Ok(false);
        }
        let refspec = format!("HEAD:refs/heads/{}", remote.branch);
        if let Err(e) = g.run(&self.dir, &["push", "-q", &remote.url, &refspec]) {
            let text = e.to_string();
            if text.contains("rejected") || text.contains("non-fast-forward") {
                return Err(ApiError::conflict(
                    "the remote has commits that this repo does not have: run `terrarium admin config sync`",
                ));
            }
            return Err(git_error(e));
        }
        self.git(&local, &["update-ref", &self.tracking(), "HEAD"])?;
        Ok(true)
    }

    /// Set the remote (and its credential), or clear it with `None`. The
    /// URL goes to the state dir, never to `.git/config`.
    pub fn set_remote(&self, remote: Option<Remote>) -> ApiResult<()> {
        if let Some(r) = &remote {
            check_url(&r.url)?;
            let b = &r.branch;
            if b.is_empty()
                || b.starts_with(['-', '.', '/'])
                || b.contains("..")
                || !b
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
            {
                return Err(ApiError::invalid(format!("bad branch `{b}`")));
            }
            if let Some(name) = &r.credential
                && crate::secrets::get(
                    &crate::secrets::open_db(&self.state_dir)?,
                    crate::secrets::GLOBAL,
                    name,
                )?
                .is_none()
            {
                return Err(ApiError::invalid(format!(
                    "no global secret `{name}`: store it first (token, github_token, or ssh_key)"
                )));
            }
        }
        let mut st = self.state();
        st.remote = remote;
        self.save_state(&st)
    }

    pub fn set_export_pending(&self, pending: bool) -> ApiResult<()> {
        let mut st = self.state();
        if st.export_pending != pending {
            st.export_pending = pending;
            self.save_state(&st)?;
        }
        Ok(())
    }

    /// The fleet files of the export in the repo: `(path in the repo,
    /// content)`.
    pub fn export_in_repo(&self) -> Vec<(String, String)> {
        self.read_rel(
            export_paths(&self.dir.join(EXPORT_DIR))
                .into_iter()
                .filter(|p| !is_secrets_file(&self.dir, p))
                .collect(),
        )
    }

    /// The secrets files of the export in the repo
    /// (`export/secrets/*.toml`): `(path in the repo, content)`.
    pub fn secrets_in_repo(&self) -> Vec<(String, String)> {
        self.read_rel(
            export_paths(&self.dir.join(EXPORT_DIR))
                .into_iter()
                .filter(|p| is_secrets_file(&self.dir, p))
                .collect(),
        )
    }

    /// The projects of the export: the names of
    /// `export/projects/<name>.fleet.toml`.
    pub fn exported_projects(&self) -> Vec<String> {
        let dir = self.dir.join(EXPORT_DIR).join("projects");
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(terrarium_core::fleet::FLEET_SUFFIX))
                    .map(str::to_owned)
            })
            .collect();
        out.sort();
        out
    }

    fn read_rel(&self, paths: Vec<PathBuf>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for p in paths {
            if let Ok(text) = std::fs::read_to_string(&p) {
                let rel = p
                    .strip_prefix(&self.dir)
                    .unwrap_or(&p)
                    .display()
                    .to_string();
                out.push((rel, text));
            }
        }
        out.sort();
        out
    }

    /// Write the export files, and remove the old ones that no object
    /// makes any more.
    pub fn write_export(&self, files: &[ExportFile]) -> ApiResult<()> {
        let dir = self.dir.join(EXPORT_DIR);
        std::fs::create_dir_all(dir.join("projects")).map_err(|e| io_error(&dir, e))?;
        let keep: Vec<PathBuf> = files.iter().map(|f| self.dir.join(&f.path)).collect();
        for old in export_paths(&dir) {
            if !keep.contains(&old) {
                std::fs::remove_file(&old).map_err(|e| io_error(&old, e))?;
            }
        }
        for f in files {
            let path = self.dir.join(&f.path);
            if std::fs::read_to_string(&path).ok().as_deref() == Some(f.content.as_str()) {
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
            }
            std::fs::write(&path, &f.content).map_err(|e| io_error(&path, e))?;
        }
        Ok(())
    }
}

/// A secrets file of the export: `export/secrets/<scope>.toml`.
fn is_secrets_file(repo: &Path, p: &Path) -> bool {
    p.parent() == Some(repo.join(crate::secrets::export::DIR).as_path())
        && p.extension().is_some_and(|e| e == "toml")
}

/// The files under `export/` that the export writes: the fleet files
/// (`*.fleet.toml`, at any depth), the board rules files
/// (`*.rules.wcl`), and the secrets files (`export/secrets/*.toml`).
fn export_paths(dir: &Path) -> Vec<PathBuf> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        let secrets = d.file_name().is_some_and(|n| n == "secrets")
            && d.parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == EXPORT_DIR);
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            let Ok(ty) = e.file_type() else { continue };
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if ty.is_dir() {
                walk(&p, out);
            } else if ty.is_file()
                && (name.ends_with(terrarium_core::fleet::FLEET_SUFFIX)
                    || name.ends_with(terrarium_core::config::RULES_SUFFIX)
                    || (secrets && name.ends_with(".toml")))
            {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

fn split_z(s: &str) -> Vec<String> {
    s.split('\0')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One file of the export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportFile {
    /// The path in the repo: `export/global.fleet.toml`,
    /// `export/projects/<project>.fleet.toml`.
    pub path: String,
    pub content: String,
}

/// The fleet name of the global export.
pub const GLOBAL_FLEET: &str = "terrarium-global";

/// A fleet with no instance state: the URL of each repo without its user
/// part, and no `thread` in a mail action (a thread ID belongs to this
/// instance).
fn clean_fleet(f: &mut terrarium_core::fleet::Fleet) {
    use terrarium_core::schedule::ScheduleAction;
    for p in &mut f.projects {
        for r in &mut p.def.repos {
            r.url = redact_url(&r.url);
        }
        for s in &mut p.def.schedules {
            if let ScheduleAction::Mail { thread, .. } = &mut s.action {
                *thread = None;
            }
        }
        for a in &mut p.agents {
            a.start = false;
        }
        // The templates are files of the repo (`templates/`), not a part
        // of the export.
        p.def.templates.clear();
    }
    f.templates.clear();
}

/// The export of the store (PLAN §8.3): one fleet file for each project
/// (its repos, agents, project targets, and schedules), one for the
/// global targets, and one secrets file for each
/// scope with secrets (`crate::secrets::export`: no value). A project that a fleet owns is
/// exported under that fleet's name, so an apply on another instance
/// keeps one owner.
pub fn export_files(conn: &rusqlite::Connection) -> ApiResult<Vec<ExportFile>> {
    use crate::store::{Page, projects, targets};
    use terrarium_core::fleet::{FLEET_VERSION, Fleet, FleetDefaults, Kind, valid_name};
    let mut out = Vec::new();
    let mut page = Page {
        limit: Some(Page::MAX),
        cursor: None,
    };
    let mut all = Vec::new();
    loop {
        let got = projects::list(conn, &page, None)?;
        all.extend(got.items);
        match got.next {
            Some(c) => page.cursor = Some(c),
            None => break,
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    for p in all {
        let owner = crate::fleet::source_of(conn, Kind::Project, &p.name)?.and_then(|s| {
            s.strip_prefix(crate::fleet::SOURCE_PREFIX)
                .map(str::to_owned)
        });
        let name = owner.unwrap_or_else(|| p.name.clone());
        let name = if valid_name(&name)
            && !terrarium_core::fleet::RESERVED_NAMES.contains(&name.as_str())
        {
            name
        } else {
            format!("project-{}", p.name.to_ascii_lowercase())
        };
        let mut f = crate::fleet::export(conn, &p.name, &name, Vec::new())?;
        clean_fleet(&mut f);
        out.push(ExportFile {
            path: format!("{EXPORT_DIR}/projects/{}.fleet.toml", p.name),
            content: f.to_toml(),
        });
    }
    let targets: Vec<_> = targets::list(conn)?
        .into_iter()
        .filter(|r| r.def.project.is_none() && r.source != crate::store::projects::SOURCE_FILE)
        .map(|r| r.def)
        .collect();
    if !targets.is_empty() {
        let f = Fleet {
            version: FLEET_VERSION,
            name: GLOBAL_FLEET.into(),
            description: Some("The global targets of a Terrarium instance".into()),
            defaults: FleetDefaults::default(),
            subscriptions: Vec::new(),
            targets,
            templates: Vec::new(),
            projects: Vec::new(),
        };
        out.push(ExportFile {
            path: format!("{EXPORT_DIR}/global.fleet.toml"),
            content: f.to_toml(),
        });
    }
    for (path, content) in crate::secrets::export::files(conn)? {
        out.push(ExportFile { path, content });
    }
    // The board rules of the API (TER-166); the rules of a file are in the
    // config dir already.
    for (project, text) in crate::store::rules::of_api(conn)? {
        out.push(ExportFile {
            path: format!("{EXPORT_DIR}/rules/{project}.rules.wcl"),
            content: text,
        });
    }
    Ok(out)
}

/// The automatic sync (`TERRARIUM_CONFIG_SYNC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSync {
    Off,
    Pull,
    Sync,
}

impl AutoSync {
    /// Read `TERRARIUM_CONFIG_SYNC`. An unknown value is `off`, with a
    /// warning.
    pub fn from_env() -> Self {
        match std::env::var(ENV_SYNC)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "off" | "0" | "false" | "no" => Self::Off,
            "pull" => Self::Pull,
            "sync" => Self::Sync,
            other => {
                tracing::warn!("{ENV_SYNC}={other} is not off, pull, or sync: auto-sync is off");
                Self::Off
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pull => "pull",
            Self::Sync => "sync",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn urls_show_no_credentials() {
        assert_eq!(
            redact_url("https://x-access-token:FAKE@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            redact_url("https://FAKEtoken@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            redact_url("ssh://git:pw@host/r.git"),
            "ssh://git@host/r.git"
        );
        assert_eq!(
            redact_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
        assert_eq!(redact_url("/srv/r.git"), "/srv/r.git");
    }

    #[test]
    fn urls_with_secrets_are_refused() {
        for ok in [
            "https://github.com/o/r.git",
            "ssh://git@host/r.git",
            "git@github.com:o/r.git",
            "/tmp/remote.git",
            "file:///tmp/remote.git",
        ] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "https://user:FAKE@github.com/o/r.git",
            "https://FAKE@github.com/o/r.git",
            "ssh://git:pw@host/r.git",
            "ext::sh -c x",
            "-upload-pack=x",
            "relative/path",
            "ftp://host/r",
            "",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }
}
