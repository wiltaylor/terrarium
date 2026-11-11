//! Link keys at run time (TER-61, wiki design/linked-instances D2, D3, D5;
//! `docs/api.md` §36).
//!
//! - [`LinkCaller`]: the link of a request that a `tlk_` key let in. The
//!   TCP auth sets it as a request extension and as the task-local
//!   [`CALLER`] for the whole handler, so [`crate::actor::Actor::label`]
//!   gives `link:<id>` in audit rows and events. For the rules the caller
//!   is the operator ([`crate::actor::Actor::Operator`]), minus the deny
//!   list of [`denied`].
//! - [`Runtime`]: the revocation signal of each key (a revoke or rotate
//!   closes the open streams and long polls of the key), the throttle of
//!   the use record and of the `link.forbidden` audit rows, and the
//!   instance ID of this daemon.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::http::Method;
use serde::Serialize;
use tokio::sync::watch;

use crate::error::{ApiError, ApiResult};

/// The file of the instance ID in the state dir.
pub const INSTANCE_FILE: &str = "instance.id";
/// How often a key's use record is written at most.
const MARK_EVERY: Duration = Duration::from_secs(60);
/// How often one `link.forbidden` row per link, method, and path is
/// written at most.
const FORBIDDEN_EVERY: Duration = Duration::from_secs(60);
/// The most entries in each throttle map; past it the map starts over.
const MAX_THROTTLE: usize = 4096;

/// The link of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCaller {
    pub id: String,
    pub label: String,
    pub read_only: bool,
    /// The SHA-256 of the key, the name of its revocation signal.
    pub key_hash: String,
}

tokio::task_local! {
    /// The link of the request that the current task serves.
    pub static CALLER: LinkCaller;
}

/// `link:<id>` when the current task serves a link request.
pub fn current_label() -> Option<String> {
    CALLER.try_with(|c| format!("link:{}", c.id)).ok()
}

/// The link ID when the current task serves a link request.
pub fn current_link_id() -> Option<String> {
    CALLER.try_with(|c| c.id.clone()).ok()
}

/// `tokio::spawn` that keeps the link of the current request in the new
/// task (TER-61 review round 5, F9), so its writes keep the link's limits.
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match CALLER.try_with(Clone::clone) {
        Ok(c) => tokio::spawn(CALLER.scope(c, fut)),
        Err(_) => tokio::spawn(fut),
    }
}

/// Whether the current task serves a link request.
pub fn is_link_call() -> bool {
    CALLER.try_with(|_| ()).is_ok()
}

/// `403 link_forbidden` for a link request, else `Ok`. `what` says what
/// the link may not do.
pub fn refuse_for_link(what: &str) -> ApiResult<()> {
    if is_link_call() {
        Err(ApiError::new(
            crate::error::Code::LinkForbidden,
            format!("a link key may not {what} (docs/api.md §36.4)"),
        ))
    } else {
        Ok(())
    }
}

/// A link may not drive an agent that holds `sysadmin` (TER-61 review,
/// O2): that agent calls the API with its own rights, outside the deny
/// list, and `sysadmin` may push the config repo and run the self-update.
/// `what` is the action, for example "send a turn to".
pub fn refuse_sysadmin_agent(roles: &[terrarium_core::Role], what: &str) -> ApiResult<()> {
    if roles.contains(&terrarium_core::Role::Sysadmin) {
        refuse_for_link(&format!("{what} an agent that holds sysadmin"))
    } else {
        Ok(())
    }
}

/// Why the deny list refuses a link request, or `None` when it may pass.
/// `path` is the request path (with `/api`), `websocket` an upgrade.
///
/// The path is compared after percent-decoding, with empty and `.`
/// segments dropped, in lower case, so that no spelling of a denied route
/// passes (the router itself matches only the plain spelling).
pub fn denied(
    method: &Method,
    path: &str,
    websocket: bool,
    read_only: bool,
) -> Option<&'static str> {
    let decoded = crate::api::percent_decode(path).to_ascii_lowercase();
    let segs: Vec<&str> = decoded
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if segs.contains(&"..") {
        return Some("a path with `..`");
    }
    let rest: &[&str] = match segs.split_first() {
        Some((&"api", rest)) => rest,
        _ => return Some("only /api is open to a link"),
    };
    let get = *method == Method::GET || *method == Method::HEAD;
    if read_only {
        if !get {
            return Some("a read-only link may only read");
        }
        if websocket && rest.last() != Some(&"events") {
            return Some("a read-only link opens only the event WebSocket");
        }
    }
    match rest {
        // A link cannot make another link; it may only end itself.
        ["links", "self"] if *method == Method::DELETE => None,
        ["links", ..] => Some("the links of this daemon"),
        // The credentials of this daemon.
        ["session"] if !get => Some("the login sessions of this daemon"),
        ["sessions"] if *method == Method::DELETE => Some("the login sessions of this daemon"),
        ["auth", ..] => Some("the operator password"),
        ["secrets", _, _, "reveal"] => Some("revealing a secret"),
        ["agent", "secrets", ..] => Some("revealing a secret"),
        ["credentials", ..] if !get => Some("the credentials of this daemon"),
        ["hosts"] if *method == Method::POST => Some("host agent tokens"),
        ["hosts", _] if *method == Method::DELETE => Some("host agent tokens"),
        ["hosts", _, "rotate"] => Some("host agent tokens"),
        ["subscriptions", "login", ..] => Some("the subscription logins"),
        ["subscriptions", _, "api-key"] => Some("the subscription API keys"),
        // A backup holds secrets.key, session.key, and every sealed secret.
        ["backups"] if !get => Some("the backups"),
        ["backups", _, ..] => Some("the backups"),
        // The daemon itself.
        ["system", "update", ..] => Some("the update of the daemon"),
        // The config repo push (design Q3), and its remote.
        ["config", "push" | "sync"] => Some("the config repo push"),
        ["config", "remote"] if !get => Some("the config repo push"),
        // Executable automation (TER-61 review round 3, F6): a schedule or a
        // board rule runs later as `system`, its owner, or the project
        // inbox, without the link's provenance, so its turn and mail actions
        // would reach a sysadmin agent around the checks above. Reads, a
        // delete, and a disable stay open.
        ["schedules"] if *method == Method::POST => Some("making a schedule"),
        ["schedules", _] if *method == Method::PATCH => Some("changing a schedule"),
        ["schedules", _, "run" | "enable" | "test"] => Some("running or enabling a schedule"),
        ["schedules", _, "deliveries", _, "redeliver"] => Some("running a schedule"),
        ["projects", _, "rules"] if !get => Some("writing the board rules"),
        ["projects", _, "rules", "migrate" | "enable" | "disable"] => {
            Some("writing the board rules")
        }
        ["projects", _, "rules", _, "enable" | "disable"] => Some("writing the board rules"),
        // The shared text that agents load at start (TER-61 review round 4,
        // F8): the system prompt library (also the personas), the skill
        // library, and the agent templates. A sysadmin agent with no own
        // prompt loads the library entry `leader`, so a write there would
        // become its system prompt. Reads stay open.
        ["system-prompts", ..] if !get => Some("writing the system prompt library"),
        ["skills", ..] if !get => Some("writing the skill library"),
        ["templates", ..] if !get => Some("writing the agent templates"),
        ["config", "defaults", "accept" | "keep"] => Some("writing the system prompt library"),
        _ => None,
    }
}

/// The revocation signal of one key: `true` once the key is revoked.
struct Signal {
    tx: watch::Sender<bool>,
}

/// A request's hold on the revocation signal of its key. Dropping the last
/// guard of a key frees its signal.
#[derive(Clone)]
pub struct LinkGuard {
    inner: Arc<GuardInner>,
}

struct GuardInner {
    rx: watch::Receiver<bool>,
    hash: String,
    runtime: Arc<Runtime>,
}

impl Drop for GuardInner {
    fn drop(&mut self) {
        let mut map = self.runtime.lock_signals();
        // This receiver is still alive here: 1 means it is the last one.
        if map
            .get(&self.hash)
            .is_some_and(|s| s.tx.receiver_count() <= 1)
        {
            map.remove(&self.hash);
        }
    }
}

impl LinkGuard {
    /// Ends when the key is revoked or rotated.
    pub async fn revoked(&self) {
        let mut rx = self.inner.rx.clone();
        // A closed channel only follows a revoke (see `Runtime::revoke`).
        let _ = rx.wait_for(|r| *r).await;
    }

    pub fn is_revoked(&self) -> bool {
        *self.inner.rx.borrow()
    }
}

/// Run `fut` until it ends or the link of `guard` is revoked. A WebSocket
/// handler wraps its socket task in it, so a revoke drops the socket.
pub async fn until_revoked<F: std::future::Future<Output = ()>>(guard: Option<LinkGuard>, fut: F) {
    match guard {
        None => fut.await,
        Some(g) => {
            tokio::select! {
                () = fut => {}
                () = g.revoked() => {}
            }
        }
    }
}

/// This daemon's identity for its links: `GET /api/system/instance`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Instance {
    /// A ULID, made at the first start, in `<state>/instance.id`.
    pub id: String,
    /// The host of `TERRARIUM_PUBLIC_URL`, else the host name.
    pub name: String,
    pub version: String,
}

/// The run-time state of the links.
#[derive(Default)]
pub struct Runtime {
    signals: Mutex<HashMap<String, Signal>>,
    used: Mutex<HashMap<String, (Instant, String)>>,
    forbidden: Mutex<HashMap<String, Instant>>,
    instance_id: OnceLock<String>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Runtime {
    fn lock_signals(&self) -> std::sync::MutexGuard<'_, HashMap<String, Signal>> {
        lock(&self.signals)
    }

    /// Hold the revocation signal of the key with this hash. Take it
    /// before the key is looked up, so a revoke that commits after the
    /// lookup still reaches the request.
    pub fn guard(self: &Arc<Self>, hash: &str) -> LinkGuard {
        let rx = self
            .lock_signals()
            .entry(hash.to_owned())
            .or_insert_with(|| Signal {
                tx: watch::channel(false).0,
            })
            .tx
            .subscribe();
        LinkGuard {
            inner: Arc::new(GuardInner {
                rx,
                hash: hash.to_owned(),
                runtime: self.clone(),
            }),
        }
    }

    /// Close every open stream and long poll of the key with this hash.
    /// Call it after the revoke has committed.
    pub fn revoke(&self, hash: &str) {
        if let Some(s) = self.lock_signals().remove(hash) {
            s.tx.send_replace(true);
        }
    }

    /// Whether a call should update the use record now: the first call of
    /// a key in this run, a new peer address, or a minute after the last
    /// write.
    pub fn should_mark(&self, id: &str, from: &str) -> bool {
        let mut m = lock(&self.used);
        let now = Instant::now();
        let due = m
            .get(id)
            .is_none_or(|(at, f)| f != from || now.duration_since(*at) >= MARK_EVERY);
        if due {
            if m.len() >= MAX_THROTTLE {
                m.clear();
            }
            m.insert(id.to_owned(), (now, from.to_owned()));
        }
        due
    }

    /// Whether a deny list hit should get a `link.forbidden` audit row now
    /// (one per link, method, and path a minute).
    pub fn should_audit_forbidden(&self, id: &str, method: &Method, path: &str) -> bool {
        let key = format!("{id} {method} {path}");
        let mut m = lock(&self.forbidden);
        let now = Instant::now();
        let due = m
            .get(&key)
            .is_none_or(|at| now.duration_since(*at) >= FORBIDDEN_EVERY);
        if due {
            if m.len() >= MAX_THROTTLE {
                m.clear();
            }
            m.insert(key, now);
        }
        due
    }

    /// The instance ID: read from `<state>/instance.id`, made there when
    /// absent.
    pub fn instance_id(&self, state_dir: &Path) -> ApiResult<String> {
        if let Some(id) = self.instance_id.get() {
            return Ok(id.clone());
        }
        let id = load_or_create_instance_id(state_dir)?;
        Ok(self.instance_id.get_or_init(|| id).clone())
    }
}

/// Read the instance ID, or make it (a new ULID) when the file is absent.
/// A file that does not hold a ULID is an error, never replaced: the ID
/// must not change under the homes that pinned it.
pub fn load_or_create_instance_id(state_dir: &Path) -> ApiResult<String> {
    let path = state_dir.join(INSTANCE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let id = s.trim();
            id.parse::<ulid::Ulid>().map_err(|_| {
                ApiError::internal(format!("{} does not hold a ULID", path.display()))
            })?;
            Ok(id.to_owned())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = ulid::Ulid::new().to_string();
            let tmp = state_dir.join(format!("{INSTANCE_FILE}.{id}.tmp"));
            std::fs::write(&tmp, format!("{id}\n"))
                .and_then(|()| {
                    // Two starts at once: the first link wins, both read it.
                    match std::fs::hard_link(&tmp, &path) {
                        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
                        _ => Ok(()),
                    }
                })
                .map_err(|e| ApiError::internal(format!("cannot write {}: {e}", path.display())))?;
            let _ = std::fs::remove_file(&tmp);
            let s = std::fs::read_to_string(&path)
                .map_err(|e| ApiError::internal(format!("cannot read {}: {e}", path.display())))?;
            Ok(s.trim().to_owned())
        }
        Err(e) => Err(ApiError::internal(format!(
            "cannot read {}: {e}",
            path.display()
        ))),
    }
}

/// The instance name: the host of the public URL, else the host name.
pub fn instance_name(public_url: Option<&str>) -> String {
    if let Some(host) = public_url
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_owned))
    {
        return host;
    }
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "terrarium".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn deny(m: Method, p: &str) -> bool {
        denied(&m, p, false, false).is_some()
    }

    #[test]
    fn every_route_of_the_deny_list_is_refused() {
        let cases: &[(Method, &str)] = &[
            (Method::POST, "/api/session"),
            (Method::DELETE, "/api/session"),
            (Method::DELETE, "/api/sessions"),
            (Method::PUT, "/api/auth/password"),
            (Method::POST, "/api/secrets/global/gh/reveal"),
            (Method::POST, "/api/agent/secrets/gh/get"),
            (Method::POST, "/api/credentials"),
            (Method::DELETE, "/api/credentials/gh"),
            (Method::POST, "/api/hosts"),
            (Method::DELETE, "/api/hosts/h1"),
            (Method::POST, "/api/hosts/h1/rotate"),
            (Method::POST, "/api/subscriptions/max/api-key"),
            (Method::POST, "/api/subscriptions/login"),
            (Method::GET, "/api/subscriptions/login/L1/pty"),
            (Method::POST, "/api/subscriptions/login/L1/input"),
            (Method::GET, "/api/links"),
            (Method::POST, "/api/links"),
            (Method::POST, "/api/links/pair"),
            (Method::DELETE, "/api/links/L1"),
            (Method::POST, "/api/links/L1/rotate"),
            (Method::POST, "/api/backups"),
            (Method::GET, "/api/backups/b1.tar"),
            (Method::GET, "/api/system/update"),
            (Method::POST, "/api/system/update"),
            (Method::POST, "/api/system/update/rollback"),
            (Method::POST, "/api/system/update/drain"),
            (Method::POST, "/api/system/update/resume"),
            (Method::POST, "/api/config/push"),
            (Method::POST, "/api/config/sync"),
            (Method::PUT, "/api/config/remote"),
            (Method::DELETE, "/api/config/remote"),
            (Method::POST, "/api/schedules"),
            (Method::PATCH, "/api/schedules/S1"),
            (Method::POST, "/api/schedules/S1/run"),
            (Method::POST, "/api/schedules/S1/enable"),
            (Method::POST, "/api/schedules/S1/test"),
            (Method::POST, "/api/schedules/S1/deliveries/D1/redeliver"),
            (Method::PUT, "/api/projects/p/rules"),
            (Method::DELETE, "/api/projects/p/rules"),
            (Method::POST, "/api/projects/p/rules/migrate"),
            (Method::POST, "/api/projects/p/rules/card-flow/enable"),
            (Method::POST, "/api/projects/p/rules/card-flow/disable"),
            (Method::PUT, "/api/system-prompts/leader"),
            (Method::POST, "/api/system-prompts"),
            (Method::DELETE, "/api/system-prompts/terse"),
            (Method::POST, "/api/skills"),
            (Method::PUT, "/api/skills/review"),
            (Method::DELETE, "/api/skills/review"),
            (Method::POST, "/api/templates"),
            (Method::PATCH, "/api/templates/T1"),
            (Method::DELETE, "/api/templates/T1"),
            (Method::POST, "/api/templates/from-agent/A1"),
            (Method::POST, "/api/config/defaults/accept"),
            (Method::POST, "/api/config/defaults/keep"),
        ];
        for (m, p) in cases {
            assert!(deny(m.clone(), p), "{m} {p} passed");
        }
    }

    #[test]
    fn other_operator_routes_pass() {
        let cases: &[(Method, &str)] = &[
            (Method::GET, "/api/session"),
            (Method::GET, "/api/agents"),
            (Method::POST, "/api/agents/a1/turn"),
            (Method::GET, "/api/agents/a1/pty"),
            (Method::POST, "/api/projects/p/cards"),
            (Method::GET, "/api/backups"),
            (Method::GET, "/api/hosts"),
            (Method::GET, "/api/config/status"),
            (Method::POST, "/api/config/commit"),
            (Method::GET, "/api/secrets"),
            (Method::DELETE, "/api/links/self"),
            (Method::GET, "/api/system/instance"),
            (Method::GET, "/api/sessions/S1"),
            (Method::GET, "/api/schedules"),
            (Method::GET, "/api/schedules/S1/runs"),
            (Method::DELETE, "/api/schedules/S1"),
            (Method::POST, "/api/schedules/S1/disable"),
            (Method::GET, "/api/projects/p/rules"),
            (Method::POST, "/api/projects/p/rules/check"),
            (Method::POST, "/api/projects/p/rules/dry-run"),
            (Method::POST, "/api/projects/p/rules/lint"),
            (Method::GET, "/api/system-prompts"),
            (Method::GET, "/api/system-prompts/leader"),
            (Method::GET, "/api/skills/review"),
            (Method::GET, "/api/skills/review/usage"),
            (Method::GET, "/api/templates/T1/resolve"),
            (Method::GET, "/api/config/defaults"),
            (Method::GET, "/api/sessions/S1/transcript"),
        ];
        for (m, p) in cases {
            assert!(!deny(m.clone(), p), "{m} {p} refused");
        }
    }

    #[test]
    fn spellings_of_a_denied_route_are_refused() {
        for p in [
            "/api//backups/b1",
            "/api/./backups/b1",
            "/api/BACKUPS/b1",
            "/api/%62ackups/b1",
            "/api/backups%2fb1",
            "/api/x/../backups/b1",
            "/api/links/self/../L1",
            "/install/host.sh",
            "/hosts/connect",
            "/a/x",
        ] {
            assert!(deny(Method::GET, p), "{p} passed");
        }
    }

    #[test]
    fn read_only_reads_and_opens_only_the_event_socket() {
        let ro = |m: Method, p: &str, ws: bool| denied(&m, p, ws, true).is_none();
        assert!(ro(Method::GET, "/api/agents", false));
        assert!(ro(Method::GET, "/api/events", true));
        assert!(ro(Method::GET, "/api/agents/a1/events", false));
        assert!(!ro(Method::GET, "/api/agents/a1/pty", true));
        assert!(!ro(Method::POST, "/api/agents/a1/turn", false));
        assert!(!ro(Method::DELETE, "/api/cards/C1", false));
        // `links/self` ends the link: not a read either.
        assert!(!ro(Method::DELETE, "/api/links/self", false));
        // The deny list holds for reads too.
        assert!(!ro(Method::GET, "/api/backups/b1", false));
    }

    #[tokio::test]
    async fn a_revoke_reaches_each_guard_and_frees_the_signal() {
        let rt = Arc::new(Runtime::default());
        let a = rt.guard("h");
        let b = rt.guard("h");
        assert!(!a.is_revoked());
        rt.revoke("h");
        tokio::time::timeout(Duration::from_secs(1), a.revoked())
            .await
            .unwrap();
        assert!(b.is_revoked());
        drop((a, b));
        assert!(rt.lock_signals().is_empty());
        // A later guard of the same hash starts clean.
        let c = rt.guard("h");
        assert!(!c.is_revoked());
        drop(c);
        assert!(rt.lock_signals().is_empty());
    }

    #[tokio::test]
    async fn until_revoked_drops_the_task() {
        let rt = Arc::new(Runtime::default());
        let g = rt.guard("h");
        let task = tokio::spawn(until_revoked(Some(g), std::future::pending()));
        rt.revoke("h");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn throttles() {
        let rt = Runtime::default();
        assert!(rt.should_mark("L1", "10.0.0.1"));
        assert!(!rt.should_mark("L1", "10.0.0.1"));
        assert!(rt.should_mark("L1", "10.0.0.2"));
        assert!(rt.should_audit_forbidden("L1", &Method::POST, "/api/backups"));
        assert!(!rt.should_audit_forbidden("L1", &Method::POST, "/api/backups"));
        assert!(rt.should_audit_forbidden("L1", &Method::GET, "/api/backups/x"));
    }

    #[test]
    fn instance_id_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create_instance_id(dir.path()).unwrap();
        assert!(a.parse::<ulid::Ulid>().is_ok());
        assert_eq!(load_or_create_instance_id(dir.path()).unwrap(), a);
        std::fs::write(dir.path().join(INSTANCE_FILE), "junk").unwrap();
        assert!(load_or_create_instance_id(dir.path()).is_err());
    }

    #[test]
    fn instance_name_prefers_the_public_url() {
        assert_eq!(
            instance_name(Some("https://lab.example.com:8443/x")),
            "lab.example.com"
        );
        assert!(!instance_name(None).is_empty());
    }
}
