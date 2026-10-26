//! `daemon`, `whoami`, `targets`, `usage`, `audit`, `events`, and the
//! target and host parts of `admin`.

use std::time::Instant;

use serde_json::{Value, json};

use super::{event_line, print_generic, print_table, set_opt};
use crate::api::{ServerError, query};
use crate::cli::{AdminTargetCmd, AuditArgs, DaemonCmd, EventsArgs, HarnessCmd, HostCmd, UsageCmd};
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

pub async fn whoami(ctx: &Ctx) -> anyhow::Result<()> {
    let Some(env) = &ctx.agent else {
        let v = json!({"actor": "operator", "endpoint": ctx.api.describe()});
        return ctx.emit(&v, |c, _| c.line("operator"));
    };
    let me = ctx.identity().await?.clone();
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

const TARGET_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("KIND", "kind"),
    ("SCOPE", "scope"),
    ("PROJECT", "project"),
    ("HOST", "host"),
    ("ADDRESS", "address"),
    ("STATE", "state"),
    ("ONLINE", "online"),
];

/// `host_online` as a word for the table: `online`, `offline`, or `-` (ssh).
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
        let rows = with_online(as_items(v));
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
        let rows = with_access(with_online(as_items(v)));
        print_table(c, &rows, AGENT_TARGET_COLS, "no targets");
    })
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

pub async fn admin_target(ctx: &Ctx, cmd: &AdminTargetCmd) -> anyhow::Result<()> {
    match cmd {
        AdminTargetCmd::Ls => admin_targets(ctx).await,
        AdminTargetCmd::New {
            name,
            kind,
            address,
            port,
            credential,
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
        } => {
            let mut body = json!({"name": name, "kind": kind});
            set_opt(&mut body, "address", address.clone());
            set_opt(&mut body, "port", *port);
            set_opt(&mut body, "credential", credential.clone());
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
            if body == json!({}) {
                return Err(crate::api::CliError::usage(
                    "give a change: --lab-path, --lab-ref, --machine, --image, --host, \
                     --ephemeral, --permanent, --keep, --no-keep, or --snapshot-before-task",
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
        } => {
            if !["up", "down", "snapshot", "restore", "status"].contains(&verb.as_str()) {
                return Err(crate::api::CliError::usage(format!(
                    "unknown verb `{verb}`: use up, down, snapshot, restore, or status"
                )));
            }
            let path = format!("/api/targets/{}/{verb}", crate::transport::encode(name));
            let mut body = json!({});
            set_opt(&mut body, "name", snapshot.clone());
            set_opt(&mut body, "agent", agent.clone());
            let body = (body != json!({})).then_some(body);
            let v = ctx.api.post(&path, body).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("{name}: {verb}: {}", field(v, "state")));
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
                    as_items(v),
                    &[
                        ("NAME", "name"),
                        ("STATUS", "status"),
                        ("OS", "os"),
                        ("ARCH", "arch"),
                        ("VERSION", "version"),
                        ("SESSIONS", "sessions"),
                        ("LAST SEEN", "last_seen"),
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

/// `admin harness ls`: `GET /api/harnesses`; `admin harness update`:
/// `POST /api/harnesses/{name}/update` or `POST /api/harnesses/update`.
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

/// One update result: npm's output, then `name from -> to`.
fn print_update_row(ctx: &Ctx, r: &Value) {
    if let Some(out) = r["output"].as_str().filter(|s| !s.trim().is_empty()) {
        ctx.line(out.trim_end());
    }
    let from = r["from"].as_str().unwrap_or("(not installed)");
    match (r["to"].as_str(), r["error"].as_str()) {
        (Some(to), _) => {
            let range = match r["in_range"].as_bool() {
                Some(false) => format!(" (outside the tested range {})", field(r, "tested")),
                _ => String::new(),
            };
            ctx.line(format!("{}: {from} -> {to}{range}", field(r, "harness")));
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

/// The `details` of a failed update: npm's output, each result of an
/// update of all, or the running agents.
fn print_failed_update(ctx: &Ctx, d: &Value) {
    if let Some(out) = d["output"].as_str().filter(|s| !s.trim().is_empty()) {
        ctx.line(out.trim_end());
    }
    if let Some(rows) = d["results"].as_array() {
        rows.iter().for_each(|r| print_update_row(ctx, r));
    }
    if let Some(running) = d["running"].as_array() {
        let names: Vec<&str> = running.iter().filter_map(Value::as_str).collect();
        ctx.line(format!(
            "running: {}. Stop them, or add --force",
            names.join(", ")
        ));
    }
}

/// The keys of an update's `details` that [`print_failed_update`] prints.
const PRINTED_UPDATE_DETAILS: [&str; 3] = ["output", "results", "running"];

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

/// The harness table, then the policy, then a note for each harness out of
/// its range.
fn print_harnesses(ctx: &Ctx, v: &Value) {
    let items = v["harnesses"].as_array().cloned().unwrap_or_default();
    let mut t =
        crate::output::Table::new(&["HARNESS", "PINNED", "TESTED", "INSTALLED", "IN RANGE"]);
    for h in &items {
        let in_range = match h["in_range"].as_bool() {
            Some(true) => "yes".to_owned(),
            Some(false) => "NO".to_owned(),
            None => "unknown".to_owned(),
        };
        t.row(vec![
            field(h, "harness"),
            field(h, "pinned"),
            field(h, "tested"),
            field(h, "installed"),
            in_range,
        ]);
    }
    if items.is_empty() {
        ctx.line("no harness adapters");
    } else {
        ctx.out.write_str(&t.render(ctx.out.width()));
    }
    ctx.line(format!(
        "policy: {} (TERRARIUM_HARNESS_VERSION_POLICY)",
        field(v, "policy")
    ));
    for h in &items {
        if h["in_range"] == false {
            ctx.line(format!(
                "{}: {} is outside {}; run `terrarium admin harness update {}` or `just harness::update {}`",
                field(h, "harness"),
                field(h, "installed"),
                field(h, "tested"),
                field(h, "harness"),
                field(h, "harness"),
            ));
        } else if h["in_range"].is_null() && !h["error"].is_null() {
            ctx.line(format!("{}: {}", field(h, "harness"), field(h, "error")));
        }
    }
}

/// The token alone on stdout, so it can go into a pipe. For an enrol
/// token, the expiry goes to stderr.
fn print_token(ctx: &Ctx, v: &Value) {
    ctx.line(field(v, "token"));
    if v["token_kind"] == "enrol" {
        let line = format!(
            "enrol token for host {}: one use, expires {}\n",
            field(v, "name"),
            field(v, "token_expires_at"),
        );
        ctx.out.write_bytes(true, line.as_bytes());
    }
}

/// `admin host show`: the link, the capabilities, and the token use record
/// (docs/api.md §17.1).
fn print_host(ctx: &Ctx, v: &Value) {
    let rows: [(&str, String); 15] = [
        ("host", field(v, "name")),
        ("id", field(v, "id")),
        ("status", field(v, "status")),
        ("online", field(v, "online")),
        ("sessions", field(v, "sessions")),
        ("os", format!("{}/{}", field(v, "os"), field(v, "arch"))),
        ("version", field(v, "version")),
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
