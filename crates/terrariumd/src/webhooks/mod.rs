//! The webhook deliveries (docs/api.md §19.5): the `webhook` action of a
//! trigger or a time schedule sends an HTTP request about each event.
//!
//! A run of the action does not send. It renders the body and queues a
//! delivery in the outbox (`crate::store::webhooks`, [`queue`]). The
//! worker ([`spawn`]) sends the due deliveries:
//!
//! - At most [`MAX_PARALLEL`] requests at once in the daemon, and
//!   [`PER_HOST`] to one host and port.
//! - A network error, a time-out, a `5xx`, or a `429` is tried again
//!   after 10 s, 1 min, 5 min, 30 min, and 2 h (or a longer
//!   `Retry-After`), then the delivery is `giving_up`. Any other answer
//!   is `failed`, with no retry.
//! - A restart loses nothing: the `pending` rows stay due, and the next
//!   pass sends them.
//! - Each attempt opens the secrets of the action again, as the trigger's
//!   `run_as` actor, so a revoked grant stops the next attempt
//!   ([`check_secret_use`]). Secret values are never stored or logged.
//! - [`DEFAULT_RATE`] deliveries of one trigger in a minute at most; more
//!   events are dropped, and a `dropped` row counts them.
//!
//! [`net`] holds the URL and address rules (the SSRF stance).

pub mod net;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};
use terrarium_core::schedule::{ScheduleAction, ScheduleSpec};
use terrarium_core::webhook::{self as hook, EventDoc, WebhookAction};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;
use tracing::warn;
use zeroize::Zeroizing;

use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult, Code};
use crate::events::StoredEvent;
use crate::secrets::{self, GLOBAL, grants};
use crate::store::schedules::{self, Outcome, Schedule, field_error};
use crate::store::webhooks::{self as store, Attempt, Class, Delivery, NewDelivery};
use net::{PrivateMode, Resolver};

/// The most requests at once in the daemon.
pub const MAX_PARALLEL: usize = 8;
/// The most requests at once to one host and port.
pub const PER_HOST: usize = 2;
/// The most deliveries of one trigger in one minute.
pub const DEFAULT_RATE: u32 = 60;
/// The most due deliveries that one pass reads.
const PASS_BATCH: usize = 64;

/// The run state of the deliveries: the slots, the deliveries in flight,
/// the rate windows, and the settings.
pub struct Runtime {
    wake: Notify,
    inflight: Mutex<HashSet<String>>,
    slots: Arc<Semaphore>,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
    rates: Mutex<HashMap<String, (Instant, u32)>>,
    rate: AtomicU32,
    private: Mutex<PrivateMode>,
    own_ports: Mutex<Vec<u16>>,
    own_token: Mutex<Option<Zeroizing<String>>>,
    resolver: Mutex<Option<Resolver>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            wake: Notify::new(),
            inflight: Mutex::default(),
            slots: Arc::new(Semaphore::new(MAX_PARALLEL)),
            hosts: Mutex::default(),
            rates: Mutex::default(),
            rate: AtomicU32::new(DEFAULT_RATE),
            private: Mutex::new(PrivateMode::from_env()),
            own_ports: Mutex::default(),
            own_token: Mutex::default(),
            resolver: Mutex::default(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A delivery in flight: the claim ends when it drops.
pub struct Claim {
    rt: Arc<Runtime>,
    id: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        lock(&self.rt.inflight).remove(&self.id);
    }
}

impl Runtime {
    /// Wake the worker: a new delivery is due.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub fn private_mode(&self) -> PrivateMode {
        *lock(&self.private)
    }

    /// Set the mode of private addresses (the tests; the daemon reads
    /// [`net::ENV_PRIVATE`]).
    pub fn set_private_mode(&self, m: PrivateMode) {
        *lock(&self.private) = m;
    }

    /// A port of the daemon's own listeners: `deny` refuses it.
    pub fn add_own_port(&self, port: u16) {
        lock(&self.own_ports).push(port);
    }

    /// The daemon's API token: no request may carry it.
    pub fn set_own_token(&self, token: Option<&str>) {
        *lock(&self.own_token) = token
            .filter(|t| !t.is_empty())
            .map(|t| Zeroizing::new(t.to_owned()));
    }

    /// The address rules for another HTTP client of the daemon (the
    /// GitHub adapter of the pipelines): the mode of private addresses,
    /// the daemon's own ports, and the resolver.
    pub fn net_rules(&self) -> (PrivateMode, Vec<u16>, Option<Resolver>) {
        (
            self.private_mode(),
            lock(&self.own_ports).clone(),
            lock(&self.resolver).clone(),
        )
    }

    /// A fake DNS resolver (the tests).
    pub fn set_resolver(&self, r: Option<Resolver>) {
        *lock(&self.resolver) = r;
    }

    /// The most deliveries of one trigger in a minute (the tests).
    pub fn set_rate(&self, n: u32) {
        self.rate.store(n.max(1), Ordering::Relaxed);
    }

    /// Mark delivery `id` as in flight. `None` when it is.
    pub fn claim(self: &Arc<Self>, id: &str) -> Option<Claim> {
        if !lock(&self.inflight).insert(id.to_owned()) {
            return None;
        }
        Some(Claim {
            rt: self.clone(),
            id: id.to_owned(),
        })
    }

    /// Count one delivery of trigger `id` in its minute. False when the
    /// minute is full.
    fn take_rate(&self, id: &str, now: Instant) -> bool {
        let max = self.rate.load(Ordering::Relaxed);
        let mut m = lock(&self.rates);
        let e = m.entry(id.to_owned()).or_insert((now, 0));
        if now.duration_since(e.0) >= Duration::from_secs(60) {
            *e = (now, 0);
        }
        if e.1 >= max {
            return false;
        }
        e.1 += 1;
        true
    }

    fn host_slot(&self, key: &str) -> Arc<Semaphore> {
        lock(&self.hosts)
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST)))
            .clone()
    }
}

// ------------------------------------------------------------ the rights

/// The actor of a `run_as` string, as the store knows it now: `None` when
/// the agent is gone.
pub fn actor_of(conn: &Connection, run_as: &str) -> ApiResult<Option<Actor>> {
    if run_as == "operator" {
        return Ok(Some(Actor::Operator));
    }
    match run_as.strip_prefix("agent:") {
        Some(id) => Ok(crate::store::agents::actor_for(conn, id)?.map(Actor::Agent)),
        None => Ok(None),
    }
}

/// May `actor` use secret `name` in a webhook of `project` (`None`:
/// global)? The project's secret first, then the global one, as the send
/// opens them.
///
/// - The operator and a sysadmin: any secret.
/// - A project_admin: a secret of its project (it manages them), or a
///   global secret that a grant (`use` or `read`) gives to it or to its
///   project.
///
/// No secret of the name is `422 invalid` on `field`; no right is `403`.
pub fn check_secret_use(
    conn: &Connection,
    actor: &Actor,
    project: Option<&str>,
    name: &str,
    field: &str,
) -> ApiResult<()> {
    check_secret_use_in(conn, actor, project, name, field, "a webhook")
}

/// [`check_secret_use`] for another user of a secret: `what` names it in
/// the errors (`a webhook`, `a pipeline`).
pub fn check_secret_use_in(
    conn: &Connection,
    actor: &Actor,
    project: Option<&str>,
    name: &str,
    field: &str,
    what: &str,
) -> ApiResult<()> {
    let project = project.filter(|p| *p != GLOBAL);
    let own = match project {
        Some(p) => secrets::get(conn, p, name)?,
        None => None,
    };
    let global = secrets::get(conn, GLOBAL, name)?;
    if own.is_none() && global.is_none() {
        let wher = project.map_or_else(
            || "the global secrets".to_owned(),
            |p| format!("project `{p}` or the global secrets"),
        );
        return Err(field_error(field, format!("no secret `{name}` in {wher}")));
    }
    let a = match actor {
        Actor::Operator => return Ok(()),
        Actor::Agent(a) if a.is_sysadmin() => return Ok(()),
        Actor::Agent(a) => a,
    };
    if let (Some(_), Some(p)) = (&own, project) {
        if a.is_admin_of(p) {
            return Ok(());
        }
        return Err(ApiError::forbidden(format!(
            "using secret `{p}/{name}` in {what} needs project_admin of `{p}`"
        )));
    }
    let granted = grants::find(conn, &a.id, &a.project, name)?
        .is_some_and(|e| e.scope == GLOBAL && project == Some(a.project.as_str()));
    if granted {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "`{name}` is a global secret with no grant to {} or its project: {what} uses a global \
         secret only with a `use` grant (terrarium secret grant {name} project:{})",
        actor.label(),
        a.project
    )))
}

/// Check every secret of a webhook action for `actor`.
pub fn check_secrets(
    conn: &Connection,
    actor: &Actor,
    project: Option<&str>,
    a: &WebhookAction,
) -> ApiResult<()> {
    for (field, name) in a.secrets() {
        check_secret_use(conn, actor, project, name, field)?;
    }
    Ok(())
}

/// Check every secret of a webhook action for a project- or system-owned
/// schedule (`crate::owner::secret_use`).
pub fn check_owned_secrets(
    conn: &Connection,
    owner: crate::owner::Owner,
    project: Option<&str>,
    a: &WebhookAction,
) -> ApiResult<()> {
    for (field, name) in a.secrets() {
        crate::owner::secret_use(conn, owner, project, name, field)?;
    }
    Ok(())
}

/// The secrets of the action for the owner of `s`: its `run_as` actor, or
/// the project or the system.
fn check_owner_secrets(conn: &Connection, s: &Schedule, a: &WebhookAction) -> ApiResult<()> {
    let project = s.project.as_deref();
    if s.owner.is_owned() {
        return check_owned_secrets(conn, s.owner, project, a);
    }
    let actor = actor_of(conn, &s.run_as)?.ok_or_else(|| {
        ApiError::forbidden(format!(
            "the trigger runs as `{}`, which no longer exists",
            s.run_as
        ))
    })?;
    check_secrets(conn, &actor, project, a)
}

/// The main value of secret `name`: the project's first, then the global
/// one.
pub(crate) fn open_value(
    conn: &Connection,
    state_dir: &std::path::Path,
    project: Option<&str>,
    name: &str,
) -> ApiResult<Zeroizing<String>> {
    let key = secrets::key(state_dir)?;
    let o = secrets::resolve(conn, &key, project.filter(|p| *p != GLOBAL), name)?
        .ok_or_else(|| ApiError::invalid(format!("no secret `{name}`")))?;
    let field = grants::main_field(o.info.kind, &o.info.secret_fields).ok_or_else(|| {
        ApiError::invalid(format!(
            "secret `{name}` has more than one secret field: give a secret with one value"
        ))
    })?;
    o.values
        .get(&field)
        .cloned()
        .ok_or_else(|| ApiError::invalid(format!("secret `{name}` has no `{field}` value")))
}

// ------------------------------------------------------------- the action

/// The webhook action of a schedule, if it has one.
pub fn action_of(s: &Schedule) -> Option<&WebhookAction> {
    match &s.action {
        ScheduleAction::Webhook(a) => Some(a),
        _ => None,
    }
}

/// The checks of a webhook action at a write, with no network: the shape
/// ([`WebhookAction::check`]), a plain URL, a template that renders valid
/// JSON for a JSON content type, and the secrets for `actor`.
pub fn check_write(
    conn: &Connection,
    actor: &Actor,
    project: Option<&str>,
    spec: &ScheduleSpec,
    a: &WebhookAction,
) -> ApiResult<()> {
    a.check().map_err(|(f, m)| field_error(f, m))?;
    if a.url_secret.is_none() {
        net::check_url(&a.url).map_err(|e| field_error("action.url", e))?;
    }
    if a.format == hook::WebhookFormat::Template && hook::is_json_type(a.content_type()) {
        let e = sample_for(spec, project, Utc::now());
        let body = hook::render(a, &e, "sample", project).body;
        if let Err(err) = serde_json::from_str::<Value>(&body) {
            return Err(field_error(
                "action.template",
                format!(
                    "the content type is JSON, but the template does not render valid JSON for \
                     a sample event ({err}); quote text placeholders (\"{{{{event.summary}}}}\") \
                     or use `| json`"
                ),
            ));
        }
    }
    check_secrets(conn, actor, project, a)
}

/// The sample event of a test send: the first kind of a trigger, or
/// `schedule.tick` for a time schedule.
pub fn sample_for(spec: &ScheduleSpec, project: Option<&str>, now: DateTime<Utc>) -> EventDoc {
    let kind = match spec {
        ScheduleSpec::On { kinds, .. } => kinds.first().cloned().unwrap_or_default(),
        _ => "schedule.tick".into(),
    };
    hook::sample_event(&kind, project, &schedules::stamp(now))
}

/// The event document of a stored event.
pub fn doc_of(e: &StoredEvent) -> EventDoc {
    EventDoc {
        seq: e.seq,
        id: e.id.clone(),
        kind: e.kind.clone(),
        at: e.at.clone(),
        project: e.project.clone(),
        agent: e.agent.clone(),
        actor: e.actor.clone(),
        summary: e.summary.clone(),
        data: e.data.clone(),
    }
}

/// The event of a time schedule's run: `schedule.tick`.
fn tick_doc(s: &Schedule, now: DateTime<Utc>) -> EventDoc {
    EventDoc {
        seq: 0,
        id: String::new(),
        kind: "schedule.tick".into(),
        at: schedules::stamp(now),
        project: s.project.clone(),
        agent: None,
        actor: Some(s.run_label()),
        summary: format!("schedule {}", s.key()),
        data: json!({"schedule": s.id, "name": s.name}),
    }
}

/// The log URL of an action, before an attempt opens a secret URL.
fn log_url_of(a: &WebhookAction) -> String {
    match &a.url_secret {
        Some(name) => format!("(secret {name})"),
        None => net::check_url(&a.url).map_or_else(|_| a.url.clone(), |u| net::log_url(&u)),
    }
}

fn new_delivery(
    s: &Schedule,
    a: &WebhookAction,
    e: &EventDoc,
    cause: Option<Value>,
    test: bool,
) -> Result<NewDelivery, String> {
    let r = hook::render(a, e, &s.name, s.project.as_deref());
    if r.body.len() > hook::MAX_BODY {
        return Err(format!(
            "the body is {} bytes; the most is {}",
            r.body.len(),
            hook::MAX_BODY
        ));
    }
    Ok(NewDelivery {
        schedule: s.id.clone(),
        event_seq: (e.seq > 0).then_some(e.seq),
        event_kind: e.kind.clone(),
        test,
        redelivery_of: None,
        url: log_url_of(a),
        method: a.method.as_str().into(),
        content_type: a.content_type().into(),
        body: r.body,
        format_headers: r.headers,
        cause,
    })
}

/// The run of a webhook action: render the body and queue a delivery
/// (the worker sends it), or drop the event when the trigger's minute is
/// full.
pub async fn queue(
    state: &Shared,
    s: &Schedule,
    a: &WebhookAction,
    event: Option<&StoredEvent>,
    cause: Option<Value>,
) -> Outcome {
    let now = Utc::now();
    let e = event.map_or_else(|| tick_doc(s, now), doc_of);
    let label = schedules::run_label(&s.id);
    if !state.webhooks.take_rate(&s.id, Instant::now()) {
        let (id, url, method) = (s.id.clone(), log_url_of(a), a.method.as_str());
        let n = state
            .db
            .write(label, move |t| {
                store::note_dropped(t, &id, &url, method, now)
            })
            .await;
        let n = n.map(|n| n.to_string()).unwrap_or_else(|_| "?".into());
        let rate = state.webhooks.rate.load(Ordering::Relaxed);
        return Outcome::skipped(format!(
            "rate limit: more than {rate} webhook deliveries in a minute ({n} dropped)"
        ));
    }
    let new = match new_delivery(s, a, &e, cause, false) {
        Ok(n) => n,
        Err(msg) => return Outcome::failed(format!("failed: {msg}"), msg, json!({})),
    };
    let queued = state
        .db
        .write(label, move |t| store::enqueue(t, &new, now))
        .await;
    match queued {
        Ok(d) => {
            state.webhooks.wake();
            Outcome::ok(
                format!("webhook delivery {} queued ({})", d.id, d.url),
                json!({"delivery": d.id, "url": d.url}),
            )
        }
        Err(e) => Outcome::failed(format!("failed: {}", e.message), e.message, json!({})),
    }
}

// --------------------------------------------------------- one attempt

/// The request of an attempt, with the secrets open.
struct Prepared {
    url: reqwest::Url,
    log_url: String,
    headers: Vec<(String, String)>,
    /// The headers as the preview shows them: `[secret:<name>]` for a
    /// secret value.
    shown: Vec<(String, String)>,
    timeout: Duration,
    method: reqwest::Method,
    /// The secret values: the response and the error never show them.
    secrets: Vec<Zeroizing<String>>,
}

/// The parts of the request that the trigger's action gives, with its
/// secrets open as `run_as`. An `Err` is a refusal (no retry).
fn prepare(
    conn: &Connection,
    state_dir: &std::path::Path,
    s: &Schedule,
    d: &Delivery,
    own_token: Option<&str>,
) -> ApiResult<Prepared> {
    let Some(a) = action_of(s) else {
        return Err(ApiError::invalid(
            "the action of the trigger is no longer a webhook",
        ));
    };
    check_owner_secrets(conn, s, a)?;
    let project = s.project.as_deref();
    let mut opened: Vec<Zeroizing<String>> = Vec::new();
    let (url, log_url) = match &a.url_secret {
        Some(name) => {
            let v = open_value(conn, state_dir, project, name)?;
            let u = net::check_url(&v)
                .map_err(|e| ApiError::invalid(format!("the URL of secret `{name}`: {e}")))?;
            opened.push(v);
            let shown = net::log_url_secret(&u, name);
            (u, shown)
        }
        None => {
            let u = net::check_url(&a.url).map_err(ApiError::invalid)?;
            let shown = net::log_url(&u);
            (u, shown)
        }
    };
    let mut headers: Vec<(String, String)> = vec![("Content-Type".into(), d.content_type.clone())];
    headers.extend(d.format_headers.iter().cloned());
    let mut shown = headers.clone();
    for h in &a.headers {
        match (&h.value, &h.secret) {
            (Some(v), _) => {
                headers.push((h.name.clone(), v.clone()));
                shown.push((h.name.clone(), v.clone()));
            }
            (None, Some(name)) => {
                let v = open_value(conn, state_dir, project, name)?;
                if v.chars().any(char::is_control) {
                    return Err(ApiError::invalid(format!(
                        "secret `{name}` has a control character: it cannot be a header value"
                    )));
                }
                headers.push((h.name.clone(), v.to_string()));
                shown.push((h.name.clone(), format!("[secret:{name}]")));
                opened.push(v);
            }
            (None, None) => {}
        }
    }
    let ts = Utc::now().timestamp().to_string();
    let base = [
        (hook::DELIVERY_HEADER.to_owned(), d.id.clone()),
        (hook::EVENT_HEADER.to_owned(), d.event_kind.clone()),
        (hook::TIMESTAMP_HEADER.to_owned(), ts),
    ];
    headers.extend(base.iter().cloned());
    shown.extend(base.iter().cloned());
    if let Some(sig) = &a.signing {
        let key = open_value(conn, state_dir, project, &sig.secret)?;
        let value = format!("sha256={}", crate::ingest::sign(&key, d.body.as_bytes()));
        headers.push((sig.header.clone(), value.clone()));
        shown.push((sig.header.clone(), value));
        opened.push(key);
    }
    if let Some(tok) = own_token.filter(|t| t.len() >= 8) {
        let carries = url.as_str().contains(tok)
            || d.body.contains(tok)
            || headers.iter().any(|(_, v)| v.contains(tok));
        if carries {
            return Err(ApiError::forbidden(
                "the request holds the daemon's API token: Terrarium never sends it",
            ));
        }
    }
    let method = match d.method.as_str() {
        "PUT" => reqwest::Method::PUT,
        _ => reqwest::Method::POST,
    };
    Ok(Prepared {
        url,
        log_url,
        headers,
        shown,
        timeout: Duration::from_secs(a.timeout()),
        method,
        secrets: opened,
    })
}

/// What a test send shows of the request: no secret value.
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

fn refused(e: &ApiError) -> Attempt {
    Attempt {
        class: Class::Fail,
        http_status: None,
        duration_ms: 0,
        response: None,
        error: Some(format!("refused: {}", e.message)),
        retry_after: None,
    }
}

/// Send one attempt of delivery `d` and record it. `Ok(None)`: the
/// trigger is gone.
async fn attempt(state: &Shared, d: Delivery) -> ApiResult<(Option<Delivery>, Option<Preview>)> {
    let key = d.schedule.clone();
    let Ok(s) = state.db.read(move |c| schedules::get(c, &key)).await else {
        return Ok((None, None));
    };
    let token = lock(&state.webhooks.own_token)
        .as_ref()
        .map(|t| t.to_string());
    let (dir, s2, d2) = (state.state_dir.clone(), s.clone(), d.clone());
    let prepared = state
        .db
        .read(move |c| Ok(prepare(c, &dir, &s2, &d2, token.as_deref())))
        .await?;
    let (result, secrets, preview) = match prepared {
        Err(e) if matches!(e.code, Code::Internal) => return Err(e),
        Err(e) => (refused(&e), Vec::new(), None),
        Ok(p) => {
            let preview = Preview {
                method: p.method.to_string(),
                url: p.log_url.clone(),
                headers: p.shown.clone(),
                body: d.body.clone(),
            };
            let rt = &state.webhooks;
            let mode = rt.private_mode();
            let ports = lock(&rt.own_ports).clone();
            let resolver = lock(&rt.resolver).clone();
            let host_key = format!(
                "{}:{}",
                p.url.host_str().unwrap_or_default(),
                p.url.port_or_known_default().unwrap_or(0)
            );
            let host = rt.host_slot(&host_key);
            let _host = host.acquire_owned().await;
            let _slot = rt.slots.clone().acquire_owned().await;
            let result = match net::resolve(&p.url, mode, &ports, resolver).await {
                Err(e) => Attempt {
                    class: Class::Fail,
                    http_status: None,
                    duration_ms: 0,
                    response: None,
                    error: Some(format!("refused: {e}")),
                    retry_after: None,
                },
                Ok(addr) => {
                    net::send(
                        net::Request {
                            method: p.method.clone(),
                            url: p.url.clone(),
                            headers: p.headers.clone(),
                            body: d.body.clone().into_bytes(),
                            timeout: p.timeout,
                        },
                        addr,
                    )
                    .await
                }
            };
            let secrets: Vec<String> = p.secrets.iter().map(|v| v.to_string()).collect();
            (result, secrets, Some(preview))
        }
    };
    let id = d.id.clone();
    let row = state
        .db
        .write("system", move |t| {
            store::record_attempt(t, &id, &result, &secrets, Utc::now())
        })
        .await?;
    Ok((Some(row), preview))
}

/// The live [`hold_retries`] holds, so that the worker reads the map only
/// while a test holds a trigger.
static HOLDS_LIVE: AtomicUsize = AtomicUsize::new(0);

/// The triggers that [`hold_retries`] holds, with their hold count.
fn retry_holds() -> &'static Mutex<HashMap<String, usize>> {
    static HOLDS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    HOLDS.get_or_init(Mutex::default)
}

/// A hold of [`hold_retries`]: dropping it lets the retries start.
#[doc(hidden)]
pub struct RetryHold {
    schedule: String,
}

impl Drop for RetryHold {
    fn drop(&mut self) {
        let mut map = lock(retry_holds());
        if let Some(n) = map.get_mut(&self.schedule) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.schedule);
            }
        }
        HOLDS_LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// For the tests: while the hold lives, no pass of any daemon in the
/// process starts a retry (a delivery with an attempt) of trigger
/// `schedule`, so a test can read a `pending` row without a time budget
/// (TER-1243). The first attempt of a delivery is not held.
#[doc(hidden)]
pub fn hold_retries(schedule: &str) -> RetryHold {
    *lock(retry_holds()).entry(schedule.to_owned()).or_insert(0) += 1;
    HOLDS_LIVE.fetch_add(1, Ordering::AcqRel);
    RetryHold {
        schedule: schedule.to_owned(),
    }
}

/// True when a [`hold_retries`] holds this retry. Outside the tests this
/// is one atomic load.
fn retry_held(d: &Delivery) -> bool {
    d.attempt > 0
        && HOLDS_LIVE.load(Ordering::Acquire) > 0
        && lock(retry_holds()).contains_key(&d.schedule)
}

/// One pass of the worker: send the deliveries that are due at `now` and
/// not in flight. Returns the started sends.
pub async fn pass(state: &Shared, now: DateTime<Utc>) -> Vec<JoinHandle<Option<Delivery>>> {
    let due = match state.db.read(move |c| store::due(c, now, PASS_BATCH)).await {
        Ok(d) => d,
        Err(e) => {
            warn!("the webhook worker cannot read the due deliveries: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for d in due {
        if retry_held(&d) {
            continue;
        }
        let Some(claim) = state.webhooks.claim(&d.id) else {
            continue;
        };
        let st = state.clone();
        out.push(tokio::spawn(async move {
            let _claim = claim;
            let id = d.id.clone();
            match attempt(&st, d).await {
                Ok((row, _)) => row,
                Err(e) => {
                    warn!(delivery = %id, "a webhook attempt failed to record: {e}");
                    None
                }
            }
        }));
    }
    out
}

/// The result of a test send.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub delivery: Delivery,
    pub request: Preview,
}

/// `POST /api/schedules/{id}/test` of a webhook: send the sample event `e`
/// now, and answer with the delivery. The rights of `run_as` and of the
/// caller (`actor`) to the secrets are checked first: `403`. A test send
/// does not retry and does not count toward the disable.
pub async fn test_send(
    state: &Shared,
    s: &Schedule,
    actor: &Actor,
    e: &EventDoc,
) -> ApiResult<TestResult> {
    let Some(a) = action_of(s).cloned() else {
        return Err(ApiError::invalid(format!(
            "schedule `{}` runs a {} action, not a webhook",
            s.key(),
            s.action.kind()
        )));
    };
    let (s2, check, a2) = (s.clone(), actor.clone(), a.clone());
    state
        .db
        .read(move |c| {
            check_secrets(c, &check, s2.project.as_deref(), &a2)?;
            check_owner_secrets(c, &s2, &a2)
        })
        .await?;
    let new = new_delivery(s, &a, e, None, true).map_err(ApiError::invalid)?;
    let label = schedules::run_label(&s.id);
    // Not due: the worker leaves it; this call sends it.
    let d = state
        .db
        .write(label, move |t| {
            let d = store::enqueue(t, &new, Utc::now())?;
            t.execute(
                "UPDATE webhook_deliveries SET next_attempt_at = NULL WHERE id = ?1",
                [&d.id],
            )?;
            store::get(t, &d.id)
        })
        .await?;
    let claim = state.webhooks.claim(&d.id);
    let fallback = Preview {
        method: d.method.clone(),
        url: d.url.clone(),
        headers: Vec::new(),
        body: d.body.clone(),
    };
    let (row, preview) = attempt(state, d).await?;
    drop(claim);
    let delivery = row.ok_or_else(|| ApiError::not_found("the trigger is gone"))?;
    Ok(TestResult {
        delivery,
        request: preview.unwrap_or(fallback),
    })
}

/// Delete the deliveries older than 7 days (the daily prune).
pub async fn prune(state: &Shared) {
    if let Err(e) = state
        .db
        .write("system", |t| store::prune(t, Utc::now()))
        .await
    {
        warn!("the webhook delivery prune failed: {e}");
    }
}

/// The worker: a pass now, then a pass when a delivery is due (at most
/// 5 s apart) or a run wakes it.
pub fn spawn(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            drop(pass(&state, Utc::now()).await);
            let next = state.db.read(store::next_due).await.ok().flatten();
            let wait = next
                .and_then(|n| DateTime::parse_from_rfc3339(&n).ok())
                .map(|n| (n.with_timezone(&Utc) - Utc::now()).num_milliseconds())
                .map_or(5000, |ms| ms.clamp(500, 5000));
            let wait = Duration::from_millis(u64::try_from(wait).unwrap_or(5000));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = state.webhooks.wake.notified() => {}
            }
        }
    })
}
