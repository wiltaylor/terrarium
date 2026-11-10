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
//! - The VM name goes into the target's runtime record before `new` runs.
//!   A daemon restart then adopts the VM (or removes it) instead of
//!   leaking it.
//! - The sweep ([`spawn`], every hour, and `POST /api/exedev/sweep`)
//!   removes a VM past its life (and mails the project), a VM that no
//!   running agent holds, and a VM with the Terrarium name prefix
//!   ([`NAME_PREFIX`]) and the target's tag that no target records.
//! - The token is never on a command line or in a log: it goes only into
//!   the `Authorization` header.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
/// The name prefix of each VM that Terrarium makes. The sweep removes only
/// VMs with this prefix.
pub const NAME_PREFIX: &str = "terrarium-x-";
/// The sweep runs this often.
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);
/// A VM younger than this is not removed for having no running holder:
/// its agent may still be starting.
const UNHELD_GRACE_SECS: i64 = 600;
/// How long `up` waits for ssh on a new VM.
const SSH_WAIT: Duration = Duration::from_secs(180);
/// The client timeout of one API call. The API itself stops at 30 s.
const API_TIMEOUT: Duration = Duration::from_secs(45);

/// The VM of an `exedev` target, in its runtime record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vm {
    /// The exe.dev VM name.
    pub name: String,
    /// The ssh host from `new` (`<name>.exe.xyz`). `None`: `new` has not
    /// answered yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_dest: Option<String>,
    /// RFC 3339: when the daemon asked for the VM.
    pub created_at: String,
    /// RFC 3339: the end of its life. The sweep removes it after.
    pub expires_at: String,
}

impl Vm {
    fn expired(&self, now: DateTime<Utc>) -> bool {
        parse_time(&self.expires_at).is_none_or(|t| now >= t)
    }

    fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        parse_time(&self.created_at).map_or(i64::MAX, |t| (now - t).num_seconds())
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

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

/// A new VM name for `target`: [`NAME_PREFIX`], the target name (lower
/// case, at most 20 characters), and 8 random characters.
pub fn new_name(target: &str) -> String {
    let mut t: String = target
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(20)
        .collect();
    t = t.trim_matches('-').to_owned();
    let id = ulid::Ulid::new().to_string().to_ascii_lowercase();
    let tail = &id[id.len() - 8..];
    if t.is_empty() {
        format!("{NAME_PREFIX}{tail}")
    } else {
        format!("{NAME_PREFIX}{t}-{tail}")
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

/// [`ssh_def`] of the VM in the target's runtime record, read now. An
/// error when the target has no VM.
pub fn exec_def(state: &Shared, def: &TargetDef) -> ApiResult<TargetDef> {
    let n = def.name.clone();
    let row = state.db.read_now(move |c| targets::get(c, &n))?;
    exec_def_of(&row)
}

/// [`exec_def`] of a row that is read.
pub fn exec_def_of(row: &targets::TargetRow) -> ApiResult<TargetDef> {
    match &row.runtime.exedev {
        Some(vm) => ssh_def(&row.def, vm),
        None => Err(ApiError::conflict(format!(
            "target `{}` has no exe.dev VM; a holder's start brings it up \
             (POST /api/targets/{}/up)",
            row.def.name, row.def.name
        ))),
    }
}

// ------------------------------------------------------------------ API

/// A VM in the answer of `ls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub tags: Vec<String>,
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
            let mut body: String = text.chars().take(300).collect();
            if body.contains(self.token.as_str()) {
                body = "(the answer is not shown)".into();
            }
            let msg = format!("exe.dev API `{verb}`: HTTP {}: {body}", status.as_u16());
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
                    v.to_string().chars().take(300).collect::<String>()
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
                v["errors"]
            )))
        }
    }
}

/// The VMs of an `ls` answer: `{"vms": [{"vm_name", "tags"}, ...]}`.
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
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The API of a target: its token secret, opened now. The token stays in
/// memory; it is never logged.
pub fn api_of(state: &Shared, def: &TargetDef) -> ApiResult<Api> {
    let name = settings(def).api_token;
    let opened = state.db.read_now(|c| {
        crate::secrets::open_named(c, &state.state_dir, targets::owner(def), &name)
    })?;
    let field =
        crate::secrets::grants::main_field(opened.info.kind, &opened.info.secret_fields)
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

/// One `new` at a time, so the cap holds.
static CREATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn row_of(state: &Shared, target: &str) -> ApiResult<targets::TargetRow> {
    let n = target.to_owned();
    state.db.read(move |c| targets::get(c, &n)).await
}

async fn set_vm(state: &Shared, target: &str, vm: Option<Vm>) -> ApiResult<()> {
    let n = target.to_owned();
    state
        .db
        .write("system", move |t| {
            let mut rt = targets::get(t, &n)?.runtime;
            rt.exedev = vm;
            targets::set_runtime(t, &n, &rt)
        })
        .await
}

/// The VMs that the `exedev` targets record now: `(target, vm)`.
async fn recorded(state: &Shared) -> ApiResult<Vec<(TargetDef, Vm)>> {
    let rows = state.db.read(targets::list).await?;
    Ok(rows
        .into_iter()
        .filter(|r| r.def.kind == TargetKind::Exedev)
        .filter_map(|r| r.runtime.exedev.map(|vm| (r.def, vm)))
        .collect())
}

/// The refusal of a new VM when [`MAX_VMS`] run: `running` are the names
/// that count (recorded by targets or listed with the prefix).
pub fn cap_refusal(running: &BTreeSet<String>) -> Option<String> {
    (running.len() >= MAX_VMS).then(|| {
        format!(
            "exe.dev cap: {} of {MAX_VMS} VMs run ({}); a new one waits until one is \
             removed (agent stop, lease end, or the hourly sweep)",
            running.len(),
            running.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    })
}

/// `up`: adopt the recorded VM when it lives and is in its life, else make
/// a new one. The answer is the new state (`running`).
pub async fn up(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let api = api_of(state, def)?;
    let s = settings(def);
    let now = Utc::now();
    if let Some(vm) = row_of(state, &def.name).await?.runtime.exedev {
        let listed = api.ls().await?;
        let live = listed.iter().any(|l| l.name == vm.name);
        if live && !vm.expired(now) && vm.ssh_dest.is_some() {
            wait_ssh(state, def, &vm).await?;
            info!(target = %def.name, vm = %vm.name, "adopted the exe.dev VM");
            return Ok("running".into());
        }
        if live {
            api.rm(std::slice::from_ref(&vm.name)).await?;
        }
        set_vm(state, &def.name, None).await?;
    }
    let _one = CREATE.lock().await;
    let mut running: BTreeSet<String> = recorded(state)
        .await?
        .into_iter()
        .map(|(_, vm)| vm.name)
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
        name: new_name(&def.name),
        ssh_dest: None,
        created_at: now.to_rfc3339(),
        expires_at: (now + life).to_rfc3339(),
    };
    // The record first: a restart finds the VM even if `new` never answers.
    set_vm(state, &def.name, Some(vm.clone())).await?;
    let dest = match api.new_vm(&vm.name, &s).await {
        Ok(d) => d,
        Err(e) => {
            // `new` can make the VM and still fail (a 504): remove it.
            if api.rm(std::slice::from_ref(&vm.name)).await.is_ok() {
                set_vm(state, &def.name, None).await?;
            }
            return Err(e);
        }
    };
    vm.ssh_dest = Some(dest);
    set_vm(state, &def.name, Some(vm.clone())).await?;
    drop(_one);
    info!(target = %def.name, vm = %vm.name, "made an exe.dev VM");
    if let Err(e) = wait_ssh(state, def, &vm).await {
        let _ = destroy(state, def).await;
        return Err(e);
    }
    Ok("running".into())
}

/// Wait until `true` runs on the VM over ssh.
async fn wait_ssh(state: &Shared, def: &TargetDef, vm: &Vm) -> ApiResult<()> {
    let d = ssh_def(def, vm)?;
    let start = std::time::Instant::now();
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
                last = String::from_utf8_lossy(&c.stderr).chars().take(200).collect();
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

/// `down` and `destroy`: remove the VM. The answer is `absent`.
pub async fn destroy(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    if let Some(vm) = row_of(state, &def.name).await?.runtime.exedev {
        api_of(state, def)?
            .rm(std::slice::from_ref(&vm.name))
            .await?;
        set_vm(state, &def.name, None).await?;
        info!(target = %def.name, vm = %vm.name, "removed the exe.dev VM");
    }
    Ok("absent".into())
}

/// `status`: `running` when the recorded VM is in `ls`, else `absent` (and
/// the record goes).
pub async fn status(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let Some(vm) = row_of(state, &def.name).await?.runtime.exedev else {
        return Ok("absent".into());
    };
    let listed = api_of(state, def)?.ls().await?;
    if listed.iter().any(|l| l.name == vm.name) {
        Ok("running".into())
    } else {
        set_vm(state, &def.name, None).await?;
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
    /// No running agent holds its target.
    Unheld,
    /// In `ls` with the prefix and the tag, but no target records it.
    Leftover,
}

/// One VM for the sweep to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Removal {
    pub vm: String,
    pub target: Option<String>,
    pub reason: Reason,
}

/// A recorded VM as the sweep sees it.
#[derive(Debug, Clone)]
pub struct Record {
    pub target: String,
    pub vm: Vm,
    /// A running agent holds the target.
    pub held: bool,
}

/// What the sweep removes: the recorded VMs past their life or with no
/// running holder (after [`UNHELD_GRACE_SECS`]), and the listed VMs with
/// [`NAME_PREFIX`] and one of `tags` that no target records.
pub fn plan(
    now: DateTime<Utc>,
    records: &[Record],
    listed: &[Listed],
    tags: &BTreeSet<String>,
) -> Vec<Removal> {
    let mut out = Vec::new();
    for r in records {
        let reason = if r.vm.expired(now) {
            Reason::Expired
        } else if !r.held && r.vm.age_secs(now) >= UNHELD_GRACE_SECS {
            Reason::Unheld
        } else {
            continue;
        };
        out.push(Removal {
            vm: r.vm.name.clone(),
            target: Some(r.target.clone()),
            reason,
        });
    }
    let known: BTreeSet<&str> = records.iter().map(|r| r.vm.name.as_str()).collect();
    for l in listed {
        if l.name.starts_with(NAME_PREFIX)
            && l.tags.iter().any(|t| tags.contains(t))
            && !known.contains(l.name.as_str())
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
    // One `ls` for each token secret (by owner and name).
    let mut accounts: BTreeMap<(Option<String>, String), TargetDef> = BTreeMap::new();
    for r in &rows {
        accounts
            .entry((r.def.project.clone(), settings(&r.def).api_token))
            .or_insert_with(|| r.def.clone());
    }
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for def in accounts.values() {
        let api = match api_of(state, def) {
            Ok(a) => a,
            Err(e) => {
                errors.push(format!("target `{}`: {}", def.name, e.message));
                continue;
            }
        };
        let listed = match api.ls().await {
            Ok(l) => l,
            Err(e) => {
                errors.push(format!("target `{}`: {}", def.name, e.message));
                continue;
            }
        };
        // The records after `ls`: a VM that `up` makes now is recorded
        // before its `new`, so it is no leftover.
        let token = settings(def).api_token;
        let same = |d: &TargetDef| d.project == def.project && settings(d).api_token == token;
        let mut records = Vec::new();
        let mut tags = BTreeSet::new();
        for r in state.db.read(targets::list).await? {
            if r.def.kind != TargetKind::Exedev || !same(&r.def) {
                continue;
            }
            tags.insert(settings(&r.def).tag);
            if let Some(vm) = r.runtime.exedev {
                let held = !crate::executor::running_holders(state, &r.def.name, None)
                    .await?
                    .is_empty();
                records.push(Record {
                    target: r.def.name.clone(),
                    vm,
                    held,
                });
            }
        }
        for rm in plan(Utc::now(), &records, &listed, &tags) {
            match api.rm(std::slice::from_ref(&rm.vm)).await {
                Ok(()) => {
                    if let Some(t) = &rm.target {
                        after_removal(state, t, &rm).await;
                    }
                    info!(vm = %rm.vm, reason = ?rm.reason, "the exe.dev sweep removed a VM");
                    removed.push(rm);
                }
                Err(e) => errors.push(format!("rm `{}`: {}", rm.vm, e.message)),
            }
        }
    }
    for e in &errors {
        warn!("exe.dev sweep: {e}");
    }
    Ok(json!({"removed": removed, "errors": errors}))
}

/// After the sweep removed a target's VM: clear the record, set the state,
/// write the audit row, and mail the project (or the operator) for a VM
/// past its life.
async fn after_removal(state: &Shared, target: &str, rm: &Removal) {
    let (t, r) = (target.to_owned(), rm.clone());
    let res = state
        .db
        .write("system", move |tx| {
            let row = targets::get(tx, &t)?;
            if row.runtime.exedev.as_ref().map(|v| v.name.as_str()) != Some(r.vm.as_str()) {
                return Ok(());
            }
            let mut rt = row.runtime.clone();
            rt.exedev = None;
            targets::set_runtime(tx, &t, &rt)?;
            targets::set_state(tx, &t, "absent", None)?;
            let project = row.def.project.clone();
            tx.audit(
                "target.destroy",
                &t,
                project.as_deref(),
                json!({"reason": format!("exedev_{}", serde_json::to_value(r.reason)
                    .ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()),
                       "vm": r.vm}),
            )?;
            if r.reason == Reason::Expired {
                let to = match &project {
                    Some(p) => crate::store::mail::Address::Project(p.clone()),
                    None => crate::store::mail::Address::Operator,
                };
                crate::store::mail::send(
                    tx,
                    crate::store::mail::Outgoing {
                        from: &crate::store::mail::Address::System,
                        to: &to,
                        subject: &format!("exe.dev VM {} removed: past its life", r.vm),
                        body: &format!(
                            "The exe.dev VM `{}` of target `{t}` was past its life \
                             (`[exedev] max_life_secs`, at most 8 hours), so the sweep \
                             removed it. A holder that still runs gets a new VM at its \
                             next start or lease grant.",
                            r.vm
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
        warn!(target, "exe.dev sweep: cannot record the removal: {e}");
    }
}

/// The hourly sweep. The first run is one minute after the start, so a
/// restart finds the VMs that the last daemon left.
pub fn spawn(state: &Shared) -> JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        let mut wait = Duration::from_secs(60);
        loop {
            tokio::time::sleep(wait).await;
            wait = SWEEP_EVERY;
            if let Err(e) = sweep(&state).await {
                warn!("exe.dev sweep failed: {e}");
            }
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
                format!(
                    "name = \"ex\"\nkind = \"exedev\"\ncredential = \"k\"\n{extra}"
                )
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
        let a = new_name("Build_Box");
        let b = new_name("Build_Box");
        assert!(a.starts_with("terrarium-x-build-box-"), "{a}");
        assert_ne!(a, b);
        assert!(plain_word(&a), "{a}");
        assert!(new_name(&"x".repeat(60)).len() <= NAME_PREFIX.len() + 20 + 9);
    }

    #[test]
    fn ssh_def_points_ssh_at_the_vm() {
        let vm = Vm {
            name: "terrarium-x-ex-1".into(),
            ssh_dest: Some("terrarium-x-ex-1.exe.xyz".into()),
            created_at: Utc::now().to_rfc3339(),
            expires_at: Utc::now().to_rfc3339(),
        };
        let d = ssh_def(&def("sync_workspace = true"), &vm).unwrap();
        assert_eq!(d.kind, TargetKind::Ssh);
        assert_eq!(d.address.as_deref(), Some("exedev@terrarium-x-ex-1.exe.xyz"));
        assert!(d.sync_workspace);
        assert_eq!(d.credential.as_deref(), Some("k"));
        let none = Vm {
            ssh_dest: None,
            ..vm
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

    fn vm(name: &str, age: i64, life: i64) -> Vm {
        let now = Utc::now();
        Vm {
            name: name.into(),
            ssh_dest: Some(format!("{name}.exe.xyz")),
            created_at: (now - chrono::Duration::seconds(age)).to_rfc3339(),
            expires_at: (now - chrono::Duration::seconds(age) + chrono::Duration::seconds(life))
                .to_rfc3339(),
        }
    }

    #[test]
    fn the_sweep_plan_removes_expired_unheld_and_leftover_vms_only() {
        let rec = |t: &str, v: Vm, held: bool| Record {
            target: t.into(),
            vm: v,
            held,
        };
        let records = vec![
            rec("a", vm("terrarium-x-a-1", 9 * 3600, 8 * 3600), true),
            rec("b", vm("terrarium-x-b-1", 3600, 8 * 3600), false),
            rec("c", vm("terrarium-x-c-1", 60, 8 * 3600), false),
            rec("d", vm("terrarium-x-d-1", 3600, 8 * 3600), true),
        ];
        let l = |n: &str, tag: &str| Listed {
            name: n.into(),
            tags: vec![tag.into()],
        };
        let listed = vec![
            l("terrarium-x-a-1", "terrarium-dev-ssh"),
            l("terrarium-x-lost-1", "terrarium-dev-ssh"),
            l("terrarium-x-other-1", "someone-else"),
            l("terrarium-ssh2", "terrarium-dev-ssh"),
            l("personal-vm", "terrarium-dev-ssh"),
        ];
        let tags: BTreeSet<String> = ["terrarium-dev-ssh".to_owned()].into();
        let got = plan(Utc::now(), &records, &listed, &tags);
        let got: Vec<(&str, Reason)> = got.iter().map(|r| (r.vm.as_str(), r.reason)).collect();
        assert_eq!(
            got,
            vec![
                ("terrarium-x-a-1", Reason::Expired),
                ("terrarium-x-b-1", Reason::Unheld),
                ("terrarium-x-lost-1", Reason::Leftover),
            ]
        );
    }

    #[test]
    fn parse_ls_reads_names_and_tags() {
        let v = json!({"vms": [
            {"vm_name": "a", "tags": ["terrarium-dev-ssh"], "status": "running"},
            {"name": "b", "tags": ["#x"]},
            {"no_name": true},
        ]});
        assert_eq!(
            parse_ls(&v),
            vec![
                Listed {
                    name: "a".into(),
                    tags: vec!["terrarium-dev-ssh".into()]
                },
                Listed {
                    name: "b".into(),
                    tags: vec!["x".into()]
                },
            ]
        );
        assert!(parse_ls(&json!({"vms": []})).is_empty());
    }

    /// A fake exe.dev API: it checks the bearer token and answers each
    /// command; it keeps the bodies it got.
    async fn fake_api(
        answers: Vec<(u16, &'static str)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
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
        ])
        .await;
        let api = Api::new(&url, Zeroizing::new("exe1.s3cret-token".into())).unwrap();
        let s = ExedevSettings::default();
        let e = api.new_vm("terrarium-x-a", &s).await.unwrap_err();
        assert!(e.message.contains("HTTP 422") && e.message.contains("2 CPUs"), "{}", e.message);
        let e = api.rm(&["terrarium-x-a".into()]).await.unwrap_err();
        assert!(e.message.contains("HTTP 403"), "{}", e.message);
        assert!(!e.message.contains("s3cret"), "{}", e.message);
        let e = api.rm(&["terrarium-x-a".into()]).await.unwrap_err();
        assert!(e.message.contains("not removed"), "{}", e.message);
        let e = api.new_vm("terrarium-x-a", &s).await.unwrap_err();
        assert!(e.message.contains("no `ssh_dest`"), "{}", e.message);
        // A name that would need quoting never reaches the API.
        assert!(api.new_vm("a; rm -rf", &s).await.is_err());
        assert!(api.rm(&["--all".into()]).await.is_err());
    }
}
