//! The `exedev` target kind (TER-294): an exe.dev VM that the daemon makes
//! when a holder starts (agent start or lease grant) and removes when the
//! last holder stops (agent stop or lease end).
//!
//! - The lifecycle goes through the exe.dev HTTPS API (`POST /exec`, a
//!   bearer token): `new`, `ls` and `rm`. The API has a 30 s timeout and a
//!   64 KB body limit, so commands never go through it: they go over ssh
//!   to `<user>@<ssh_dest>`, as on an `ssh` target ([`ssh_def`]), with the
//!   workspace sync of TER-293 when `sync_workspace = true`.
//! - Caps (operator, E4): at most [`MAX_VMS`] VMs at once over all
//!   `exedev` targets, with a clear refusal; a life of at most 8 hours
//!   (`[exedev] max_life_secs`).
//! - Each VM has a row in the table `exedev_vms` ([`Vm`]), written before
//!   `new` runs. The row names the account the VM was made on (the token
//!   secret, its scope) and the tag, and it outlives its target: a daemon
//!   restart adopts the VM, and the sweep removes a VM whose target is
//!   gone, on the account it was made on.
//! - One lock per target ([`Locks`]) serializes `up`, `destroy`, `status`
//!   and the sweep's removal of the target's VM.
//! - The sweep ([`spawn`]: one minute after the start, then at the next
//!   `expires_at` and at least every hour; and `POST /api/exedev/sweep`)
//!   removes a VM past its life (and mails the project), a VM whose target
//!   is gone, a VM with no running holder or no ssh host after
//!   [`GRACE_SECS`], and a VM with this daemon's name prefix
//!   ([`prefix_of`] its [`instance`] id) and a known tag that no row
//!   records, after [`GRACE_SECS`]. A VM of another daemon on the same
//!   account has another prefix, so it is never a leftover here (TER-653).
//! - The token is never on a command line or in a log: it goes only into
//!   the `Authorization` header, and an answer that holds it is not shown.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use terrarium_core::config::{ExedevSettings, TargetDef, TargetKind};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use zeroize::Zeroizing;

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::store::targets;

/// The exe.dev HTTPS API.
pub const DEFAULT_API: &str = "https://exe.dev/exec";
/// At most this many exe.dev VMs at once (operator, E4).
pub const MAX_VMS: usize = 3;
/// The name prefix of each VM that Terrarium makes, of every daemon. The
/// cap counts the VMs in `ls` with it. The sweep removes an unrecorded VM
/// only with the longer prefix of its own daemon ([`prefix_of`]).
pub const NAME_PREFIX: &str = "terrarium-x-";
/// The sweep runs at least this often.
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);
/// The first sweep runs this long after the daemon starts.
pub const FIRST_SWEEP: Duration = Duration::from_secs(60);
/// A VM younger than this is not removed for having no running holder, no
/// ssh host, or no record: its `up` or its agent may still be starting.
pub const GRACE_SECS: i64 = 600;
/// How long `up` waits for ssh on a new VM.
const SSH_WAIT: Duration = Duration::from_secs(180);
/// The client timeout of one API call. The API itself stops at 30 s.
const API_TIMEOUT: Duration = Duration::from_secs(45);
/// The longest part of an API answer that goes into an error.
const SHOWN_CHARS: usize = 300;

// ---------------------------------------------------------------- state

/// The exedev state of one daemon (in [`crate::executor::Executor`]).
#[derive(Default)]
pub struct Locks {
    /// One lock per target name.
    targets: std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// One `new` at a time, so the cap holds.
    create: tokio::sync::Mutex<()>,
    /// Wakes the sweep timer when a VM with a new `expires_at` is recorded.
    wake: tokio::sync::Notify,
    /// When the sweep first saw each unrecorded VM that `ls` gives no
    /// `created_at` for.
    seen: std::sync::Mutex<BTreeMap<String, DateTime<Utc>>>,
}

impl Locks {
    fn target(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        m.entry(name.to_owned()).or_default().clone()
    }
}

// --------------------------------------------------------------- records

/// The account an exe.dev VM is made on: the token secret, as the target
/// that made it opens it (its project, and whether an admin set it).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Account {
    pub project: Option<String>,
    pub api_token: String,
    pub admin_set: bool,
}

impl Account {
    /// The account of a target now.
    pub fn of_target(conn: &Connection, def: &TargetDef) -> ApiResult<Self> {
        Ok(Self {
            project: def.project.clone(),
            api_token: settings(def).api_token,
            admin_set: targets::credential_admin(conn, &def.name)?,
        })
    }
}

/// An exe.dev VM that Terrarium made: a row of `exedev_vms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vm {
    /// The exe.dev VM name.
    pub name: String,
    /// The target that made it. The row stays when the target goes.
    pub target: String,
    /// The account it was made on.
    pub project: Option<String>,
    pub api_token: String,
    pub admin_set: bool,
    /// The tag it was made with.
    pub tag: String,
    /// The ssh host from `new` (`<name>.exe.xyz`). `None`: `new` has not
    /// answered yet.
    pub ssh_dest: Option<String>,
    /// RFC 3339: when the daemon asked for the VM.
    pub created_at: String,
    /// RFC 3339: the end of its life.
    pub expires_at: String,
}

impl Vm {
    pub fn account(&self) -> Account {
        Account {
            project: self.project.clone(),
            api_token: self.api_token.clone(),
            admin_set: self.admin_set,
        }
    }

    /// Past `expires_at`, or a time that does not parse.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        parse_time(&self.expires_at).is_none_or(|t| now >= t)
    }

    fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        parse_time(&self.created_at).map_or(i64::MAX, |t| (now - t).num_seconds())
    }

    /// No ssh host yet and younger than [`GRACE_SECS`]: its `new` may still
    /// run, so a miss in `ls` does not mean that the VM is gone.
    pub fn starting(&self, now: DateTime<Utc>) -> bool {
        self.ssh_dest.is_none() && self.age_secs(now) < GRACE_SECS
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

const VM_COLS: &str =
    "name, target, project, api_token, admin_set, tag, ssh_dest, created_at, expires_at";

fn vm_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Vm> {
    Ok(Vm {
        name: r.get(0)?,
        target: r.get(1)?,
        project: r.get(2)?,
        api_token: r.get(3)?,
        admin_set: r.get(4)?,
        tag: r.get(5)?,
        ssh_dest: r.get(6)?,
        created_at: r.get(7)?,
        expires_at: r.get(8)?,
    })
}

/// Every recorded VM, by name.
pub fn records(conn: &Connection) -> ApiResult<Vec<Vm>> {
    let mut stmt = conn.prepare(&format!("SELECT {VM_COLS} FROM exedev_vms ORDER BY name"))?;
    let rows = stmt.query_map([], vm_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The recorded VM of a target.
pub fn record_of(conn: &Connection, target: &str) -> ApiResult<Option<Vm>> {
    Ok(conn
        .query_row(
            &format!("SELECT {VM_COLS} FROM exedev_vms WHERE target = ?1 ORDER BY created_at DESC"),
            [target],
            vm_row,
        )
        .optional()?)
}

/// The exe.dev instance id of this daemon: the one row of
/// `exedev_instance`, 8 random hex characters, written at the first call
/// and kept after. Read each time, so a test can set it.
pub fn instance(conn: &Connection) -> ApiResult<String> {
    // The last 8 hex digits of the 80 random bits of a ULID.
    let random = format!("{:032x}", ulid::Ulid::new().random());
    conn.execute(
        "INSERT INTO exedev_instance (id) SELECT ?1
         WHERE NOT EXISTS (SELECT 1 FROM exedev_instance)",
        [&random[random.len() - 8..]],
    )?;
    let id: String = conn.query_row("SELECT id FROM exedev_instance LIMIT 1", [], |r| r.get(0))?;
    if plain_word(&id) {
        Ok(id)
    } else {
        Err(ApiError::internal(format!(
            "bad exe.dev instance id `{id}` in `exedev_instance`"
        )))
    }
}

/// [`instance`] in a write: the first call writes the row.
async fn instance_of(state: &Shared) -> ApiResult<String> {
    state.db.write("system", |t| instance(t)).await
}

/// The name prefix of the VMs of one daemon: `terrarium-x-<instance>-`.
pub fn prefix_of(instance: &str) -> String {
    format!("{NAME_PREFIX}{instance}-")
}

fn record_named(conn: &Connection, name: &str) -> ApiResult<Option<Vm>> {
    Ok(conn
        .query_row(
            &format!("SELECT {VM_COLS} FROM exedev_vms WHERE name = ?1"),
            [name],
            vm_row,
        )
        .optional()?)
}

async fn put(state: &Shared, vm: &Vm) -> ApiResult<()> {
    let v = vm.clone();
    state
        .db
        .write("system", move |t| {
            t.execute(
                &format!(
                    "INSERT OR REPLACE INTO exedev_vms ({VM_COLS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                ),
                params![
                    v.name,
                    v.target,
                    v.project,
                    v.api_token,
                    v.admin_set,
                    v.tag,
                    v.ssh_dest,
                    v.created_at,
                    v.expires_at
                ],
            )?;
            Ok(())
        })
        .await
}

async fn forget(state: &Shared, name: &str) -> ApiResult<()> {
    let n = name.to_owned();
    state
        .db
        .write("system", move |t| {
            t.execute("DELETE FROM exedev_vms WHERE name = ?1", [&n])?;
            Ok(())
        })
        .await
}

// -------------------------------------------------------------- settings

/// The settings of an `exedev` target: its `[exedev]` table, or the
/// defaults (2 vCPU, 4 GiB, tag `terrarium-dev-ssh`, 8 h).
pub fn settings(def: &TargetDef) -> ExedevSettings {
    def.exedev.clone().unwrap_or_default()
}

/// A word that goes into an API command as it is: lower case letters,
/// digits and `-`, so it needs no quoting.
fn plain_word(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The rules of an `exedev` target (called from `targets::validate`).
pub fn validate(def: &TargetDef) -> ApiResult<()> {
    let bad = |m: String| Err(ApiError::invalid(m));
    if !def.ephemeral || def.keep {
        return bad(
            "an exedev target needs `ephemeral = true` and no `keep`: its VM is made on \
             agent start and removed on agent stop"
                .into(),
        );
    }
    if def.address.is_some() || def.host.is_some() || def.port.is_some() {
        return bad(
            "an exedev target has no `address`, `host` or `port`: exe.dev names the VM's \
             ssh host at `new`"
                .into(),
        );
    }
    if def.credential.is_none() {
        return bad(
            "an exedev target needs `credential`: an ssh_key that exe.dev accepts for VMs \
             with the target's tag"
                .into(),
        );
    }
    if def.snapshot_before_task {
        return bad("an exedev target has no snapshots".into());
    }
    let s = settings(def);
    if !(1..=32).contains(&s.cpu) {
        return bad(format!("`[exedev] cpu` must be 1 to 32, not {}", s.cpu));
    }
    if s.memory_gb == 0 || s.memory_gb > 2 * s.cpu {
        return bad(format!(
            "`[exedev] memory_gb` must be 1 to {} (2 GiB per vCPU), not {}",
            2 * s.cpu,
            s.memory_gb
        ));
    }
    if !(60..=ExedevSettings::MAX_LIFE_SECS).contains(&s.max_life_secs) {
        return bad(format!(
            "`[exedev] max_life_secs` must be 60 to {} (8 hours), not {}",
            ExedevSettings::MAX_LIFE_SECS,
            s.max_life_secs
        ));
    }
    if !plain_word(&s.tag) {
        return bad(format!(
            "bad `[exedev] tag` `{}`: lower case letters, digits and `-`",
            s.tag
        ));
    }
    if !plain_word(&s.user) {
        return bad(format!(
            "bad `[exedev] user` `{}`: lower case letters, digits and `-`",
            s.user
        ));
    }
    if s.api_token.is_empty() {
        return bad("`[exedev] api_token` names the API token secret".into());
    }
    Ok(())
}

/// A new VM name for `target` on the daemon `instance`: [`prefix_of`] the
/// instance, the target name (lower case, at most 20 characters), and 8
/// random characters.
pub fn new_name(instance: &str, target: &str) -> String {
    let mut t: String = target
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(20)
        .collect();
    t = t.trim_matches('-').to_owned();
    let id = ulid::Ulid::new().to_string().to_ascii_lowercase();
    let tail = &id[id.len() - 8..];
    let prefix = prefix_of(instance);
    if t.is_empty() {
        format!("{prefix}{tail}")
    } else {
        format!("{prefix}{t}-{tail}")
    }
}

/// The `ssh` target that runs the commands of an `exedev` target on its VM.
pub fn ssh_def(def: &TargetDef, vm: &Vm) -> ApiResult<TargetDef> {
    let dest = vm.ssh_dest.as_deref().ok_or_else(|| {
        ApiError::conflict(format!(
            "target `{}`: exe.dev VM `{}` has no ssh host yet",
            def.name, vm.name
        ))
    })?;
    let mut d = def.clone();
    d.kind = TargetKind::Ssh;
    d.address = Some(format!("{}@{dest}", settings(def).user));
    d.port = None;
    d.ephemeral = false;
    d.keep = false;
    d.exedev = None;
    Ok(d)
}

/// [`ssh_def`] of the target's recorded VM, read now. An error when the
/// target has no VM.
pub fn exec_def(state: &Shared, def: &TargetDef) -> ApiResult<TargetDef> {
    let n = def.name.clone();
    match state.db.read_now(move |c| record_of(c, &n))? {
        Some(vm) => ssh_def(def, &vm),
        None => Err(ApiError::conflict(format!(
            "target `{}` has no exe.dev VM; a holder's start brings it up \
             (POST /api/targets/{}/up)",
            def.name, def.name
        ))),
    }
}

/// Check the saved root's VM without starting one. Only a complete, valid
/// account listing can prove absence. Local records alone are not evidence.
pub async fn root_vm_absent(state: &Shared, def: &TargetDef, saved: &str) -> ApiResult<bool> {
    let old: TargetDef = serde_json::from_str(saved)
        .map_err(|e| ApiError::invalid(format!("invalid saved root definition: {e}")))?;
    let host = old.address.as_deref().and_then(|a| a.rsplit('@').next());
    let name = host.and_then(|h| h.strip_suffix(".exe.xyz"))
        .filter(|n| !n.is_empty() && !n.contains(['/', ':', '@']));
    let Some(name) = name else {
        return Err(ApiError::conflict("cannot identify the saved root's exe.dev VM"));
    };
    let account = state.db.read_now(|c| {
        match record_of(c, &def.name)? {
            Some(vm) => Ok(vm.account()),
            None => Account::of_target(c, def),
        }
    })?;
    let value = api_for(state, &account)?.run("ls --json").await?;
    let vms = value["vms"].as_array()
        .ok_or_else(|| ApiError::internal("exe.dev listing has no VM array"))?;
    let mut found = false;
    for vm in vms {
        let listed = vm["vm_name"].as_str().or_else(|| vm["name"].as_str())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| ApiError::internal("exe.dev listing has an unnamed VM"))?;
        found |= listed == name;
    }
    Ok(!found)
}

// ------------------------------------------------------------------ API

/// A VM in the answer of `ls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub tags: Vec<String>,
    pub ssh_dest: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

/// The exe.dev HTTPS API with one token. `Debug` shows no token.
pub struct Api {
    url: String,
    token: Zeroizing<String>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").field("url", &self.url).finish()
    }
}

impl Api {
    pub fn new(url: &str, token: Zeroizing<String>) -> ApiResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .map_err(|e| ApiError::internal(format!("exe.dev API client: {e}")))?;
        Ok(Self {
            url: url.to_owned(),
            token,
            http,
        })
    }

    /// The part of an API answer that may go into an error: nothing when
    /// any part of it holds the token, else its first [`SHOWN_CHARS`]
    /// characters.
    fn shown(&self, text: &str) -> String {
        shown(text, &self.token)
    }

    /// Run one command. The answer is its JSON. A status other than 2xx is
    /// an error with the status and the start of the body.
    pub async fn run(&self, cmd: &str) -> ApiResult<Value> {
        let verb = cmd.split_whitespace().next().unwrap_or("").to_owned();
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(self.token.as_str())
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(cmd.to_owned())
            .send()
            .await
            .map_err(|e| {
                ApiError::internal(format!("exe.dev API `{verb}`: {}", e.without_url()))
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = format!(
                "exe.dev API `{verb}`: HTTP {}: {}",
                status.as_u16(),
                self.shown(&text)
            );
            return Err(match status.as_u16() {
                401 | 403 => ApiError::forbidden(msg),
                429 => ApiError::resource_busy(msg),
                400 | 404 | 413 | 422 => ApiError::invalid(msg),
                _ => ApiError::internal(msg),
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| {
            ApiError::internal(format!("exe.dev API `{verb}`: the answer is no JSON: {e}"))
        })
    }

    /// `new`: make a standalone VM. The answer has `ssh_dest`.
    pub async fn new_vm(&self, name: &str, s: &ExedevSettings) -> ApiResult<String> {
        if !plain_word(name) {
            return Err(ApiError::invalid(format!("bad exe.dev VM name `{name}`")));
        }
        if !plain_word(&s.tag) {
            return Err(ApiError::invalid(format!("bad exe.dev tag `{}`", s.tag)));
        }
        let cmd = format!(
            "new --name={name} --sandbox --tag={} --cpu={} --memory={}GB --no-email --json",
            s.tag, s.cpu, s.memory_gb
        );
        let v = self.run(&cmd).await?;
        v["ssh_dest"]
            .as_str()
            .or_else(|| v["ssh_host"].as_str())
            .filter(|d| !d.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::internal(format!(
                    "exe.dev API `new`: the answer has no `ssh_dest`: {}",
                    self.shown(&v.to_string())
                ))
            })
    }

    /// `ls`: the VMs of the account.
    pub async fn ls(&self) -> ApiResult<Vec<Listed>> {
        Ok(parse_ls(&self.run("ls --json").await?))
    }

    /// `rm`: remove VMs. Ok when each one is deleted or not found.
    pub async fn rm(&self, names: &[String]) -> ApiResult<()> {
        if names.is_empty() {
            return Ok(());
        }
        if let Some(n) = names.iter().find(|n| !plain_word(n)) {
            return Err(ApiError::invalid(format!("bad exe.dev VM name `{n}`")));
        }
        let v = self.run(&format!("rm {} --json", names.join(" "))).await?;
        let listed = |k: &str| -> BTreeSet<String> {
            v[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let gone: BTreeSet<String> = listed("deleted")
            .into_iter()
            .chain(listed("not_found"))
            .collect();
        let left: Vec<&String> = names.iter().filter(|n| !gone.contains(*n)).collect();
        if left.is_empty() {
            Ok(())
        } else {
            Err(ApiError::internal(format!(
                "exe.dev API `rm`: not removed: {left:?}; errors: {}",
                self.shown(&v["errors"].to_string())
            )))
        }
    }
}

/// [`Api::shown`]: the whole text is checked for the token before the cut.
fn shown(text: &str, token: &str) -> String {
    if !token.is_empty() && text.contains(token) {
        return "(the answer is not shown: it holds the API token)".into();
    }
    text.chars().take(SHOWN_CHARS).collect()
}

/// The VMs of an `ls` answer: `{"vms": [{"vm_name", "tags", ...}, ...]}`.
pub fn parse_ls(v: &Value) -> Vec<Listed> {
    v["vms"]
        .as_array()
        .map(|vms| {
            vms.iter()
                .filter_map(|vm| {
                    let name = vm["vm_name"].as_str().or_else(|| vm["name"].as_str())?;
                    let tags = vm["tags"]
                        .as_array()
                        .map(|t| {
                            t.iter()
                                .filter_map(|x| x.as_str())
                                .map(|x| x.trim_start_matches('#').to_owned())
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(Listed {
                        name: name.to_owned(),
                        tags,
                        ssh_dest: vm["ssh_dest"].as_str().map(str::to_owned),
                        created_at: vm["created_at"].as_str().and_then(parse_time),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The API of an account: its token secret, opened now. The token stays in
/// memory; it is never logged.
pub fn api_for(state: &Shared, account: &Account) -> ApiResult<Api> {
    let name = account.api_token.clone();
    // The VM can outlive its target, so the owner is the account's scope
    // as the target had it, not the target row.
    let owner = match account.project.as_deref() {
        None => crate::secrets::Owner::Global,
        Some(project) => crate::secrets::Owner::Repo {
            project,
            name: "exedev",
            admin_set: account.admin_set,
        },
    };
    let opened = state
        .db
        .read_now(|c| crate::secrets::open_named(c, &state.state_dir, owner, &name))?;
    let field = crate::secrets::grants::main_field(opened.info.kind, &opened.info.secret_fields)
        .ok_or_else(|| {
            ApiError::invalid(format!(
                "secret `{name}` has more than one field; the exe.dev API token needs one"
            ))
        })?;
    let token = opened
        .values
        .get(&field)
        .cloned()
        .ok_or_else(|| ApiError::invalid(format!("secret `{name}` has no `{field}`")))?;
    Api::new(&state.executor.exedev_api, token)
}

// ------------------------------------------------------------ lifecycle

/// The refusal of a new VM when [`MAX_VMS`] run: `running` are the names
/// that count (recorded, or listed with the prefix).
pub fn cap_refusal(running: &BTreeSet<String>) -> Option<String> {
    (running.len() >= MAX_VMS).then(|| {
        format!(
            "exe.dev cap: {} of {MAX_VMS} VMs run ({}); a new one waits until one is \
             removed (agent stop, lease end, or the sweep)",
            running.len(),
            running.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    })
}

/// `up`: adopt the recorded VM when `ls` lists it and it is in its life,
/// else make a new one. The answer is the new state (`running`).
pub async fn up(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let lock = state.executor.exedev.target(&def.name);
    let _target = lock.lock().await;
    up_locked(state, def).await
}

async fn up_locked(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let s = settings(def);
    let now = Utc::now();
    let n = def.name.clone();
    if let Some(mut vm) = state.db.read(move |c| record_of(c, &n)).await? {
        let api = api_for(state, &vm.account())?;
        let listed = api.ls().await?;
        match listed.iter().find(|l| l.name == vm.name) {
            Some(l) if !vm.expired(now) => {
                // A record written before `new` answered (a restart): the
                // VM exists, so take its ssh host from `ls`.
                if vm.ssh_dest.is_none() {
                    vm.ssh_dest = Some(
                        l.ssh_dest
                            .clone()
                            .unwrap_or_else(|| format!("{}.exe.xyz", vm.name)),
                    );
                    put(state, &vm).await?;
                }
                wait_ssh(state, def, &vm).await?;
                info!(target = %def.name, vm = %vm.name, "adopted the exe.dev VM");
                return Ok("running".into());
            }
            // Past its life: never adopted.
            Some(_) => {
                api.rm(std::slice::from_ref(&vm.name)).await?;
                forget(state, &vm.name).await?;
            }
            // Not in `ls` while its `new` may still run (a restart during
            // `new`): the record stays, so the sweep removes the VM when it
            // shows up (`unready` after the grace time). A new VM is made.
            None if vm.starting(now) => {
                info!(target = %def.name, vm = %vm.name, "the exe.dev VM is not in ls yet; its record stays");
            }
            None => forget(state, &vm.name).await?,
        }
    }
    let d = def.clone();
    let account = state.db.read(move |c| Account::of_target(c, &d)).await?;
    let api = api_for(state, &account)?;
    let inst = instance_of(state).await?;
    let one = state.executor.exedev.create.lock().await;
    let mut running: BTreeSet<String> = state
        .db
        .read(records)
        .await?
        .into_iter()
        .map(|v| v.name)
        .collect();
    running.extend(
        api.ls()
            .await?
            .into_iter()
            .filter(|l| l.name.starts_with(NAME_PREFIX))
            .map(|l| l.name),
    );
    if let Some(why) = cap_refusal(&running) {
        return Err(ApiError::conflict(why));
    }
    let life = chrono::Duration::seconds(i64::try_from(s.max_life_secs).unwrap_or(i64::MAX));
    let mut vm = Vm {
        name: new_name(&inst, &def.name),
        target: def.name.clone(),
        project: account.project.clone(),
        api_token: account.api_token.clone(),
        admin_set: account.admin_set,
        tag: s.tag.clone(),
        ssh_dest: None,
        created_at: now.to_rfc3339(),
        expires_at: (now + life).to_rfc3339(),
    };
    // The record first: a restart finds the VM even if `new` never answers.
    put(state, &vm).await?;
    state.executor.exedev.wake.notify_one();
    let dest = match api.new_vm(&vm.name, &s).await {
        Ok(d) => d,
        Err(e) => {
            // `new` can make the VM and still fail (a 504): remove it. When
            // the `rm` fails too, the record stays for the sweep.
            match api.rm(std::slice::from_ref(&vm.name)).await {
                Ok(()) => forget(state, &vm.name).await?,
                Err(r) => warn!(vm = %vm.name, "cannot remove the VM after a failed new: {r}"),
            }
            return Err(e);
        }
    };
    vm.ssh_dest = Some(dest);
    put(state, &vm).await?;
    drop(one);
    info!(target = %def.name, vm = %vm.name, "made an exe.dev VM");
    if let Err(e) = wait_ssh(state, def, &vm).await {
        if let Err(r) = destroy_locked(state, def).await {
            warn!(vm = %vm.name, "cannot remove the VM that ssh did not reach: {r}");
        }
        return Err(e);
    }
    Ok("running".into())
}

/// Wait until `true` runs on the VM over ssh.
async fn wait_ssh(state: &Shared, def: &TargetDef, vm: &Vm) -> ApiResult<()> {
    let d = ssh_def(def, vm)?;
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < SSH_WAIT {
        let cmd = crate::executor::RemoteCmd {
            argv: vec!["true".into()],
            timeout_secs: Some(30),
            ..Default::default()
        };
        match crate::executor::start_cmd(state, &d, &cmd) {
            Ok(run) => {
                let c = run.collect(4096).await;
                if c.end.as_ref().is_some_and(crate::executor::End::success) {
                    return Ok(());
                }
                last = String::from_utf8_lossy(&c.stderr)
                    .chars()
                    .take(200)
                    .collect();
            }
            Err(e) => last = e.message,
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    Err(ApiError::internal(format!(
        "exe.dev VM `{}`: ssh did not answer in {} s: {last}",
        vm.name,
        SSH_WAIT.as_secs()
    )))
}

/// `down` and `destroy`: remove the VM, on the account it was made on.
/// The answer is `absent`. When `rm` fails, the record stays (also when
/// the target is deleted after), so the sweep removes the VM later.
pub async fn destroy(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let lock = state.executor.exedev.target(&def.name);
    let _target = lock.lock().await;
    destroy_locked(state, def).await
}

async fn destroy_locked(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let n = def.name.clone();
    if let Some(vm) = state.db.read(move |c| record_of(c, &n)).await? {
        api_for(state, &vm.account())?
            .rm(std::slice::from_ref(&vm.name))
            .await?;
        forget(state, &vm.name).await?;
        info!(target = %def.name, vm = %vm.name, "removed the exe.dev VM");
    }
    Ok("absent".into())
}

/// `status`: `running` when the recorded VM is in `ls`, else `absent`. The
/// record goes, except while the VM is [`Vm::starting`]: then it stays for
/// the grace time.
pub async fn status(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let lock = state.executor.exedev.target(&def.name);
    let _target = lock.lock().await;
    let n = def.name.clone();
    let Some(vm) = state.db.read(move |c| record_of(c, &n)).await? else {
        return Ok("absent".into());
    };
    let listed = api_for(state, &vm.account())?.ls().await?;
    if listed.iter().any(|l| l.name == vm.name) {
        Ok("running".into())
    } else {
        if !vm.starting(Utc::now()) {
            forget(state, &vm.name).await?;
        }
        Ok("absent".into())
    }
}

// ---------------------------------------------------------------- sweep

/// Why the sweep removes a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Past `expires_at`.
    Expired,
    /// Its target is gone.
    Orphaned,
    /// No ssh host after the grace time: `new` never answered.
    Unready,
    /// No running agent holds its target, after the grace time.
    Unheld,
    /// In `ls` with this daemon's prefix and a known tag, but no row
    /// records it, after the grace time.
    Leftover,
}

impl Reason {
    fn word(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Orphaned => "orphaned",
            Self::Unready => "unready",
            Self::Unheld => "unheld",
            Self::Leftover => "leftover",
        }
    }
}

/// One VM for the sweep to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Removal {
    pub vm: String,
    pub target: Option<String>,
    pub reason: Reason,
}

/// What the sweep knows when it plans one account.
pub struct Facts<'a> {
    pub now: DateTime<Utc>,
    /// The account of this `ls`.
    pub account: &'a Account,
    /// The name prefix of this daemon's VMs ([`prefix_of`]).
    pub prefix: &'a str,
    /// Every recorded VM, of every account.
    pub records: &'a [Vm],
    /// The names of the `exedev` targets that exist.
    pub targets: &'a BTreeSet<String>,
    /// The targets that a running agent holds.
    pub held: &'a BTreeSet<String>,
    /// The `ls` of the account.
    pub listed: &'a [Listed],
    /// The tags of the account's records and targets.
    pub tags: &'a BTreeSet<String>,
}

/// What the sweep removes on one account.
pub fn plan(f: &Facts<'_>) -> Vec<Removal> {
    let mut out = Vec::new();
    for v in f.records.iter().filter(|v| v.account() == *f.account) {
        if let Some(reason) = record_reason(f, v, f.held.contains(&v.target)) {
            out.push(Removal {
                vm: v.name.clone(),
                target: Some(v.target.clone()),
                reason,
            });
        }
    }
    let known: BTreeSet<&str> = f.records.iter().map(|v| v.name.as_str()).collect();
    for l in f.listed {
        let old = l
            .created_at
            .is_some_and(|t| (f.now - t).num_seconds() >= GRACE_SECS);
        if l.name.starts_with(f.prefix)
            && l.tags.iter().any(|t| f.tags.contains(t))
            && !known.contains(l.name.as_str())
            && old
        {
            out.push(Removal {
                vm: l.name.clone(),
                target: None,
                reason: Reason::Leftover,
            });
        }
    }
    out
}

/// Why the sweep removes a recorded VM. `held`: a running agent holds its
/// target.
fn record_reason(f: &Facts<'_>, v: &Vm, held: bool) -> Option<Reason> {
    let old = v.age_secs(f.now) >= GRACE_SECS;
    if v.expired(f.now) {
        Some(Reason::Expired)
    } else if !f.targets.contains(&v.target) {
        Some(Reason::Orphaned)
    } else if old && v.ssh_dest.is_none() {
        Some(Reason::Unready)
    } else if old && !held {
        Some(Reason::Unheld)
    } else {
        None
    }
}

/// Run the sweep one time. The answer lists what it removed and the
/// errors.
pub async fn sweep(state: &Shared) -> ApiResult<Value> {
    let rows: Vec<targets::TargetRow> = state
        .db
        .read(targets::list)
        .await?
        .into_iter()
        .filter(|r| r.def.kind == TargetKind::Exedev)
        .collect();
    let names: BTreeSet<String> = rows.iter().map(|r| r.def.name.clone()).collect();
    // The accounts and their tags: of the records (also of gone targets)
    // and of the targets.
    let mut accounts: BTreeMap<Account, BTreeSet<String>> = BTreeMap::new();
    for v in state.db.read(records).await? {
        accounts.entry(v.account()).or_default().insert(v.tag);
    }
    for r in &rows {
        let d = r.def.clone();
        let a = state.db.read(move |c| Account::of_target(c, &d)).await?;
        accounts.entry(a).or_default().insert(settings(&r.def).tag);
    }
    let prefix = prefix_of(&instance_of(state).await?);
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for (account, tags) in &accounts {
        let who = format!(
            "token `{}` ({})",
            account.api_token,
            account.project.as_deref().unwrap_or("global")
        );
        let api = match api_for(state, account) {
            Ok(a) => a,
            Err(e) => {
                errors.push(format!("{who}: {}", e.message));
                continue;
            }
        };
        let mut listed = match api.ls().await {
            Ok(l) => l,
            Err(e) => {
                errors.push(format!("{who}: {}", e.message));
                continue;
            }
        };
        let now = Utc::now();
        {
            let mut seen = state
                .executor
                .exedev
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for l in &mut listed {
                if l.created_at.is_none() {
                    l.created_at = Some(*seen.entry(l.name.clone()).or_insert(now));
                }
            }
        }
        // The records after `ls`: a VM that `up` makes now is recorded
        // before its `new`, so it is no leftover.
        let all = state.db.read(records).await?;
        let mut held = BTreeSet::new();
        for v in all.iter().filter(|v| v.account() == *account) {
            if names.contains(&v.target)
                && !crate::executor::running_holders(state, &v.target, None)
                    .await?
                    .is_empty()
            {
                held.insert(v.target.clone());
            }
        }
        let facts = Facts {
            now,
            account,
            prefix: &prefix,
            records: &all,
            targets: &names,
            held: &held,
            listed: &listed,
            tags,
        };
        for rm in plan(&facts) {
            match remove(state, &api, &facts, &rm).await {
                Ok(true) => {
                    info!(vm = %rm.vm, reason = ?rm.reason, "the exe.dev sweep removed a VM");
                    removed.push(rm);
                }
                Ok(false) => {}
                Err(e) => errors.push(format!("rm `{}`: {}", rm.vm, e.message)),
            }
        }
    }
    // Forget the first sightings of VMs that are gone.
    {
        let all: BTreeSet<String> = removed.iter().map(|r: &Removal| r.vm.clone()).collect();
        let mut seen = state
            .executor
            .exedev
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.retain(|n, _| !all.contains(n));
    }
    for e in &errors {
        warn!("exe.dev sweep: {e}");
    }
    Ok(json!({"removed": removed, "errors": errors}))
}

/// Remove one planned VM. A recorded VM is removed under its target's
/// lock, after a fresh read of the record and of the target's running
/// holders shows the reason still holds. `false`: no longer to remove.
async fn remove(state: &Shared, api: &Api, f: &Facts<'_>, rm: &Removal) -> ApiResult<bool> {
    let Some(target) = &rm.target else {
        // A leftover: still unrecorded right before the `rm`.
        let n = rm.vm.clone();
        if state.db.read(move |c| record_named(c, &n)).await?.is_some() {
            return Ok(false);
        }
        api.rm(std::slice::from_ref(&rm.vm)).await?;
        return Ok(true);
    };
    let lock = state.executor.exedev.target(target);
    let _target = lock.lock().await;
    let n = rm.vm.clone();
    let Some(vm) = state.db.read(move |c| record_named(c, &n)).await? else {
        return Ok(false);
    };
    // The holders now, under the lock: an agent that started after the
    // plan holds the VM (TER-653).
    let held = f.targets.contains(&vm.target)
        && !crate::executor::running_holders(state, &vm.target, None)
            .await?
            .is_empty();
    if record_reason(f, &vm, held) != Some(rm.reason) {
        return Ok(false);
    }
    api.rm(std::slice::from_ref(&vm.name)).await?;
    after_removal(state, &vm, rm.reason, f.targets.contains(&vm.target)).await;
    Ok(true)
}

/// After the sweep removed a recorded VM: forget the record, set the
/// target `absent` when it still exists, write the audit row, and mail the
/// project (or the operator) for a VM past its life.
async fn after_removal(state: &Shared, vm: &Vm, reason: Reason, target_exists: bool) {
    let v = vm.clone();
    let res = state
        .db
        .write("system", move |t| {
            t.execute("DELETE FROM exedev_vms WHERE name = ?1", [&v.name])?;
            if target_exists && targets::find(t, &v.target)?.is_some() {
                targets::set_state(t, &v.target, "absent", None)?;
            }
            t.audit(
                "target.destroy",
                &v.target,
                v.project.as_deref(),
                json!({"reason": format!("exedev_{}", reason.word()), "vm": v.name}),
            )?;
            if reason == Reason::Expired {
                let to = match &v.project {
                    Some(p) => crate::store::mail::Address::Project(p.clone()),
                    None => crate::store::mail::Address::Operator,
                };
                crate::store::mail::send(
                    t,
                    crate::store::mail::Outgoing {
                        from: &crate::store::mail::Address::System,
                        to: &to,
                        subject: &format!("exe.dev VM {} removed: past its life", v.name),
                        body: &format!(
                            "The exe.dev VM `{}` of target `{}` was past its life \
                             (`[exedev] max_life_secs`, at most 8 hours), so the sweep \
                             removed it. A holder that still runs gets a new VM at its \
                             next start or lease grant.",
                            v.name, v.target
                        ),
                        thread: None,
                        in_reply_to: None,
                    },
                )?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = res {
        warn!(vm = %vm.name, "exe.dev sweep: cannot record the removal: {e}");
    }
}

/// The time until the sweep must run: the hourly tick, or the earliest
/// `expires_at` (one second after it), whichever comes first. A VM that is
/// past its life already waits at least `retry` (its `rm` may fail).
pub fn next_wait(
    now: DateTime<Utc>,
    until_tick: Duration,
    expires: &[DateTime<Utc>],
    retry: Duration,
) -> Duration {
    let mut wait = until_tick;
    if let Some(first) = expires.iter().min() {
        let to = (*first - now).to_std().unwrap_or(Duration::ZERO) + Duration::from_secs(1);
        wait = wait.min(to.max(retry));
    }
    wait
}

/// The sweep timer: the first run [`FIRST_SWEEP`] after the start, then at
/// the next `expires_at` of a recorded VM, and at least every
/// [`SWEEP_EVERY`]. `up` wakes it when it records a VM.
pub fn spawn(state: &Shared) -> JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        let mut tick = Instant::now() + FIRST_SWEEP;
        let mut retry = Duration::ZERO;
        loop {
            let expires: Vec<DateTime<Utc>> = match state.db.read(records).await {
                Ok(vms) => vms
                    .iter()
                    .filter_map(|v| parse_time(&v.expires_at))
                    .collect(),
                Err(e) => {
                    warn!("exe.dev sweep: cannot read the VMs: {e}");
                    Vec::new()
                }
            };
            let wait = next_wait(
                Utc::now(),
                tick.saturating_duration_since(Instant::now()),
                &expires,
                retry,
            );
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = state.executor.exedev.wake.notified() => continue,
            }
            if Instant::now() >= tick {
                tick = Instant::now() + SWEEP_EVERY;
            }
            if let Err(e) = sweep(&state).await {
                warn!("exe.dev sweep failed: {e}");
            }
            retry = Duration::from_secs(60);
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn def(extra: &str) -> TargetDef {
        toml::from_str(&format!(
            "name = \"ex\"\nkind = \"exedev\"\nephemeral = true\ncredential = \"k\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn the_defaults_are_2_vcpu_4_gib_8_hours_and_the_ssh_tag() {
        let s = settings(&def(""));
        assert_eq!((s.cpu, s.memory_gb), (2, 4));
        assert_eq!(s.max_life_secs, 8 * 3600);
        assert_eq!(s.tag, "terrarium-dev-ssh");
        assert_eq!(s.api_token, "exedev-api-token");
        assert_eq!(s.user, "exedev");
        validate(&def("")).unwrap();
    }

    #[test]
    fn validate_refuses_what_an_exedev_target_cannot_be() {
        let cases = [
            ("ephemeral = false", "ephemeral = true"),
            ("keep = true", "ephemeral = true"),
            ("address = \"a@b\"", "no `address`"),
            ("[exedev]\ncpu = 0", "cpu"),
            ("[exedev]\ncpu = 2\nmemory_gb = 5", "2 GiB per vCPU"),
            ("[exedev]\nmax_life_secs = 30000", "8 hours"),
            ("[exedev]\nmax_life_secs = 10", "8 hours"),
            ("[exedev]\ntag = \"Bad Tag\"", "tag"),
        ];
        for (extra, want) in cases {
            let text = if extra.starts_with("ephemeral") {
                format!("name = \"ex\"\nkind = \"exedev\"\ncredential = \"k\"\n{extra}")
            } else {
                format!(
                    "name = \"ex\"\nkind = \"exedev\"\nephemeral = true\ncredential = \"k\"\n{extra}"
                )
            };
            let d: TargetDef = toml::from_str(&text).unwrap();
            let e = validate(&d).expect_err(extra);
            assert!(e.message.contains(want), "{extra}: {}", e.message);
        }
        let mut d = def("");
        d.credential = None;
        assert!(validate(&d).unwrap_err().message.contains("credential"));
    }

    #[test]
    fn a_new_name_has_the_prefix_the_target_and_a_random_tail() {
        let a = new_name("0a1b2c3d", "Build_Box");
        let b = new_name("0a1b2c3d", "Build_Box");
        assert!(a.starts_with("terrarium-x-0a1b2c3d-build-box-"), "{a}");
        assert_ne!(a, b);
        assert!(plain_word(&a), "{a}");
        // A DNS label (`<name>.exe.xyz`) has at most 63 characters.
        let long = new_name("0a1b2c3d", &"x".repeat(60));
        assert_eq!(long.len(), prefix_of("0a1b2c3d").len() + 20 + 9);
        assert!(long.len() <= 63, "{long}");
    }

    /// The first call makes one instance id, a plain word of 8 hex
    /// characters, and later calls (also after a reopen) keep it.
    #[test]
    fn the_instance_id_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let id = instance(&crate::db::open(&path).unwrap()).unwrap();
        assert_eq!(id.len(), 8, "{id}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "{id}"
        );
        assert_eq!(instance(&crate::db::open(&path).unwrap()).unwrap(), id);
        let other = tempfile::tempdir().unwrap();
        let id2 = instance(&crate::db::open(&other.path().join("t.db")).unwrap()).unwrap();
        assert_ne!(id, id2, "two daemons have two ids");
    }

    fn account() -> Account {
        Account {
            project: None,
            api_token: "exedev-api-token".into(),
            admin_set: true,
        }
    }

    fn vm(name: &str, target: &str, age: i64, life: i64) -> Vm {
        let now = Utc::now();
        Vm {
            name: name.into(),
            target: target.into(),
            project: None,
            api_token: "exedev-api-token".into(),
            admin_set: true,
            tag: "terrarium-dev-ssh".into(),
            ssh_dest: Some(format!("{name}.exe.xyz")),
            created_at: (now - chrono::Duration::seconds(age)).to_rfc3339(),
            expires_at: (now - chrono::Duration::seconds(age) + chrono::Duration::seconds(life))
                .to_rfc3339(),
        }
    }

    #[test]
    fn ssh_def_points_ssh_at_the_vm() {
        let v = vm("terrarium-x-ex-1", "ex", 0, 60);
        let d = ssh_def(&def("sync_workspace = true"), &v).unwrap();
        assert_eq!(d.kind, TargetKind::Ssh);
        assert_eq!(
            d.address.as_deref(),
            Some("exedev@terrarium-x-ex-1.exe.xyz")
        );
        assert!(d.sync_workspace);
        assert_eq!(d.credential.as_deref(), Some("k"));
        let none = Vm {
            ssh_dest: None,
            ..v
        };
        assert!(ssh_def(&def(""), &none).is_err());
    }

    #[test]
    fn the_cap_refuses_the_fourth_vm() {
        let names = |n: usize| -> BTreeSet<String> { (0..n).map(|i| format!("v{i}")).collect() };
        assert!(cap_refusal(&names(2)).is_none());
        let why = cap_refusal(&names(3)).unwrap();
        assert!(why.contains("3 of 3"), "{why}");
    }

    fn listed(name: &str, tag: &str, age: i64) -> Listed {
        Listed {
            name: name.into(),
            tags: vec![tag.into()],
            ssh_dest: None,
            created_at: Some(Utc::now() - chrono::Duration::seconds(age)),
        }
    }

    /// The prefix of this daemon in the plan tests.
    const P: &str = "terrarium-x-aaaa0001-";

    fn run_plan(
        records: &[Vm],
        targets: &[&str],
        held: &[&str],
        listed: &[Listed],
    ) -> Vec<(String, Reason)> {
        let targets: BTreeSet<String> = targets.iter().map(|s| (*s).to_owned()).collect();
        let held: BTreeSet<String> = held.iter().map(|s| (*s).to_owned()).collect();
        let tags: BTreeSet<String> = ["terrarium-dev-ssh".to_owned()].into();
        let a = account();
        plan(&Facts {
            now: Utc::now(),
            account: &a,
            prefix: P,
            records,
            targets: &targets,
            held: &held,
            listed,
            tags: &tags,
        })
        .into_iter()
        .map(|r| (r.vm, r.reason))
        .collect()
    }

    #[test]
    fn the_sweep_plan_removes_only_what_it_must() {
        let records = vec![
            vm("terrarium-x-a-1", "a", 9 * 3600, 8 * 3600),
            vm("terrarium-x-b-1", "b", 3600, 8 * 3600),
            vm("terrarium-x-c-1", "c", 60, 8 * 3600),
            vm("terrarium-x-d-1", "d", 3600, 8 * 3600),
            vm("terrarium-x-gone-1", "gone", 60, 8 * 3600),
            Vm {
                ssh_dest: None,
                ..vm("terrarium-x-e-1", "e", 3600, 8 * 3600)
            },
            Vm {
                ssh_dest: None,
                ..vm("terrarium-x-f-1", "f", 60, 8 * 3600)
            },
        ];
        let ls = vec![
            listed("terrarium-x-a-1", "terrarium-dev-ssh", 3600),
            listed(&format!("{P}lost-1"), "terrarium-dev-ssh", 3600),
            listed(&format!("{P}new-1"), "terrarium-dev-ssh", 60),
            listed(&format!("{P}other-1"), "someone-else", 3600),
            // TER-653: a VM of another daemon on the account, and one of
            // the old prefix with no instance, are no leftovers here.
            listed("terrarium-x-bbbb0002-lost-1", "terrarium-dev-ssh", 3600),
            listed("terrarium-x-lost-2", "terrarium-dev-ssh", 3600),
            listed("terrarium-ssh2", "terrarium-dev-ssh", 3600),
            listed("personal-vm", "terrarium-dev-ssh", 3600),
        ];
        let got = run_plan(
            &records,
            &["a", "b", "c", "d", "e", "f"],
            &["a", "d", "e", "f"],
            &ls,
        );
        assert_eq!(
            got,
            vec![
                ("terrarium-x-a-1".into(), Reason::Expired),
                ("terrarium-x-b-1".into(), Reason::Unheld),
                ("terrarium-x-gone-1".into(), Reason::Orphaned),
                ("terrarium-x-e-1".into(), Reason::Unready),
                (format!("{P}lost-1"), Reason::Leftover),
            ]
        );
    }

    /// Finding 2: a VM recorded by a target of another project or token
    /// (of the same exe.dev account) is no leftover of this account.
    #[test]
    fn a_vm_of_another_account_record_is_no_leftover() {
        let other = Vm {
            project: Some("p".into()),
            admin_set: false,
            ..vm(&format!("{P}p-1"), "p-ex", 3600, 8 * 3600)
        };
        let ls = vec![listed(&format!("{P}p-1"), "terrarium-dev-ssh", 3600)];
        assert!(run_plan(&[other], &["p-ex"], &["p-ex"], &ls).is_empty());
        // Unrecorded, it is a leftover.
        assert_eq!(
            run_plan(&[], &[], &[], &ls),
            vec![(format!("{P}p-1"), Reason::Leftover)]
        );
    }

    /// The first live run (60 s life) lost its VM to the daemon's own
    /// first sweep at [`FIRST_SWEEP`]: a VM is expired from `expires_at`
    /// on, so a sweep one second after it removes the VM, and one second
    /// before keeps it.
    #[test]
    fn a_vm_is_expired_from_its_expires_at_on() {
        let v = vm("terrarium-x-live-1", "live", 0, 60);
        let made = parse_time(&v.created_at).unwrap();
        let a = account();
        let (t, h, tags) = (
            BTreeSet::from(["live".to_owned()]),
            BTreeSet::from(["live".to_owned()]),
            BTreeSet::new(),
        );
        let recs = [v.clone()];
        let at = |s: i64| {
            plan(&Facts {
                now: made + chrono::Duration::seconds(s),
                account: &a,
                prefix: P,
                records: &recs,
                targets: &t,
                held: &h,
                listed: &[],
                tags: &tags,
            })
        };
        assert!(at(59).is_empty());
        assert_eq!(at(61)[0].reason, Reason::Expired);
        assert!(v.expired(made + chrono::Duration::seconds(60)));
        // A bad time counts as expired: the VM is never kept by mistake.
        let bad = Vm {
            expires_at: "never".into(),
            ..v
        };
        assert!(bad.expired(made));
    }

    /// Finding 5: the timer runs at the next `expires_at`, not only hourly.
    #[test]
    fn the_sweep_timer_runs_at_the_next_expiry() {
        let now = Utc::now();
        let hour = Duration::from_secs(3600);
        let r = Duration::from_secs(60);
        assert_eq!(next_wait(now, hour, &[], r), hour);
        let soon = [now + chrono::Duration::seconds(120)];
        assert_eq!(
            next_wait(now, hour, &soon, Duration::ZERO),
            Duration::from_secs(121)
        );
        let later = [now + chrono::Duration::seconds(7200)];
        assert_eq!(next_wait(now, hour, &later, Duration::ZERO), hour);
        // A VM past its life whose `rm` failed: retried, not in a hot loop.
        let past = [now - chrono::Duration::seconds(5)];
        assert_eq!(next_wait(now, hour, &past, r), r);
        assert_eq!(
            next_wait(now, hour, &past, Duration::ZERO),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn parse_ls_reads_names_tags_hosts_and_times() {
        let v = json!({"vms": [
            {"vm_name": "a", "tags": ["terrarium-dev-ssh"], "status": "running",
             "ssh_dest": "a.exe.xyz", "created_at": "2026-11-08T12:17:53Z"},
            {"name": "b", "tags": ["#x"]},
            {"no_name": true},
        ]});
        let got = parse_ls(&v);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "a");
        assert_eq!(got[0].tags, vec!["terrarium-dev-ssh".to_owned()]);
        assert_eq!(got[0].ssh_dest.as_deref(), Some("a.exe.xyz"));
        assert_eq!(got[0].created_at, parse_time("2026-11-08T12:17:53Z"));
        assert_eq!(
            (got[1].name.as_str(), got[1].tags.clone()),
            ("b", vec!["x".to_owned()])
        );
        assert!(got[1].created_at.is_none());
        assert!(parse_ls(&json!({"vms": []})).is_empty());
    }

    /// Finding 7: the whole answer is checked for the token before the
    /// cut, also when the token starts at character 299.
    #[test]
    fn an_answer_with_the_token_is_never_shown() {
        let token = "exe1.s3cret-token";
        let at_299 = format!("{}{token} tail", "x".repeat(299));
        let s = shown(&at_299, token);
        assert!(!s.contains('x') && !s.contains("exe1"), "{s}");
        assert!(s.contains("not shown"), "{s}");
        let long = "y".repeat(400);
        assert_eq!(shown(&long, token).len(), SHOWN_CHARS);
        assert_eq!(shown("short", token), "short");
    }

    /// A fake exe.dev API: it checks the bearer token and answers each
    /// command; it keeps the bodies it got.
    async fn fake_api(
        answers: Vec<(u16, &'static str)>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    ) {
        use axum::{Router, http::HeaderMap, routing::post};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let answers = std::sync::Arc::new(std::sync::Mutex::new(answers));
        let s = seen.clone();
        let app = Router::new().route(
            "/exec",
            post(move |h: HeaderMap, body: String| {
                let (s, answers) = (s.clone(), answers.clone());
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    s.lock().unwrap().push((auth, body));
                    let (code, text) = answers.lock().unwrap().remove(0);
                    (
                        axum::http::StatusCode::from_u16(code).unwrap(),
                        text.to_owned(),
                    )
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/exec", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, seen)
    }

    #[tokio::test]
    async fn the_api_sends_the_command_with_the_token_in_the_header_only() {
        let (url, seen) = fake_api(vec![
            (
                200,
                r#"{"vm_name":"terrarium-x-ex-1","ssh_dest":"terrarium-x-ex-1.exe.xyz","tags":["terrarium-dev-ssh"]}"#,
            ),
            (200, r#"{"vms":[{"vm_name":"terrarium-x-ex-1","tags":["terrarium-dev-ssh"]}]}"#),
            (
                200,
                r#"{"deleted":["terrarium-x-ex-1"],"errors":{},"failed":[],"not_found":[]}"#,
            ),
            (200, r#"{"vms":[]}"#),
        ])
        .await;
        let api = Api::new(&url, Zeroizing::new("exe1.s3cret-token".into())).unwrap();
        assert!(!format!("{api:?}").contains("s3cret"));
        let dest = api
            .new_vm("terrarium-x-ex-1", &ExedevSettings::default())
            .await
            .unwrap();
        assert_eq!(dest, "terrarium-x-ex-1.exe.xyz");
        assert_eq!(api.ls().await.unwrap().len(), 1);
        api.rm(&["terrarium-x-ex-1".into()]).await.unwrap();
        assert!(api.ls().await.unwrap().is_empty());
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        for (auth, body) in &seen {
            assert_eq!(auth, "Bearer exe1.s3cret-token");
            assert!(!body.contains("s3cret"), "{body}");
        }
        assert_eq!(
            seen[0].1,
            "new --name=terrarium-x-ex-1 --sandbox --tag=terrarium-dev-ssh --cpu=2 \
             --memory=4GB --no-email --json"
        );
        assert_eq!(seen[1].1, "ls --json");
        assert_eq!(seen[2].1, "rm terrarium-x-ex-1 --json");
    }

    #[tokio::test]
    async fn api_errors_name_the_status_and_never_the_token() {
        let (url, _) = fake_api(vec![
            (422, r#"{"error":"personal plan allows at most 2 CPUs"}"#),
            (403, "token exe1.s3cret-token may not run rm"),
            (
                200,
                r#"{"deleted":[],"errors":{"x":"busy"},"failed":["terrarium-x-a"],"not_found":[]}"#,
            ),
            (200, r#"{"vm_name":"terrarium-x-a"}"#),
            (200, r#"{"vm_name":"terrarium-x-a","note":"exe1.s3cret-token"}"#),
            (
                200,
                r#"{"deleted":[],"errors":{"terrarium-x-a":"exe1.s3cret-token"},"failed":[],"not_found":[]}"#,
            ),
        ])
        .await;
        let api = Api::new(&url, Zeroizing::new("exe1.s3cret-token".into())).unwrap();
        let s = ExedevSettings::default();
        let e = api.new_vm("terrarium-x-a", &s).await.unwrap_err();
        assert!(
            e.message.contains("HTTP 422") && e.message.contains("2 CPUs"),
            "{}",
            e.message
        );
        let e = api.rm(&["terrarium-x-a".into()]).await.unwrap_err();
        assert!(e.message.contains("HTTP 403"), "{}", e.message);
        assert!(!e.message.contains("s3cret"), "{}", e.message);
        let e = api.rm(&["terrarium-x-a".into()]).await.unwrap_err();
        assert!(e.message.contains("not removed"), "{}", e.message);
        let e = api.new_vm("terrarium-x-a", &s).await.unwrap_err();
        assert!(e.message.contains("no `ssh_dest`"), "{}", e.message);
        // A 2xx `new` answer and the `rm` errors are checked too.
        let e = api.new_vm("terrarium-x-a", &s).await.unwrap_err();
        assert!(
            !e.message.contains("s3cret") && e.message.contains("not shown"),
            "{}",
            e.message
        );
        let e = api.rm(&["terrarium-x-a".into()]).await.unwrap_err();
        assert!(
            !e.message.contains("s3cret") && e.message.contains("not shown"),
            "{}",
            e.message
        );
        // A name or a tag that would need quoting never reaches the API.
        assert!(api.new_vm("a; rm -rf", &s).await.is_err());
        let bad_tag = ExedevSettings {
            tag: "x --all".into(),
            ..ExedevSettings::default()
        };
        let e = api.new_vm("terrarium-x-a", &bad_tag).await.unwrap_err();
        assert!(e.message.contains("bad exe.dev tag"), "{}", e.message);
        assert!(api.rm(&["--all".into()]).await.is_err());
    }
}
