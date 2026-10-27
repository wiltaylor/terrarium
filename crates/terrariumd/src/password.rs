//! The operator password (`docs/api.md` §1.1, `docs/security-review.md`
//! F15). The daemon keeps only an Argon2id hash (a PHC string) and the
//! password version in `<state>/operator.password` (JSON, mode 0600):
//!
//! ```json
//! {"hash": "$argon2id$v=19$m=19456,t=2,p=1$...", "version": 3}
//! ```
//!
//! Every session token carries the version as its `pv` claim. A new
//! password, or "log out everywhere" (`DELETE /api/sessions`), adds one to
//! the version, and every token with an older `pv` stops working.
//!
//! `terrariumd password set` writes the file with no daemon. A running
//! daemon sees the new file at the next check: [`PasswordStore::current`]
//! reads the file again when its inode, size, or mtime changed.
//!
//! [`LoginLimiter`] limits the failed password checks per client address.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use serde::{Deserialize, Serialize};

/// The file of the hash and the version, in the state dir.
pub const PASSWORD_FILE: &str = "operator.password";
/// The shortest password, in characters.
pub const MIN_LEN: usize = 12;
/// The longest password, in bytes. Argon2 takes any length; the bound
/// keeps a request body from making the hash slow.
pub const MAX_LEN: usize = 1024;
/// `TERRARIUM_PASSWORD`: the first password, set at the start when no
/// password exists. Remove it from the env after the first start.
pub const ENV_PASSWORD: &str = "TERRARIUM_PASSWORD";
/// `TERRARIUM_PASSWORD_FILE`: a file that holds the first password.
pub const ENV_PASSWORD_FILE: &str = "TERRARIUM_PASSWORD_FILE";
/// The variables that no child process may inherit. `main` starts the
/// daemon again without them, and the spawns strip them too.
pub const SECRET_ENV: [&str; 2] = [ENV_PASSWORD, ENV_PASSWORD_FILE];
/// The lock file of the read-modify-write of the password file, in the
/// state dir.
pub const LOCK_FILE: &str = "operator.password.lock";

/// `TERRARIUM_PASSWORD` from the env, if set and not empty. Read here, not
/// as a flag, so the value never shows in the process arguments.
pub fn env_password() -> Option<String> {
    std::env::var(ENV_PASSWORD).ok().filter(|v| !v.is_empty())
}

/// Refuse to write the password file as another user than the owner of
/// the state dir: the daemon reads the file (mode 0600) as that owner, so
/// a file of another user (root) would lock every login out. A state dir
/// that does not exist yet is fine.
pub fn check_owner(state_dir: &Path) -> Result<(), PasswordError> {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(state_dir) else {
        return Ok(());
    };
    let me = nix::unistd::geteuid().as_raw();
    if meta.uid() != me {
        return Err(PasswordError::Owner {
            dir: state_dir.to_owned(),
            owner: meta.uid(),
            me,
        });
    }
    Ok(())
}

/// The hint when no password is set.
pub const NO_PASSWORD: &str = "no password set; run terrariumd password set";

/// The errors of the password store.
#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("the password must have at least {MIN_LEN} characters")]
    TooShort,
    #[error("the password must have at most {MAX_LEN} bytes")]
    TooLong,
    #[error("cannot hash the password: {0}")]
    Hash(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid password file: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error(
        "{} belongs to uid {owner}, but this runs as uid {me}; run it as the daemon's user, \
         for example `docker compose exec terrarium terrariumd password set`",
        dir.display()
    )]
    Owner { dir: PathBuf, owner: u32, me: u32 },
}

/// Check the rules for a new password: [`MIN_LEN`] characters or more,
/// and [`MAX_LEN`] bytes or fewer.
pub fn check_new(password: &str) -> Result<(), PasswordError> {
    if password.len() > MAX_LEN {
        return Err(PasswordError::TooLong);
    }
    if password.chars().count() < MIN_LEN {
        return Err(PasswordError::TooShort);
    }
    Ok(())
}

/// Hash a password: Argon2id with the default parameters of the crate
/// (m = 19 MiB, t = 2, p = 1) and a random salt, as a PHC string.
pub fn hash(password: &str) -> Result<String, PasswordError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))
}

/// Does `password` match the PHC string `phc`? The parameters come from
/// the PHC string. A malformed string matches nothing.
pub fn verify(phc: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// The content of the password file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordState {
    /// The PHC string. `None`: no password is set (a "log out everywhere"
    /// with no password writes the version alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    /// The password version: the `pv` claim of every valid token.
    #[serde(default)]
    pub version: u64,
}

impl PasswordState {
    pub fn is_set(&self) -> bool {
        self.hash.is_some()
    }

    /// Does `password` match? `false` when no password is set.
    pub fn verify(&self, password: &str) -> bool {
        self.hash.as_deref().is_some_and(|h| verify(h, password))
    }
}

impl std::fmt::Debug for PasswordStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// What identifies one version of the file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
}

fn stamp(meta: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    Stamp {
        ino: meta.ino(),
        len: meta.len(),
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
    }
}

/// The password file, with a copy in memory that follows the file.
pub struct PasswordStore {
    path: PathBuf,
    /// The last read: the stamp of the file (`None`: no file) and its state.
    cache: Mutex<Option<(Option<Stamp>, PasswordState)>>,
    /// One write at a time in this process.
    write: Mutex<()>,
}

impl PasswordStore {
    /// The store of `<state_dir>/operator.password`. It reads nothing yet.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(PASSWORD_FILE),
            cache: Mutex::new(None),
            write: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, source: std::io::Error) -> PasswordError {
        PasswordError::Io {
            path: self.path.clone(),
            source,
        }
    }

    /// Read the file. No file: no password, version 0. A file that others
    /// may read is set back to mode 0600, with a warning.
    fn read_file(&self) -> Result<(Option<Stamp>, PasswordState), PasswordError> {
        use std::os::unix::fs::PermissionsExt;
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((None, PasswordState::default()));
            }
            Err(e) => return Err(self.io(e)),
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %self.path.display(),
                mode = format!("{mode:o}"),
                "the password file was readable by others; setting mode 0600"
            );
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| self.io(e))?;
        }
        let text = std::fs::read_to_string(&self.path).map_err(|e| self.io(e))?;
        let state: PasswordState =
            serde_json::from_str(&text).map_err(|e| PasswordError::Corrupt {
                path: self.path.clone(),
                message: e.to_string(),
            })?;
        if let Some(h) = &state.hash
            && PasswordHash::new(h).is_err()
        {
            return Err(PasswordError::Corrupt {
                path: self.path.clone(),
                message: "the hash is not a PHC string".into(),
            });
        }
        Ok((Some(stamp(&meta)), state))
    }

    /// The current state. It reads the file again only when the file
    /// changed (another inode, size, or mtime), so a check per request
    /// costs one `stat`. A file that cannot be read or parsed is an error:
    /// the callers then refuse every token and every login.
    pub fn current(&self) -> Result<PasswordState, PasswordError> {
        let now = match std::fs::metadata(&self.path) {
            Ok(m) => Some(stamp(&m)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(self.io(e)),
        };
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((s, state)) = cache.as_ref()
            && *s == now
        {
            return Ok(state.clone());
        }
        let (s, state) = self.read_file()?;
        *cache = Some((s, state.clone()));
        Ok(state)
    }

    /// Run `f` (a read-modify-write of the file) under the write lock: a
    /// mutex in this process and a `flock` on [`LOCK_FILE`] across
    /// processes (the daemon and `terrariumd password set`).
    fn locked<T>(&self, f: impl FnOnce() -> Result<T, PasswordError>) -> Result<T, PasswordError> {
        use nix::fcntl::{Flock, FlockArg};
        use std::os::unix::fs::OpenOptionsExt;
        let _w = self
            .write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = self.dir();
        std::fs::create_dir_all(dir).map_err(|e| self.io(e))?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(LOCK_FILE))
            .map_err(|e| self.io(e))?;
        let _lock = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, e)| self.io(std::io::Error::from(e)))?;
        f()
    }

    fn dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Write `state`: a unique temp file (mode 0600) in the same dir,
    /// fsync, a rename over the file, and an fsync of the dir, so that a
    /// reader never sees half a file and the change survives a crash. Call
    /// it under [`Self::locked`].
    fn write_file(&self, state: &PasswordState) -> Result<(), PasswordError> {
        use std::io::Write;
        let dir = self.dir();
        let body = serde_json::to_vec(state).map_err(|e| PasswordError::Hash(e.to_string()))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".operator.password.")
            .tempfile_in(dir)
            .map_err(|e| self.io(e))?;
        tmp.write_all(&body).map_err(|e| self.io(e))?;
        tmp.as_file().sync_all().map_err(|e| self.io(e))?;
        tmp.persist(&self.path).map_err(|e| self.io(e.error))?;
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| self.io(e))?;
        *self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }

    /// Set a new password. The version goes up by one, so every session
    /// ends. Answers the new version. Slow (Argon2): call it from a
    /// blocking task.
    pub fn set(&self, password: &str) -> Result<u64, PasswordError> {
        check_new(password)?;
        let phc = hash(password)?;
        self.locked(|| {
            let (_, old) = self.read_file()?;
            let state = PasswordState {
                hash: Some(phc),
                version: old.version.saturating_add(1),
            };
            self.write_file(&state)?;
            Ok(state.version)
        })
    }

    /// Set the password only when none is set (`TERRARIUM_PASSWORD` at the
    /// start). Answers `true` when it set it. The check and the write are
    /// one step under the lock.
    pub fn set_if_none(&self, password: &str) -> Result<bool, PasswordError> {
        if self.current()?.is_set() {
            return Ok(false);
        }
        check_new(password)?;
        let phc = hash(password)?;
        self.locked(|| {
            let (_, old) = self.read_file()?;
            if old.is_set() {
                return Ok(false);
            }
            self.write_file(&PasswordState {
                hash: Some(phc),
                version: old.version.saturating_add(1),
            })?;
            Ok(true)
        })
    }

    /// End every session: the version goes up by one, the hash stays.
    /// Answers the new version.
    pub fn bump(&self) -> Result<u64, PasswordError> {
        self.locked(|| {
            let (_, mut state) = self.read_file()?;
            state.version = state.version.saturating_add(1);
            self.write_file(&state)?;
            Ok(state.version)
        })
    }
}

/// The first password from `TERRARIUM_PASSWORD` (`password`) or
/// `TERRARIUM_PASSWORD_FILE` (the first line of `file`), at the start.
/// `None`: neither is set. `Some(true)`: it set the password.
/// `Some(false)`: a password exists already, so it did nothing.
pub fn initial_password(
    state_dir: &Path,
    password: Option<&str>,
    file: Option<&Path>,
) -> anyhow::Result<Option<bool>> {
    use anyhow::Context;
    let value = match (password.filter(|p| !p.is_empty()), file) {
        (Some(p), _) => p.to_owned(),
        (None, Some(file)) => std::fs::read_to_string(file)
            .with_context(|| format!("cannot read {ENV_PASSWORD_FILE} {}", file.display()))?
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        (None, None) => return Ok(None),
    };
    let set = PasswordStore::new(state_dir)
        .set_if_none(&value)
        .with_context(|| format!("cannot set the operator password from {ENV_PASSWORD}"))?;
    Ok(Some(set))
}

// ---- the login guard ----

/// The failed attempts from one address before the backoff starts.
pub const FREE_FAILURES: u32 = 5;
/// The first lockout, after the last free failure. Each more failure
/// doubles it, up to [`MAX_LOCKOUT`].
pub const BASE_LOCKOUT: Duration = Duration::from_secs(30);
/// The longest lockout.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// An address with no failure for this long starts again from zero.
pub const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);
/// The most addresses the guard tracks. Past it, a new address gets no
/// entry of its own (the global budget still counts it).
pub const MAX_TRACKED: usize = 10_000;
/// The most password checks of one address at one time.
pub const MAX_PENDING_PER_ADDR: u32 = 2;
/// The most password checks in flight (waiting for Argon2, or in it), from
/// every address together. Past it, a login gets `429` at once.
pub const MAX_IN_FLIGHT: usize = 16;
/// The failures from every address together that start a global lockout
/// of the password login (the static token still works).
pub const GLOBAL_FAILURES: u32 = 100;
/// The window of [`GLOBAL_FAILURES`]. A global lockout lasts until the
/// window ends.
pub const GLOBAL_WINDOW: Duration = Duration::from_secs(10 * 60);
/// One `auth.login_failed` audit row per address for each window.
pub const AUDIT_WINDOW: Duration = Duration::from_secs(10 * 60);
/// The wait that a refusal names when the address has checks in flight.
const IN_FLIGHT_WAIT: Duration = Duration::from_secs(1);
/// The shortest time between two prunes of a full map.
const PRUNE_EVERY: Duration = Duration::from_secs(60);

/// The lockout after `count` failures: none up to [`FREE_FAILURES`] - 1,
/// then [`BASE_LOCKOUT`] doubled for each failure past it, at most
/// [`MAX_LOCKOUT`].
pub fn lockout(count: u32) -> Option<Duration> {
    let past = count.checked_sub(FREE_FAILURES)?;
    let factor = 1u32.checked_shl(past.min(16)).unwrap_or(u32::MAX);
    Some(
        BASE_LOCKOUT
            .checked_mul(factor)
            .unwrap_or(MAX_LOCKOUT)
            .min(MAX_LOCKOUT),
    )
}

/// The limiter key of a client address: an IPv4 address as it is, an IPv6
/// address as its /64 (one host usually has a whole /64), an IPv4-mapped
/// IPv6 address as the IPv4 address. Anything else as it is.
pub fn addr_key(ip: &str) -> String {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                let net = std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0);
                format!("{net}/64")
            }
        },
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Err(_) => ip.to_owned(),
    }
}

/// Why the guard refuses a password check now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The address (or, with `global`, every address) is locked out, or
    /// has the most checks in flight. Try again after the wait.
    Locked { wait: Duration, global: bool },
    /// Too many checks in flight from every address together.
    Busy { wait: Duration },
}

impl Refusal {
    pub fn wait(self) -> Duration {
        match self {
            Self::Locked { wait, .. } | Self::Busy { wait } => wait,
        }
    }
}

/// One `auth.login_failed` audit row: one per address per [`AUDIT_WINDOW`].
#[derive(Debug, Clone)]
struct AuditRow {
    id: String,
    started: Instant,
    count: u32,
}

#[derive(Debug, Clone)]
struct Entry {
    failures: u32,
    pending: u32,
    last: Instant,
    locked_until: Option<Instant>,
    audit: Option<AuditRow>,
}

#[derive(Debug, Default)]
struct GuardState {
    map: HashMap<String, Entry>,
    in_flight: usize,
    global_failures: u32,
    global_window: Option<Instant>,
    global_locked_until: Option<Instant>,
    last_prune: Option<Instant>,
}

/// What one failure did, for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailReport {
    /// The limiter key ([`addr_key`]).
    pub key: String,
    /// The `auth.login_failed` row of this window.
    pub audit_id: String,
    /// `true`: a new row; `false`: update the row's count.
    pub new_row: bool,
    /// The failures of this address in this window.
    pub count: u32,
    /// The lockout of the address that starts now, if any.
    pub lockout: Option<Duration>,
    /// The global lockout that starts now, if any.
    pub global_lockout: Option<Duration>,
}

/// The failed password checks per client address (and of every address
/// together), in memory. A restart clears it. The clock is a parameter, so
/// tests move it by hand.
///
/// A check takes a [`Ticket`] **before** the Argon2 check, so a burst of
/// parallel requests cannot pass the limit: the checks in flight count
/// against the budget of the address, and against [`MAX_IN_FLIGHT`].
#[derive(Debug, Default)]
pub struct LoginLimiter {
    state: Mutex<GuardState>,
}

/// One password check in flight. Drop it with no verdict (an error, a
/// refusal after the wait) and it counts as nothing.
#[derive(Debug)]
pub struct Ticket<'a> {
    limiter: &'a LoginLimiter,
    key: String,
    tracked: bool,
    done: bool,
}

impl LoginLimiter {
    fn state(&self) -> std::sync::MutexGuard<'_, GuardState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn global_lock(st: &GuardState, now: Instant) -> Option<Refusal> {
        match st.global_locked_until {
            Some(until) if until > now => Some(Refusal::Locked {
                wait: until - now,
                global: true,
            }),
            _ => None,
        }
    }

    fn entry_lock(e: &Entry, now: Instant) -> Option<Refusal> {
        match e.locked_until {
            Some(until) if until > now => Some(Refusal::Locked {
                wait: until - now,
                global: false,
            }),
            _ => None,
        }
    }

    /// Prune a full map, at most once each [`PRUNE_EVERY`]: keep the
    /// entries that have checks in flight, a lockout, or a recent failure.
    fn prune(st: &mut GuardState, now: Instant) {
        if st.map.len() < MAX_TRACKED
            || st
                .last_prune
                .is_some_and(|t| now.saturating_duration_since(t) < PRUNE_EVERY)
        {
            return;
        }
        st.last_prune = Some(now);
        st.map.retain(|_, e| {
            e.pending > 0
                || e.locked_until.is_some_and(|u| u > now)
                || now.saturating_duration_since(e.last) < FORGET_AFTER
        });
    }

    /// Take a ticket for one password check from `ip` at `now`, or refuse.
    pub fn begin(&self, ip: &str, now: Instant) -> Result<Ticket<'_>, Refusal> {
        let mut st = self.state();
        if let Some(r) = Self::global_lock(&st, now) {
            return Err(r);
        }
        if st.in_flight >= MAX_IN_FLIGHT {
            return Err(Refusal::Busy {
                wait: IN_FLIGHT_WAIT,
            });
        }
        Self::prune(&mut st, now);
        let key = addr_key(ip);
        let full = st.map.len() >= MAX_TRACKED;
        let tracked = match st.map.get_mut(&key) {
            Some(e) => {
                if e.pending == 0
                    && !e.locked_until.is_some_and(|u| u > now)
                    && now.saturating_duration_since(e.last) >= FORGET_AFTER
                {
                    e.failures = 0;
                }
                if let Some(r) = Self::entry_lock(e, now) {
                    return Err(r);
                }
                // The checks in flight use the budget: the free failures
                // left, or one check after a lockout ended.
                let budget = FREE_FAILURES.saturating_sub(e.failures).max(1);
                if e.pending >= budget.min(MAX_PENDING_PER_ADDR) {
                    return Err(Refusal::Locked {
                        wait: IN_FLIGHT_WAIT,
                        global: false,
                    });
                }
                e.pending += 1;
                true
            }
            None if !full => {
                st.map.insert(
                    key.clone(),
                    Entry {
                        failures: 0,
                        pending: 1,
                        last: now,
                        locked_until: None,
                        audit: None,
                    },
                );
                true
            }
            // Full: no entry of its own. The global budget still counts it.
            None => false,
        };
        st.in_flight += 1;
        Ok(Ticket {
            limiter: self,
            key,
            tracked,
            done: false,
        })
    }

    fn release(st: &mut GuardState, t: &Ticket<'_>) {
        st.in_flight = st.in_flight.saturating_sub(1);
        if t.tracked
            && let Some(e) = st.map.get_mut(&t.key)
        {
            e.pending = e.pending.saturating_sub(1);
        }
    }

    /// The checks in flight now (tests).
    pub fn in_flight(&self) -> usize {
        self.state().in_flight
    }
}

impl Ticket<'_> {
    /// The limiter key of this check.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// After the wait for the Argon2 gate: is the check still allowed? A
    /// lockout may have started while it waited.
    pub fn recheck(&self, now: Instant) -> Result<(), Refusal> {
        let st = self.limiter.state();
        if let Some(r) = LoginLimiter::global_lock(&st, now) {
            return Err(r);
        }
        match st
            .map
            .get(&self.key)
            .and_then(|e| LoginLimiter::entry_lock(e, now))
        {
            Some(r) => Err(r),
            None => Ok(()),
        }
    }

    /// A good password: the address starts again from zero.
    pub fn succeed(mut self) {
        let mut st = self.limiter.state();
        LoginLimiter::release(&mut st, &self);
        if self.tracked
            && let Some(e) = st.map.get(&self.key)
            && e.pending == 0
        {
            st.map.remove(&self.key);
        } else if let Some(e) = st.map.get_mut(&self.key) {
            e.failures = 0;
            e.locked_until = None;
        }
        self.done = true;
    }

    /// A wrong password at `now`: count it for the address and for every
    /// address, and start a lockout when the budget is gone.
    pub fn fail(mut self, now: Instant) -> FailReport {
        let mut st = self.limiter.state();
        LoginLimiter::release(&mut st, &self);
        self.done = true;
        // The global budget.
        if st
            .global_window
            .is_none_or(|w| now.saturating_duration_since(w) >= GLOBAL_WINDOW)
        {
            st.global_window = Some(now);
            st.global_failures = 0;
        }
        st.global_failures = st.global_failures.saturating_add(1);
        let mut global_lockout = None;
        if st.global_failures >= GLOBAL_FAILURES
            && LoginLimiter::global_lock(&st, now).is_none()
            && let Some(w) = st.global_window
        {
            let until = (w + GLOBAL_WINDOW).max(now + BASE_LOCKOUT);
            st.global_locked_until = Some(until);
            global_lockout = Some(until - now);
        }
        let mut report = FailReport {
            key: self.key.clone(),
            audit_id: ulid::Ulid::new().to_string(),
            new_row: true,
            count: 1,
            lockout: None,
            global_lockout,
        };
        if self.tracked
            && let Some(e) = st.map.get_mut(&self.key)
        {
            e.failures = e.failures.saturating_add(1);
            e.last = now;
            if let Some(d) = lockout(e.failures) {
                e.locked_until = Some(now + d);
                report.lockout = Some(d);
            }
            match &mut e.audit {
                Some(a) if now.saturating_duration_since(a.started) < AUDIT_WINDOW => {
                    a.count = a.count.saturating_add(1);
                    report.audit_id = a.id.clone();
                    report.new_row = false;
                    report.count = a.count;
                }
                _ => {
                    e.audit = Some(AuditRow {
                        id: report.audit_id.clone(),
                        started: now,
                        count: 1,
                    });
                }
            }
        }
        report
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.done {
            let mut st = self.limiter.state();
            LoginLimiter::release(&mut st, self);
        }
    }
}

/// The verdict of [`OperatorAuth::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The password matches the hash of this password version.
    Good { version: u64 },
    /// A wrong password, counted.
    Wrong(FailReport),
    /// No password is set. Not counted.
    NoPassword,
    /// The guard refused the check; no Argon2 ran.
    Refused(Refusal),
}

/// The operator auth of the daemon: the password store, the login guard,
/// and a gate that lets few Argon2 checks run at one time.
#[derive(Debug)]
pub struct OperatorAuth {
    pub store: PasswordStore,
    pub limiter: LoginLimiter,
    gate: tokio::sync::Semaphore,
    /// The Argon2 checks run so far (a test seam).
    verifications: std::sync::atomic::AtomicUsize,
}

/// The most Argon2 checks at one time. Each one takes about 19 MiB.
const PARALLEL_CHECKS: usize = 2;

impl OperatorAuth {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            store: PasswordStore::new(state_dir),
            limiter: LoginLimiter::default(),
            gate: tokio::sync::Semaphore::new(PARALLEL_CHECKS),
            verifications: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The Argon2 checks run so far.
    pub fn verifications(&self) -> usize {
        self.verifications
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check `password` from the client at `ip`. The guard takes a ticket
    /// first (or refuses at once), then the check waits for the Argon2
    /// gate, checks the guard again, reads the current hash, and runs
    /// Argon2 off the async threads. A good password answers the password
    /// version it matched, so the caller mints with that version.
    pub async fn check(&self, ip: &str, password: String) -> Result<Verdict, PasswordError> {
        let ticket = match self.limiter.begin(ip, Instant::now()) {
            Ok(t) => t,
            Err(r) => return Ok(Verdict::Refused(r)),
        };
        if !self.store.current()?.is_set() {
            return Ok(Verdict::NoPassword);
        }
        let _permit = self.gate.acquire().await.ok();
        if let Err(r) = ticket.recheck(Instant::now()) {
            return Ok(Verdict::Refused(r));
        }
        let state = self.store.current()?;
        if !state.is_set() {
            return Ok(Verdict::NoPassword);
        }
        self.verifications
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let version = state.version;
        let ok = tokio::task::spawn_blocking(move || state.verify(&password))
            .await
            .unwrap_or(false);
        Ok(if ok {
            ticket.succeed();
            Verdict::Good { version }
        } else {
            Verdict::Wrong(ticket.fail(Instant::now()))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn argon2_hash_and_verify() {
        let phc = hash("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$v=19$"), "{phc}");
        assert!(verify(&phc, "correct horse battery"));
        assert!(!verify(&phc, "correct horse batterY"));
        assert!(!verify("not a phc", "x"));
        // A new salt each time.
        assert_ne!(phc, hash("correct horse battery").unwrap());
    }

    #[test]
    fn new_password_rules() {
        assert!(matches!(check_new("short"), Err(PasswordError::TooShort)));
        assert!(matches!(
            check_new("12345678901"),
            Err(PasswordError::TooShort)
        ));
        assert!(check_new("123456789012").is_ok());
        // Characters, not bytes.
        assert!(check_new("ééééééééééé").is_err());
        assert!(check_new("éééééééééééé").is_ok());
        assert!(matches!(
            check_new(&"x".repeat(MAX_LEN + 1)),
            Err(PasswordError::TooLong)
        ));
    }

    #[test]
    fn store_set_bump_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = PasswordStore::new(dir.path());
        let none = s.current().unwrap();
        assert!(!none.is_set());
        assert_eq!(none.version, 0);
        assert!(s.set("short").is_err());
        assert_eq!(s.set("a long password").unwrap(), 1);
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let text = std::fs::read_to_string(s.path()).unwrap();
        assert!(!text.contains("a long password"));
        let cur = s.current().unwrap();
        assert!(cur.verify("a long password"));
        assert!(!cur.verify("another password"));
        assert_eq!(s.bump().unwrap(), 2);
        assert!(s.current().unwrap().verify("a long password"));
        assert!(!s.set_if_none("other password!").unwrap());
        assert_eq!(s.set("other password!").unwrap(), 3);
    }

    /// Another process (`terrariumd password set`) writes the file: the
    /// store sees it at the next read.
    #[test]
    fn store_follows_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = PasswordStore::new(dir.path());
        assert_eq!(daemon.current().unwrap().version, 0);
        let cli = PasswordStore::new(dir.path());
        cli.set("a long password").unwrap();
        let cur = daemon.current().unwrap();
        assert!(cur.is_set());
        assert_eq!(cur.version, 1);
        cli.bump().unwrap();
        assert_eq!(daemon.current().unwrap().version, 2);
        // A broken file fails closed.
        std::fs::write(daemon.path(), "{nope").unwrap();
        assert!(daemon.current().is_err());
        std::fs::write(daemon.path(), r#"{"hash":"plain","version":1}"#).unwrap();
        assert!(daemon.current().is_err());
    }

    /// A file that others may read goes back to 0600 at the next read.
    #[test]
    fn loose_mode_is_fixed_on_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = PasswordStore::new(dir.path());
        s.set("a long password").unwrap();
        std::fs::set_permissions(s.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        // Another inode stamp is not needed: a fresh store reads the file.
        let fresh = PasswordStore::new(dir.path());
        assert!(fresh.current().unwrap().is_set());
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn owner_check() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_owner(dir.path()).is_ok());
        assert!(check_owner(&dir.path().join("missing")).is_ok());
        // `/` belongs to root; the tests do not run as root.
        if nix::unistd::geteuid().as_raw() != 0 {
            let err = check_owner(Path::new("/")).unwrap_err().to_string();
            assert!(err.contains("docker compose exec terrarium"), "{err}");
        }
    }

    /// Two stores (two processes, as far as the lock goes) bump at the same
    /// time: no version is lost, and no temp file stays.
    #[test]
    fn concurrent_writes_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_owned();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || {
                    let s = PasswordStore::new(&p);
                    for _ in 0..10 {
                        s.bump().unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(PasswordStore::new(&p).current().unwrap().version, 80);
        let names: Vec<String> = std::fs::read_dir(&p)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| n == PASSWORD_FILE || n == LOCK_FILE),
            "{names:?}"
        );
    }

    #[test]
    fn set_if_none_sets_the_first_password_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = PasswordStore::new(dir.path());
        assert!(s.set_if_none("first password").unwrap());
        assert!(!s.set_if_none("second password").unwrap());
        assert!(s.current().unwrap().verify("first password"));
    }

    #[test]
    fn initial_password_from_env_or_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(initial_password(dir.path(), None, None).unwrap(), None);
        assert_eq!(initial_password(dir.path(), Some(""), None).unwrap(), None);
        // Too short: an error, nothing written.
        assert!(initial_password(dir.path(), Some("short"), None).is_err());
        assert!(!dir.path().join(PASSWORD_FILE).exists());
        let file = dir.path().join("pw");
        std::fs::write(&file, "from the file 1\nignored\n").unwrap();
        assert_eq!(
            initial_password(dir.path(), None, Some(&file)).unwrap(),
            Some(true)
        );
        // A password exists: the env is ignored.
        assert_eq!(
            initial_password(dir.path(), Some("from the env 12"), None).unwrap(),
            Some(false)
        );
        let s = PasswordStore::new(dir.path()).current().unwrap();
        assert!(s.verify("from the file 1"));
        assert_eq!(s.version, 1);
    }

    #[test]
    fn lockout_schedule() {
        assert_eq!(lockout(4), None);
        assert_eq!(lockout(5), Some(Duration::from_secs(30)));
        assert_eq!(lockout(6), Some(Duration::from_secs(60)));
        assert_eq!(lockout(9), Some(Duration::from_secs(480)));
        assert_eq!(lockout(10), Some(MAX_LOCKOUT));
        assert_eq!(lockout(u32::MAX), Some(MAX_LOCKOUT));
    }

    #[test]
    fn ipv6_keys_by_64() {
        assert_eq!(addr_key("10.0.0.1"), "10.0.0.1");
        assert_eq!(addr_key("2001:db8:1:2:aaaa::1"), "2001:db8:1:2::/64");
        assert_eq!(
            addr_key("2001:db8:1:2:ffff:1:2:3"),
            addr_key("2001:db8:1:2::9")
        );
        assert_ne!(addr_key("2001:db8:1:2::1"), addr_key("2001:db8:1:3::1"));
        assert_eq!(addr_key("::ffff:10.0.0.1"), "10.0.0.1");
        assert_eq!(addr_key("unknown"), "unknown");
    }

    /// Fail `n` checks of `ip` at `now`, one after another.
    fn fail_n(l: &LoginLimiter, ip: &str, n: u32, now: Instant) -> Option<FailReport> {
        let mut last = None;
        for _ in 0..n {
            last = Some(l.begin(ip, now).unwrap().fail(now));
        }
        last
    }

    #[test]
    fn limiter_backs_off_per_address() {
        let l = LoginLimiter::default();
        let t0 = Instant::now();
        let r = fail_n(&l, "a", FREE_FAILURES - 1, t0).unwrap();
        assert_eq!(r.lockout, None);
        let r = l.begin("a", t0).unwrap().fail(t0);
        assert_eq!(r.lockout, Some(BASE_LOCKOUT));
        assert_eq!(
            l.begin("a", t0).unwrap_err(),
            Refusal::Locked {
                wait: BASE_LOCKOUT,
                global: false
            }
        );
        // Another address is free.
        assert!(l.begin("b", t0).is_ok());
        // After the lockout, one more failure doubles it.
        let t1 = t0 + BASE_LOCKOUT;
        assert_eq!(
            l.begin("a", t1).unwrap().fail(t1).lockout,
            Some(BASE_LOCKOUT * 2)
        );
        // Success forgets.
        let t2 = t1 + BASE_LOCKOUT * 2;
        l.begin("a", t2).unwrap().succeed();
        assert!(!l.state().map.contains_key("a"));
        // An hour with no failure starts again from zero.
        fail_n(&l, "c", FREE_FAILURES - 1, t0);
        let late = t0 + FORGET_AFTER;
        assert_eq!(l.begin("c", late).unwrap().fail(late).lockout, None);
        // Every ticket went back.
        assert_eq!(l.in_flight(), 0);
    }

    /// Checks in flight use the budget: at most [`MAX_PENDING_PER_ADDR`]
    /// at one time, and at most one after a lockout ends.
    #[test]
    fn tickets_in_flight_count() {
        let l = LoginLimiter::default();
        let t0 = Instant::now();
        let a = l.begin("x", t0).unwrap();
        let b = l.begin("x", t0).unwrap();
        assert!(matches!(
            l.begin("x", t0),
            Err(Refusal::Locked { global: false, .. })
        ));
        drop(a);
        let c = l.begin("x", t0).unwrap();
        drop((b, c));
        // Four failures: one free check left, so one ticket at a time.
        fail_n(&l, "x", FREE_FAILURES - 1, t0);
        let d = l.begin("x", t0).unwrap();
        assert!(l.begin("x", t0).is_err());
        assert!(d.fail(t0).lockout.is_some());
        let after = t0 + BASE_LOCKOUT;
        let e = l.begin("x", after).unwrap();
        assert!(l.begin("x", after).is_err(), "one check after a lockout");
        drop(e);
        // The global in-flight cap.
        let tickets: Vec<_> = (0..MAX_IN_FLIGHT)
            .map(|i| l.begin(&format!("10.1.0.{i}"), t0).unwrap())
            .collect();
        assert!(matches!(l.begin("10.2.0.1", t0), Err(Refusal::Busy { .. })));
        drop(tickets);
        assert!(l.begin("10.2.0.1", t0).is_ok());
    }

    #[test]
    fn global_budget_locks_password_logins() {
        let l = LoginLimiter::default();
        let t0 = Instant::now();
        let mut lock = None;
        for i in 0..GLOBAL_FAILURES {
            lock = l
                .begin(&format!("10.3.{}.{}", i / 256, i % 256), t0)
                .unwrap()
                .fail(t0)
                .global_lockout;
        }
        assert_eq!(lock, Some(GLOBAL_WINDOW));
        assert!(matches!(
            l.begin("10.9.9.9", t0),
            Err(Refusal::Locked { global: true, .. })
        ));
        assert!(l.begin("10.9.9.9", t0 + GLOBAL_WINDOW).is_ok());
    }

    /// One audit row per address per window, with a count.
    #[test]
    fn audit_rows_aggregate() {
        let l = LoginLimiter::default();
        let t0 = Instant::now();
        let a = l.begin("a", t0).unwrap().fail(t0);
        assert!(a.new_row);
        let b = l.begin("a", t0).unwrap().fail(t0);
        assert!(!b.new_row);
        assert_eq!((b.audit_id.as_str(), b.count), (a.audit_id.as_str(), 2));
        l.state().map.get_mut("a").unwrap().locked_until = None;
        let later = t0 + AUDIT_WINDOW;
        let c = l.begin("a", later).unwrap().fail(later);
        assert!(c.new_row);
        assert_ne!(c.audit_id, a.audit_id);
    }

    /// A full map: a new address gets no entry, and no lockout of another
    /// address reaches it. A prune runs at most once a minute.
    #[test]
    fn limiter_is_bounded() {
        let l = LoginLimiter::default();
        let t0 = Instant::now();
        {
            let mut st = l.state();
            for i in 0..MAX_TRACKED {
                st.map.insert(
                    format!("10.0.{}.{}", i / 256, i % 256),
                    Entry {
                        failures: 9,
                        pending: 0,
                        last: t0,
                        locked_until: Some(t0 + MAX_LOCKOUT),
                        audit: None,
                    },
                );
            }
        }
        // Untracked addresses: never locked by another's failures.
        for _ in 0..FREE_FAILURES * 2 {
            l.begin("new-1", t0).unwrap().fail(t0);
        }
        assert!(l.begin("new-2", t0).is_ok());
        assert!(l.begin("new-1", t0).is_ok());
        assert!(l.state().map.len() <= MAX_TRACKED);
        assert_eq!(l.state().last_prune, Some(t0));
        // Past the lockouts and the forget time, a prune makes room.
        let late = t0 + FORGET_AFTER + PRUNE_EVERY;
        assert!(l.begin("new-3", late).is_ok());
        assert!(l.state().map.len() < 10);
    }
}
