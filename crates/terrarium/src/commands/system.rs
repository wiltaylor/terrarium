//! `daemon`, `whoami`, `targets`, `usage`, `audit`, `events`, and the
//! target and host parts of `admin`.

use std::time::Instant;

use serde_json::{Value, json};

use super::{event_line, print_generic, print_table, set_opt};
use crate::api::{ServerError, query};
use crate::cli::{
    AdminTargetCmd, AuditArgs, BackupCmd, DaemonCmd, EventsArgs, HarnessCmd, HostCmd, LinkCmd,
    RemoteCmd, SnapshotArgs, SnapshotCmd, SnapshotOn, SubscriptionCmd, UsageCmd,
};
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::{ApiRequest, Method};

pub fn format_uptime(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s}s"),
        (0, _, _) => format!("{h}h{m}m"),
        _ => format!("{d}d{h}h"),
    }
}

pub async fn daemon(ctx: &Ctx, cmd: &DaemonCmd) -> anyhow::Result<()> {
    match cmd {
        DaemonCmd::Status => {
            let h = ctx.api.get("/api/health", vec![]).await?;
            ctx.emit(&h, |c, h| {
                c.line(format!(
                    "terrariumd {} running at {} (up {})",
                    field(h, "version"),
                    c.api.describe(),
                    format_uptime(h["uptime"].as_u64().unwrap_or(0))
                ));
            })
        }
        DaemonCmd::Backup { command } => backup(ctx, command).await,
        DaemonCmd::Reload => {
            let r = ctx.api.post("/api/reload", None).await?;
            ctx.emit(&r, |c, r| {
                for kind in ["projects", "agents"] {
                    let n = |k: &str| field(&r[kind], k);
                    c.line(format!(
                        "{kind}: added {}, updated {}, removed {}",
                        n("added"),
                        n("updated"),
                        n("removed")
                    ));
                }
                for e in r["errors"].as_array().into_iter().flatten() {
                    c.line(format!(
                        "error: {}",
                        e.as_str().map_or_else(|| e.to_string(), str::to_owned)
                    ));
                }
            })
        }
    }
}

/// `terrarium daemon backup ...` (docs/cli.md, "Backups").
async fn backup(ctx: &Ctx, cmd: &BackupCmd) -> anyhow::Result<()> {
    match cmd {
        BackupCmd::ReleaseRecovery { run } => {
            let r = ctx
                .api
                .post("/api/backups/recovery-release", Some(json!({"run":run})))
                .await?;
            ctx.emit(&r, |c, _| {
                c.line(format!("recovery protection released for {run}"))
            })
        }
        BackupCmd::Create {
            kind,
            work,
            base,
            passphrase_stdin,
        } => {
            let mut body = json!({"kind": kind, "work": work});
            set_opt(&mut body, "base", base.as_deref());
            if *passphrase_stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let p = line.trim_end_matches(['\r', '\n']);
                if p.is_empty() {
                    return Err(crate::api::CliError::usage("the passphrase is empty"));
                }
                body["passphrase"] = json!(p);
            }
            let r = ctx.api.post("/api/backups", Some(body)).await?;
            ctx.emit(&r, |c, r| {
                let sealed = if r["header"]["encryption"].is_object() {
                    "sealed with the passphrase"
                } else {
                    "not sealed: it holds the daemon's secrets in plain form"
                };
                c.line(format!(
                    "{} backup {} ({} bytes, {} files stored); {sealed}",
                    field(&r["header"], "kind"),
                    field(r, "name"),
                    field(r, "size"),
                    field(&r["header"], "stored_files"),
                ));
            })
        }
        BackupCmd::Ls => {
            let r = ctx.api.get("/api/backups", vec![]).await?;
            ctx.emit(&r, |c, r| {
                let items = r.as_array().cloned().unwrap_or_default();
                let items: Vec<Value> = items
                    .into_iter()
                    .map(|mut v| {
                        v["sealed"] = json!(v["encryption"].is_object());
                        v["base_name"] = v["base"]["name"].clone();
                        v
                    })
                    .collect();
                print_table(
                    c,
                    &items,
                    &[
                        ("NAME", "name"),
                        ("KIND", "kind"),
                        ("CREATED", "created_at"),
                        ("SIZE", "size"),
                        ("BASE", "base_name"),
                        ("SEALED", "sealed"),
                    ],
                    "no backups",
                );
            })
        }
        BackupCmd::Download { name, output } => {
            let Some(http) = &ctx.http else {
                return Err(crate::api::CliError::usage(
                    "a download needs the operator CLI (the daemon socket or TERRARIUM_URL)",
                ));
            };
            let out = output.clone().unwrap_or_else(|| name.into());
            let req = ApiRequest {
                method: Method::Get,
                path: format!("/api/backups/{}", crate::transport::encode(name)),
                query: vec![],
                body: None,
            };
            let part = out.with_extension("zip.partial");
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&part)?;
                tokio::fs::File::from_std(f)
            };
            match http.download(&req, &mut f).await {
                Ok(Ok(n)) => {
                    drop(f);
                    std::fs::rename(&part, &out)?;
                    let v = json!({"name": name, "path": out, "size": n});
                    ctx.emit(&v, |c, _| {
                        c.line(format!(
                            "{} ({n} bytes); it holds secrets: keep it safe",
                            out.display()
                        ));
                    })
                }
                Ok(Err(resp)) => {
                    let _ = std::fs::remove_file(&part);
                    Err(ServerError::from_response(&resp).into())
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    Err(e)
                }
            }
        }
    }
}

pub async fn whoami(ctx: &Ctx) -> anyhow::Result<()> {
    let Some(env) = &ctx.agent else {
        let v = json!({"actor": "operator", "endpoint": ctx.api.describe()});
        return ctx.emit(&v, |c, _| c.line("operator"));
    };
    let mut me = ctx.identity().await?.clone();
    // The runner copies the roles when it launches and never sees a later
    // grant. `agent show` reads the daemon row, and so does policy, so the
    // roles come from that same route.
    let row = ctx.api.get(&super::agent_path(&env.agent), vec![]).await?;
    me["roles"] = row["roles"].clone();
    ctx.emit(&me, |c, me| {
        let name = me["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or(&env.agent);
        c.line(format!(
            "agent: {name} ({})\nproject: {}\nharness: {}\nstatus: {}\nroles: {}",
            super::super::output::first_field(me, &["agent_id", "agent"]),
            field(me, "project"),
            field(me, "harness"),
            field(me, "status"),
            field(me, "roles"),
        ));
    })
}

/// `version`: this CLI's build and `GET /api/system/version`. A daemon
/// that cannot answer (older, or down) is an error after the client line.
pub async fn version(ctx: &Ctx) -> anyhow::Result<()> {
    let client = terrarium_version::FULL;
    let daemon = match ctx.api.get("/api/system/version", vec![]).await {
        Ok(d) => d,
        Err(e) => {
            ctx.line(format!("client: terrarium {client}"));
            return Err(e);
        }
    };
    let daemon_version = daemon["version"].as_str().unwrap_or_default().to_owned();
    let stale = terrarium_version::is_stale(client, &daemon_version);
    let v = json!({
        "client": {
            "version": client,
            "git_ref": terrarium_version::GIT_REF,
            "dirty": terrarium_version::DIRTY,
            "build_time": terrarium_version::BUILD_TIME,
        },
        "daemon": daemon,
        "stale": stale,
    });
    ctx.emit(&v, |c, v| {
        c.line(format!(
            "client: terrarium {client} (built {})",
            terrarium_version::BUILD_TIME
        ));
        c.line(format!(
            "daemon: terrariumd {} (built {})",
            field(&v["daemon"], "version"),
            field(&v["daemon"], "build_time")
        ));
        if stale {
            c.line(format!(
                "warning: this CLI is built from another ref than the daemon ({client} vs \
                 {daemon_version}). Install the CLI of the daemon's build"
            ));
        }
        if let Some(missing) = v["daemon"]["missing_templates"].as_array() {
            for key in missing.iter().filter_map(Value::as_str) {
                c.line(format!(
                    "warning: required daemon template {key} is missing"
                ));
            }
        }
    })
}

const TARGET_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("KIND", "kind"),
    ("SCOPE", "scope"),
    ("PROJECT", "project"),
    ("HOST", "host"),
    ("ADDRESS", "address"),
    ("STATE", "state"),
    ("RECREATE", "recreate_pending"),
    ("ONLINE", "online"),
    ("KEY", "key"),
];

/// The STALE cell of `admin host ls`: `STALE` for a host agent built from
/// another ref than the daemon (`stale`, docs/api.md §17), else empty.
fn with_stale_mark(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|h| {
            let mut h = h.clone();
            h["stale_mark"] = if h["stale"] == true { "STALE" } else { "" }.into();
            h
        })
        .collect()
}

/// `host_online` as a word for the table: `online`, `offline`, or `-` (ssh).
/// Also the KEY cell: the credential and the fingerprint of its key.
fn with_online(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|t| {
            let mut t = t.clone();
            t["online"] = match t["host_online"].as_bool() {
                Some(true) => "online".into(),
                Some(false) => "offline".into(),
                None => "-".into(),
            };
            t["key"] = super::ssh_keys::key_cell(&t).into();
            t
        })
        .collect()
}

/// The columns of `targets ls` in agent mode: also how the agent holds
/// each target (`policy` or `lease`), and the lease's flag and end.
const AGENT_TARGET_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("KIND", "kind"),
    ("SCOPE", "scope"),
    ("HOST", "host"),
    ("STATE", "state"),
    ("RECREATE", "recreate_pending"),
    ("ONLINE", "online"),
    ("ACCESS", "access"),
    ("EXCLUSIVE", "exclusive"),
    ("EXPIRES", "expires"),
];

/// The EXCLUSIVE and EXPIRES cells of a lease, the same in every table
/// (`targets ls` in agent mode, `admin target leases`, `agent leases`):
/// `yes`/`no`, and for EXPIRES: `revoked` for a lease that a revoke
/// ended, `expired` for one that expired, else the RFC 3339 expiry, or
/// `until revoked` for an active lease with no end. No lease: `-` for both.
pub fn lease_cells(lease: Option<&Value>) -> (String, String) {
    let Some(l) = lease.filter(|l| l.is_object()) else {
        return ("-".into(), "-".into());
    };
    let exclusive = if l["exclusive"] == true { "yes" } else { "no" };
    let ended = l["end_reason"].as_str().or(l["state"].as_str());
    let expires = match (ended, l["expires_at"].as_str()) {
        (Some("revoked"), _) => "revoked",
        (Some("expired"), _) => "expired",
        (_, Some(t)) => t,
        (_, None) => "until revoked",
    };
    (exclusive.into(), expires.into())
}

/// The lease fields of a target for the agent table.
fn with_access(items: Vec<Value>) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut t| {
            let (exclusive, expires) = lease_cells(t.get("lease"));
            t["exclusive"] = exclusive.into();
            t["expires"] = expires.into();
            t
        })
        .collect()
}

/// `targets ls`: an agent sees the targets its policy or a lease lets it
/// use; the operator sees all, or a project's with `-p`.
pub async fn targets(ctx: &Ctx) -> anyhow::Result<()> {
    let (path, q) = if ctx.is_agent() {
        (
            "/api/targets".to_owned(),
            query([("usable", Some("true".to_owned()))]),
        )
    } else {
        match &ctx.project {
            Some(p) => (
                format!("/api/projects/{}/targets", crate::transport::encode(p)),
                vec![],
            ),
            None => ("/api/targets".into(), vec![]),
        }
    };
    let items = ctx.api.list(&path, q, None).await?;
    let v = Value::Array(items);
    let agent = ctx.is_agent();
    ctx.emit(&v, |c, v| {
        let rows = with_recreate_pending(with_online(as_items(v)));
        if agent {
            print_table(c, &with_access(rows), AGENT_TARGET_COLS, "no targets");
        } else {
            print_table(c, &rows, TARGET_COLS, "no targets");
        }
    })
}

/// `admin target ls`: in agent mode, every target of the agent's project
/// and the global ones (not only the usable ones), with the scope; else as
/// `targets ls`.
pub async fn admin_targets(ctx: &Ctx) -> anyhow::Result<()> {
    if !ctx.is_agent() {
        return targets(ctx).await;
    }
    let project = ctx.project_or(None).await?;
    let path = format!(
        "/api/projects/{}/targets",
        crate::transport::encode(&project)
    );
    let items = ctx.api.list(&path, vec![], None).await?;
    let v = Value::Array(items);
    ctx.emit(&v, |c, v| {
        let rows = with_access(with_recreate_pending(with_online(as_items(v))));
        print_table(c, &rows, AGENT_TARGET_COLS, "no targets");
    })
}

fn with_recreate_pending(items: Vec<Value>) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut t| {
            t["recreate_pending"] = if t["runtime"]["recreate_required"] == true {
                "pending"
            } else {
                "-"
            }
            .into();
            t
        })
        .collect()
}

const LEASE_COLS: &[(&str, &str)] = &[
    ("TARGET", "target"),
    ("AGENT", "agent_name"),
    ("STATE", "state"),
    ("EXCLUSIVE", "exclusive"),
    ("EXPIRES", "expires"),
    ("GRANTED BY", "granted_by"),
    ("REASON", "reason"),
];

/// A lease list as a table.
pub fn print_leases(ctx: &Ctx, v: &Value) {
    let rows: Vec<Value> = as_items(v)
        .iter()
        .map(|l| {
            let mut l = l.clone();
            let (exclusive, expires) = lease_cells(Some(&l));
            l["exclusive"] = exclusive.into();
            l["expires"] = expires.into();
            l
        })
        .collect();
    print_table(ctx, &rows, LEASE_COLS, "no leases");
}

/// `until <time>` or `until revoked`, for a lease.
fn until(l: &Value) -> String {
    match l["expires_at"].as_str() {
        Some(t) => format!("until {t}"),
        None => "until revoked".into(),
    }
}

fn lease_path(target: &str, agent: Option<&str>) -> String {
    let base = format!("/api/targets/{}/leases", crate::transport::encode(target));
    match agent {
        Some(a) => format!("{base}/{}", crate::transport::encode(a)),
        None => base,
    }
}

fn all_query(all: bool) -> Vec<(String, String)> {
    query([("all", all.then(|| "true".to_owned()))])
}

/// The snapshot commands of `targets` and `admin target` (docs/api.md
/// §17.3).
pub async fn snapshot(ctx: &Ctx, cmd: &SnapshotCmd) -> anyhow::Result<()> {
    let base = |t: &str| format!("/api/targets/{}", crate::transport::encode(t));
    let on_query =
        |on: &SnapshotOn| query([("scope", on.scope.clone()), ("agent", on.agent.clone())]);
    match cmd {
        SnapshotCmd::Snapshot(a) => snapshot_verb(ctx, "snapshot", a, false).await,
        SnapshotCmd::Restore {
            snap,
            discard_guest_changes,
        } => snapshot_verb(ctx, "restore", snap, *discard_guest_changes).await,
        SnapshotCmd::Snapshots { target, on } => {
            let v = ctx
                .api
                .get(&format!("{}/snapshots", base(target)), on_query(on))
                .await?;
            ctx.emit(&v, |c, v| print_snapshots(c, target, v))
        }
        SnapshotCmd::SnapshotRm {
            target,
            name,
            on,
            force,
        } => {
            let mut q = on_query(on);
            if *force {
                q.push(("force".into(), "true".into()));
            }
            let path = format!(
                "{}/snapshots/{}",
                base(target),
                crate::transport::encode(name)
            );
            let v = ctx.api.delete(&path, q).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{target}: deleted snapshot {name} on {}",
                    v["machines"]
                        .as_array()
                        .map(|m| m
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default()
                ));
            })
        }
    }
}

/// `POST /api/targets/{name}/snapshot|restore`.
async fn snapshot_verb(
    ctx: &Ctx,
    verb: &str,
    a: &SnapshotArgs,
    discard: bool,
) -> anyhow::Result<()> {
    let mut body = json!({});
    set_opt(&mut body, "name", a.name.clone());
    set_opt(&mut body, "scope", a.on.scope.clone());
    set_opt(&mut body, "agent", a.on.agent.clone());
    if discard {
        body["discard_guest_changes"] = true.into();
    }
    let body = (body != json!({})).then_some(body);
    let path = format!(
        "/api/targets/{}/{verb}",
        crate::transport::encode(&a.target)
    );
    let v = ctx.api.post(&path, body).await?;
    let done = if verb == "snapshot" {
        "took"
    } else {
        "restored"
    };
    ctx.emit(&v, |c, v| {
        c.line(format!(
            "{}: {done} snapshot {} ({}): {}",
            a.target,
            field(v, "snapshot"),
            field(v, "scope"),
            field(v, "state")
        ));
    })
}

/// The table of `targets snapshots`.
fn print_snapshots(c: &Ctx, target: &str, v: &Value) {
    if v["lab_running"] == false {
        c.line(format!(
            "the lab of {target} does not run: vmlab names no machines"
        ));
        return;
    }
    let rows: Vec<Value> = v["snapshots"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut x| {
            x["managed"] = if x["managed"] == true { "yes" } else { "-" }.into();
            let ms: Vec<&str> = x["machines"]
                .as_array()
                .map(|m| m.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            x["machines"] = ms.join(",").into();
            x
        })
        .collect();
    print_table(
        c,
        &rows,
        &[
            ("NAME", "name"),
            ("KIND", "kind"),
            ("TAKEN", "taken"),
            ("MACHINES", "machines"),
            ("MANAGED", "managed"),
        ],
        &format!("no snapshots on {target}"),
    );
    if let Some(d) = v["dropped"].as_array().filter(|d| !d.is_empty()) {
        let d: Vec<&str> = d.iter().filter_map(Value::as_str).collect();
        c.line(format!(
            "the host no longer has: {} (removed from the target's snapshots)",
            d.join(", ")
        ));
    }
}

/// The `limits` object of the limit flags, or `None` when none is given.
/// The daemon checks the values (`422 invalid`).
pub fn limits_body(a: &crate::cli::LimitArgs) -> Option<Value> {
    let mut l = json!({});
    if let Some(c) = a.cpu {
        // `2` goes as an integer, `0.5` as a float.
        #[allow(clippy::cast_possible_truncation)]
        let v: Value = if c.fract() == 0.0 && c.abs() < 1e15 {
            (c as i64).into()
        } else {
            c.into()
        };
        l["cpu"] = v;
    }
    set_opt(&mut l, "memory", a.memory.clone());
    set_opt(&mut l, "swap", a.swap.clone());
    set_opt(&mut l, "pids", a.pids);
    set_opt(&mut l, "io_weight", a.io_weight);
    (l != json!({})).then_some(l)
}

pub async fn admin_target(ctx: &Ctx, cmd: &AdminTargetCmd) -> anyhow::Result<()> {
    match cmd {
        AdminTargetCmd::Ls => admin_targets(ctx).await,
        AdminTargetCmd::Snapshot(c) => snapshot(ctx, c).await,
        AdminTargetCmd::New(a) => {
            let crate::cli::AdminTargetNewArgs {
                name,
                kind,
                address,
                port,
                credential,
                sync_workspace,
                username,
                domain,
                display,
                display_name,
                host,
                image,
                container,
                lab,
                machine,
                lab_repo,
                lab_ref,
                lab_path,
                lab_from_workspace,
                ephemeral,
                keep,
                global,
                limits,
            } = &**a;
            let mut body = json!({"name": name, "kind": kind});
            set_opt(&mut body, "address", address.clone());
            set_opt(&mut body, "port", *port);
            set_opt(&mut body, "credential", credential.clone());
            if *sync_workspace {
                body["sync_workspace"] = true.into();
            }
            set_opt(&mut body, "username", username.clone());
            set_opt(&mut body, "domain", domain.clone());
            set_opt(&mut body, "display", display.clone());
            set_opt(&mut body, "display_name", display_name.clone());
            set_opt(&mut body, "host", host.clone());
            set_opt(&mut body, "image", image.clone());
            set_opt(&mut body, "container", container.clone());
            set_opt(&mut body, "lab", lab.clone());
            set_opt(&mut body, "machine", machine.clone());
            set_opt(&mut body, "lab_repo", lab_repo.clone());
            set_opt(&mut body, "lab_ref", lab_ref.clone());
            set_opt(&mut body, "lab_path", lab_path.clone());
            if *lab_from_workspace {
                body["lab_from_workspace"] = true.into();
            }
            if *ephemeral {
                body["ephemeral"] = true.into();
            }
            if *keep {
                body["keep"] = true.into();
            }
            if let Some(l) = limits_body(limits) {
                body["limits"] = l;
            }
            if !global {
                body["project"] = ctx.project_or(None).await?.into();
            }
            let t = ctx.api.post("/api/targets", Some(body)).await?;
            ctx.emit(&t, |c, t| {
                let made = field(t, "name");
                if made == *name {
                    c.line(format!("created target {made}"));
                } else {
                    c.line(format!(
                        "created target {made} (a project target gets the project prefix)"
                    ));
                }
            })
        }
        AdminTargetCmd::Edit(a) => {
            let mut body = json!({});
            if a.sync_workspace || a.no_sync_workspace {
                body["sync_workspace"] = a.sync_workspace.into();
            }
            set_opt(&mut body, "lab_path", a.lab_path.clone());
            set_opt(&mut body, "lab_ref", a.lab_ref.clone());
            set_opt(&mut body, "machine", a.machine.clone());
            set_opt(&mut body, "image", a.image.clone());
            set_opt(&mut body, "host", a.host.clone());
            if a.ephemeral || a.permanent {
                body["ephemeral"] = a.ephemeral.into();
            }
            if a.keep || a.no_keep {
                body["keep"] = a.keep.into();
            }
            set_opt(&mut body, "snapshot_before_task", a.snapshot_before_task);
            if a.clear_work_root {
                body["work_root"] = Value::Null;
            } else {
                set_opt(&mut body, "work_root", a.work_root.clone());
            }
            if a.no_limits {
                body["limits"] = Value::Null;
            } else if let Some(l) = limits_body(&a.limits) {
                // A merge patch: the limits that are not given stay.
                body["limits"] = l;
            }
            if body == json!({}) {
                return Err(crate::api::CliError::usage(
                    "give a change: --lab-path, --lab-ref, --machine, --image, --host, \
                     --ephemeral, --permanent, --keep, --no-keep, --snapshot-before-task, \
                     --work-root, --clear-work-root, --sync-workspace, --no-sync-workspace, \
                     --cpu, --memory, --swap, --pids, --io-weight, or --no-limits",
                ));
            }
            let path = format!("/api/targets/{}", crate::transport::encode(&a.name));
            let t = ctx.api.patch(&path, body.clone()).await?;
            ctx.emit(&t, |c, t| {
                let keys: Vec<&str> = body
                    .as_object()
                    .map(|o| o.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                c.line(format!(
                    "changed target {}: {}",
                    field(t, "name"),
                    keys.join(", ")
                ));
                if let Some(note) = t["recreate_note"].as_str() {
                    c.line(note);
                    c.line(format!(
                        "run: terrarium admin target verb {} recreate",
                        a.name
                    ));
                }
            })
        }
        AdminTargetCmd::Rm { name } => {
            let path = format!("/api/targets/{}", crate::transport::encode(name));
            let v = ctx.api.delete(&path, vec![]).await?;
            ctx.emit(&v, |c, _| c.line(format!("removed target {name}")))
        }
        AdminTargetCmd::Verb {
            name,
            verb,
            snapshot,
            agent,
            force,
            discard_volumes,
        } => {
            if (*force || !discard_volumes.is_empty()) && verb != "recreate" {
                return Err(crate::api::CliError::usage(
                    "--force and --discard-volumes require recreate",
                ));
            }
            if !["up", "down", "snapshot", "restore", "status", "recreate"].contains(&verb.as_str())
            {
                return Err(crate::api::CliError::usage(format!(
                    "unknown verb `{verb}`: use up, down, snapshot, restore, status, or recreate"
                )));
            }
            let path = format!("/api/targets/{}/{verb}", crate::transport::encode(name));
            let mut body = json!({});
            set_opt(&mut body, "name", snapshot.clone());
            set_opt(&mut body, "agent", agent.clone());
            if *force {
                body["force"] = true.into();
            }
            if !discard_volumes.is_empty() {
                body["discard_volumes"] = json!(discard_volumes);
            }
            let body = (body != json!({})).then_some(body);
            let v = ctx.api.post(&path, body).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("{name}: {verb}: {}", field(v, "state")));
                if let Some(note) = v["recreate_note"].as_str() {
                    c.line(format!("warning: {note}"));
                }
                if let Some(e) = v["attach_error"].as_str() {
                    c.line(format!("warning: {e}"));
                }
            })
        }
        AdminTargetCmd::Grant {
            target,
            agent,
            until: end,
            exclusive,
            reason,
            no_lifecycle_check,
        } => {
            let mut body = json!({"agent": agent});
            set_opt(&mut body, "until", end.clone());
            if *exclusive {
                body["exclusive"] = true.into();
            }
            set_opt(&mut body, "reason", reason.clone());
            let v = ctx.api.post(&lease_path(target, None), Some(body)).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "granted {target} to {} {}{}",
                    field(v, "agent_name"),
                    until(v),
                    if v["exclusive"] == true {
                        " (exclusive)"
                    } else {
                        ""
                    }
                ));
                if let Some(e) = v["lifecycle_error"].as_str() {
                    c.line(format!("warning: the target did not come up: {e}"));
                }
            })?;
            // The lease stands, but the target is not up: a non-zero exit
            // stops an `&&` chain before the `card assign`.
            match v["lifecycle_error"].as_str() {
                Some(e) if !no_lifecycle_check => Err(anyhow::anyhow!(
                    "the lease on {target} is granted, but the target did not come up: {e}. \
                     Fix the target (`terrarium admin target edit`), then revoke the lease \
                     and grant it again"
                )),
                _ => Ok(()),
            }
        }
        AdminTargetCmd::Revoke { target, agent } => {
            let v = ctx
                .api
                .delete(&lease_path(target, Some(agent)), vec![])
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "revoked the lease of {} on {target}",
                    field(v, "agent_name")
                ));
            })
        }
        AdminTargetCmd::Extend {
            target,
            agent,
            until: end,
        } => {
            let end = Some(end.clone()).filter(|u| u != "none");
            let v = ctx
                .api
                .patch(&lease_path(target, Some(agent)), json!({"until": end}))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "the lease of {} on {target} lasts {}",
                    field(v, "agent_name"),
                    until(v)
                ));
            })
        }
        AdminTargetCmd::Leases { target, all } => {
            let v = Value::Array(
                ctx.api
                    .list(&lease_path(target, None), all_query(*all), None)
                    .await?,
            );
            ctx.emit(&v, print_leases)
        }
    }
}

pub async fn hosts(ctx: &Ctx, cmd: &HostCmd) -> anyhow::Result<()> {
    match cmd {
        HostCmd::Ls => {
            let v = Value::Array(ctx.api.list("/api/hosts", vec![], None).await?);
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    &with_stale_mark(as_items(v)),
                    &[
                        ("NAME", "name"),
                        ("STATUS", "status"),
                        ("OS", "os"),
                        ("ARCH", "arch"),
                        ("VERSION", "version"),
                        ("STALE", "stale_mark"),
                        ("SESSIONS", "sessions"),
                        ("LAST SEEN", "last_seen"),
                        ("OFFLINE REASON", "offline_reason"),
                    ],
                    "no host agents",
                )
            })
        }
        HostCmd::Show { name } => {
            let path = format!("/api/hosts/{}", crate::transport::encode(name));
            let v = ctx.api.get(&path, vec![]).await?;
            ctx.emit(&v, print_host)
        }
        HostCmd::New { name, enrol } => {
            let mut body = json!({"name": name});
            if *enrol {
                body["enrol"] = true.into();
            }
            let v = ctx.api.post("/api/hosts", Some(body)).await?;
            ctx.emit(&v, print_token)
        }
        HostCmd::Rotate { name, enrol } => {
            let path = format!("/api/hosts/{}/rotate", crate::transport::encode(name));
            let body = enrol.then(|| json!({"enrol": true}));
            let v = ctx.api.post(&path, body).await?;
            ctx.emit(&v, print_token)
        }
        HostCmd::Rm { name } => {
            let path = format!("/api/hosts/{}", crate::transport::encode(name));
            let v = ctx.api.delete(&path, vec![]).await?;
            ctx.emit(&v, |c, _| c.line(format!("revoked host agent {name}")))
        }
    }
}

/// `admin link ...`: the link keys of this daemon (docs/api.md §36). The
/// pairing code prints alone on stdout; the long-lived key never reaches
/// the CLI (the home gets it from `POST /api/links/pair`).
pub async fn remotes(ctx: &Ctx, cmd: &RemoteCmd) -> anyhow::Result<()> {
    fn code() -> anyhow::Result<String> {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim().to_owned())
    }
    fn ca(file: &Option<std::path::PathBuf>) -> anyhow::Result<Option<String>> {
        file.as_ref()
            .map(std::fs::read_to_string)
            .transpose()
            .map_err(Into::into)
    }
    let v = match cmd {
        RemoteCmd::Ls => ctx.api.get("/api/remotes", vec![]).await?,
        RemoteCmd::Add {
            name,
            remote_url,
            instance_id,
            pin_sha256,
            ca_file,
            label,
        } => {
            ctx.api
                .post(
                    "/api/remotes",
                    Some(json!({
                        "name": name, "url": remote_url, "instance_id": instance_id,
                        "pin_sha256": pin_sha256, "ca_pem": ca(ca_file)?, "label": label,
                        "code": code()?,
                    })),
                )
                .await?
        }
        RemoteCmd::Rm { name } => {
            ctx.api
                .delete(
                    &format!("/api/remotes/{}", crate::transport::encode(name)),
                    vec![],
                )
                .await?
        }
        RemoteCmd::Repin {
            name,
            pin_sha256,
            ca_file,
        } => {
            ctx.api
                .post(
                    &format!("/api/remotes/{}/repin", crate::transport::encode(name)),
                    Some(json!({"pin_sha256": pin_sha256, "ca_pem": ca(ca_file)?})),
                )
                .await?
        }
        RemoteCmd::ArtifactRepin {
            name,
            origin,
            pin_sha256,
        } => {
            ctx.api
                .post(
                    &format!(
                        "/api/remotes/{}/artifact-repin",
                        crate::transport::encode(name)
                    ),
                    Some(json!({"origin": origin, "pin_sha256": pin_sha256})),
                )
                .await?
        }
        RemoteCmd::Pair { name } => {
            ctx.api
                .post(
                    &format!("/api/remotes/{}/pair", crate::transport::encode(name)),
                    Some(json!({"code": code()?})),
                )
                .await?
        }
    };
    ctx.emit(&v, |c, v| print_generic(c, v, "no remotes"))
}

pub async fn links(ctx: &Ctx, cmd: &LinkCmd) -> anyhow::Result<()> {
    match cmd {
        LinkCmd::Ls { all } => {
            let q = if *all {
                vec![("revoked".to_owned(), "true".to_owned())]
            } else {
                vec![]
            };
            let v = Value::Array(ctx.api.list("/api/links", q, None).await?);
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    as_items(v),
                    &[
                        ("ID", "id"),
                        ("LABEL", "label"),
                        ("STATUS", "status"),
                        ("READ ONLY", "read_only"),
                        ("HOME", "home_name"),
                        ("LAST USED", "last_used_at"),
                        ("FROM", "last_used_from"),
                    ],
                    "no links",
                )
            })
        }
        LinkCmd::Show { id } => {
            let path = format!("/api/links/{}", crate::transport::encode(id));
            let v = ctx.api.get(&path, vec![]).await?;
            ctx.emit(&v, |c, v| print_generic(c, v, "no link"))
        }
        LinkCmd::New { label, read_only } => {
            let body = json!({"label": label, "read_only": read_only});
            let v = ctx.api.post("/api/links", Some(body)).await?;
            ctx.emit(&v, print_pairing_code)
        }
        LinkCmd::Rotate { id } => {
            let path = format!("/api/links/{}/rotate", crate::transport::encode(id));
            let v = ctx.api.post(&path, None).await?;
            ctx.emit(&v, print_pairing_code)
        }
        LinkCmd::Revoke { id } => {
            let path = format!("/api/links/{}", crate::transport::encode(id));
            let v = ctx.api.delete(&path, vec![]).await?;
            ctx.emit(&v, |c, _| c.line(format!("revoked link {id}")))
        }
    }
}

/// The pairing code alone on stdout; on stderr the link, the expiry, and
/// what the operator types and compares on the home.
fn print_pairing_code(ctx: &Ctx, v: &Value) {
    ctx.line(field(v, "code"));
    let link = &v["link"];
    let inst = &v["instance"];
    let text = format!(
        "pairing code for link {} ({}{}): one use, expires {}\n\
         on the home, add a remote with:\n  url: {}\n  the code above\n\
         then compare what the home shows with:\n  instance: {} ({}, {})\n  \
         the TLS certificate SHA-256 of your reverse proxy\n",
        field(link, "label"),
        field(link, "id"),
        if link["read_only"] == true {
            ", read-only"
        } else {
            ""
        },
        field(v, "code_expires_at"),
        match v["url"].as_str() {
            Some(u) => u.to_owned(),
            None => "<this daemon's https URL> (set TERRARIUM_PUBLIC_URL to print it)".to_owned(),
        },
        field(inst, "id"),
        field(inst, "name"),
        field(inst, "version"),
    );
    ctx.out.write_bytes(true, text.as_bytes());
}

/// `admin harness ls`: `GET /api/harnesses`; `admin harness update`:
/// `POST /api/harnesses/{name}/update` or `POST /api/harnesses/update`;
/// `pin`, `unpin`, and `check`: `POST /api/harnesses/{name}/pin`,
/// `.../unpin`, and `POST /api/harnesses/check`.
pub async fn harnesses(ctx: &Ctx, cmd: &HarnessCmd) -> anyhow::Result<()> {
    match cmd {
        HarnessCmd::Ls { refresh } => {
            let q = if *refresh {
                vec![("refresh".to_owned(), "true".to_owned())]
            } else {
                vec![]
            };
            let v = ctx.api.get("/api/harnesses", q).await?;
            ctx.emit(&v, print_harnesses)
        }
        HarnessCmd::Pin { name, version } => {
            let body = match version {
                Some(v) => serde_json::json!({ "version": v }),
                None => serde_json::json!({}),
            };
            let path = format!("/api/harnesses/{}/pin", crate::transport::encode(name));
            let v = ctx.api.post(&path, Some(body)).await?;
            ctx.emit(&v, print_pin)
        }
        HarnessCmd::Unpin { name } => {
            let path = format!("/api/harnesses/{}/unpin", crate::transport::encode(name));
            let v = ctx.api.post(&path, None).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{}: auto-update on (stays at {} until the next check)",
                    field(v, "harness"),
                    field(v, "to")
                ));
            })
        }
        HarnessCmd::Check => {
            let v = ctx.api.post("/api/harnesses/check", None).await?;
            ctx.emit(&v, print_check)
        }
        HarnessCmd::Update {
            name,
            all,
            version,
            force,
        } => {
            let mut body = serde_json::Map::new();
            if let Some(v) = version {
                body.insert("version".into(), Value::String(v.clone()));
            }
            if *force {
                body.insert("force".into(), Value::Bool(true));
            }
            let path = match (name, all) {
                (Some(n), false) => {
                    format!("/api/harnesses/{}/update", crate::transport::encode(n))
                }
                _ => "/api/harnesses/update".to_owned(),
            };
            let r = ctx.api.post(&path, Some(Value::Object(body))).await;
            match r {
                Ok(v) => ctx.emit(&v, print_update),
                Err(e) => {
                    // npm's output of a failed install, before the error.
                    if !ctx.json
                        && let Some(se) = e.downcast_ref::<crate::api::ServerError>()
                        && let Some(d) = &se.details
                    {
                        print_failed_update(ctx, d);
                        // The generic `details:` line must not print the
                        // same output and names again.
                        let mut se = se.clone();
                        se.details = without_printed_details(d);
                        return Err(se.into());
                    }
                    Err(e)
                }
            }
        }
    }
}

pub async fn subscriptions(ctx: &Ctx, cmd: &SubscriptionCmd) -> anyhow::Result<()> {
    match cmd {
        SubscriptionCmd::Ls => {
            let v = ctx.api.get("/api/subscriptions", vec![]).await?;
            ctx.emit(&v, print_subscriptions)
        }
        SubscriptionCmd::Default { name } => {
            let path = format!(
                "/api/subscriptions/{}/default",
                crate::transport::encode(name)
            );
            let v = ctx.api.post(&path, None).await?;
            ctx.emit(&v, |c, v| {
                let harness = v["account"]["harness"].as_str().unwrap_or("its harness");
                c.line(format!(
                    "{} is the default {harness} account",
                    field(v, "name")
                ));
            })
        }
        SubscriptionCmd::Show { name } => {
            let v = ctx.api.get(&sub_path(name, ""), vec![]).await?;
            ctx.emit(&v, print_subscription)
        }
        SubscriptionCmd::New(a) => {
            let mut body = json!({ "name": a.name, "provider": a.provider });
            set_opt(&mut body, "plan", a.plan.as_deref());
            set_opt(&mut body, "probe", a.probe.as_deref());
            if let Some(n) = a.alert_at {
                body["alert_at"] = json!(n);
            }
            if !a.harnesses.is_empty() {
                body["harnesses"] = json!(a.harnesses);
            }
            let server = selfhosted_body(&a.server);
            if server.as_object().is_some_and(|m| !m.is_empty()) {
                body["selfhosted"] = server;
            }
            let key = if a.key_stdin {
                Some(read_key(ctx)?)
            } else {
                None
            };
            let mut v = ctx.api.post("/api/subscriptions", Some(body)).await?;
            if let Some(k) = key {
                v = store_key(ctx, &a.name, &v, k).await?;
            }
            ctx.emit(&v, print_subscription)
        }
        SubscriptionCmd::Set(a) => {
            let mut body = json!({});
            if let Some(n) = a.alert_at {
                body["alert_at"] = json!(n);
            }
            if !a.harnesses.is_empty() {
                body["harnesses"] = json!(a.harnesses);
            }
            let mut server = selfhosted_body(&a.server);
            for (clear, key) in [
                (a.clear_default_model, "default_model"),
                (a.clear_context_window, "context_window"),
                (a.clear_max_output_tokens, "max_output_tokens"),
                (a.clear_max_concurrent, "max_concurrent"),
                (a.clear_status_url, "status_url"),
            ] {
                if clear {
                    server[key] = Value::Null;
                }
            }
            if server.as_object().is_some_and(|m| !m.is_empty()) {
                body["selfhosted"] = server;
            }
            let key = if a.key_stdin {
                Some(read_key(ctx)?)
            } else {
                None
            };
            let mut v = if body.as_object().is_some_and(|m| !m.is_empty()) {
                ctx.api.patch(&sub_path(&a.name, ""), body).await?
            } else {
                ctx.api.get(&sub_path(&a.name, ""), vec![]).await?
            };
            if a.budget_usd.is_some() || a.clear_budget_usd {
                let mut cloud = v["cloud"].clone();
                if !cloud.is_object() {
                    cloud = json!({});
                }
                cloud["budget_usd"] = json!(a.budget_usd);
                v = ctx.api.put(&sub_path(&a.name, "/cloud"), cloud).await?;
            }
            if let Some(k) = key {
                v = store_key(ctx, &a.name, &v, k).await?;
            }
            ctx.emit(&v, print_subscription)
        }
        SubscriptionCmd::Enable { name } => {
            let v = ctx.api.post(&sub_path(name, "/enable"), None).await?;
            ctx.emit(&v, |c, v| c.line(format!("{}: enabled", field(v, "name"))))
        }
        SubscriptionCmd::Disable { name } => {
            let v = ctx.api.post(&sub_path(name, "/disable"), None).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{}: disabled (agents that run go on; none starts or wakes)",
                    field(v, "name")
                ));
            })
        }
        SubscriptionCmd::StopAll { name, disable } => {
            let v = ctx
                .api
                .post(
                    &sub_path(name, "/stop-all"),
                    Some(json!({ "disable": disable })),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                let list = v["stopped"].as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    c.line("no agent of the subscription was running");
                }
                for a in &list {
                    match a["error"].as_str() {
                        Some(e) => c.line(format!(
                            "{}/{}: not stopped: {e}",
                            field(a, "project"),
                            field(a, "name")
                        )),
                        None => c.line(format!(
                            "{}/{}: stopping",
                            field(a, "project"),
                            field(a, "name")
                        )),
                    }
                }
                if v["subscription"]["enabled"].as_bool() == Some(false) {
                    c.line(format!("{}: disabled", field(&v["subscription"], "name")));
                }
            })
        }
        SubscriptionCmd::FetchModels {
            subscription,
            base_url,
            api,
            key_stdin,
            refresh,
        } => {
            let mut body = json!({ "refresh": refresh });
            set_opt(&mut body, "subscription", subscription.as_deref());
            set_opt(&mut body, "base_url", base_url.as_deref());
            set_opt(&mut body, "api", api.as_deref());
            if *key_stdin {
                body["key"] = json!(read_key(ctx)?);
            }
            let v = ctx
                .api
                .post("/api/subscriptions/fetch-models", Some(body))
                .await?;
            ctx.emit(&v, |c, v| {
                for m in v["models"].as_array().cloned().unwrap_or_default() {
                    c.line(field(&m, "id"));
                }
            })
        }
        SubscriptionCmd::Label { name, label, clear } => {
            let label = if *clear { None } else { label.clone() };
            let v = ctx
                .api
                .put(&sub_path(name, "/label"), json!({ "label": label }))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{}: {}",
                    field(v, "name"),
                    field(v, "display_name")
                ));
            })
        }
    }
}

/// `/api/subscriptions/<name><rest>`.
fn sub_path(name: &str, rest: &str) -> String {
    format!(
        "/api/subscriptions/{}{rest}",
        crate::transport::encode(name)
    )
}

/// The first line of stdin, as an API key.
fn read_key(ctx: &Ctx) -> anyhow::Result<String> {
    let text = ctx.text("-")?;
    let key = text.lines().next().unwrap_or_default().trim().to_owned();
    if key.is_empty() {
        return Err(crate::api::CliError::usage("no key on stdin"));
    }
    Ok(key)
}

/// Store `key` on subscription `name` (`v` is its view: the provider).
async fn store_key(ctx: &Ctx, name: &str, v: &Value, key: String) -> anyhow::Result<Value> {
    let body = json!({ "provider": v["provider"], "key": key });
    ctx.api.post(&sub_path(name, "/api-key"), Some(body)).await
}

/// The `selfhosted` object of the server flags; only the flags given.
fn selfhosted_body(a: &crate::cli::SelfHostedArgs) -> Value {
    let mut s = json!({});
    set_opt(&mut s, "api", a.api.as_deref());
    set_opt(&mut s, "base_url", a.base_url.as_deref());
    set_opt(&mut s, "default_model", a.default_model.as_deref());
    set_opt(&mut s, "status_url", a.status_url.as_deref());
    if !a.models.is_empty() {
        s["models"] = json!(a.models);
    }
    if let Some(n) = a.context_window {
        s["context_window"] = json!(n);
    }
    if let Some(n) = a.max_output_tokens {
        s["max_output_tokens"] = json!(n);
    }
    if let Some(n) = a.max_concurrent {
        s["max_concurrent"] = json!(n);
    }
    s
}

/// A token count as `128k` or `1M`.
fn tokens(n: u64) -> String {
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}M", n / 1_000_000)
    } else if n >= 1024 && n.is_multiple_of(1024) {
        format!("{}k", n / 1024)
    } else if n >= 1000 && n.is_multiple_of(1000) {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// One subscription, as lines.
fn print_subscription(ctx: &Ctx, v: &Value) {
    ctx.line(format!("{} ({})", field(v, "name"), field(v, "provider")));
    if let Some(d) = v["display_name"].as_str() {
        ctx.line(format!("  display name: {d}"));
    }
    let enabled = v["enabled"].as_bool() != Some(false);
    ctx.line(format!("  enabled: {}", if enabled { "yes" } else { "no" }));
    if let Some(p) = v["plan"].as_str() {
        ctx.line(format!("  plan: {p}"));
    }
    let harnesses: Vec<&str> = v["harnesses"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    ctx.line(format!("  harnesses: {}", harnesses.join(", ")));
    ctx.line(format!("  login: {}", field(v, "login_status")));
    let sh = &v["selfhosted"];
    if sh.is_object() {
        ctx.line(format!("  api: {}", field(sh, "api")));
        ctx.line(format!("  base url: {}", field(sh, "base_url")));
        let models: Vec<&str> = sh["models"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !models.is_empty() {
            ctx.line(format!("  models: {}", models.join(", ")));
        }
        if let Some(m) = sh["default_model"].as_str() {
            ctx.line(format!("  default model: {m}"));
        }
        if let Some(n) = sh["context_window"].as_u64() {
            ctx.line(format!("  context window: {} tokens", tokens(n)));
        }
        if let Some(n) = sh["max_output_tokens"].as_u64() {
            ctx.line(format!("  max output: {} tokens", tokens(n)));
        }
        ctx.line(format!(
            "  api key: {}",
            if v["has_api_key"].as_bool() == Some(true) {
                "stored"
            } else {
                "none"
            }
        ));
    }
    if let Some(st) = v["streams"].as_object() {
        ctx.line(format!(
            "  streams: {}/{} in use, {} waiting",
            st.get("in_use").and_then(Value::as_u64).unwrap_or(0),
            st.get("max").and_then(Value::as_u64).unwrap_or(0),
            st.get("waiting").and_then(Value::as_u64).unwrap_or(0)
        ));
    }
    if let Some(u) = v["status_url"].as_str() {
        ctx.line(format!("  status page: {u}"));
    }
    let agents = v["agents"].as_array().map_or(0, Vec::len);
    ctx.line(format!("  agents: {agents}"));
    if let Some(period) = v.get("session_period") {
        ctx.line(format!(
            "  sessions: {} – {}",
            field(period, "since"),
            field(period, "until")
        ));
    }
    if let Some(sessions) = v["sessions"].as_array() {
        for s in sessions {
            ctx.line(format!(
                "    {}/{} {} {} {} {} {} in / {} out ({})",
                field(s, "project"),
                field(s, "agent"),
                field(s, "harness"),
                field(s, "model"),
                field(s, "card"),
                field(s, "started_at"),
                field(s, "input_tokens"),
                field(s, "output_tokens"),
                field(s, "id")
            ));
        }
    }
}

/// The subscriptions as a table. `DEFAULT` is `*` on the default account
/// of each harness (PLAN §16.3).
fn print_subscriptions(ctx: &Ctx, v: &Value) {
    let items = v.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        ctx.line("no subscriptions");
        return;
    }
    let mut t = crate::output::Table::new(&[
        "NAME",
        "DISPLAY NAME",
        "PROVIDER",
        "PLAN",
        "ACCOUNT",
        "LOGIN",
        "ENABLED",
        "DEFAULT",
        "AGENTS",
    ]);
    for s in &items {
        let account = match (s["account"]["harness"].as_str(), s["has_api_key"].as_bool()) {
            (Some(h), _) => h.to_owned(),
            (None, Some(true)) => "API key".to_owned(),
            (None, _) => "-".to_owned(),
        };
        let agents = s["agents"].as_array().map_or(0, Vec::len);
        t.row(vec![
            field(s, "name"),
            field(s, "display_name"),
            field(s, "provider"),
            field(s, "plan"),
            account,
            field(s, "login_status"),
            if s["enabled"].as_bool() == Some(false) {
                "no".to_owned()
            } else {
                "yes".to_owned()
            },
            if s["default"].as_bool() == Some(true) {
                "*".to_owned()
            } else {
                String::new()
            },
            agents.to_string(),
        ]);
    }
    ctx.out.write_str(&t.render(ctx.out.width()));
}

/// One update result: npm's output, then `name from -> to`.
fn print_update_row(ctx: &Ctx, r: &Value) {
    if let Some(out) = r["output"].as_str().filter(|s| !s.trim().is_empty()) {
        ctx.line(out.trim_end());
    }
    let from = r["from"].as_str().unwrap_or("(not installed)");
    match (r["to"].as_str(), r["error"].as_str()) {
        (Some(to), _) => {
            ctx.line(format!("{}: {from} -> {to}", field(r, "harness")));
            if let Some(note) = r["note"].as_str() {
                ctx.line(note);
            }
            if let Some(removed) = r["removed"].as_array().filter(|a| !a.is_empty()) {
                let v: Vec<&str> = removed.iter().filter_map(Value::as_str).collect();
                ctx.line(format!(
                    "{}: removed the unused version(s) {}",
                    field(r, "harness"),
                    v.join(", ")
                ));
            }
        }
        (None, Some(e)) => ctx.line(format!("{}: failed: {e}", field(r, "harness"))),
        (None, None) => ctx.line(format!("{}: failed", field(r, "harness"))),
    }
}

fn print_update(ctx: &Ctx, v: &Value) {
    match v["results"].as_array() {
        Some(rows) => rows.iter().for_each(|r| print_update_row(ctx, r)),
        None => print_update_row(ctx, v),
    }
}

/// The `details` of a failed update: npm's output, or each result of an
/// update of all.
fn print_failed_update(ctx: &Ctx, d: &Value) {
    if let Some(out) = d["output"].as_str().filter(|s| !s.trim().is_empty()) {
        ctx.line(out.trim_end());
    }
    if let Some(rows) = d["results"].as_array() {
        rows.iter().for_each(|r| print_update_row(ctx, r));
    }
}

/// The keys of an update's `details` that [`print_failed_update`] prints.
const PRINTED_UPDATE_DETAILS: [&str; 2] = ["output", "results"];

/// The `details` of a failed update without the keys that
/// [`print_failed_update`] printed; `None` when nothing is left.
fn without_printed_details(d: &Value) -> Option<Value> {
    let Value::Object(map) = d else {
        return Some(d.clone());
    };
    let rest: serde_json::Map<String, Value> = map
        .iter()
        .filter(|(k, _)| !PRINTED_UPDATE_DETAILS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!rest.is_empty()).then_some(Value::Object(rest))
}

/// The answer of a pin: the switch, the agents on the old version, and
/// the removed version dirs.
fn print_pin(ctx: &Ctx, v: &Value) {
    if let Some(out) = v["output"].as_str().filter(|s| !s.trim().is_empty()) {
        ctx.line(out.trim_end());
    }
    let h = field(v, "harness");
    let from = v["from"].as_str().unwrap_or("(not installed)");
    let to = field(v, "to");
    if v["from"].as_str() == v["to"].as_str() {
        ctx.line(format!("{h}: pinned to {to}; no auto-update"));
    } else {
        ctx.line(format!(
            "{h}: {from} -> {to}, pinned; no auto-update. New agent starts use {to}"
        ));
    }
    if let Some(note) = v["note"].as_str() {
        ctx.line(note);
    }
    if let Some(removed) = v["removed"].as_array().filter(|a| !a.is_empty()) {
        let r: Vec<&str> = removed.iter().filter_map(Value::as_str).collect();
        ctx.line(format!(
            "{h}: removed the unused version(s) {}",
            r.join(", ")
        ));
    }
    ctx.line(format!("undo: terrarium admin harness unpin {h}"));
}

/// The answer of a check: one line for each harness.
fn print_check(ctx: &Ctx, v: &Value) {
    let rows = v["results"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        ctx.line("no harness to check");
    }
    for r in &rows {
        let mailed = if r["mailed"] == true { " (mailed)" } else { "" };
        ctx.line(format!(
            "{}: {}: {}{mailed}",
            field(r, "harness"),
            field(r, "result"),
            field(r, "detail")
        ));
    }
}

/// The runtime pin of a row (`auto_update.pinned_version`), `-` when the
/// harness has none.
fn pin_cell(h: &Value) -> String {
    h["auto_update"]["pinned_version"]
        .as_str()
        .unwrap_or("-")
        .to_owned()
}

/// The update policy of a row: `auto` or `pinned`. The version of a pin is
/// in the `PINNED` column.
fn policy_cell(h: &Value) -> String {
    h["auto_update"]["policy"]
        .as_str()
        .unwrap_or("-")
        .to_owned()
}

/// The base version of a row (`harnesses.toml`). An older daemon sends it
/// as `pinned` only.
fn base_cell(h: &Value) -> String {
    if h["base"].is_string() {
        field(h, "base")
    } else {
        field(h, "pinned")
    }
}

/// The harness table, then the auto-update schedule, then a note for each
/// harness with an unknown version or an agent on an older version.
fn print_harnesses(ctx: &Ctx, v: &Value) {
    let items = v["harnesses"].as_array().cloned().unwrap_or_default();
    let mut t = crate::output::Table::new(&[
        "HARNESS",
        "BASE",
        "PINNED",
        "INSTALLED",
        "UPDATES",
        "ON DISK",
    ]);
    for h in &items {
        let on_disk: Vec<&str> = h["versions"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        t.row(vec![
            field(h, "harness"),
            base_cell(h),
            pin_cell(h),
            field(h, "installed"),
            policy_cell(h),
            if on_disk.is_empty() {
                "-".to_owned()
            } else {
                on_disk.join(" ")
            },
        ]);
    }
    if items.is_empty() {
        ctx.line("no harness adapters");
    } else {
        ctx.out.write_str(&t.render(ctx.out.width()));
    }
    if let Some(s) = v["auto_update"]["schedule"].as_str() {
        let next = v["auto_update"]["next_check_at"]
            .as_str()
            .map(|t| format!(", next check {t}"))
            .unwrap_or_default();
        ctx.line(format!(
            "auto-update: {s} (TERRARIUM_HARNESS_AUTO_UPDATE){next}"
        ));
    }
    for h in &items {
        if h["installed"].is_null() && !h["error"].is_null() {
            ctx.line(format!("{}: {}", field(h, "harness"), field(h, "error")));
        }
        // The live agents on another version than the active one.
        let Some(active) = h["active"].as_str() else {
            continue;
        };
        for a in h["agents"].as_array().into_iter().flatten() {
            if a["version"].as_str() == Some(active) {
                continue;
            }
            ctx.line(format!(
                "{}: {} runs {}; it moves to {active} at its next restart",
                field(h, "harness"),
                field(a, "agent"),
                a["version"].as_str().unwrap_or("an older install"),
            ));
        }
    }
}

/// The token alone on stdout, so it can go into a pipe. For an enrol
/// token, the expiry and the line that installs the host agent
/// (`install_command`, `GET /install/host.sh`) go to stderr.
fn print_token(ctx: &Ctx, v: &Value) {
    ctx.line(field(v, "token"));
    if v["token_kind"] == "enrol" {
        let mut text = format!(
            "enrol token for host {}: one use, expires {}\n",
            field(v, "name"),
            field(v, "token_expires_at"),
        );
        match v["install_command"].as_str() {
            Some(cmd) => text.push_str(&format!(
                "install on the host (needs sudo, systemd, x86_64):\n  {cmd}\n"
            )),
            None => text.push_str(
                "install on the host: curl -fsSL <daemon URL>/install/host.sh | sudo sh -s -- \
                 --token <token> (set TERRARIUM_PUBLIC_URL on the daemon to get the full line)\n",
            ),
        }
        ctx.out.write_bytes(true, text.as_bytes());
    }
}

/// `admin host show`: the link, the capabilities, and the token use record
/// (docs/api.md §17.1).
fn print_host(ctx: &Ctx, v: &Value) {
    let rows: [(&str, String); 16] = [
        ("host", field(v, "name")),
        ("id", field(v, "id")),
        ("status", field(v, "status")),
        ("offline_reason", field(v, "offline_reason")),
        ("online", field(v, "online")),
        ("sessions", field(v, "sessions")),
        ("os", format!("{}/{}", field(v, "os"), field(v, "arch"))),
        (
            "version",
            if v["stale"] == true {
                format!(
                    "{} (STALE: another build than the daemon)",
                    field(v, "version")
                )
            } else {
                field(v, "version")
            },
        ),
        ("connected_at", field(v, "connected_at")),
        ("created_at", field(v, "created_at")),
        ("first_used_at", field(v, "first_used_at")),
        ("last_used_at", field(v, "last_used_at")),
        ("last_used_from", field(v, "last_used_from")),
        ("rotated_at", field(v, "rotated_at")),
        ("enrol_expires_at", field(v, "enrol_expires_at")),
        ("capabilities", capability_line(v)),
    ];
    for (k, val) in rows {
        ctx.line(format!("{k:<17}{val}"));
    }
}

/// `docker 29.8.0, vmlab no, shell /bin/bash`, or `-` before the first `hello`.
fn capability_line(v: &Value) -> String {
    let caps = &v["capabilities"];
    if caps.as_object().is_none_or(serde_json::Map::is_empty) {
        return "-".into();
    }
    let tool = |name: &str| {
        let t = &caps[name];
        match (t["present"].as_bool(), t["version"].as_str()) {
            (Some(true), Some(ver)) => format!("{name} {ver}"),
            (Some(true), None) => format!("{name} yes"),
            _ => format!("{name} no"),
        }
    };
    let mut parts = vec![tool("docker"), tool("vmlab")];
    if let Some(sh) = caps["shell"].as_str() {
        parts.push(format!("shell {sh}"));
    }
    parts.join(", ")
}

pub async fn usage(ctx: &Ctx, cmd: Option<&UsageCmd>) -> anyhow::Result<()> {
    let v = match cmd {
        None => {
            let q = if ctx.is_agent() {
                query([("agent", Some(ctx.me().await?))])
            } else {
                vec![]
            };
            ctx.api.get("/api/usage", q).await?
        }
        Some(UsageCmd::Project) => {
            let p = ctx.project_or(None).await?;
            ctx.api
                .get("/api/usage", query([("project", Some(p))]))
                .await?
        }
        Some(UsageCmd::Subscriptions) => ctx.api.get("/api/subscriptions", vec![]).await?,
    };
    ctx.emit(&v, |c, v| print_generic(c, v, "no usage"))
}

/// The `--since` of `audit`: an RFC 3339 time, a bare `HH:MM[:SS]` (today,
/// in local time), or a duration back from now (`30m`, `2h`, `1d`, `90s`).
/// The result is the RFC 3339 form that the API compares (UTC, `Z`,
/// milliseconds).
pub fn parse_since(s: &str, now: chrono::DateTime<chrono::Local>) -> anyhow::Result<String> {
    use chrono::{NaiveTime, TimeZone, Utc};
    let s = s.trim();
    let utc = if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        t.with_timezone(&Utc)
    } else if let Ok(t) =
        NaiveTime::parse_from_str(s, "%H:%M:%S").or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
    {
        let local = now.date_naive().and_time(t);
        chrono::Local
            .from_local_datetime(&local)
            .earliest()
            .ok_or_else(|| anyhow::anyhow!("`{s}` does not exist today in local time"))?
            .with_timezone(&Utc)
    } else {
        let bad = || {
            anyhow::anyhow!(
                "bad --since `{s}`: an RFC 3339 time, HH:MM[:SS] (today, local time), \
                 or a duration such as 30m, 2h, 1d"
            )
        };
        let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
        let (num, unit) = s.split_at(split);
        let n: i64 = num.parse().map_err(|_| bad())?;
        let secs = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            "d" => n * 86_400,
            _ => return Err(bad()),
        };
        now.with_timezone(&Utc) - chrono::Duration::seconds(secs)
    };
    Ok(utc.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

pub async fn audit(ctx: &Ctx, a: &AuditArgs) -> anyhow::Result<()> {
    let since = a
        .since
        .as_deref()
        .map(|s| parse_since(s, chrono::Local::now()))
        .transpose()?;
    let q = query([
        ("since", since),
        ("actor", a.actor.clone()),
        ("action", a.action.clone()),
        ("project", ctx.project.clone()),
    ]);
    let rows = ctx.api.list("/api/audit", q, Some(a.limit)).await?;
    let v = Value::Array(rows);
    ctx.emit(&v, |c, v| {
        print_table(
            c,
            as_items(v),
            &[
                ("SEQ", "seq"),
                ("AT", "at"),
                ("ACTOR", "actor"),
                ("ACTION", "action"),
                ("TARGET", "target"),
                ("PROJECT", "project"),
            ],
            "no audit rows",
        );
    })
}

pub async fn events(ctx: &Ctx, a: &EventsArgs) -> anyhow::Result<()> {
    let q = query([
        ("kind", a.kind.clone()),
        ("project", ctx.project.clone()),
        ("agent", a.agent.clone()),
    ]);
    show_events(ctx, "/api/events", q, a.since.clone(), a.limit, a.follow).await
}

/// List events, then keep following when asked: over SSE when the CLI has
/// the HTTP transport, else by polling `?since=`.
pub async fn show_events(
    ctx: &Ctx,
    path: &str,
    q: Vec<(String, String)>,
    since: Option<String>,
    limit: usize,
    follow: bool,
) -> anyhow::Result<()> {
    let print = |e: &Value| -> anyhow::Result<()> {
        if ctx.json {
            ctx.line(serde_json::to_string(e)?);
        } else {
            ctx.line(event_line(e));
        }
        Ok(())
    };
    if follow && let Some(http) = &ctx.http {
        let mut q = q;
        if let Some(s) = since {
            q.push(("since".into(), s));
        }
        let req = ApiRequest {
            method: Method::Get,
            path: path.to_owned(),
            query: q,
            body: None,
        };
        if let Some(resp) = http
            .sse(&req, |e| {
                print(&e)?;
                Ok(true)
            })
            .await?
        {
            return Err(ServerError::from_response(&resp).into());
        }
        return Ok(());
    }
    let mut q1 = q.clone();
    if let Some(s) = &since {
        q1.push(("since".into(), s.clone()));
    }
    let items = ctx.api.list(path, q1, Some(limit)).await?;
    if !follow {
        if ctx.json {
            ctx.line(serde_json::to_string_pretty(&items)?);
        } else if items.is_empty() {
            ctx.line("no events");
        } else {
            for e in &items {
                print(e)?;
            }
        }
        return Ok(());
    }
    let mut last = since;
    for e in &items {
        print(e)?;
        last = Some(field(e, "seq"));
    }
    loop {
        let started = Instant::now();
        let mut q2 = q.clone();
        if let Some(s) = &last {
            q2.push(("since".into(), s.clone()));
        }
        q2.push(("wait".into(), crate::ctx::MAX_WAIT.to_string()));
        let page = ctx.api.page(path, q2).await?;
        for e in &page.items {
            print(e)?;
            last = Some(field(e, "seq"));
        }
        if page.items.is_empty() && started.elapsed() < ctx.poll_floor {
            tokio::time::sleep(ctx.poll_floor - started.elapsed()).await;
        }
    }
}

pub fn as_items(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}
