//! The repo browser (PLAN §9a, docs/api.md §5.3): the files, the changes,
//! and the commits of one tree of a repo. A tree is `main` (the daemon's
//! bare mirror of the repo, `crate::git::refresh_mirror`) or the checkout
//! of the repo in an agent workspace (`<work>/<project>/<agent>/<path>`).
//!
//! The daemon runs outside every sandbox, and the agent controls each file
//! of its checkout. So:
//!
//! - The checkout is opened once for each request ([`CheckoutDir`]: no
//!   symlink followed under the workspace). Every read goes through that
//!   fd: git binds it into its sandbox, and the daemon reads the work tree
//!   from it ([`Root`]). No read resolves the checkout path again.
//! - git runs through [`ReadGit`]: for an agent checkout, in a `bwrap`
//!   sandbox that shows only the checkout (read-only, by its fd) and
//!   `/usr`, with no network. A symlink or an alternates file in `.git`
//!   that names another repo or a daemon file finds nothing there. Also
//!   the hardening of a workspace update, no external diff, no textconv,
//!   no attributes file, no signature check, a deadline and an output cap
//!   on each command. With no usable `bwrap`, an agent checkout is not
//!   read ([`GitError::Unavailable`]). Before the first command,
//!   [`ReadGit::check_checkout`] checks a copy of `.git/config` against
//!   the allowlist.
//! - Only commands that run no filter driver: `rev-parse`, `symbolic-ref`,
//!   `rev-list`, `log` (no patch), `ls-files`, `ls-tree`, `cat-file blob`
//!   (no `--filters`, no `--textconv`), `diff-tree` (tree to tree), and
//!   `diff-index --cached` (tree to index). git runs a clean or a smudge
//!   filter only when a blob moves between the work tree and the object
//!   store; these commands read blobs from the object store only, and the
//!   plumbing commands never refresh the index (a refresh can hash a work
//!   tree file through the clean filter). With `--no-textconv` and
//!   `--no-ext-diff`, the `diff.<driver>` programs do not run. So a key
//!   that the agent adds to the live config after the check (the window
//!   that `crate::git` names) runs nothing here.
//! - The work tree is read from disk by the daemon, not by git: each path
//!   component is opened with `O_NOFOLLOW` from the checkout's directory
//!   fd ([`Root`]). A symlink is shown as a symlink (its target text) and
//!   is never followed. `.git` is not shown and cannot be named.
//! - The agent can write any path into `.git/index`. Each index path is
//!   checked with [`check_git_path`] before the daemon opens it, and
//!   [`Root::lookup`] refuses `..`, `.`, `.git`, and an absolute path too.
//! - [`ReadGit`] refuses a checkout with an alternates file or a symlink
//!   in place of a main name of `.git`, before each command too
//!   (`crate::git`), for a clear error. The sandbox is the boundary.
//! - An error has no stderr of git and no path of the daemon: paths are
//!   relative to the checkout.
//! - The changes in the work tree (not staged) are found without git: the
//!   daemon reads each index entry's file, makes its blob ID in Rust
//!   (`blob <len>\0<bytes>`), and makes the diff with `similar`. `git
//!   status`, `git diff` against the work tree, and `git hash-object` run
//!   the clean filter, so they are not used. A file that a filter or
//!   `core.autocrlf` changes on checkout shows as changed. The Rust diff
//!   has a line cap and deadlines ([`MAX_DIFF_INPUT_LINES`],
//!   [`DIFF_FILE_TIME`], [`DIFF_TOTAL_TIME`]).
//!
//! Every call is blocking: the API runs it on the blocking pool.

use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat, readlinkat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};
use serde::Serialize;

use crate::git::{CheckoutDir, GitError, ReadGit, ReadRepo};

/// The longest one git command of the browser may take.
pub const CMD_TIMEOUT: Duration = Duration::from_secs(10);
/// The most bytes of one file in an answer.
pub const MAX_FILE: usize = 1 << 20;
/// The most entries of one directory listing.
pub const MAX_ENTRIES: usize = 5000;
/// The most diff lines of one file, and of one answer.
pub const MAX_FILE_LINES: usize = 2000;
pub const MAX_TOTAL_LINES: usize = 20_000;
/// The most bytes of `git diff-*` output that the daemon reads.
const MAX_DIFF_OUT: usize = 16 << 20;
/// The most commits on one page.
pub const MAX_COMMITS: usize = 200;
/// The most untracked paths in an answer.
const MAX_UNTRACKED: usize = 1000;
/// The most changed work tree files that get a patch.
const MAX_PATCHED: usize = 300;
/// The most lines of one side of a diff that the daemon makes in Rust.
/// A file with more gets no patch, and its counts are not known.
pub const MAX_DIFF_INPUT_LINES: usize = 20_000;
/// The longest the daemon diffs one file in Rust, and all the files of
/// one answer. At the file deadline, `similar` stops its search and
/// gives a diff that is correct but not the smallest.
pub const DIFF_FILE_TIME: Duration = Duration::from_secs(2);
pub const DIFF_TOTAL_TIME: Duration = Duration::from_secs(8);
/// The work tree files and bytes that one comparison reads at most.
#[derive(Debug, Clone, Copy)]
pub struct HashBudget {
    pub files: usize,
    pub bytes: u64,
}
pub const FULL_BUDGET: HashBudget = HashBudget {
    files: 50_000,
    bytes: 256 << 20,
};
/// For the tree list: one comparison for each agent tree.
pub const LIST_BUDGET: HashBudget = HashBudget {
    files: 20_000,
    bytes: 64 << 20,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum BrowseError {
    /// The request names a path or an ID that the browser refuses.
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{0}")]
    Io(String),
}

fn io_err(e: impl std::fmt::Display) -> BrowseError {
    BrowseError::Io(e.to_string())
}

/// Where a tree reads from.
#[derive(Debug, Clone)]
pub enum Tree {
    /// The daemon's bare mirror, at the commit of `rev`.
    Mirror { git_dir: PathBuf, rev: String },
    /// An agent checkout, opened ([`CheckoutDir`]): git reads its object
    /// store and index in the read sandbox, the daemon reads its work
    /// tree from the same fd.
    Checkout { dir: Arc<CheckoutDir> },
}

impl Tree {
    fn repo(&self) -> ReadRepo<'_> {
        match self {
            Tree::Mirror { git_dir, .. } => ReadRepo::Mirror(git_dir),
            Tree::Checkout { dir } => ReadRepo::Checkout(dir),
        }
    }
}

/// A path from a request, normalised: relative, `/` between the parts, no
/// empty part, no `.` or `..`, no NUL, no `.git` part. The root is `""`.
pub fn clean_path(p: &str) -> Result<String, BrowseError> {
    let bad = |why: &str| BrowseError::BadRequest(format!("bad path `{p}`: {why}"));
    if p.contains('\0') {
        return Err(bad("it has a NUL"));
    }
    if p.starts_with('/') {
        return Err(bad("it is absolute; give a path relative to the repo"));
    }
    let mut parts = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(bad("`..` is not allowed")),
            x if x.eq_ignore_ascii_case(".git") => {
                return Err(bad("`.git` is not shown"));
            }
            x => parts.push(x),
        }
    }
    Ok(parts.join("/"))
}

/// A path that git gives (an index entry): it reaches the disk as it is,
/// with no normalisation, so it must be relative, with `/` between the
/// parts, and with no empty part, no `.` or `..`, no NUL, and no `.git`
/// part in any case. The answer is the reason for a refusal.
pub fn check_git_path(p: &str) -> Result<(), &'static str> {
    if p.is_empty() {
        return Err("the path is empty");
    }
    if p.contains('\0') {
        return Err("the path has a NUL");
    }
    if p.starts_with('/') {
        return Err("the path is absolute");
    }
    for part in p.split('/') {
        match part {
            "" => return Err("the path has an empty part"),
            "." | ".." => return Err("the path has a `.` or `..` part"),
            x if x.eq_ignore_ascii_case(".git") => return Err("the path has a `.git` part"),
            _ => {}
        }
    }
    Ok(())
}

/// An object ID that git gives: 40 or 64 lower-case hex digits.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A commit ID from a request: 4 to 64 hex digits.
pub fn check_sha(s: &str) -> Result<(), BrowseError> {
    if (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(BrowseError::BadRequest(format!(
            "bad commit `{s}`: 4 to 64 hex digits"
        )))
    }
}

// --- The work tree on disk, with no symlink followed ---------------------

/// What a path in the work tree is.
enum Node {
    Dir(OwnedFd),
    File(std::fs::File, u64),
    Symlink(String),
    Missing,
    Other,
}

/// The checkout's directory: the fd of the [`CheckoutDir`], which the
/// daemon opened once. Every lookup walks from it, one component at a
/// time, with `O_NOFOLLOW`; no lookup uses a path from the top.
struct Root(OwnedFd);

const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);

fn kind_of(st: &FileStat) -> SFlag {
    SFlag::from_bits_truncate(st.st_mode & SFlag::S_IFMT.bits())
}

impl Root {
    fn of(c: &CheckoutDir) -> Result<Self, BrowseError> {
        Ok(Self(c.fd().try_clone().map_err(io_err)?))
    }

    /// Look up a [`clean_path`] path (`""` is the root). Any other path,
    /// and a path with a `.git` part, is `BadRequest`: no caller can step
    /// out of the checkout or into `.git`.
    fn lookup(&self, rel: &str) -> Result<Node, BrowseError> {
        if rel.is_empty() {
            return Ok(Node::Dir(self.0.try_clone().map_err(io_err)?));
        }
        check_git_path(rel).map_err(|why| BrowseError::BadRequest(format!("bad path: {why}")))?;
        let parts: Vec<&str> = rel.split('/').collect();
        let (last, dirs) = parts.split_last().ok_or_else(|| io_err("empty path"))?;
        let mut cur: Option<OwnedFd> = None;
        for d in dirs {
            let next = {
                let at = cur.as_ref().map_or(self.0.as_fd(), |f| f.as_fd());
                openat(at, *d, DIR_FLAGS, Mode::empty())
            };
            match next {
                Ok(fd) => cur = Some(fd),
                // A part is a symlink, a file, or gone: nothing under it.
                Err(Errno::ENOENT | Errno::ELOOP | Errno::ENOTDIR) => return Ok(Node::Missing),
                Err(e) => return Err(io_err(e)),
            }
        }
        let at = cur.as_ref().map_or(self.0.as_fd(), |f| f.as_fd());
        let st = match fstatat(at, *last, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::ENOENT | Errno::ENOTDIR) => return Ok(Node::Missing),
            Err(e) => return Err(io_err(e)),
        };
        let kind = kind_of(&st);
        if kind == SFlag::S_IFLNK {
            let t = readlinkat(at, *last).map_err(io_err)?;
            return Ok(Node::Symlink(t.to_string_lossy().into_owned()));
        }
        if kind == SFlag::S_IFDIR {
            return match openat(at, *last, DIR_FLAGS, Mode::empty()) {
                Ok(fd) => Ok(Node::Dir(fd)),
                Err(Errno::ENOENT | Errno::ELOOP | Errno::ENOTDIR) => Ok(Node::Missing),
                Err(e) => Err(io_err(e)),
            };
        }
        if kind != SFlag::S_IFREG {
            return Ok(Node::Other);
        }
        // O_NONBLOCK: a FIFO put here after the stat does not hang the
        // open. The fstat below then says it is not a file.
        let fd = match openat(
            at,
            *last,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(Node::Missing),
            Err(Errno::ELOOP) => return Ok(Node::Other),
            Err(e) => return Err(io_err(e)),
        };
        let st = fstat(&fd).map_err(io_err)?;
        if kind_of(&st) != SFlag::S_IFREG {
            return Ok(Node::Other);
        }
        Ok(Node::File(
            std::fs::File::from(fd),
            st.st_size.max(0) as u64,
        ))
    }
}

// --- Shapes ---------------------------------------------------------------

/// One entry of a directory.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: String,
    /// `dir`, `file`, `symlink`, `submodule`, or `other`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The target text of a symlink. The daemon does not follow it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub path: String,
    pub entries: Vec<Entry>,
    /// More than [`MAX_ENTRIES`] entries: the rest are not in the answer.
    pub truncated: bool,
}

/// One file of a tree.
#[derive(Debug, Clone)]
pub struct FileData {
    pub path: String,
    pub size: u64,
    /// At most [`MAX_FILE`] bytes.
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// One file of a diff.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    /// `added`, `deleted`, `modified`, `typechange`, `mode`, `unmerged`,
    /// or `refused` (an index entry with a path or an ID that the daemon
    /// does not read; `note` says why).
    pub status: &'static str,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    /// The hunks of a unified diff (from the first `@@` line), with no
    /// file header. Empty for a binary file or a mode change.
    pub patch: String,
    /// The patch is cut: at [`MAX_FILE_LINES`], at the answer's line cap,
    /// or at the output cap.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiffSet {
    pub files: Vec<FileDiff>,
    /// A cap cut a patch or left files out.
    pub truncated: bool,
}

/// The lines that are left in one answer.
pub struct LineBudget(pub usize);

impl Default for LineBudget {
    fn default() -> Self {
        Self(MAX_TOTAL_LINES)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    /// RFC 3339.
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitDetail {
    #[serde(flatten)]
    pub commit: Commit,
    pub committer_name: String,
    pub committer_date: String,
    pub message: String,
    pub diff: DiffSet,
}

/// The counts of the work tree changes of a checkout.
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct Dirty {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    /// The budget ran out: the counts are a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Uncommitted {
    /// The index against HEAD.
    pub staged: DiffSet,
    /// The work tree against the index.
    pub unstaged: DiffSet,
    pub untracked: Vec<String>,
    pub untracked_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The facts of an agent checkout for the tree list.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CheckoutInfo {
    pub head: Option<String>,
    pub branch: Option<String>,
    /// The ref that the branch is compared with, e.g.
    /// `refs/remotes/origin/main`.
    pub base_ref: Option<String>,
    /// The merge base of HEAD and `base_ref`.
    pub base: Option<String>,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
}

// --- git helpers ----------------------------------------------------------

fn opt_line(git: &ReadGit, repo: ReadRepo<'_>, args: &[&str]) -> Option<String> {
    git.text(repo, args)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// The object format of the repo: `sha1` or `sha256`.
fn object_format(git: &ReadGit, repo: ReadRepo<'_>) -> String {
    opt_line(git, repo, &["rev-parse", "--show-object-format"]).unwrap_or_else(|| "sha1".into())
}

fn empty_tree(format: &str) -> &'static str {
    if format == "sha256" {
        "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
    } else {
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    }
}

/// The blob ID of `bytes`, as git makes it, with no filter.
fn blob_id(format: &str, bytes: &[u8]) -> String {
    use sha1::Digest as _;
    let header = format!("blob {}\0", bytes.len());
    if format == "sha256" {
        let mut h = sha2::Sha256::new();
        h.update(header.as_bytes());
        h.update(bytes);
        hex(&h.finalize())
    } else {
        let mut h = sha1::Sha1::new();
        h.update(header.as_bytes());
        h.update(bytes);
        hex(&h.finalize())
    }
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// Text: no NUL in the first 8000 bytes, and UTF-8 (a character cut at
/// the end counts as UTF-8).
pub fn is_text(b: &[u8]) -> bool {
    if b[..b.len().min(8000)].contains(&0) {
        return false;
    }
    match std::str::from_utf8(b) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// The HEAD commit, or `None` on an unborn branch.
fn head(git: &ReadGit, repo: ReadRepo<'_>) -> Option<String> {
    opt_line(git, repo, &["rev-parse", "-q", "--verify", "HEAD^{commit}"])
}

/// The default branch of a mirror: its HEAD.
pub fn mirror_default_branch(git: &ReadGit, git_dir: &Path) -> Option<String> {
    opt_line(
        git,
        git_dir.into(),
        &["symbolic-ref", "-q", "--short", "HEAD"],
    )
}

/// The commit of `rev` in a repo, if it has one.
pub fn resolve<'a>(git: &ReadGit, repo: impl Into<ReadRepo<'a>>, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    opt_line(git, repo.into(), &["rev-parse", "-q", "--verify", &spec])
}

/// The facts of an agent checkout. `default_branch` is the repo's; with
/// none, `refs/remotes/origin/HEAD` names it.
pub fn checkout_info(
    git: &ReadGit,
    dir: &CheckoutDir,
    default_branch: Option<&str>,
) -> CheckoutInfo {
    let dir = ReadRepo::from(dir);
    let head = head(git, dir);
    let branch = opt_line(git, dir, &["symbolic-ref", "-q", "--short", "HEAD"]);
    let default = default_branch.map(str::to_owned).or_else(|| {
        opt_line(
            git,
            dir,
            &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
        )
        .map(|s| s.trim_start_matches("origin/").to_owned())
    });
    let mut out = CheckoutInfo {
        head: head.clone(),
        branch,
        ..Default::default()
    };
    let Some(default) = default else {
        return out;
    };
    let base_ref = [
        format!("refs/remotes/origin/{default}"),
        format!("refs/heads/{default}"),
    ]
    .into_iter()
    .find(|r| resolve(git, dir, r).is_some());
    let (Some(base_ref), Some(head)) = (base_ref, head) else {
        return out;
    };
    out.base = opt_line(git, dir, &["merge-base", &head, &base_ref]);
    let range = format!("{base_ref}...{head}");
    if let Some(counts) = opt_line(git, dir, &["rev-list", "--left-right", "--count", &range])
        && let Some((l, r)) = counts.split_once('\t')
    {
        out.behind = l.trim().parse().ok();
        out.ahead = r.trim().parse().ok();
    }
    out.base_ref = Some(base_ref);
    out
}

// --- Files ----------------------------------------------------------------

/// The `<rev>:<path>` of a mirror tree.
fn spec(rev: &str, path: &str) -> String {
    format!("{rev}:{path}")
}

/// One `ls-tree -z -l` line: mode, type, id, size, name.
fn parse_ls_tree(rec: &[u8]) -> Option<(String, String, String, Option<u64>, String)> {
    let tab = rec.iter().position(|&b| b == b'\t')?;
    let head = String::from_utf8_lossy(&rec[..tab]).into_owned();
    let name = String::from_utf8_lossy(&rec[tab + 1..]).into_owned();
    let mut it = head.split_whitespace();
    let mode = it.next()?.to_owned();
    let kind = it.next()?.to_owned();
    let id = it.next()?.to_owned();
    let size = it.next().and_then(|s| s.parse().ok());
    Some((mode, kind, id, size, name))
}

fn entry_kind(mode: &str, kind: &str) -> &'static str {
    match (mode, kind) {
        (_, "tree") => "dir",
        (_, "commit") => "submodule",
        ("120000", _) => "symlink",
        _ => "file",
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn sort_entries(v: &mut [Entry]) {
    v.sort_by(|a, b| {
        (a.kind != "dir")
            .cmp(&(b.kind != "dir"))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// The entries of the directory at `path` ([`clean_path`]): folders first,
/// then by name. `.git` is not in the answer.
pub fn list_dir(git: &ReadGit, tree: &Tree, path: &str) -> Result<Listing, BrowseError> {
    let not_dir = || BrowseError::NotFound(format!("no directory `{path}` in this tree"));
    let mut entries = Vec::new();
    let mut truncated = false;
    match tree {
        Tree::Mirror { git_dir, rev } => {
            let out = git
                .run(git_dir, &["ls-tree", "-z", "-l", &spec(rev, path)], 8 << 20)
                .map_err(|_| not_dir())?;
            let mut links = 0;
            for rec in out.bytes.split(|&b| b == 0).filter(|r| !r.is_empty()) {
                let Some((mode, kind, id, size, name)) = parse_ls_tree(rec) else {
                    continue;
                };
                if name.eq_ignore_ascii_case(".git") {
                    continue;
                }
                if entries.len() >= MAX_ENTRIES {
                    truncated = true;
                    break;
                }
                let k = entry_kind(&mode, &kind);
                let target = if k == "symlink" && links < 200 {
                    links += 1;
                    git.run(git_dir, &["cat-file", "blob", &id], 4096)
                        .ok()
                        .map(|o| o.text())
                } else {
                    None
                };
                entries.push(Entry {
                    path: join(path, &name),
                    name,
                    kind: k,
                    size: if k == "file" { size } else { None },
                    target,
                });
            }
            truncated |= out.truncated;
        }
        Tree::Checkout { dir } => {
            let root = Root::of(dir)?;
            let Node::Dir(fd) = root.lookup(path)? else {
                return Err(not_dir());
            };
            let at = fd.try_clone().map_err(io_err)?;
            let mut d = nix::dir::Dir::from_fd(fd).map_err(io_err)?;
            let mut names = Vec::new();
            for e in d.iter() {
                let e = e.map_err(io_err)?;
                let n = e.file_name().to_bytes();
                if n == b"." || n == b".." || n.eq_ignore_ascii_case(b".git") {
                    continue;
                }
                if names.len() >= MAX_ENTRIES {
                    truncated = true;
                    break;
                }
                names.push(n.to_vec());
            }
            for n in names {
                let os = OsStr::from_bytes(&n);
                let name = os.to_string_lossy().into_owned();
                let Ok(st) = fstatat(&at, os, AtFlags::AT_SYMLINK_NOFOLLOW) else {
                    continue;
                };
                let k = kind_of(&st);
                let (kind, size, target) = if k == SFlag::S_IFDIR {
                    ("dir", None, None)
                } else if k == SFlag::S_IFREG {
                    ("file", Some(st.st_size.max(0) as u64), None)
                } else if k == SFlag::S_IFLNK {
                    let t = readlinkat(&at, os)
                        .ok()
                        .map(|t| t.to_string_lossy().into_owned());
                    ("symlink", None, t)
                } else {
                    ("other", None, None)
                };
                entries.push(Entry {
                    path: join(path, &name),
                    name,
                    kind,
                    size,
                    target,
                });
            }
        }
    }
    sort_entries(&mut entries);
    Ok(Listing {
        path: path.to_owned(),
        entries,
        truncated,
    })
}

/// The file at `path` ([`clean_path`]): at most [`MAX_FILE`] bytes. A
/// directory, a symlink, or a submodule is `BadRequest`.
pub fn read_file(git: &ReadGit, tree: &Tree, path: &str) -> Result<FileData, BrowseError> {
    let missing = || BrowseError::NotFound(format!("no file `{path}` in this tree"));
    if path.is_empty() {
        return Err(BrowseError::BadRequest("name a file".into()));
    }
    match tree {
        Tree::Mirror { git_dir, rev } => {
            let out = git
                .run(git_dir, &["ls-tree", "-z", "-l", rev, "--", path], 1 << 16)
                .map_err(|_| missing())?;
            let rec = out
                .bytes
                .split(|&b| b == 0)
                .find(|r| !r.is_empty())
                .ok_or_else(missing)?;
            let (mode, kind, id, size, name) = parse_ls_tree(rec).ok_or_else(missing)?;
            if name != path {
                return Err(missing());
            }
            match entry_kind(&mode, &kind) {
                "file" => {}
                k => {
                    return Err(BrowseError::BadRequest(format!(
                        "`{path}` is a {k}, not a file"
                    )));
                }
            }
            let out = git.run(git_dir, &["cat-file", "blob", &id], MAX_FILE)?;
            Ok(FileData {
                path: path.to_owned(),
                size: size.unwrap_or(out.bytes.len() as u64),
                bytes: out.bytes,
                truncated: out.truncated,
            })
        }
        Tree::Checkout { dir } => match Root::of(dir)?.lookup(path)? {
            Node::File(f, size) => {
                let mut bytes = Vec::new();
                f.take(MAX_FILE as u64)
                    .read_to_end(&mut bytes)
                    .map_err(io_err)?;
                Ok(FileData {
                    path: path.to_owned(),
                    size,
                    truncated: size > bytes.len() as u64,
                    bytes,
                })
            }
            Node::Missing => Err(missing()),
            Node::Dir(_) => Err(BrowseError::BadRequest(format!(
                "`{path}` is a dir, not a file"
            ))),
            Node::Symlink(t) => Err(BrowseError::BadRequest(format!(
                "`{path}` is a symlink to `{t}`; the daemon does not follow it"
            ))),
            Node::Other => Err(BrowseError::BadRequest(format!(
                "`{path}` is not a regular file"
            ))),
        },
    }
}

// --- Diffs ----------------------------------------------------------------

/// Cut a patch to the per-file cap and to the answer's budget.
fn cap_patch(patch: &str, budget: &mut LineBudget) -> (String, bool) {
    let limit = MAX_FILE_LINES.min(budget.0);
    let mut out = String::new();
    let mut n = 0;
    let mut cut = false;
    for line in patch.split_inclusive('\n') {
        if n >= limit {
            cut = true;
            break;
        }
        out.push_str(line);
        n += 1;
    }
    budget.0 = budget.0.saturating_sub(n);
    (out, cut)
}

/// One file section of `git diff-* -p` output: the status and the hunks.
fn split_section(sec: &str) -> (&'static str, bool, &str) {
    let mut status = "modified";
    let mut binary = false;
    let mut body_at = sec.len();
    let mut pos = 0;
    for line in sec.split_inclusive('\n') {
        if line.starts_with("@@") {
            body_at = pos;
            break;
        }
        if line.starts_with("new file mode") {
            status = "added";
        } else if line.starts_with("deleted file mode") {
            status = "deleted";
        } else if line.starts_with("old mode") && status == "modified" {
            status = "mode";
        } else if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
            binary = true;
        }
        pos += line.len();
    }
    if status == "mode" && body_at < sec.len() {
        status = "modified";
    }
    (status, binary, &sec[body_at..])
}

/// A diff from git plumbing: `args` is `diff-tree -r <a> <b>` or
/// `diff-index --cached <tree>`. The file list comes from `--numstat -z`
/// and the patches from `-p`, in the same order.
fn git_diff(
    git: &ReadGit,
    dir: ReadRepo<'_>,
    args: &[&str],
    budget: &mut LineBudget,
) -> Result<DiffSet, BrowseError> {
    let common = [
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-color",
    ];
    let (cmd, rest) = args
        .split_first()
        .ok_or_else(|| io_err("no diff command"))?;
    let mut numstat: Vec<&str> = vec![cmd, "--numstat", "-z"];
    numstat.extend(common);
    numstat.extend(rest);
    let stats = git.run(dir, &numstat, 8 << 20)?;
    let mut patch_args: Vec<&str> = vec![cmd, "-p"];
    patch_args.extend(common);
    patch_args.extend(rest);
    let patch = git.run(dir, &patch_args, MAX_DIFF_OUT)?;
    let text = patch.text();
    let mut sections: Vec<&str> = Vec::new();
    let mut start = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(s) = start {
                sections.push(&text[s..pos]);
            }
            start = Some(pos);
        }
        pos += line.len();
    }
    if let Some(s) = start {
        sections.push(&text[s..]);
    }
    let mut set = DiffSet {
        truncated: stats.truncated || patch.truncated,
        ..Default::default()
    };
    let last = sections.len();
    for (i, rec) in stats
        .bytes
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .enumerate()
    {
        let rec = String::from_utf8_lossy(rec);
        let mut it = rec.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let binary = a == "-";
        let mut f = FileDiff {
            path: path.to_owned(),
            status: "modified",
            additions: a.parse().unwrap_or(0),
            deletions: d.parse().unwrap_or(0),
            binary,
            patch: String::new(),
            truncated: false,
            note: None,
        };
        match sections.get(i) {
            Some(sec) => {
                // The last section of a cut output is cut too.
                let cut_out = patch.truncated && i + 1 == last;
                let (status, bin, body) = split_section(sec);
                f.status = status;
                f.binary |= bin;
                if !f.binary {
                    let (p, cut) = cap_patch(body, budget);
                    f.patch = p;
                    f.truncated = cut || cut_out;
                }
            }
            None => {
                f.truncated = !binary;
                f.note = Some("the diff output was over the cap".into());
            }
        }
        set.truncated |= f.truncated;
        set.files.push(f);
    }
    Ok(set)
}

/// The diff between two commits (tree to tree: no filter runs).
pub fn diff_commits(
    git: &ReadGit,
    tree: &Tree,
    from: &str,
    to: &str,
    budget: &mut LineBudget,
) -> Result<DiffSet, BrowseError> {
    git_diff(git, tree.repo(), &["diff-tree", "-r", from, to], budget)
}

/// A unified diff of two texts, made in Rust, until `deadline`. After the
/// deadline, the diff is correct but not the smallest; the last value is
/// true then.
fn unified(
    old: &str,
    new: &str,
    budget: &mut LineBudget,
    deadline: Instant,
) -> (String, u64, u64, bool, bool) {
    let diff = similar::TextDiff::configure()
        .deadline(deadline)
        .diff_lines(old, new);
    let late = Instant::now() >= deadline;
    let mut adds = 0;
    let mut dels = 0;
    for op in diff.iter_all_changes() {
        match op.tag() {
            similar::ChangeTag::Insert => adds += 1,
            similar::ChangeTag::Delete => dels += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let mut text = String::new();
    for h in diff.unified_diff().context_radius(3).iter_hunks() {
        text.push_str(&h.to_string());
    }
    let (p, cut) = cap_patch(&text, budget);
    (p, adds, dels, cut, late)
}

/// One index entry: mode, blob ID, stage, path.
struct IndexEntry {
    mode: String,
    id: String,
    stage: u8,
    path: String,
}

/// The entries of the index, and the entries that the daemon refuses
/// (path and reason). The agent can write any bytes in `.git/index`, so
/// each path is checked with [`check_git_path`] before it reaches the
/// disk (a path `../../x` would read a file of the daemon), and each mode
/// and ID before it reaches git.
#[allow(clippy::type_complexity)]
fn index_entries(
    git: &ReadGit,
    dir: &CheckoutDir,
) -> Result<(Vec<IndexEntry>, Vec<(String, &'static str)>, bool), BrowseError> {
    let out = git.run(dir, &["ls-files", "-s", "-z"], 64 << 20)?;
    let mut v = Vec::new();
    let mut refused = Vec::new();
    let mut recs: Vec<&[u8]> = out
        .bytes
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .collect();
    // A cut output ends in a cut entry.
    if out.truncated {
        recs.pop();
    }
    for rec in recs {
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let head = String::from_utf8_lossy(&rec[..tab]);
        let path = String::from_utf8_lossy(&rec[tab + 1..]).into_owned();
        let mut it = head.split_whitespace();
        let (Some(mode), Some(id), Some(stage)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if let Err(why) = check_git_path(&path) {
            refused.push((path, why));
            continue;
        }
        if !matches!(mode, "100644" | "100755" | "120000" | "160000") || !is_object_id(id) {
            refused.push((path, "the index entry has a bad mode or ID"));
            continue;
        }
        v.push(IndexEntry {
            mode: mode.to_owned(),
            id: id.to_owned(),
            stage: stage.parse().unwrap_or(0),
            path,
        });
    }
    Ok((v, refused, out.truncated))
}

/// A work tree file against its index entry.
enum WorkState {
    Same,
    Modified(Vec<u8>),
    Deleted,
    TypeChange,
    /// The budget ran out before this file.
    Unknown,
}

fn compare(
    root: &Root,
    e: &IndexEntry,
    format: &str,
    left: &mut HashBudget,
) -> Result<WorkState, BrowseError> {
    let link = e.mode == "120000";
    Ok(match root.lookup(&e.path)? {
        Node::Missing => WorkState::Deleted,
        Node::Symlink(t) if link => {
            if blob_id(format, t.as_bytes()) == e.id {
                WorkState::Same
            } else {
                WorkState::Modified(t.into_bytes())
            }
        }
        Node::File(f, size) if !link => {
            if left.files == 0 || left.bytes < size {
                return Ok(WorkState::Unknown);
            }
            left.files -= 1;
            left.bytes -= size;
            let mut bytes = Vec::with_capacity(size.min(1 << 24) as usize);
            f.take(size).read_to_end(&mut bytes).map_err(io_err)?;
            if blob_id(format, &bytes) == e.id {
                WorkState::Same
            } else {
                WorkState::Modified(bytes)
            }
        }
        _ => WorkState::TypeChange,
    })
}

/// The counts of the uncommitted changes of a checkout (no patches).
pub fn dirty(git: &ReadGit, dir: &CheckoutDir, budget: HashBudget) -> Result<Dirty, BrowseError> {
    let u = uncommitted_inner(git, dir, budget, false, &mut LineBudget(0))?;
    Ok(Dirty {
        staged: u.0.staged.files.len(),
        unstaged: u.0.unstaged.files.len(),
        untracked: u.0.untracked.len(),
        partial: u.1 || u.0.untracked_truncated || u.0.staged.truncated,
    })
}

/// The uncommitted changes of a checkout: staged (the index against
/// HEAD, by `diff-index --cached`), not staged (the work tree against the
/// index, compared in Rust), and the untracked paths (`ls-files
/// --others`, which reads no file content).
pub fn uncommitted(
    git: &ReadGit,
    dir: &CheckoutDir,
    budget: &mut LineBudget,
) -> Result<Uncommitted, BrowseError> {
    Ok(uncommitted_inner(git, dir, FULL_BUDGET, true, budget)?.0)
}

fn uncommitted_inner(
    git: &ReadGit,
    dir: &CheckoutDir,
    hash: HashBudget,
    patches: bool,
    budget: &mut LineBudget,
) -> Result<(Uncommitted, bool), BrowseError> {
    let repo = ReadRepo::from(dir);
    let format = object_format(git, repo);
    let base = head(git, repo).unwrap_or_else(|| empty_tree(&format).to_owned());
    let staged = if patches {
        git_diff(git, repo, &["diff-index", "--cached", &base], budget)?
    } else {
        let out = git.run(
            dir,
            &[
                "diff-index",
                "--cached",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--name-only",
                "-z",
                &base,
            ],
            8 << 20,
        )?;
        DiffSet {
            files: out
                .bytes
                .split(|&b| b == 0)
                .filter(|r| !r.is_empty())
                .map(|p| FileDiff {
                    path: String::from_utf8_lossy(p).into_owned(),
                    status: "modified",
                    additions: 0,
                    deletions: 0,
                    binary: false,
                    patch: String::new(),
                    truncated: false,
                    note: None,
                })
                .collect(),
            truncated: out.truncated,
        }
    };

    let root = Root::of(dir)?;
    let (entries, refused, index_cut) = index_entries(git, dir)?;
    let mut left = hash;
    let mut partial = index_cut;
    let mut unstaged = DiffSet::default();
    for (path, why) in refused {
        unstaged.files.push(FileDiff {
            path,
            status: "refused",
            additions: 0,
            deletions: 0,
            binary: false,
            patch: String::new(),
            truncated: false,
            note: Some(format!("{why}; the daemon does not read it")),
        });
    }
    // The time for all the Rust diffs of this answer.
    let until = Instant::now() + DIFF_TOTAL_TIME;
    let mut patched = 0;
    let mut unmerged: Vec<String> = Vec::new();
    for e in &entries {
        if e.stage != 0 {
            if unmerged.last() != Some(&e.path) {
                unmerged.push(e.path.clone());
            }
            continue;
        }
        // A submodule (gitlink) is not compared.
        if e.mode == "160000" {
            continue;
        }
        dir.in_time()?;
        let state = compare(&root, e, &format, &mut left)?;
        let mut f = FileDiff {
            path: e.path.clone(),
            status: "modified",
            additions: 0,
            deletions: 0,
            binary: false,
            patch: String::new(),
            truncated: false,
            note: None,
        };
        let new = match state {
            WorkState::Same => continue,
            WorkState::Unknown => {
                partial = true;
                continue;
            }
            WorkState::TypeChange => {
                f.status = "typechange";
                unstaged.files.push(f);
                continue;
            }
            WorkState::Deleted => {
                f.status = "deleted";
                None
            }
            WorkState::Modified(b) => Some(b),
        };
        if patches && patched < MAX_PATCHED && Instant::now() < until {
            patched += 1;
            let old = git.run(dir, &["cat-file", "blob", &e.id], MAX_FILE + 1)?;
            let new_b = new.as_deref().unwrap_or(b"");
            let lines = |b: &[u8]| b.iter().filter(|&&c| c == b'\n').count();
            if old.truncated || new_b.len() > MAX_FILE {
                f.truncated = true;
                f.note = Some("the file is over 1 MiB; no patch".into());
            } else if !is_text(&old.bytes) || !is_text(new_b) {
                f.binary = true;
            } else if lines(&old.bytes) > MAX_DIFF_INPUT_LINES
                || lines(new_b) > MAX_DIFF_INPUT_LINES
            {
                f.truncated = true;
                f.note = Some(format!(
                    "the file is too large to diff (over {MAX_DIFF_INPUT_LINES} lines); \
                     no patch, the counts are not known"
                ));
            } else {
                let deadline = until.min(Instant::now() + DIFF_FILE_TIME);
                let (p, a, d, cut, late) = unified(
                    &String::from_utf8_lossy(&old.bytes),
                    &String::from_utf8_lossy(new_b),
                    budget,
                    deadline,
                );
                f.patch = p;
                f.additions = a;
                f.deletions = d;
                f.truncated = cut;
                if late {
                    f.note = Some(
                        "the diff timed out: it is correct, but it can be larger than needed"
                            .into(),
                    );
                }
            }
        } else if patches && patched >= MAX_PATCHED {
            f.truncated = true;
            f.note = Some(format!("over {MAX_PATCHED} changed files; no patch"));
        } else if patches {
            f.truncated = true;
            f.note = Some("the diff time of the answer ran out; no patch".into());
        }
        unstaged.truncated |= f.truncated;
        unstaged.files.push(f);
    }
    for p in unmerged {
        unstaged.files.push(FileDiff {
            path: p,
            status: "unmerged",
            additions: 0,
            deletions: 0,
            binary: false,
            patch: String::new(),
            truncated: false,
            note: None,
        });
    }

    let out = git.run(
        dir,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        1 << 20,
    )?;
    let mut untracked = Vec::new();
    let mut untracked_truncated = out.truncated;
    for p in out.bytes.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        if untracked.len() >= MAX_UNTRACKED {
            untracked_truncated = true;
            break;
        }
        untracked.push(String::from_utf8_lossy(p).into_owned());
    }
    // A cut output ends in a cut path.
    if out.truncated {
        untracked.pop();
    }
    let note =
        partial.then(|| "the checkout is large: not every work tree file was compared".to_owned());
    Ok((
        Uncommitted {
            staged,
            unstaged,
            untracked,
            untracked_truncated,
            note,
        },
        partial,
    ))
}

// --- Commits --------------------------------------------------------------

const LOG_FORMAT: &str = "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x1e";

/// A page of commits of `range` (`<base>..<head>`, or one commit and its
/// history), newest first. `skip` is the cursor. The answer has the
/// commits and the cursor of the next page.
pub fn commits(
    git: &ReadGit,
    tree: &Tree,
    range: &str,
    skip: usize,
    limit: usize,
) -> Result<(Vec<Commit>, Option<usize>), BrowseError> {
    let limit = limit.clamp(1, MAX_COMMITS);
    let skip_s = format!("--skip={skip}");
    let max = format!("--max-count={}", limit + 1);
    let out = git.text(
        tree.repo(),
        &["log", "--no-color", LOG_FORMAT, &skip_s, &max, range, "--"],
    )?;
    let mut v: Vec<Commit> = out
        .split('\u{1e}')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            let f: Vec<&str> = rec.splitn(6, '\0').collect();
            (f.len() == 6).then(|| Commit {
                sha: f[0].to_owned(),
                parents: f[1].split_whitespace().map(str::to_owned).collect(),
                author_name: f[2].to_owned(),
                author_email: f[3].to_owned(),
                date: f[4].to_owned(),
                subject: f[5].trim_end().to_owned(),
            })
        })
        .collect();
    let next = (v.len() > limit).then_some(skip + limit);
    v.truncate(limit);
    Ok((v, next))
}

/// One commit: its message and its diff against its first parent (the
/// empty tree for a root commit).
pub fn commit(git: &ReadGit, tree: &Tree, sha: &str) -> Result<CommitDetail, BrowseError> {
    check_sha(sha)?;
    let dir = tree.repo();
    let full = resolve(git, dir, sha)
        .ok_or_else(|| BrowseError::NotFound(format!("no commit `{sha}` in this tree")))?;
    let out = git.text(
        dir,
        &[
            "log",
            "-1",
            "--no-color",
            "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x00%cn%x00%cI%x00%B",
            &full,
            "--",
        ],
    )?;
    let f: Vec<&str> = out.splitn(9, '\0').collect();
    if f.len() != 9 {
        return Err(io_err("cannot read the commit"));
    }
    let parents: Vec<String> = f[1].split_whitespace().map(str::to_owned).collect();
    let format = object_format(git, dir);
    let from = parents
        .first()
        .cloned()
        .unwrap_or_else(|| empty_tree(&format).to_owned());
    let diff = diff_commits(git, tree, &from, &full, &mut LineBudget::default())?;
    Ok(CommitDetail {
        commit: Commit {
            sha: f[0].to_owned(),
            parents,
            author_name: f[2].to_owned(),
            author_email: f[3].to_owned(),
            date: f[4].to_owned(),
            subject: f[5].to_owned(),
        },
        committer_name: f[6].to_owned(),
        committer_date: f[7].to_owned(),
        message: f[8].trim_end().to_owned(),
        diff,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::secrets::RunDirs;
    use std::process::Command;

    fn sh(dir: &Path, script: &str) {
        let st = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(st.success(), "{script}");
    }

    /// The runner of the tests: in the read sandbox, or on the host (with
    /// a note) when this machine cannot make one.
    fn git() -> ReadGit {
        ReadGit::sandboxed(CMD_TIMEOUT).unwrap_or_else(|| ReadGit::on_host(CMD_TIMEOUT))
    }

    /// The checkout at `dir`, opened as the API opens it.
    fn open(dir: &Path) -> CheckoutDir {
        let name = dir.file_name().unwrap().to_str().unwrap();
        CheckoutDir::open(dir.parent().unwrap(), name, "test").unwrap()
    }

    fn tree(dir: &Path) -> Tree {
        Tree::Checkout {
            dir: Arc::new(open(dir)),
        }
    }

    /// A checkout `ws/proj` of `origin.git`: `main` has `a.txt` and
    /// `docs/b.md`; the branch `feat` adds `c.txt` and changes `a.txt`.
    fn checkout(t: &Path) -> PathBuf {
        sh(
            t,
            "git init -q --bare -b main origin.git && git clone -q origin.git ws/proj 2>/dev/null; \
             cd ws/proj && git checkout -q -b main 2>/dev/null; \
             printf 'one\\ntwo\\n' > a.txt && mkdir docs && echo '# B' > docs/b.md && \
             git add . && git commit -q -m base && git push -q origin main && \
             git checkout -q -b feat && printf 'one\\nTWO\\n' > a.txt && echo c > c.txt && \
             git add . && git commit -q -m 'feat: c' -m 'The body.'",
        );
        t.join("ws/proj")
    }

    #[test]
    fn paths_are_normalised_and_refused() {
        assert_eq!(clean_path("").unwrap(), "");
        assert_eq!(clean_path("./a//b/").unwrap(), "a/b");
        for bad in [
            "..",
            "a/../b",
            "/etc/passwd",
            "a\0b",
            ".git",
            "a/.git/config",
            ".GIT/HEAD",
        ] {
            assert!(
                matches!(clean_path(bad), Err(BrowseError::BadRequest(_))),
                "{bad:?}"
            );
        }
        check_sha("abcd1234").unwrap();
        for bad in ["abc", "HEAD", "-p", "abcd^", &"a".repeat(65)] {
            assert!(check_sha(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_symlink_is_shown_and_never_followed() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let secret = t.path().join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("key"), "hunter2").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("out")).unwrap();
        std::os::unix::fs::symlink(secret.join("key"), dir.join("key")).unwrap();
        std::os::unix::fs::symlink("../../secret", dir.join("rel")).unwrap();
        let tree = tree(&dir);
        let g = git();

        let root = list_dir(&g, &tree, "").unwrap();
        let names: Vec<&str> = root.entries.iter().map(|e| e.name.as_str()).collect();
        // Folders first; `.git` is not there.
        assert_eq!(names, ["docs", "a.txt", "c.txt", "key", "out", "rel"]);
        let out = root.entries.iter().find(|e| e.name == "out").unwrap();
        assert_eq!(out.kind, "symlink");
        assert_eq!(out.target.as_deref(), Some(secret.to_str().unwrap()));
        let rel = root.entries.iter().find(|e| e.name == "rel").unwrap();
        assert_eq!(rel.target.as_deref(), Some("../../secret"));

        // A file symlink is refused, a dir symlink is not a dir, a path
        // through a symlink is not found.
        assert!(matches!(
            read_file(&g, &tree, "key"),
            Err(BrowseError::BadRequest(m)) if m.contains("symlink")
        ));
        assert!(matches!(
            list_dir(&g, &tree, "out"),
            Err(BrowseError::NotFound(_))
        ));
        for p in ["out/key", "rel/key"] {
            assert!(
                matches!(read_file(&g, &tree, p), Err(BrowseError::NotFound(_))),
                "{p}"
            );
        }
        assert!(matches!(
            list_dir(&g, &tree, "rel"),
            Err(BrowseError::NotFound(_))
        ));

        // The checkout dir itself as a symlink is refused, and the
        // message does not name the workspace.
        let link = t.path().join("ws/link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        match CheckoutDir::open(&t.path().join("ws"), "link", "t") {
            Err(GitError::Refused(m)) => {
                assert!(m.contains("not a plain directory"), "{m}");
                assert!(!m.contains(t.path().to_str().unwrap()), "{m}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_fifo_does_not_hang_a_read() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        nix::unistd::mkfifo(&dir.join("pipe"), Mode::from_bits_truncate(0o644)).unwrap();
        let tree = tree(&dir);
        let l = list_dir(&git(), &tree, "").unwrap();
        assert!(
            l.entries
                .iter()
                .any(|e| e.name == "pipe" && e.kind == "other")
        );
        assert!(matches!(
            read_file(&git(), &tree, "pipe"),
            Err(BrowseError::BadRequest(_))
        ));
    }

    #[test]
    fn files_and_caps() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let g = git();
        let tree = tree(&dir);
        let f = read_file(&g, &tree, "docs/b.md").unwrap();
        assert_eq!(f.bytes, b"# B\n");
        assert!(!f.truncated);

        // A big file: the first MiB only, and the real size.
        std::fs::write(dir.join("big.bin"), vec![b'x'; MAX_FILE * 2]).unwrap();
        let f = read_file(&g, &tree, "big.bin").unwrap();
        assert_eq!(f.bytes.len(), MAX_FILE);
        assert_eq!(f.size, (MAX_FILE * 2) as u64);
        assert!(f.truncated);
        assert!(is_text(&f.bytes));
        assert!(!is_text(b"PNG\0\x01"));

        // A big directory: the first MAX_ENTRIES entries.
        let many = dir.join("many");
        std::fs::create_dir(&many).unwrap();
        for i in 0..MAX_ENTRIES + 10 {
            std::fs::write(many.join(format!("f{i}")), "").unwrap();
        }
        let l = list_dir(&g, &tree, "many").unwrap();
        assert_eq!(l.entries.len(), MAX_ENTRIES);
        assert!(l.truncated);

        // The mirror reads the object store.
        let mirror = Tree::Mirror {
            git_dir: t.path().join("origin.git"),
            rev: "refs/heads/main".into(),
        };
        let l = list_dir(&g, &mirror, "").unwrap();
        let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["docs", "a.txt"]);
        assert_eq!(l.entries[1].size, Some(8));
        assert_eq!(
            read_file(&g, &mirror, "a.txt").unwrap().bytes,
            b"one\ntwo\n"
        );
        assert!(matches!(
            read_file(&g, &mirror, "docs"),
            Err(BrowseError::BadRequest(_))
        ));
        assert!(matches!(
            read_file(&g, &mirror, "nope"),
            Err(BrowseError::NotFound(_))
        ));
        assert!(matches!(
            list_dir(&g, &mirror, "a.txt"),
            Err(BrowseError::NotFound(_))
        ));
    }

    #[test]
    fn a_long_diff_is_cut() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let long: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("long.txt"), &long).unwrap();
        sh(&dir, "git add long.txt && git commit -q -m long");
        let g = git();
        let c = Arc::new(open(&dir));
        let tree = Tree::Checkout { dir: c.clone() };
        let info = checkout_info(&g, &c, Some("main"));
        let mut budget = LineBudget::default();
        let d = diff_commits(
            &g,
            &tree,
            info.base.as_deref().unwrap(),
            info.head.as_deref().unwrap(),
            &mut budget,
        )
        .unwrap();
        let f = d.files.iter().find(|f| f.path == "long.txt").unwrap();
        assert_eq!(f.status, "added");
        assert_eq!(f.additions, 3000);
        assert!(f.truncated);
        assert_eq!(f.patch.lines().count(), MAX_FILE_LINES);
        assert!(d.truncated);
        // The answer's budget: a second file gets what is left.
        let mut small = LineBudget(10);
        let d = diff_commits(
            &g,
            &tree,
            info.base.as_deref().unwrap(),
            info.head.as_deref().unwrap(),
            &mut small,
        )
        .unwrap();
        let lines: usize = d.files.iter().map(|f| f.patch.lines().count()).sum();
        assert!(lines <= 10, "{lines}");
    }

    #[test]
    fn the_blob_id_is_the_one_git_makes() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let o = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["hash-object", "a.txt"])
            .output()
            .unwrap();
        let want = String::from_utf8(o.stdout).unwrap().trim().to_owned();
        let bytes = std::fs::read(dir.join("a.txt")).unwrap();
        assert_eq!(blob_id("sha1", &bytes), want);
    }

    /// The fixture of the hostile repo: a filter, a textconv, and an
    /// external diff that each touch `marker` if git runs them.
    fn hostile(t: &Path) -> (PathBuf, PathBuf) {
        let dir = checkout(t);
        let marker = t.join("pwned");
        let m = marker.display();
        // A staged change and the attributes, before the drivers exist.
        sh(
            &dir,
            "echo '* filter=x diff=x' > .gitattributes && echo staged > s.txt && \
             git add .gitattributes s.txt",
        );
        let cfg = dir.join(".git/config");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        text.push_str(&format!(
            "[filter \"x\"]\n\tclean = touch {m}; cat\n\tsmudge = touch {m}; cat\n\
             \tprocess = touch {m}\n\trequired = true\n\
             [diff \"x\"]\n\ttextconv = touch {m}; cat\n\tcommand = touch {m}\n\
             [core]\n\tfsmonitor = touch {m}\n\tpager = touch {m}\n\
             [log]\n\tshowSignature = true\n[gpg]\n\tprogram = touch {m}\n\
             [diff]\n\texternal = touch {m}\n"
        ));
        std::fs::write(&cfg, text).unwrap();
        // A change in the work tree and an untracked file.
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        (dir, marker)
    }

    #[test]
    fn a_hostile_repo_config_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let (dir, marker) = hostile(t.path());
        let store = RunDirs::new(&t.path().join("state"));
        match git().check_checkout(&open(&dir), &store) {
            Err(GitError::Refused(m)) => {
                assert!(!m.contains(t.path().to_str().unwrap()), "{m}");
                assert!(m.contains("filter.x.clean"), "{m}");
                assert!(m.contains("diff.x.textconv"), "{m}");
                assert!(m.contains("core.fsmonitor"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!marker.exists());
        // A clean config passes.
        std::fs::create_dir_all(t.path().join("c")).unwrap();
        let clean = checkout(&t.path().join("c"));
        git().check_checkout(&open(&clean), &store).unwrap();
    }

    /// The race of `crate::git`: the drivers are in the live config (as
    /// after an edit that follows the check). No call of the browser runs
    /// one of them. git on the host shows it for the commands themselves
    /// (a driver in the sandbox would leave no mark); the sandbox runs
    /// the same calls too, when this machine has one.
    #[test]
    fn no_filter_textconv_or_external_diff_runs() {
        let mut runners = vec![ReadGit::on_host(CMD_TIMEOUT)];
        runners.extend(ReadGit::sandboxed(CMD_TIMEOUT));
        for g in runners {
            hostile_reads(&g);
        }
    }

    fn hostile_reads(g: &ReadGit) {
        let t = tempfile::tempdir().unwrap();
        let (dir, marker) = hostile(t.path());
        let c = Arc::new(open(&dir));
        let tree = Tree::Checkout { dir: c.clone() };

        let l = list_dir(g, &tree, "").unwrap();
        assert!(l.entries.iter().any(|e| e.name == ".gitattributes"));
        assert_eq!(
            read_file(g, &tree, "a.txt").unwrap().bytes,
            b"one\nTWO\nthree\n"
        );
        let info = checkout_info(g, &c, Some("main"));
        assert_eq!(info.branch.as_deref(), Some("feat"));
        assert_eq!(info.ahead, Some(1));
        assert_eq!(info.behind, Some(0));
        let mut budget = LineBudget::default();
        let committed = diff_commits(
            g,
            &tree,
            info.base.as_deref().unwrap(),
            info.head.as_deref().unwrap(),
            &mut budget,
        )
        .unwrap();
        let paths: Vec<&str> = committed.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "c.txt"]);
        assert!(committed.files[0].patch.contains("+TWO"), "{committed:?}");

        let u = uncommitted(g, &c, &mut budget).unwrap();
        let staged: Vec<&str> = u.staged.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(staged, [".gitattributes", "s.txt"]);
        assert_eq!(u.unstaged.files.len(), 1, "{u:?}");
        let a = &u.unstaged.files[0];
        assert_eq!((a.path.as_str(), a.status), ("a.txt", "modified"));
        assert_eq!((a.additions, a.deletions), (1, 0));
        assert!(a.patch.starts_with("@@ "), "{}", a.patch);
        assert!(a.patch.contains("+three"), "{}", a.patch);
        assert_eq!(u.untracked, ["new.txt"]);
        let d = dirty(g, &c, LIST_BUDGET).unwrap();
        assert_eq!((d.staged, d.unstaged, d.untracked), (2, 1, 1));

        let (list, next) =
            commits(g, &tree, &format!("{}..HEAD", info.base.unwrap()), 0, 50).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].subject, "feat: c");
        assert!(next.is_none());
        let c = commit(g, &tree, &list[0].sha).unwrap();
        assert_eq!(c.message, "feat: c\n\nThe body.");
        assert_eq!(c.diff.files.len(), 2);
        // The root commit diffs against the empty tree.
        let (all, _) = commits(g, &tree, "HEAD", 0, 50).unwrap();
        let root = commit(g, &tree, &all.last().unwrap().sha).unwrap();
        assert!(root.diff.files.iter().all(|f| f.status == "added"));

        assert!(!marker.exists(), "a driver ran");
        // The fixture is real: a plain `git diff` runs the drivers.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["diff", "--stat"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(marker.exists(), "the fixture does not run its drivers");
    }

    /// Write `.git/index` by hand (version 2), with one entry for each
    /// path, each with the blob ID `id`: git itself never writes a path
    /// with `..`, `.git`, or a `/` in front, but an agent can.
    fn write_index(dir: &Path, paths: &[&str], id: &str) {
        use sha1::Digest as _;
        let mut paths: Vec<&str> = paths.to_vec();
        paths.sort();
        let sha: Vec<u8> = (0..20)
            .map(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let mut d = b"DIRC".to_vec();
        d.extend(2u32.to_be_bytes());
        d.extend((paths.len() as u32).to_be_bytes());
        for p in paths {
            let mut e = Vec::new();
            for v in [0u32, 0, 0, 0, 0, 0, 0o100644, 0, 0, 0] {
                e.extend(v.to_be_bytes());
            }
            e.extend(&sha);
            e.extend((p.len() as u16).to_be_bytes());
            e.extend(p.as_bytes());
            let pad = 8 - (e.len() % 8);
            e.extend(std::iter::repeat_n(0u8, pad));
            d.extend(e);
        }
        let sum = sha1::Sha1::digest(&d);
        d.extend(sum);
        std::fs::write(dir.join(".git/index"), d).unwrap();
    }

    fn rev(dir: &Path, spec: &str) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", spec])
            .output()
            .unwrap();
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    }

    /// R1 and R3 (docs/security-review.md §15): an index entry that names
    /// a path out of the checkout, a path in `.git`, or an absolute path
    /// is refused, and its file is never read.
    #[test]
    fn an_index_path_out_of_the_checkout_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        std::fs::write(t.path().join("secret.txt"), "TOPSECRET\n").unwrap();
        std::fs::write(dir.join(".git/secret.txt"), "TOPSECRET\n").unwrap();
        let abs = t.path().join("secret.txt");
        let abs = abs.to_str().unwrap();
        let id = rev(&dir, "HEAD:a.txt");
        let bad = [
            "../../secret.txt",
            "docs/../../../secret.txt",
            ".git/config",
            ".GIT/secret.txt",
            "docs/.git/x",
            "./a.txt",
            "docs//b.md",
            abs,
        ];
        let mut all = bad.to_vec();
        all.push("a.txt");
        write_index(&dir, &all, &id);
        let g = git();
        let c = open(&dir);
        let u = uncommitted(&g, &c, &mut LineBudget::default()).unwrap();
        let json = serde_json::to_string(&u).unwrap();
        assert!(!json.contains("TOPSECRET"), "{json}");
        assert!(!json.contains("[core]"), "{json}");
        for p in bad {
            let f = u.unstaged.files.iter().find(|f| f.path == p);
            let f = f.unwrap_or_else(|| panic!("{p} is not in {json}"));
            assert_eq!(f.status, "refused", "{p}");
            assert!(f.patch.is_empty(), "{p}");
            assert!(f.note.as_deref().unwrap().contains("does not read"), "{p}");
        }
        // `a.txt` is the same as its index entry (`two` against `TWO` is
        // the committed change, not in the work tree).
        assert!(
            !u.unstaged.files.iter().any(|f| f.path == "a.txt"),
            "{json}"
        );
        let d = dirty(&g, &c, LIST_BUDGET).unwrap();
        assert_eq!(d.unstaged, bad.len());

        // Defence in depth: the walk itself refuses such a path.
        let root = Root::of(&c).unwrap();
        for p in [
            "..",
            "../secret.txt",
            "a/../b",
            "/etc/passwd",
            ".",
            ".git/config",
            "a//b",
        ] {
            assert!(
                matches!(root.lookup(p), Err(BrowseError::BadRequest(_))),
                "{p}"
            );
        }
        for p in ["a.txt", "a..b", "..a", ".gitx"] {
            check_git_path(p).unwrap();
        }
    }

    /// An object store of another repo, named by `objects/info/alternates`:
    /// git would read its commits by ID. `other` has a commit that only it
    /// has.
    fn foreign(t: &Path) -> (PathBuf, String) {
        let other = t.join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(
            &other,
            "git init -q -b main && echo FOREIGN > f.txt && git add f.txt && git commit -q -m foreign",
        );
        let sha = rev(&other, "HEAD");
        (other.join(".git/objects"), sha)
    }

    /// R2: an alternates file (or an http-alternates file) is refused
    /// before any read; an empty alternates file is allowed.
    #[test]
    fn an_alternates_file_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let (objects, sha) = foreign(t.path());
        let store = RunDirs::new(&t.path().join("state"));
        let g = git();
        let c = Arc::new(open(&dir));
        let tree = Tree::Checkout { dir: c.clone() };
        let info = dir.join(".git/objects/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("alternates"), "").unwrap();
        g.check_checkout(&c, &store).unwrap();
        assert!(matches!(
            commit(&g, &tree, &sha),
            Err(BrowseError::NotFound(_))
        ));

        for (name, text) in [
            ("alternates", format!("{}\n", objects.display())),
            ("http-alternates", "http://127.0.0.1:1/x\n".to_owned()),
        ] {
            let _ = std::fs::remove_file(info.join("alternates"));
            std::fs::write(info.join(name), text).unwrap();
            match g.check_checkout(&c, &store) {
                Err(GitError::Refused(m)) => assert!(m.contains(name), "{m}"),
                other => panic!("{name}: {other:?}"),
            }
            // Each read command checks again: the file can come after the
            // check.
            for short in [&sha[..], &sha[..7]] {
                assert!(commit(&g, &tree, short).is_err(), "{name} {short}");
                match commits(&g, &tree, short, 0, 10) {
                    Err(BrowseError::Git(GitError::Refused(m))) => assert!(m.contains(name), "{m}"),
                    other => panic!("{name} {short}: {other:?}"),
                }
            }
            assert!(uncommitted(&g, &c, &mut LineBudget::default()).is_err());
            assert!(checkout_info(&g, &c, Some("main")).head.is_none());
            std::fs::remove_file(info.join(name)).unwrap();
        }
        // The fixture is real: git reads the foreign commit through the
        // file.
        std::fs::write(info.join("alternates"), format!("{}\n", objects.display())).unwrap();
        let o = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["cat-file", "-p", &format!("{sha}:f.txt")])
            .output()
            .unwrap();
        assert_eq!(o.stdout, b"FOREIGN\n");
    }

    /// R2: a symlink in place of a main part of `.git` (the object store,
    /// the index, HEAD, `objects/info`, `packed-refs`) is refused by the
    /// check. A symlink deeper in `.git` (a ref, a loose object) is not
    /// checked for reads: in the read sandbox it finds nothing.
    #[test]
    fn a_symlink_in_the_git_dir_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let (objects, _) = foreign(t.path());
        let store = RunDirs::new(&t.path().join("state"));
        let g = git();
        type Plant<'a> = &'a dyn Fn(&Path);
        let cases: [(&str, Plant); 5] = [
            ("objects", &|git: &Path| {
                std::fs::rename(git.join("objects"), git.join("objects.old")).unwrap();
                std::os::unix::fs::symlink(&objects, git.join("objects")).unwrap();
            }),
            ("index", &|git: &Path| {
                std::fs::remove_file(git.join("index")).unwrap();
                std::os::unix::fs::symlink(objects.join("../index"), git.join("index")).unwrap();
            }),
            ("HEAD", &|git: &Path| {
                std::fs::remove_file(git.join("HEAD")).unwrap();
                std::os::unix::fs::symlink(objects.join("../HEAD"), git.join("HEAD")).unwrap();
            }),
            ("objects/info", &|git: &Path| {
                let _ = std::fs::remove_dir_all(git.join("objects/info"));
                std::os::unix::fs::symlink(objects.join("info"), git.join("objects/info")).unwrap();
            }),
            ("packed-refs", &|git: &Path| {
                let _ = std::fs::remove_file(git.join("packed-refs"));
                std::os::unix::fs::symlink("/etc/hostname", git.join("packed-refs")).unwrap();
            }),
        ];
        for (i, (name, plant)) in cases.iter().enumerate() {
            let base = t.path().join(format!("c{i}"));
            std::fs::create_dir_all(&base).unwrap();
            let dir = checkout(&base);
            plant(&dir.join(".git"));
            match g.check_checkout(&open(&dir), &store) {
                Err(GitError::Refused(m)) => {
                    assert!(m.contains(name), "{name}: {m}");
                    assert!(!m.contains(t.path().to_str().unwrap()), "{m}");
                }
                other => panic!("{name}: {other:?}"),
            }
        }
        // The cheap check before each command sees the main parts.
        std::fs::create_dir_all(t.path().join("c10")).unwrap();
        let dir = checkout(&t.path().join("c10"));
        std::fs::remove_file(dir.join(".git/HEAD")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.join(".git/HEAD")).unwrap();
        assert!(matches!(
            g.run(&open(&dir), &["rev-parse", "HEAD"], 1024),
            Err(GitError::Refused(_))
        ));

        // A ref and a loose object as symlinks to files of the host: the
        // walk of the check refuses them, and when one comes after the
        // check, git in the sandbox does not read it.
        let Some(g) = ReadGit::sandboxed(CMD_TIMEOUT) else {
            return;
        };
        std::fs::create_dir_all(t.path().join("c11")).unwrap();
        let dir = checkout(&t.path().join("c11"));
        let host = t.path().join("host-file");
        std::fs::write(&host, "0123456789012345678901234567890123456789\n").unwrap();
        let c = open(&dir);
        g.check_checkout(&c, &store).unwrap();
        std::os::unix::fs::symlink(&host, dir.join(".git/refs/heads/x")).unwrap();
        match g.check_checkout(&c, &store) {
            Err(GitError::Refused(m)) => assert!(m.contains(".git/refs/heads/x"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(
            g.run(&c, &["rev-parse", "--verify", "refs/heads/x"], 1024)
                .is_err()
        );
        let id = rev(&dir, "HEAD:a.txt");
        let one = dir.join(".git/objects").join(&id[..2]).join(&id[2..]);
        let text = rev(&dir, "HEAD:docs/b.md");
        let other = dir.join(".git/objects").join(&text[..2]).join(&text[2..]);
        std::fs::remove_file(&one).unwrap();
        std::os::unix::fs::symlink(&other, &one).unwrap();
        // On the host, git follows the symlink (the fixture is real).
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["cat-file", "-e", &id])
                .status()
                .unwrap()
                .success()
        );
        // The absolute target is not in the sandbox.
        assert!(g.run(&c, &["cat-file", "blob", &id], 1024).is_err());
    }

    /// N1 (docs/security-review.md §15): while the browser reads in a
    /// loop, the agent puts the objects of another repo into `.git` and
    /// takes them away again: symlinks `objects/<xx>` (or loose object
    /// files) to the other repo's objects, an alternates file, a symlink
    /// `.git` to the other repo's `.git`, and a symlink in place of the
    /// checkout dir. The checks before each command cannot see such a
    /// race; the read sandbox makes it harmless. The foreign commit and
    /// its file are never served.
    #[test]
    fn a_race_in_the_git_dir_never_serves_a_foreign_repo() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let Some(g) = ReadGit::sandboxed(CMD_TIMEOUT) else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let (objects, sha) = foreign(t.path());
        let blob = rev(&t.path().join("other"), "HEAD:f.txt");
        let store = RunDirs::new(&t.path().join("state"));
        g.check_checkout(&open(&dir), &store).unwrap();

        // The links: a dir `objects/<xx>` that the checkout does not have
        // (relative, as the agent would write it), else each loose file.
        let mine = dir.join(".git/objects");
        let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
        for e in std::fs::read_dir(&objects).unwrap() {
            let xx = e.unwrap().file_name();
            if xx.len() != 2 {
                continue;
            }
            if mine.join(&xx).exists() {
                for f in std::fs::read_dir(objects.join(&xx)).unwrap() {
                    let f = f.unwrap().file_name();
                    if !mine.join(&xx).join(&f).exists() {
                        links.push((mine.join(&xx).join(&f), objects.join(&xx).join(&f)));
                    }
                }
            } else {
                let rel = Path::new("../../../../other/.git/objects").join(&xx);
                links.push((mine.join(&xx), rel));
            }
        }
        assert!(!links.is_empty());
        let links = Arc::new(links);
        let plant = |links: &[(PathBuf, PathBuf)], on: bool| {
            for (l, target) in links {
                if on {
                    let _ = std::os::unix::fs::symlink(target, l);
                } else {
                    let _ = std::fs::remove_file(l);
                }
            }
        };
        // The fixture is real: with the links, git on the host serves the
        // foreign commit and file.
        plant(&links, true);
        let o = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["cat-file", "-p", &format!("{sha}:f.txt")])
            .output()
            .unwrap();
        assert_eq!(o.stdout, b"FOREIGN\n");
        plant(&links, false);

        let stop = Arc::new(AtomicBool::new(false));
        let toggler = {
            let stop = stop.clone();
            let dir = dir.clone();
            let objects = objects.clone();
            let links = links.clone();
            std::thread::spawn(move || {
                // Each round puts one of the four in place for a short
                // time, then leaves the checkout as it is for longer, so
                // that the reads hit both states.
                let present = || std::thread::sleep(Duration::from_micros(400));
                let absent = || std::thread::sleep(Duration::from_micros(1500));
                let git = dir.join(".git");
                let alt = git.join("objects/info/alternates");
                let ws = dir.parent().unwrap().to_path_buf();
                let mut rounds = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    match rounds % 4 {
                        0 => {
                            plant(&links, true);
                            present();
                            plant(&links, false);
                        }
                        1 => {
                            let _ = std::fs::create_dir_all(git.join("objects/info"));
                            std::fs::write(&alt, format!("{}\n", objects.display())).unwrap();
                            present();
                            std::fs::remove_file(&alt).unwrap();
                        }
                        2 => {
                            std::fs::rename(&git, dir.join(".git.real")).unwrap();
                            std::os::unix::fs::symlink("../../other/.git", &git).unwrap();
                            present();
                            std::fs::remove_file(&git).unwrap();
                            std::fs::rename(dir.join(".git.real"), &git).unwrap();
                        }
                        _ => {
                            std::fs::rename(&dir, ws.join("proj.real")).unwrap();
                            std::os::unix::fs::symlink("../other", &dir).unwrap();
                            present();
                            std::fs::remove_file(&dir).unwrap();
                            std::fs::rename(ws.join("proj.real"), &dir).unwrap();
                        }
                    }
                    absent();
                    rounds += 1;
                }
                rounds
            })
        };

        // The loop runs until it has both 300 tries and 30 good reads, so
        // a slow runner reads longer instead of failing on a count in a
        // time window. The limit only stops a hang.
        let (tries_wanted, good_wanted) = (300, 30);
        let limit = Duration::from_secs(600);
        let start = Instant::now();
        let (mut tries, mut good) = (0, 0);
        while tries < tries_wanted || good < good_wanted {
            if start.elapsed() > limit {
                break;
            }
            // Each try opens the checkout, as each request does.
            let Ok(c) = CheckoutDir::open(&t.path().join("ws"), "proj", "race") else {
                continue;
            };
            tries += 1;
            let c = Arc::new(c);
            let tree = Tree::Checkout { dir: c.clone() };
            if let Ok(d) = commit(&g, &tree, &sha) {
                panic!("the foreign commit was served: {d:?}");
            }
            if let Ok((list, _)) = commits(&g, &tree, &sha, 0, 5) {
                assert!(list.iter().all(|x| x.sha != sha), "{list:?}");
            }
            if let Ok(o) = g.run(&*c, &["cat-file", "blob", &blob], 1024) {
                panic!("the foreign file was served: {:?}", o.text());
            }
            let head = resolve(&g, &*c, "HEAD");
            assert_ne!(head.as_deref(), Some(sha.as_str()));
            if head.is_some() {
                good += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let rounds = toggler.join().unwrap();
        eprintln!("race: {tries} tries, {good} good reads, {rounds} toggle rounds");
        // The loop did read: the checkout as it is, while the toggler
        // changed it.
        assert!(
            tries >= tries_wanted && good >= good_wanted && rounds > 30,
            "{tries} tries, {good} good reads, {rounds} rounds in {limit:?}"
        );
    }

    /// N3: an `include.path` that the agent adds to the live config after
    /// the check. A file of the host is not in the sandbox, so git reads
    /// nothing of it and says nothing of it. A bad include inside the
    /// checkout fails the command, and the error has no stderr.
    #[test]
    fn an_include_added_after_the_check_shows_nothing() {
        let Some(g) = ReadGit::sandboxed(CMD_TIMEOUT) else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let secret = t.path().join("secret.cfg");
        // Not valid config: git on the host names the file in a fatal error.
        std::fs::write(&secret, "[TOPSECRET\n").unwrap();
        let store = RunDirs::new(&t.path().join("state"));
        let c = Arc::new(open(&dir));
        g.check_checkout(&c, &store).unwrap();
        let cfg = dir.join(".git/config");
        let clean = std::fs::read_to_string(&cfg).unwrap();
        std::fs::write(
            &cfg,
            format!("{clean}[include]\n\tpath = {}\n", secret.display()),
        )
        .unwrap();
        // The fixture is real: git on the host names the file.
        let o = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(!o.status.success());
        assert!(String::from_utf8_lossy(&o.stderr).contains("secret.cfg"));

        let tree = Tree::Checkout { dir: c.clone() };
        let mut seen = String::new();
        let info = checkout_info(&g, &c, Some("main"));
        assert!(info.head.is_some());
        seen.push_str(&format!("{info:?}"));
        seen.push_str(&format!("{:?}", commits(&g, &tree, "HEAD", 0, 5)));
        seen.push_str(&format!(
            "{:?}",
            uncommitted(&g, &c, &mut LineBudget::default())
        ));
        seen.push_str(&format!(
            "{:?}",
            g.run(&*c, &["cat-file", "-p", "HEAD"], 4096)
        ));
        assert!(!seen.contains("TOPSECRET"), "{seen}");
        assert!(!seen.contains("secret.cfg"), "{seen}");
        assert!(!seen.contains(t.path().to_str().unwrap()), "{seen}");

        std::fs::write(dir.join("bad.cfg"), "[TOPSECRET\n").unwrap();
        std::fs::write(&cfg, format!("{clean}[include]\n\tpath = ../bad.cfg\n")).unwrap();
        match commits(&g, &tree, "HEAD", 0, 5) {
            Err(BrowseError::Git(GitError::Failed { stderr, .. })) => {
                assert!(stderr.is_empty(), "{stderr}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// M1 (docs/security-review.md §15): a FIFO in `.git/refs` (a branch,
    /// or the ref of HEAD) would hold each git command until its deadline.
    /// The check refuses it at once. A request has one deadline for all its
    /// reads ([`crate::git::READ_REQUEST_TIME`]).
    #[test]
    fn a_fifo_in_the_git_dir_is_refused_at_once() {
        let t = tempfile::tempdir().unwrap();
        let store = RunDirs::new(&t.path().join("state"));
        let g = git();
        // `main`, and `feat`: the ref of HEAD in the fixture.
        for (i, name) in ["main", "feat"].iter().enumerate() {
            let base = t.path().join(format!("f{i}"));
            std::fs::create_dir_all(&base).unwrap();
            let dir = checkout(&base);
            let r = dir.join(".git/refs/heads").join(name);
            std::fs::remove_file(&r).unwrap();
            nix::unistd::mkfifo(&r, Mode::from_bits_truncate(0o644)).unwrap();
            let start = Instant::now();
            match g.check_checkout(&open(&dir), &store) {
                Err(GitError::Refused(m)) => {
                    assert!(m.contains("special file"), "{m}");
                    assert!(m.contains(&format!(".git/refs/heads/{name}")), "{m}");
                }
                other => panic!("{name}: {other:?}"),
            }
            assert!(start.elapsed() < Duration::from_secs(2));
        }

        // After the time of the request, no read starts.
        let base = t.path().join("f9");
        std::fs::create_dir_all(&base).unwrap();
        let dir = checkout(&base);
        let mut c = open(&dir);
        g.run(&c, &["rev-parse", "HEAD"], 64).unwrap();
        c.expire();
        assert!(matches!(
            g.run(&c, &["rev-parse", "HEAD"], 64),
            Err(GitError::Timeout(_))
        ));
        assert!(matches!(
            uncommitted(&g, &c, &mut LineBudget::default()),
            Err(BrowseError::Git(GitError::Timeout(_)))
        ));
    }

    /// With no `bwrap`, git does not read an agent checkout at all; the
    /// daemon's mirror is read on the host.
    #[test]
    fn no_sandbox_no_agent_tree() {
        let g = ReadGit::without_sandbox(CMD_TIMEOUT);
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        match g.run(&open(&dir), &["rev-parse", "HEAD"], 64) {
            Err(GitError::Unavailable(m)) => assert!(m.contains("bwrap"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            commits(&g, &tree(&dir), "HEAD", 0, 5),
            Err(BrowseError::Git(GitError::Unavailable(_)))
        ));
        let mirror = Tree::Mirror {
            git_dir: t.path().join("origin.git"),
            rev: "refs/heads/main".into(),
        };
        assert_eq!(list_dir(&g, &mirror, "").unwrap().entries.len(), 2);
    }

    /// R4: a pair of large texts with no line in common is diffed in a
    /// bounded time, and a text over the line cap is not diffed.
    #[test]
    fn a_pathological_diff_is_bounded() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        let n = MAX_DIFF_INPUT_LINES - 10;
        let old: String = (0..n).map(|i| format!("{i}\n")).collect();
        let big: String = (0..MAX_DIFF_INPUT_LINES + 10)
            .map(|i| format!("{i}\n"))
            .collect();
        std::fs::write(dir.join("p.txt"), &old).unwrap();
        std::fs::write(dir.join("q.txt"), "x\n").unwrap();
        sh(&dir, "git add p.txt q.txt && git commit -q -m p");
        let new: String = (0..n).map(|i| format!("{}\n", i * 7 + 3_000_001)).collect();
        std::fs::write(dir.join("p.txt"), &new).unwrap();
        std::fs::write(dir.join("q.txt"), &big).unwrap();

        let start = Instant::now();
        let u = uncommitted(&git(), &open(&dir), &mut LineBudget::default()).unwrap();
        let took = start.elapsed();
        assert!(took < DIFF_TOTAL_TIME + Duration::from_secs(4), "{took:?}");
        let p = u.unstaged.files.iter().find(|f| f.path == "p.txt").unwrap();
        // All lines changed: every old line out, every new line in.
        assert_eq!((p.additions, p.deletions), (n as u64, n as u64));
        let q = u.unstaged.files.iter().find(|f| f.path == "q.txt").unwrap();
        assert!(q.truncated && q.patch.is_empty());
        assert!(q.note.as_deref().unwrap().contains("too large"), "{q:?}");

        // Straight into `unified`: a deadline in the past ends the search
        // at once, and the diff says so.
        let start = Instant::now();
        let (_, a, d, _, late) = unified(&old, &new, &mut LineBudget::default(), Instant::now());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(late);
        assert_eq!((a, d), (n as u64, n as u64));
    }

    #[test]
    fn commits_page() {
        let t = tempfile::tempdir().unwrap();
        let dir = checkout(t.path());
        sh(
            &dir,
            "for i in 1 2 3 4 5; do echo $i > n.txt && git add n.txt && git commit -q -m \"n $i\"; done",
        );
        let tree = tree(&dir);
        let (p1, next) = commits(&git(), &tree, "main..HEAD", 0, 4).unwrap();
        assert_eq!(p1.len(), 4);
        assert_eq!(p1[0].subject, "n 5");
        let (p2, next2) = commits(&git(), &tree, "main..HEAD", next.unwrap(), 4).unwrap();
        assert_eq!(p2.len(), 2);
        assert_eq!(p2[1].subject, "feat: c");
        assert!(next2.is_none());
    }
}
