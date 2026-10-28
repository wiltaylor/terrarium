//! Plan accounts (PLAN §16.3): many plan logins per provider, one shared
//! settings home per harness.
//!
//! Each plan-login subscription has an account directory. Only the
//! credential is per account; the settings, the caches, and the per-agent
//! overlays stay in the shared harness home (`/auth/<harness>`).
//!
//! ```text
//! <state>/credentials/accounts/<subscription>/      0700, the daemon only
//!   account.json            harness, provider, login name, Claude's oauthAccount (no secret)
//!   store/                  0700, bound read-write into the sandboxes of the pool (Claude)
//!     claude/.credentials.json   Claude: the CLAUDE_SECURESTORAGE_CONFIG_DIR target
//!     claude.lock/               Claude's own legacy lock, `<realpath(dir)>.lock`
//!     auth.json                  Codex
//!     token                      Copilot: the GitHub token (`gho_`)
//! <state>/credentials/backup/   0700: copies of the shared logins that the migration moved (0600)
//! <harness dir>/cred/account    the per-agent pointer: a symlink to one account's `store/`
//! ```
//!
//! How each harness reads its account:
//!
//! | Harness | Pointer use | Swap |
//! |---|---|---|
//! | Claude Code | `CLAUDE_SECURESTORAGE_CONFIG_DIR=<harness dir>/cred/account/claude`. Claude resolves the dir for each request (an mtime check), takes its locks inside it, and writes by a rename inside it. | live: take `.oauth_refresh.lock` and `.storage-write.lock` of both accounts, re-point, release |
//! | Codex | The launch binds the account's `auth.json` over `$CODEX_HOME/auth.json`. Codex writes it in place. | restart with resume |
//! | Copilot CLI | The launch sets `COPILOT_GITHUB_TOKEN` from `store/token`. | restart with resume |
//!
//! pi uses API keys only and has no plan account (PLAN §16.3). An install
//! from before can have pi accounts and the shared pi `auth.json` as a
//! symlink to `/run/terrarium/cred/account/auth.json` (the link of the pi
//! plan accounts). The migration makes that file a plain `{}` file again
//! when no pi agent runs ([`migrate`]); until then a pi launch points the
//! agent at an empty store, so the link reads `{}`. An old pi account
//! stays on disk until its logout or the delete of its subscription.
//!
//! The daemon never refreshes a token and never calls a token endpoint
//! (PLAN §16.3, "The refresh rule"). It reads, copies, and moves credential
//! files, makes symlinks, and takes the harness's own lock for a swap.
//! Each harness refreshes its own token with its own code.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::{FileOverlay, HarnessId};
use tracing::{info, warn};

use crate::store::subscriptions::Subscription;

/// `<state>/credentials/accounts`.
pub const ACCOUNTS_DIR: &str = "accounts";
/// The bound part of an account directory.
pub const STORE_DIR: &str = "store";
/// The metadata of an account (no secret).
pub const META_FILE: &str = "account.json";
/// `<state>/credentials/backup`.
pub const BACKUP_DIR: &str = "backup";
/// The pointer directory in the agent's harness dir.
pub const POINTER_DIR: &str = "cred";
/// The pointer: a symlink to the store of the current account.
pub const POINTER: &str = "account";
/// The store of a pi agent while the old link is there: an empty
/// `auth.json`.
pub const NONE_STORE: &str = "none";
/// Where the sandbox shows `<harness dir>/cred` (read-only). The old pi
/// link (an install from before pi took API keys only) goes through it.
pub const SANDBOX_CRED: &str = "/run/terrarium/cred";
/// Claude's credential directory in a store.
pub const CLAUDE_DIR: &str = "claude";
/// Claude's credential file.
pub const CLAUDE_CREDENTIALS: &str = ".credentials.json";
/// The credential file of Codex (and of the pi accounts from before).
pub const AUTH_FILE: &str = "auth.json";
/// Copilot's token file.
pub const TOKEN_FILE: &str = "token";
/// The env var that moves Claude's credential directory away from its
/// config directory (Claude Code 2.1.310, `O0()`; not in the public docs).
pub const CLAUDE_CRED_ENV: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";
/// The env var that gives Copilot its GitHub token.
pub const COPILOT_TOKEN_ENV: &str = "COPILOT_GITHUB_TOKEN";
/// `TERRARIUM_AUTO_SWAP`: `on` (the default) or `off`.
pub const AUTO_SWAP_ENV: &str = "TERRARIUM_AUTO_SWAP";

/// Claude's refresh lock in its credential directory (`proper-lockfile`,
/// `lockfilePath`, `stale: 60000`, `update: 5000`).
pub const CLAUDE_REFRESH_LOCK: &str = ".oauth_refresh.lock";
/// Claude's write lock: `proper-lockfile` on `<dir>/.storage-write` with
/// the default `lockfilePath`, so the directory is `.storage-write.lock`
/// (`stale: 15000`). Every save of the credential is a read-modify-write
/// under it.
pub const CLAUDE_STORAGE_LOCK: &str = ".storage-write.lock";

const CLAUDE_REFRESH_STALE: Duration = Duration::from_secs(60);
const CLAUDE_STORAGE_STALE: Duration = Duration::from_secs(15);
/// How long a swap waits for a harness to release its lock. A Claude
/// refresh holds it for at most about 30 s.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(45);
/// The migration runs while the supervisor holds its slot lock, and no
/// agent of the harness runs: a harness lock then belongs to a dead
/// process or a status command. It waits briefly, then tries again at the
/// next tick.
const MIGRATION_LOCK_WAIT: Duration = Duration::from_secs(2);

/// One swap or import at a time, so the daemon never takes the locks of
/// two accounts in two orders.
static SWAPS: Mutex<()> = Mutex::new(());

fn swaps() -> std::sync::MutexGuard<'static, ()> {
    SWAPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True unless `TERRARIUM_AUTO_SWAP` says `off` (or `0`, `false`, `no`).
pub fn auto_swap_from_env() -> bool {
    std::env::var(AUTO_SWAP_ENV).map_or(true, |v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    })
}

// ---- Paths ----

/// `<state>/credentials`.
pub fn credentials_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("credentials")
}

/// `<state>/credentials/accounts/<subscription>`.
pub fn account_dir(state_dir: &Path, sub: &str) -> PathBuf {
    credentials_dir(state_dir).join(ACCOUNTS_DIR).join(sub)
}

/// `<state>/credentials/accounts/<subscription>/store`.
pub fn store_dir(state_dir: &Path, sub: &str) -> PathBuf {
    account_dir(state_dir, sub).join(STORE_DIR)
}

/// The credential file of `harness` in a store.
pub fn credential_path(store: &Path, harness: HarnessId) -> PathBuf {
    match harness {
        HarnessId::Claude => store.join(CLAUDE_DIR).join(CLAUDE_CREDENTIALS),
        HarnessId::Codex | HarnessId::Pi => store.join(AUTH_FILE),
        HarnessId::Copilot => store.join(TOKEN_FILE),
    }
}

/// The dir that a probe or a status check reads as the harness home of
/// the account: `store/claude` for Claude, else the store.
pub fn probe_home(store: &Path, harness: HarnessId) -> PathBuf {
    match harness {
        HarnessId::Claude => store.join(CLAUDE_DIR),
        _ => store.to_path_buf(),
    }
}

/// True when `path` is a regular file (not a symlink).
fn is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// True when `path` is a directory (not a symlink).
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

/// True when the account of `sub` holds a credential of `harness`.
pub fn has_credential(state_dir: &Path, sub: &str, harness: HarnessId) -> bool {
    let store = store_dir(state_dir, sub);
    is_real_dir(&store) && is_regular(&credential_path(&store, harness))
}

/// The account of `sub` exists: its directory is there.
pub fn exists(state_dir: &Path, sub: &str) -> bool {
    is_real_dir(&account_dir(state_dir, sub))
}

/// `<harness dir>/cred/account`.
pub fn pointer_path(harness_dir: &Path) -> PathBuf {
    harness_dir.join(POINTER_DIR).join(POINTER)
}

/// The store that the agent's pointer names, if any.
pub fn pointed(harness_dir: &Path) -> Option<PathBuf> {
    fs::read_link(pointer_path(harness_dir)).ok()
}

/// The subscription of the store `store` (its account dir name).
pub fn sub_of_store(store: &Path) -> Option<String> {
    (store.file_name()? == STORE_DIR)
        .then(|| store.parent()?.file_name())
        .flatten()
        .map(|n| n.to_string_lossy().into_owned())
}

/// The env value of `CLAUDE_SECURESTORAGE_CONFIG_DIR` for an agent: the
/// `claude` dir through the pointer.
pub fn claude_cred_dir(harness_dir: &Path) -> PathBuf {
    pointer_path(harness_dir).join(CLAUDE_DIR)
}

/// The target of the old shared pi `auth.json` link, the same in every
/// sandbox.
pub fn pi_link_target() -> PathBuf {
    Path::new(SANDBOX_CRED).join(POINTER).join(AUTH_FILE)
}

/// True when the shared pi home still has its `auth.json` link of the pi
/// plan accounts (an install from before pi took API keys only).
pub fn pi_link_installed(home: &Path) -> bool {
    fs::read_link(home.join(AUTH_FILE)).is_ok_and(|t| t == pi_link_target())
}

// ---- Files ----

/// Make `dir` (and its parents) with mode 0700. A symlink at `dir` is an
/// error: the daemon writes credentials there.
fn private_dir(dir: &Path) -> io::Result<()> {
    if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other(format!(
            "{} is a symlink; the daemon does not write credentials through it",
            dir.display()
        )));
    }
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Read a regular file without following a symlink at its last part.
pub fn read_nofollow(path: &Path) -> io::Result<Vec<u8>> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nofollow())
        .open(path)?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)?;
    Ok(out)
}

/// `O_NOFOLLOW`.
fn libc_o_nofollow() -> i32 {
    nix::fcntl::OFlag::O_NOFOLLOW.bits()
}

/// Write `bytes` to `path` (mode 0600) through a new temp file in the
/// same dir and a rename. The rename replaces the inode.
fn write_renamed(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.terrarium-{}", ulid::Ulid::new()));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Write `bytes` into the existing regular file `path`, in place (the
/// inode stays, so a file bind of it in a running sandbox sees the new
/// content). No symlink follow. A missing file is made as with
/// [`write_renamed`].
fn write_in_place(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if !is_regular(path) {
        return write_renamed(path, bytes);
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc_o_nofollow())
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Copy `bytes` to `<state>/credentials/backup/<time>-<harness>-<name>`,
/// mode 0600. Returns the path.
pub fn backup(
    state_dir: &Path,
    harness: HarnessId,
    name: &str,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let dir = credentials_dir(state_dir).join(BACKUP_DIR);
    private_dir(&credentials_dir(state_dir))?;
    private_dir(&dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let clean = name.trim_start_matches('.');
    let path = dir.join(format!("{stamp}-{harness}-{clean}"));
    write_renamed(&path, bytes)?;
    Ok(path)
}

// ---- Metadata ----

/// `account.json`: what the daemon knows about an account. No secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub harness: HarnessId,
    /// The provider of the subscription at the import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The login name, when the harness says it (Copilot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Claude Code's `oauthAccount` of the login (account and org UUIDs,
    /// email). The launch writes it into the agent's `.claude.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_account: Option<Value>,
    /// `migration` or `login`.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// The metadata of the account of `sub`.
pub fn read_meta(state_dir: &Path, sub: &str) -> Option<Meta> {
    let bytes = read_nofollow(&account_dir(state_dir, sub).join(META_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_meta(state_dir: &Path, sub: &str, meta: &Meta) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
    write_renamed(&account_dir(state_dir, sub).join(META_FILE), &bytes)
}

/// Every account, by subscription name.
pub fn list(state_dir: &Path) -> Vec<(String, Meta)> {
    let Ok(entries) = fs::read_dir(credentials_dir(state_dir).join(ACCOUNTS_DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Meta)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            read_meta(state_dir, &name).map(|m| (name, m))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---- Locks ----

/// A `proper-lockfile` lock directory that this process holds. Dropping
/// it removes the directory.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

/// Take a `proper-lockfile` lock: `mkdir` the lock directory. While
/// another process holds it, wait. A lock whose mtime is older than
/// `stale` is a dead holder's: remove it and try again, as
/// `proper-lockfile` does (a live holder touches the mtime every
/// `stale / 2`). Gives up after `timeout` with `TimedOut`.
///
/// The lock has no owner record (Claude's `.oauth_refresh.lock.owner`).
/// Claude's dead-holder takeover then answers `no_owner_record` and does
/// not take it over; it waits as for any holder.
pub fn lock_dir(path: &Path, stale: Duration, timeout: Duration) -> io::Result<DirLock> {
    let start = Instant::now();
    loop {
        match fs::create_dir(path) {
            Ok(()) => {
                return Ok(DirLock {
                    path: path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let old = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age > stale);
                if old {
                    warn!(lock = %path.display(), "removing a stale harness lock");
                    let _ = fs::remove_dir(path);
                    continue;
                }
            }
            Err(e) => return Err(e),
        }
        if start.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} is still held after {} s: the harness is refreshing or saving its login",
                    path.display(),
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The harness locks that keep a swap or a credential write atomic.
/// Claude: `.oauth_refresh.lock`, then `.storage-write.lock`, in each
/// store, in path order. Codex and Copilot have none (they swap by a
/// restart).
fn harness_locks(
    harness: HarnessId,
    stores: &[&Path],
    timeout: Duration,
) -> io::Result<Vec<DirLock>> {
    let mut out = Vec::new();
    if harness == HarnessId::Claude {
        let mut dirs: Vec<PathBuf> = stores
            .iter()
            .map(|s| s.join(CLAUDE_DIR))
            .filter(|d| is_real_dir(d))
            .collect();
        dirs.sort();
        dirs.dedup();
        for d in dirs {
            out.push(lock_dir(
                &d.join(CLAUDE_REFRESH_LOCK),
                CLAUDE_REFRESH_STALE,
                timeout,
            )?);
            out.push(lock_dir(
                &d.join(CLAUDE_STORAGE_LOCK),
                CLAUDE_STORAGE_STALE,
                timeout,
            )?);
        }
    }
    Ok(out)
}

// ---- The pointer ----

/// Point the agent's `cred/account` at `target`, atomically: a new
/// symlink, then a rename over the old one.
pub fn point(harness_dir: &Path, target: &Path) -> io::Result<()> {
    let dir = harness_dir.join(POINTER_DIR);
    fs::create_dir_all(&dir)?;
    let link = dir.join(POINTER);
    if fs::read_link(&link).is_ok_and(|t| t == target) {
        return Ok(());
    }
    let tmp = dir.join(format!(".{POINTER}.{}.tmp", ulid::Ulid::new()));
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, &link).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// The store for a pi agent while the old link is there:
/// `cred/none/auth.json` holds `{}`.
fn none_store(harness_dir: &Path) -> io::Result<PathBuf> {
    let dir = harness_dir.join(POINTER_DIR).join(NONE_STORE);
    fs::create_dir_all(&dir)?;
    let auth = dir.join(AUTH_FILE);
    if !is_regular(&auth) {
        write_renamed(&auth, b"{}")?;
    }
    Ok(dir)
}

/// Swap the account of a running Claude agent (PLAN §16.3): take the
/// harness's locks on the old and the new account, re-point the agent's
/// pointer at the store of `to`, and release. The harness reads the new
/// credential at its next request.
pub fn live_swap(
    state_dir: &Path,
    harness: HarnessId,
    harness_dir: &Path,
    to: &str,
    timeout: Duration,
) -> Result<(), String> {
    if harness != HarnessId::Claude {
        return Err(format!("the {harness} harness has no live swap"));
    }
    if !has_credential(state_dir, to, harness) {
        return Err(format!("subscription `{to}` has no {harness} login"));
    }
    let new = store_dir(state_dir, to);
    let _one = swaps();
    let old = pointed(harness_dir);
    let mut stores: Vec<&Path> = vec![&new];
    if let Some(o) = old.as_deref() {
        stores.push(o);
    }
    let locks = harness_locks(harness, &stores, timeout).map_err(|e| e.to_string())?;
    let result = point(harness_dir, &new).map_err(|e| format!("cannot re-point the agent: {e}"));
    drop(locks);
    result
}

// ---- Writing a credential ----

/// Write the credential `bytes` of `harness` into the store of `sub`,
/// under the harness locks. Claude: a rename inside the store (it reaches
/// the file through a directory). Codex: in place, because a running Codex
/// has the file bound. Copilot: a rename.
pub fn write_credential(
    state_dir: &Path,
    sub: &str,
    harness: HarnessId,
    bytes: &[u8],
) -> Result<(), String> {
    write_credential_within(state_dir, sub, harness, bytes, LOCK_TIMEOUT)
}

/// [`write_credential`] with its own wait for the harness locks.
fn write_credential_within(
    state_dir: &Path,
    sub: &str,
    harness: HarnessId,
    bytes: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    let err = |e: io::Error| format!("cannot write the {harness} login of `{sub}`: {e}");
    let account = account_dir(state_dir, sub);
    let store = store_dir(state_dir, sub);
    private_dir(&credentials_dir(state_dir)).map_err(err)?;
    private_dir(&credentials_dir(state_dir).join(ACCOUNTS_DIR)).map_err(err)?;
    private_dir(&account).map_err(err)?;
    private_dir(&store).map_err(err)?;
    if harness == HarnessId::Claude {
        private_dir(&store.join(CLAUDE_DIR)).map_err(err)?;
    }
    let _one = swaps();
    let _locks = harness_locks(harness, &[&store], timeout).map_err(err)?;
    let path = credential_path(&store, harness);
    match harness {
        HarnessId::Codex => write_in_place(&path, bytes),
        _ => write_renamed(&path, bytes),
    }
    .map_err(err)
}

/// Make or update `account.json` of `sub`.
fn record_meta(
    state_dir: &Path,
    sub: &str,
    harness: HarnessId,
    provider: Option<&str>,
    login: Option<String>,
    oauth_account: Option<Value>,
    source: &str,
) -> Result<(), String> {
    let now = crate::db::now();
    let old = read_meta(state_dir, sub);
    let meta = Meta {
        harness,
        provider: provider
            .map(str::to_owned)
            .or_else(|| old.as_ref().and_then(|m| m.provider.clone())),
        login: login.or_else(|| old.as_ref().and_then(|m| m.login.clone())),
        oauth_account: oauth_account.or_else(|| old.as_ref().and_then(|m| m.oauth_account.clone())),
        source: source.to_owned(),
        created_at: old.map_or_else(|| now.clone(), |m| m.created_at),
        updated_at: now,
    };
    write_meta(state_dir, sub, &meta).map_err(|e| format!("cannot write the account record: {e}"))
}

/// The credential of `harness` in a harness home `home` (a shared home or
/// the scratch home of a login), as the bytes the store keeps, with the
/// login name and Claude's `oauthAccount`. `None`: no plan login there.
/// pi has no plan login (it uses API keys only): always `None`.
pub fn credential_in_home(
    harness: HarnessId,
    home: &Path,
) -> Option<(Vec<u8>, Option<String>, Option<Value>)> {
    let json = |p: &Path| -> Option<Value> { serde_json::from_slice(&read_nofollow(p).ok()?).ok() };
    match harness {
        HarnessId::Claude => {
            let path = home.join(CLAUDE_CREDENTIALS);
            let v = json(&path)?;
            let oauth = v.get("claudeAiOauth")?;
            let usable = ["refreshToken", "accessToken"].iter().any(|k| {
                oauth
                    .get(k)
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
            });
            if !usable {
                return None;
            }
            let account = json(&home.join(".claude.json"))
                .and_then(|s| s.get("oauthAccount").cloned())
                .filter(Value::is_object);
            Some((read_nofollow(&path).ok()?, None, account))
        }
        HarnessId::Codex => {
            let path = home.join(AUTH_FILE);
            let v = json(&path)?;
            // A plan login has ChatGPT tokens; an API key alone is not one.
            v.get("tokens").filter(|t| t.is_object())?;
            Some((read_nofollow(&path).ok()?, None, None))
        }
        HarnessId::Copilot => {
            let text = String::from_utf8(read_nofollow(&home.join("config.json")).ok()?).ok()?;
            let v: Value =
                serde_json::from_str(&crate::probes::github::strip_line_comments(&text)).ok()?;
            let token = crate::probes::github::token_in_config(&v)?;
            let login = v
                .get("lastLoggedInUser")
                .and_then(|u| u.get("login"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some((token.into_bytes(), login, None))
        }
        HarnessId::Pi => None,
    }
}

/// Move the login of a finished login session from its scratch home into
/// the account of `sub` (PLAN §16.3, "The login wizard"). Only the
/// credential moves; nothing else of the scratch home, and no other
/// account or the shared home changes. A login of a subscription that has
/// an account replaces that account's credential.
pub fn import_login(
    state_dir: &Path,
    sub: &str,
    harness: HarnessId,
    provider: &str,
    scratch: &Path,
) -> Result<(), String> {
    let Some((bytes, login, oauth)) = credential_in_home(harness, scratch) else {
        return Err(format!(
            "the login finished, but its scratch home has no {harness} plan login"
        ));
    };
    write_credential(state_dir, sub, harness, &bytes)?;
    record_meta(
        state_dir,
        sub,
        harness,
        Some(provider),
        login,
        oauth,
        "login",
    )?;
    info!(subscription = sub, harness = %harness, "the login is in its account");
    Ok(())
}

/// Remove the account of `sub` (its logout). The caller makes sure no
/// agent runs on it.
pub fn remove(state_dir: &Path, sub: &str) -> io::Result<bool> {
    let dir = account_dir(state_dir, sub);
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir()) {
        return Ok(false);
    }
    let _one = swaps();
    fs::remove_dir_all(&dir)?;
    Ok(true)
}

/// The credential env of a launched account (PLAN §16b.4): the Copilot
/// token of `account` for a Copilot agent, else nothing. `account` is the
/// account that the launch picked (`private/accounts.json`). Secrets:
/// never log the values.
pub fn credential_env(
    state_dir: &Path,
    harness: HarnessId,
    account: Option<&str>,
) -> Vec<(String, zeroize::Zeroizing<String>)> {
    match (harness, account) {
        (HarnessId::Copilot, Some(a)) => copilot_token(state_dir, a)
            .map(|t| vec![(COPILOT_TOKEN_ENV.to_owned(), zeroize::Zeroizing::new(t))])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The Copilot token of the account of `sub`. A secret.
pub fn copilot_token(state_dir: &Path, sub: &str) -> Option<String> {
    let bytes = read_nofollow(&credential_path(
        &store_dir(state_dir, sub),
        HarnessId::Copilot,
    ))
    .ok()?;
    let t = String::from_utf8(bytes).ok()?.trim().to_owned();
    (!t.is_empty()).then_some(t)
}

// ---- The migration ----

/// True when `provider` is the plan provider of `harness` (the provider a
/// login of that harness makes, `LoginKind::provider`). pi has none: it
/// uses API keys only.
pub fn plan_provider(harness: HarnessId, provider: &str) -> bool {
    match harness {
        HarnessId::Claude => provider == "anthropic",
        HarnessId::Codex => matches!(provider, "openai" | "openai-codex"),
        HarnessId::Copilot => matches!(provider, "github" | "github-copilot" | "copilot"),
        HarnessId::Pi => false,
    }
}

/// The one subscription that the shared login of `harness` belongs to: a
/// plan provider of the harness, the harness can spend it, no stored API
/// key, and no account yet. `Err` names why there is not exactly one.
fn owner<'a>(
    state_dir: &Path,
    subs: &'a [Subscription],
    harness: HarnessId,
) -> Result<&'a Subscription, String> {
    let fits: Vec<&Subscription> = subs
        .iter()
        .filter(|s| s.harnesses.contains(&harness) && s.env_var.is_none())
        .filter(|s| plan_provider(harness, &s.provider))
        .filter(|s| !exists(state_dir, &s.name))
        .collect();
    match fits.as_slice() {
        [one] => Ok(one),
        [] => Err("no subscription without an account fits it".into()),
        many => Err(format!(
            "{} subscriptions fit it ({}); move it with the login wizard",
            many.len(),
            many.iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// What a migration of one harness did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Migration {
    pub harness: Option<HarnessId>,
    /// `<subscription>`: the shared login moved into its account.
    pub moved: Vec<String>,
    /// The backup copies of the shared files.
    pub backups: Vec<PathBuf>,
    /// Why a shared login stayed.
    pub skipped: Vec<String>,
    /// pi: the shared `auth.json` was the link of the pi plan accounts
    /// (an install from before pi took API keys only); it is a plain `{}`
    /// file again.
    pub pi_unlinked: bool,
}

impl Migration {
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.skipped.is_empty() && !self.pi_unlinked
    }
}

/// True when the shared home of `harness` has something that the
/// migration moves (a cheap check, for the start-up loop).
pub fn migration_pending(harness: HarnessId, home: &Path) -> bool {
    match harness {
        HarnessId::Claude | HarnessId::Codex | HarnessId::Copilot => {
            credential_in_home(harness, home).is_some()
        }
        HarnessId::Pi => pi_link_installed(home),
    }
}

/// Move the shared login of `harness` in `home` into the account of the
/// subscription that uses it (PLAN §16.3, "The migration"). For pi, make
/// the old shared `auth.json` link a plain `{}` file again. Idempotent:
/// a second run finds nothing to move. Call it only while no agent of the
/// harness runs (`Supervisor::with_quiet_harness`): a running agent reads
/// the shared file that this moves away.
///
/// Each moved file is copied to `<state>/credentials/backup/` first
/// (0600). A move is a copy, a sync, and then the removal of the shared
/// file (the shared home and the state dir can be two mounts). After a
/// crash between the two, the next run finds the same bytes in the
/// account and finishes the removal.
pub fn migrate(
    state_dir: &Path,
    harness: HarnessId,
    home: &Path,
    subs: &[Subscription],
) -> Migration {
    let mut m = Migration {
        harness: Some(harness),
        ..Migration::default()
    };
    match harness {
        HarnessId::Pi => unlink_pi(home, &mut m),
        _ => migrate_one(state_dir, harness, home, subs, &mut m),
    }
    m
}

fn migrate_one(
    state_dir: &Path,
    harness: HarnessId,
    home: &Path,
    subs: &[Subscription],
    m: &mut Migration,
) {
    let Some((bytes, login, oauth)) = credential_in_home(harness, home) else {
        return;
    };
    let shared = match harness {
        HarnessId::Claude => home.join(CLAUDE_CREDENTIALS),
        HarnessId::Codex => home.join(AUTH_FILE),
        _ => home.join("config.json"),
    };
    // A move that a crash cut short: the account has the same login.
    let done_before = subs.iter().find(|s| {
        has_credential(state_dir, &s.name, harness)
            && read_nofollow(&credential_path(&store_dir(state_dir, &s.name), harness))
                .is_ok_and(|b| b == bytes)
    });
    let sub = match done_before {
        Some(s) => s,
        None => match owner(state_dir, subs, harness) {
            Ok(s) => s,
            Err(why) => {
                m.skipped
                    .push(format!("the shared {harness} login stays: {why}"));
                return;
            }
        },
    };
    let original = match read_nofollow(&shared) {
        Ok(b) => b,
        Err(e) => {
            m.skipped
                .push(format!("cannot read {}: {e}", shared.display()));
            return;
        }
    };
    let file_name = shared
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match backup(state_dir, harness, &file_name, &original) {
        Ok(p) => m.backups.push(p),
        Err(e) => {
            m.skipped
                .push(format!("cannot back up {}: {e}", shared.display()));
            return;
        }
    }
    if done_before.is_none() {
        if let Err(e) =
            write_credential_within(state_dir, &sub.name, harness, &bytes, MIGRATION_LOCK_WAIT)
        {
            m.skipped.push(e);
            return;
        }
        if let Err(e) = record_meta(
            state_dir,
            &sub.name,
            harness,
            Some(&sub.provider),
            login,
            oauth,
            "migration",
        ) {
            m.skipped.push(e);
            return;
        }
    }
    let removed = match harness {
        // Copilot: only the tokens leave `config.json`; the rest of the
        // file (trusted folders, flags) stays shared.
        HarnessId::Copilot => strip_copilot_tokens(&shared, &original),
        _ => fs::remove_file(&shared),
    };
    if let Err(e) = removed {
        m.skipped.push(format!(
            "the {harness} login is in `{}`, but the shared copy stays: {e}",
            sub.name
        ));
        return;
    }
    m.moved.push(sub.name.clone());
}

/// Remove `authTokens` (and `copilotTokens`) from Copilot's `config.json`.
/// The `//` comment lines at the top stay. Written by a rename, as
/// Copilot does.
fn strip_copilot_tokens(path: &Path, original: &[u8]) -> io::Result<()> {
    let text = String::from_utf8_lossy(original);
    let head: String = text
        .lines()
        .take_while(|l| l.trim_start().starts_with("//"))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut v: Value = serde_json::from_str(&crate::probes::github::strip_line_comments(&text))
        .map_err(io::Error::other)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("authTokens");
        o.remove("copilotTokens");
    }
    let body = serde_json::to_string_pretty(&v).map_err(io::Error::other)?;
    write_renamed(path, format!("{head}{body}\n").as_bytes())
}

/// pi: the shared `auth.json` of an install from before pi took API keys
/// only can be the link of the pi plan accounts. Make it a plain `{}`
/// file again, by a rename over the link. pi then reads no login there;
/// its agents get their API key in the env. A plain file stays as it is.
fn unlink_pi(home: &Path, m: &mut Migration) {
    if !pi_link_installed(home) {
        return;
    }
    // A rename replaces the link itself, not its target.
    match write_renamed(&home.join(AUTH_FILE), b"{}") {
        Ok(()) => m.pi_unlinked = true,
        Err(e) => m.skipped.push(format!(
            "cannot make the shared pi auth.json a plain file again: {e}"
        )),
    }
}

// ---- The launch ----

/// What the launch of one agent needs for its account.
#[derive(Debug, Clone, Copy)]
pub struct LaunchInput<'a> {
    pub state_dir: &'a Path,
    pub harness: HarnessId,
    pub subscription: Option<&'a str>,
    /// The default account of the harness (`account_defaults`), for an
    /// agent with no subscription.
    pub default: Option<&'a str>,
    pub pool: &'a [String],
    /// `<state>/agents/<ulid>/harness`.
    pub harness_dir: &'a Path,
    /// The shared home on the host.
    pub shared_home: &'a Path,
    /// The shared home as the harness sees it.
    pub home_inside: &'a Path,
    /// The harness runs in bwrap.
    pub sandboxed: bool,
}

/// The account part of a launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchAccounts {
    /// The subscription whose account the agent runs on. `None`: no
    /// account (an API key, or a shared login that is not migrated yet).
    pub account: Option<String>,
    /// Env vars with no credential value (a path). The launch spec holds
    /// them.
    pub env: Vec<(String, String)>,
    /// The names of the credential env vars of the account (the Copilot
    /// token). The values never go into the launch spec: `terrarium
    /// exec-env` gets them from the daemon inside the sandbox
    /// ([`credential_env`], PLAN §16b.4).
    pub credential_vars: Vec<String>,
    pub overlays: Vec<FileOverlay>,
    /// Account stores to bind read-write at their own path: each pool
    /// member with a login of the harness (Claude).
    pub stores: Vec<PathBuf>,
    /// Bind `<harness dir>/cred` read-only at [`SANDBOX_CRED`]: pi, while
    /// the old shared `auth.json` link is there.
    pub cred_alias: Option<PathBuf>,
    /// The subscriptions of `stores`: a live swap may pick these.
    pub bound: Vec<String>,
    /// A problem that does not stop the launch (an event for the agent).
    pub warning: Option<String>,
}

/// The pool members (with the current subscription) that have a login
/// of `harness`, current first.
fn pool_with_logins(input: &LaunchInput<'_>, current: &str) -> Vec<String> {
    let mut names = vec![current.to_owned()];
    names.extend(input.pool.iter().filter(|p| *p != current).cloned());
    names
        .into_iter()
        .filter(|n| has_credential(input.state_dir, n, input.harness))
        .collect()
}

/// The plan accounts with a login, for the default of each harness
/// (`store::subscriptions::settle_defaults`).
pub fn default_candidates(state_dir: &Path) -> Vec<crate::store::subscriptions::AccountRef> {
    list(state_dir)
        .into_iter()
        .filter(|(n, m)| has_credential(state_dir, n, m.harness))
        .map(|(n, m)| crate::store::subscriptions::AccountRef {
            subscription: n,
            harness: m.harness,
            created_at: Some(m.created_at).filter(|c| !c.is_empty()),
        })
        .collect()
}

/// The account of a Claude, Codex, or Copilot agent with no subscription
/// (PLAN §16.3): the default account of `harness` when it has a login;
/// else the one account of `harness`, as the shared login was before the
/// migration. With two or more and no default, the start fails. With
/// none, nothing (an API key, or the shared login before the migration).
fn default_account(
    state_dir: &Path,
    harness: HarnessId,
    default: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(d) = default.filter(|d| has_credential(state_dir, d, harness)) {
        return Ok(Some(d.to_owned()));
    }
    let mut names: Vec<String> = fs::read_dir(credentials_dir(state_dir).join(ACCOUNTS_DIR))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| has_credential(state_dir, n, harness))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    match names.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        many => Err(format!(
            "the agent has no subscription and the {harness} accounts ({}) have no default: \
             run `terrarium admin subscription default <name>` or give the agent a subscription",
            many.join(", ")
        )),
    }
}

/// Point the agent at its account and give the env, the binds, and the
/// overlays of the launch (PLAN §16.3). An agent whose subscription has
/// no account of the harness gets nothing: it uses an API key, or the
/// shared login before the migration.
pub fn prepare_launch(input: &LaunchInput<'_>) -> Result<LaunchAccounts, String> {
    let mut out = LaunchAccounts::default();
    let fs_err = |e: io::Error| format!("cannot point the agent at its account: {e}");
    // pi uses API keys only: no account, and no default account. While the
    // old shared `auth.json` link is there (until the migration makes it a
    // plain file), the link reads the agent's empty store.
    if input.harness == HarnessId::Pi {
        if pi_link_installed(input.shared_home) {
            if !input.sandboxed {
                return Err(
                    "the shared pi auth.json is still the link of the old pi plan accounts, \
                     which needs the sandbox; stop every pi agent once, so the daemon makes it \
                     a plain file again"
                        .into(),
                );
            }
            out.cred_alias = Some(input.harness_dir.join(POINTER_DIR));
            let none = none_store(input.harness_dir).map_err(fs_err)?;
            point(input.harness_dir, &none).map_err(fs_err)?;
        }
        return Ok(out);
    }
    let default = match input.subscription {
        None => default_account(input.state_dir, input.harness, input.default)?,
        Some(_) => None,
    };
    let current = input
        .subscription
        .or(default.as_deref())
        .filter(|s| has_credential(input.state_dir, s, input.harness));
    let Some(current) = current else {
        return Ok(out);
    };
    let store = store_dir(input.state_dir, current);
    point(input.harness_dir, &store).map_err(fs_err)?;
    out.account = Some(current.to_owned());
    match input.harness {
        HarnessId::Claude => {
            out.env.push((
                CLAUDE_CRED_ENV.to_owned(),
                claude_cred_dir(input.harness_dir).display().to_string(),
            ));
            out.bound = pool_with_logins(input, current);
        }
        // pi returned above: it has no account.
        HarnessId::Pi => {}
        HarnessId::Codex => {
            // bwrap needs a mount point in the shared home. `{}` is "no
            // login" for an agent without an account.
            let mount = input.shared_home.join(AUTH_FILE);
            if fs::symlink_metadata(&mount).is_err() {
                write_renamed(&mount, b"{}").map_err(fs_err)?;
            }
            let source =
                fs::canonicalize(credential_path(&store, HarnessId::Codex)).map_err(fs_err)?;
            out.overlays.push(FileOverlay {
                source,
                target: input.home_inside.join(AUTH_FILE),
                writable: true,
            });
        }
        HarnessId::Copilot => {
            if copilot_token(input.state_dir, current).is_none() {
                return Err(format!("the Copilot token of `{current}` cannot be read"));
            }
            out.credential_vars.push(COPILOT_TOKEN_ENV.to_owned());
            // The env token wins over `config.json`. A token there again
            // (a login with the shared home) is another account that every
            // agent could read: the operator moves it with the wizard.
            if credential_in_home(HarnessId::Copilot, input.shared_home).is_some() {
                out.warning = Some(
                    "the shared Copilot config.json holds a login token again; the agent uses \
                     its account token (COPILOT_GITHUB_TOKEN wins), but every Copilot agent can \
                     read the shared one. Stop the Copilot agents, and the daemon moves it to \
                     an account"
                        .into(),
                );
            }
        }
    }
    out.stores = out
        .bound
        .iter()
        .map(|n| store_dir(input.state_dir, n))
        .collect();
    Ok(out)
}

/// Write Claude's `oauthAccount` of the agent's account into its
/// per-agent `.claude.json` (`claude_json`), before the launch. The rest
/// of the file stays. Nothing happens when the account recorded none.
pub fn set_oauth_account(state_dir: &Path, sub: &str, claude_json: &Path) -> io::Result<()> {
    let Some(account) = read_meta(state_dir, sub).and_then(|m| m.oauth_account) else {
        return Ok(());
    };
    let mut v: Value = fs::read(claude_json)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if v.get("oauthAccount") == Some(&account) {
        return Ok(());
    }
    v["oauthAccount"] = account;
    let bytes = serde_json::to_vec_pretty(&v).map_err(io::Error::other)?;
    // Before the launch nothing has the file bound: a rename is safe.
    write_renamed(claude_json, &bytes)
}

/// The homes of every harness, for the migration loop.
pub fn harnesses() -> [HarnessId; 4] {
    [
        HarnessId::Claude,
        HarnessId::Codex,
        HarnessId::Copilot,
        HarnessId::Pi,
    ]
}

/// The shared home of each harness, from a map with defaults.
pub fn home_of(homes: &HashMap<HarnessId, PathBuf>, h: HarnessId) -> PathBuf {
    homes
        .get(&h)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(h.auth_dir()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sub(name: &str, provider: &str, harness: HarnessId) -> Subscription {
        serde_json::from_value(json!({
            "name": name, "provider": provider, "harnesses": [harness], "alert_at": 80,
            "source": "api", "login_status": "logged_in", "probe_status": "none",
            "created_at": "", "updated_at": ""
        }))
        .unwrap()
    }

    fn claude_login(tag: &str) -> String {
        json!({"claudeAiOauth": {"accessToken": format!("at-{tag}"),
               "refreshToken": format!("rt-{tag}"), "expiresAt": 1}})
        .to_string()
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// The pointer is a symlink that a rename replaces: a reader sees the
    /// old or the new target, never none.
    #[test]
    fn the_pointer_swaps_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path().join("harness");
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        point(&h, &a).unwrap();
        assert_eq!(pointed(&h).as_deref(), Some(a.as_path()));
        point(&h, &b).unwrap();
        assert_eq!(pointed(&h).as_deref(), Some(b.as_path()));
        let left: Vec<_> = fs::read_dir(h.join(POINTER_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, [POINTER], "no temp link stays");
        assert_eq!(
            sub_of_store(&store_dir(Path::new("/s"), "x")).as_deref(),
            Some("x")
        );
    }

    /// The lock is a `proper-lockfile` directory: a held lock makes the
    /// daemon wait, a stale one is taken over, and the drop removes it.
    #[test]
    fn locks_follow_proper_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("auth.json.lock");
        let held = lock_dir(&lock, Duration::from_secs(30), Duration::from_secs(1)).unwrap();
        assert!(lock.is_dir());
        let e = lock_dir(&lock, Duration::from_secs(30), Duration::from_millis(120)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        drop(held);
        assert!(!lock.exists());

        // A holder that another process left and that did not touch the
        // lock for longer than `stale` is gone.
        fs::create_dir(&lock).unwrap();
        let old = SystemTime::now() - Duration::from_secs(120);
        fs::File::open(&lock).unwrap().set_modified(old).unwrap();
        let taken = lock_dir(&lock, Duration::from_secs(60), Duration::from_millis(200)).unwrap();
        drop(taken);

        // A lock that the harness releases while the daemon waits.
        fs::create_dir(&lock).unwrap();
        let l2 = lock.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            fs::remove_dir(&l2).unwrap();
        });
        lock_dir(&lock, Duration::from_secs(60), Duration::from_secs(5)).unwrap();
        t.join().unwrap();
    }

    /// A live Claude swap waits for Claude's refresh lock in the old
    /// account and its write lock in the new one, then re-points.
    #[test]
    fn a_claude_swap_takes_both_accounts_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let home = tmp.path().join("auth/claude");
        fs::create_dir_all(&home).unwrap();
        for s in ["max-a", "max-b"] {
            write_credential(&state, s, HarnessId::Claude, claude_login(s).as_bytes()).unwrap();
        }
        let h = tmp.path().join("agent/harness");
        point(&h, &store_dir(&state, "max-a")).unwrap();
        let old_refresh = store_dir(&state, "max-a")
            .join("claude")
            .join(CLAUDE_REFRESH_LOCK);
        fs::create_dir(&old_refresh).unwrap();
        let e = live_swap(
            &state,
            HarnessId::Claude,
            &h,
            "max-b",
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(e.contains("still held"), "{e}");
        assert_eq!(
            pointed(&h),
            Some(store_dir(&state, "max-a")),
            "no swap while held"
        );
        fs::remove_dir(&old_refresh).unwrap();
        let new_write = store_dir(&state, "max-b")
            .join("claude")
            .join(CLAUDE_STORAGE_LOCK);
        fs::create_dir(&new_write).unwrap();
        let w = new_write.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            fs::remove_dir(&w).unwrap();
        });
        live_swap(
            &state,
            HarnessId::Claude,
            &h,
            "max-b",
            Duration::from_secs(5),
        )
        .unwrap();
        t.join().unwrap();
        assert_eq!(pointed(&h), Some(store_dir(&state, "max-b")));
        // The env path reads the new account through the link.
        let through = fs::read_to_string(claude_cred_dir(&h).join(CLAUDE_CREDENTIALS)).unwrap();
        assert!(through.contains("rt-max-b"));
        for s in ["max-a", "max-b"] {
            let d = store_dir(&state, s).join("claude");
            assert!(!d.join(CLAUDE_REFRESH_LOCK).exists());
            assert!(!d.join(CLAUDE_STORAGE_LOCK).exists());
        }
        // Codex has no live swap.
        assert!(live_swap(&state, HarnessId::Codex, &h, "max-b", LOCK_TIMEOUT).is_err());
    }

    /// The account layout: mode 0700 dirs, 0600 files, the Claude store
    /// dir holds the file, and Claude's legacy `<realpath>.lock` lands in
    /// the store (shared by every agent of the account).
    #[test]
    fn the_store_layout_keeps_the_locks_per_account() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        write_credential(state, "a", HarnessId::Claude, b"{}").unwrap();
        let store = store_dir(state, "a");
        for d in [
            credentials_dir(state),
            account_dir(state, "a"),
            store.clone(),
            store.join("claude"),
        ] {
            assert_eq!(mode(&d), 0o700, "{}", d.display());
        }
        assert_eq!(mode(&credential_path(&store, HarnessId::Claude)), 0o600);
        // Through two agents' pointers, the realpath of the cred dir is
        // one dir, so its sibling lock is one path.
        let (h1, h2) = (tmp.path().join("h1"), tmp.path().join("h2"));
        point(&h1, &store).unwrap();
        point(&h2, &store).unwrap();
        let real1 = fs::canonicalize(claude_cred_dir(&h1)).unwrap();
        let real2 = fs::canonicalize(claude_cred_dir(&h2)).unwrap();
        assert_eq!(real1, real2);
        let legacy = PathBuf::from(format!("{}.lock", real1.display()));
        assert_eq!(
            legacy.parent(),
            Some(fs::canonicalize(&store).unwrap().as_path())
        );
    }

    fn fake_homes(root: &Path) -> HashMap<HarnessId, PathBuf> {
        let mut homes = HashMap::new();
        for h in harnesses() {
            let d = root.join("config").join(h.as_str());
            fs::create_dir_all(&d).unwrap();
            homes.insert(h, d);
        }
        homes
    }

    /// The migration moves each shared login into the account of the one
    /// subscription that uses it, keeps a 0600 backup, and does nothing on
    /// a second run.
    #[test]
    fn the_migration_moves_each_shared_login_once() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let homes = fake_homes(tmp.path());
        let claude = &homes[&HarnessId::Claude];
        fs::write(claude.join(CLAUDE_CREDENTIALS), claude_login("x")).unwrap();
        fs::write(
            claude.join(".claude.json"),
            r#"{"oauthAccount": {"accountUuid": "u1", "emailAddress": "a@x"}, "theme": "dark"}"#,
        )
        .unwrap();
        let codex = &homes[&HarnessId::Codex];
        fs::write(
            codex.join(AUTH_FILE),
            r#"{"tokens": {"access_token": "t", "account_id": "c"}}"#,
        )
        .unwrap();
        let copilot = &homes[&HarnessId::Copilot];
        fs::write(
            copilot.join("config.json"),
            "// This file is managed automatically.\n{\"authTokens\": {\"https://github.com:wil\": {\"token\": \"gho_abc\"}}, \"lastLoggedInUser\": {\"host\": \"https://github.com\", \"login\": \"wil\"}, \"firstLaunchAt\": \"x\"}\n",
        )
        .unwrap();
        fs::write(homes[&HarnessId::Pi].join(AUTH_FILE), "{}").unwrap();
        let subs = vec![
            sub("anthropic-max-20x", "anthropic", HarnessId::Claude),
            sub("glm", "zai", HarnessId::Claude),
            sub("openai-pro", "openai", HarnessId::Codex),
            sub("github-copilot", "github", HarnessId::Copilot),
        ];
        for h in harnesses() {
            let m = migrate(&state, h, &homes[&h], &subs);
            match h {
                HarnessId::Claude => assert_eq!(m.moved, ["anthropic-max-20x"]),
                HarnessId::Codex => assert_eq!(m.moved, ["openai-pro"]),
                HarnessId::Copilot => assert_eq!(m.moved, ["github-copilot"]),
                HarnessId::Pi => assert!(m.is_empty(), "{m:?}"),
            }
            for b in &m.backups {
                assert_eq!(mode(b), 0o600);
            }
        }
        assert!(!claude.join(CLAUDE_CREDENTIALS).exists());
        assert!(!codex.join(AUTH_FILE).exists());
        let cfg = fs::read_to_string(copilot.join("config.json")).unwrap();
        assert!(
            cfg.starts_with("// This file is managed automatically.\n"),
            "{cfg}"
        );
        assert!(
            !cfg.contains("gho_") && !cfg.contains("authTokens"),
            "{cfg}"
        );
        assert!(cfg.contains("firstLaunchAt"));
        assert_eq!(
            copilot_token(&state, "github-copilot").as_deref(),
            Some("gho_abc")
        );
        assert!(has_credential(
            &state,
            "anthropic-max-20x",
            HarnessId::Claude
        ));
        assert!(has_credential(&state, "openai-pro", HarnessId::Codex));
        let meta = read_meta(&state, "anthropic-max-20x").unwrap();
        assert_eq!(meta.oauth_account.unwrap()["accountUuid"], "u1");
        assert_eq!(
            read_meta(&state, "github-copilot")
                .unwrap()
                .login
                .as_deref(),
            Some("wil")
        );
        assert!(
            !pi_link_installed(&homes[&HarnessId::Pi]),
            "pi never gets the link"
        );
        // Idempotent.
        for h in harnesses() {
            assert!(!migration_pending(h, &homes[&h]), "{h}");
            assert!(migrate(&state, h, &homes[&h], &subs).is_empty(), "{h}");
        }
        let backups = fs::read_dir(credentials_dir(&state).join(BACKUP_DIR))
            .unwrap()
            .count();
        assert_eq!(backups, 3);
    }

    /// A move that a crash cut short (the account has the login, the
    /// shared file is still there) finishes; a second subscription of the
    /// provider leaves the choice to the operator.
    #[test]
    fn the_migration_finishes_a_cut_move_and_refuses_a_guess() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let homes = fake_homes(tmp.path());
        let claude = &homes[&HarnessId::Claude];
        let login = claude_login("x");
        fs::write(claude.join(CLAUDE_CREDENTIALS), &login).unwrap();
        let two = vec![
            sub("max-a", "anthropic", HarnessId::Claude),
            sub("max-b", "anthropic", HarnessId::Claude),
        ];
        let m = migrate(&state, HarnessId::Claude, claude, &two);
        assert!(m.moved.is_empty());
        assert!(m.skipped[0].contains("2 subscriptions fit it"), "{m:?}");
        assert!(claude.join(CLAUDE_CREDENTIALS).exists());

        write_credential(&state, "max-a", HarnessId::Claude, login.as_bytes()).unwrap();
        let m = migrate(&state, HarnessId::Claude, claude, &two);
        assert_eq!(m.moved, ["max-a"]);
        assert!(!claude.join(CLAUDE_CREDENTIALS).exists());
    }

    /// pi uses API keys only: a login in the shared `auth.json` moves
    /// nowhere, and a plain file stays as it is. The old link of the pi
    /// plan accounts becomes a plain `{}` file again, once.
    #[test]
    fn the_pi_migration_makes_the_old_link_a_plain_file() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let homes = fake_homes(tmp.path());
        let pi = &homes[&HarnessId::Pi];
        let file =
            r#"{"anthropic": {"type": "oauth", "refresh": "r", "access": "a", "expires": 1}}"#;
        fs::write(pi.join(AUTH_FILE), file).unwrap();
        let subs = vec![sub("pi-anthropic", "anthropic", HarnessId::Pi)];
        assert!(!migration_pending(HarnessId::Pi, pi));
        assert!(migrate(&state, HarnessId::Pi, pi, &subs).is_empty());
        assert_eq!(fs::read_to_string(pi.join(AUTH_FILE)).unwrap(), file);
        assert!(!exists(&state, "pi-anthropic"));

        fs::remove_file(pi.join(AUTH_FILE)).unwrap();
        std::os::unix::fs::symlink(pi_link_target(), pi.join(AUTH_FILE)).unwrap();
        assert!(pi_link_installed(pi) && migration_pending(HarnessId::Pi, pi));
        let m = migrate(&state, HarnessId::Pi, pi, &subs);
        assert!(m.pi_unlinked && m.moved.is_empty(), "{m:?}");
        let meta = fs::symlink_metadata(pi.join(AUTH_FILE)).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(fs::read_to_string(pi.join(AUTH_FILE)).unwrap(), "{}");
        assert!(!migration_pending(HarnessId::Pi, pi));
        assert!(migrate(&state, HarnessId::Pi, pi, &subs).is_empty());
    }

    /// The launch binds only the pool's accounts with a login of the
    /// harness, never another account; Codex gets a file bind, Copilot an
    /// env token.
    #[test]
    fn a_launch_binds_only_the_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let home = tmp.path().join("auth/claude");
        fs::create_dir_all(&home).unwrap();
        for s in ["max-a", "max-b", "max-c"] {
            write_credential(&state, s, HarnessId::Claude, claude_login(s).as_bytes()).unwrap();
        }
        let h = tmp.path().join("agent/harness");
        let pool = vec![
            "max-a".to_owned(),
            "max-b".to_owned(),
            "no-login".to_owned(),
        ];
        let input = LaunchInput {
            state_dir: &state,
            harness: HarnessId::Claude,
            subscription: Some("max-b"),
            default: None,
            pool: &pool,
            harness_dir: &h,
            shared_home: &home,
            home_inside: Path::new("/home/agent/.claude"),
            sandboxed: true,
        };
        let l = prepare_launch(&input).unwrap();
        assert_eq!(l.account.as_deref(), Some("max-b"));
        assert_eq!(l.bound, ["max-b", "max-a"]);
        assert_eq!(
            l.stores,
            [store_dir(&state, "max-b"), store_dir(&state, "max-a")]
        );
        assert!(!l.stores.contains(&store_dir(&state, "max-c")));
        assert_eq!(
            l.env,
            [(
                CLAUDE_CRED_ENV.to_owned(),
                claude_cred_dir(&h).display().to_string()
            )]
        );
        assert_eq!(pointed(&h), Some(store_dir(&state, "max-b")));

        // No subscription: the default account of the harness first.
        let d = tmp.path().join("agent-d/harness");
        let l = prepare_launch(&LaunchInput {
            subscription: None,
            default: Some("max-c"),
            harness_dir: &d,
            ..input
        })
        .unwrap();
        assert_eq!(l.account.as_deref(), Some("max-c"));
        assert_eq!(pointed(&d), Some(store_dir(&state, "max-c")));
        // A default with no login counts as no default. Three accounts and
        // no default: the start fails and says what to do.
        for default in [None, Some("no-login")] {
            let e = prepare_launch(&LaunchInput {
                subscription: None,
                default,
                ..input
            })
            .unwrap_err();
            assert_eq!(
                e,
                "the agent has no subscription and the claude accounts (max-a, max-b, max-c) \
                 have no default: run `terrarium admin subscription default <name>` or give \
                 the agent a subscription"
            );
        }
        // A default of another harness is not a Claude login.
        let phome0 = tmp.path().join("auth/copilot0");
        fs::create_dir_all(&phome0).unwrap();
        write_credential(&state, "gh0", HarnessId::Copilot, b"gho_y\n").unwrap();
        assert!(
            prepare_launch(&LaunchInput {
                subscription: None,
                default: Some("gh0"),
                ..input
            })
            .is_err()
        );
        // The subscription wins over the default.
        let l = prepare_launch(&LaunchInput {
            default: Some("max-c"),
            ..input
        })
        .unwrap();
        assert_eq!(l.account.as_deref(), Some("max-b"));
        let solo = tmp.path().join("solo");
        write_credential(
            &solo,
            "only",
            HarnessId::Claude,
            claude_login("only").as_bytes(),
        )
        .unwrap();
        let h1 = tmp.path().join("agent1/harness");
        let one = prepare_launch(&LaunchInput {
            state_dir: &solo,
            subscription: None,
            harness_dir: &h1,
            ..input
        })
        .unwrap();
        assert_eq!(one.account.as_deref(), Some("only"));
        assert_eq!(pointed(&h1), Some(store_dir(&solo, "only")));
        let empty = tmp.path().join("empty");
        let none = prepare_launch(&LaunchInput {
            state_dir: &empty,
            subscription: None,
            ..input
        })
        .unwrap();
        assert_eq!(none, LaunchAccounts::default());

        // No account for the subscription: nothing (an API key, or the
        // shared login before the migration).
        let none = prepare_launch(&LaunchInput {
            subscription: Some("glm"),
            ..input
        })
        .unwrap();
        assert_eq!(none, LaunchAccounts::default());

        // Codex: the account file over `$CODEX_HOME/auth.json`, and a
        // `{}` mount point in the shared home.
        let chome = tmp.path().join("auth/codex");
        fs::create_dir_all(&chome).unwrap();
        write_credential(&state, "pro", HarnessId::Codex, br#"{"tokens": {}}"#).unwrap();
        let ch = tmp.path().join("agent2/harness");
        let l = prepare_launch(&LaunchInput {
            harness: HarnessId::Codex,
            subscription: Some("pro"),
            pool: &[],
            harness_dir: &ch,
            shared_home: &chome,
            home_inside: Path::new("/home/agent/.codex"),
            ..input
        })
        .unwrap();
        assert!(l.stores.is_empty() && l.env.is_empty());
        assert_eq!(l.overlays.len(), 1);
        assert_eq!(
            l.overlays[0].target,
            Path::new("/home/agent/.codex/auth.json")
        );
        assert_eq!(fs::read_to_string(chome.join(AUTH_FILE)).unwrap(), "{}");

        // Copilot: the token name only (exec-env gets the value), no bind.
        let phome = tmp.path().join("auth/copilot");
        fs::create_dir_all(&phome).unwrap();
        write_credential(&state, "gh", HarnessId::Copilot, b"gho_x\n").unwrap();
        let l = prepare_launch(&LaunchInput {
            harness: HarnessId::Copilot,
            subscription: Some("gh"),
            pool: &[],
            harness_dir: &tmp.path().join("agent3/harness"),
            shared_home: &phome,
            home_inside: Path::new("/home/agent/.copilot"),
            ..input
        })
        .unwrap();
        assert!(l.env.is_empty());
        assert_eq!(l.credential_vars, [COPILOT_TOKEN_ENV.to_owned()]);
        assert_eq!(
            credential_env(&state, HarnessId::Copilot, Some("gh"))
                .into_iter()
                .map(|(k, v)| (k, v.to_string()))
                .collect::<Vec<_>>(),
            [(COPILOT_TOKEN_ENV.to_owned(), "gho_x".to_owned())]
        );
        assert!(credential_env(&state, HarnessId::Claude, Some("gh")).is_empty());
        assert!(l.stores.is_empty() && l.overlays.is_empty());
    }

    /// pi has no account, also when an old pi account exists. While the
    /// old shared link is there, the agent's `cred` dir is shown at the
    /// sandbox path and points at an empty store; after the unlink, the
    /// launch adds nothing.
    #[test]
    fn a_pi_launch_has_no_account() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let home = tmp.path().join("auth/pi");
        fs::create_dir_all(&home).unwrap();
        // An old pi account, from before pi took API keys only.
        write_credential(&state, "pi-a", HarnessId::Pi, br#"{"anthropic": {}}"#).unwrap();
        record_meta(
            &state,
            "pi-a",
            HarnessId::Pi,
            Some("anthropic"),
            None,
            None,
            "login",
        )
        .unwrap();
        std::os::unix::fs::symlink(pi_link_target(), home.join(AUTH_FILE)).unwrap();
        let h = tmp.path().join("agent/harness");
        let input = LaunchInput {
            state_dir: &state,
            harness: HarnessId::Pi,
            subscription: Some("pi-a"),
            default: Some("pi-a"),
            pool: &[],
            harness_dir: &h,
            shared_home: &home,
            home_inside: Path::new("/home/agent/.pi/agent"),
            sandboxed: true,
        };
        let l = prepare_launch(&input).unwrap();
        assert_eq!(l.account, None);
        assert!(l.stores.is_empty() && l.bound.is_empty() && l.env.is_empty());
        assert_eq!(l.cred_alias, Some(h.join(POINTER_DIR)));
        let p = pointed(&h).unwrap();
        assert_eq!(fs::read_to_string(p.join(AUTH_FILE)).unwrap(), "{}");
        let e = prepare_launch(&LaunchInput {
            sandboxed: false,
            ..input
        })
        .unwrap_err();
        assert!(e.contains("stop every pi agent"), "{e}");
        // After the unlink: nothing at all.
        assert!(migrate(&state, HarnessId::Pi, &home, &[]).pi_unlinked);
        let other = tmp.path().join("agent2/harness");
        let l = prepare_launch(&LaunchInput {
            harness_dir: &other,
            sandboxed: false,
            ..input
        })
        .unwrap();
        assert_eq!(l, LaunchAccounts::default());
        assert_eq!(pointed(&other), None);
        // pi has no live swap.
        assert!(live_swap(&state, HarnessId::Pi, &h, "pi-a", LOCK_TIMEOUT).is_err());
    }

    /// A login moves only its credential from the scratch home; another
    /// account and the shared home stay as they were.
    #[test]
    fn an_import_touches_only_its_account() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let home = tmp.path().join("auth/claude");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("settings.json"), "{}").unwrap();
        write_credential(
            &state,
            "max-a",
            HarnessId::Claude,
            claude_login("a").as_bytes(),
        )
        .unwrap();
        let scratch = tmp.path().join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join(CLAUDE_CREDENTIALS), claude_login("b")).unwrap();
        fs::write(
            scratch.join(".claude.json"),
            r#"{"oauthAccount": {"accountUuid": "ub"}}"#,
        )
        .unwrap();
        import_login(&state, "max-b", HarnessId::Claude, "anthropic", &scratch).unwrap();
        let a = fs::read_to_string(credential_path(
            &store_dir(&state, "max-a"),
            HarnessId::Claude,
        ))
        .unwrap();
        let b = fs::read_to_string(credential_path(
            &store_dir(&state, "max-b"),
            HarnessId::Claude,
        ))
        .unwrap();
        assert!(a.contains("rt-a") && b.contains("rt-b"));
        let entries: Vec<_> = fs::read_dir(&home)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries, ["settings.json"]);
        assert_eq!(
            read_meta(&state, "max-b").unwrap().oauth_account.unwrap()["accountUuid"],
            "ub"
        );
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(import_login(&state, "max-c", HarnessId::Claude, "anthropic", &empty,).is_err());
        assert!(remove(&state, "max-b").unwrap());
        assert!(!exists(&state, "max-b"));
        assert!(has_credential(&state, "max-a", HarnessId::Claude));

        // The per-agent `.claude.json` gets the account's oauthAccount.
        let cj = tmp.path().join("claude.json");
        fs::write(
            &cj,
            r#"{"oauthAccount": {"accountUuid": "old"}, "projects": {}}"#,
        )
        .unwrap();
        record_meta(
            &state,
            "max-a",
            HarnessId::Claude,
            None,
            None,
            Some(json!({"accountUuid": "ua"})),
            "login",
        )
        .unwrap();
        set_oauth_account(&state, "max-a", &cj).unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&cj).unwrap()).unwrap();
        assert_eq!(v["oauthAccount"]["accountUuid"], "ua");
        assert!(v.get("projects").is_some());
    }

    /// Codex runs with the account file bound: a re-login writes it in
    /// place, so the bound inode sees the new login.
    #[test]
    fn a_codex_relogin_keeps_the_inode() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let home = tmp.path().join("auth/codex");
        fs::create_dir_all(&home).unwrap();
        write_credential(&state, "pro", HarnessId::Codex, br#"{"tokens": {"a": 1}}"#).unwrap();
        let p = credential_path(&store_dir(&state, "pro"), HarnessId::Codex);
        let ino = fs::metadata(&p).unwrap().ino();
        write_credential(&state, "pro", HarnessId::Codex, br#"{"tokens": {"a": 2}}"#).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().ino(), ino);
        assert!(fs::read_to_string(&p).unwrap().contains('2'));
    }

    #[test]
    fn plan_providers() {
        assert!(plan_provider(HarnessId::Copilot, "github-copilot"));
        assert!(!plan_provider(HarnessId::Claude, "zai"));
        assert!(!plan_provider(HarnessId::Claude, "moonshot"));
    }
}
