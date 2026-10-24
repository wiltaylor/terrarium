//! `daemon`, `whoami`, `targets`, `usage`, `audit`, `events`, and the
//! target and host parts of `admin`.

use std::time::Instant;

use serde_json::{Value, json};

use super::{event_line, print_generic, print_table, set_opt};
use crate::api::{ServerError, query};
use crate::cli::{AdminTargetCmd, AuditArgs, DaemonCmd, EventsArgs, UsageCmd};
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
    ("ADDRESS", "address"),
    ("STATE", "state"),
];

pub async fn targets(ctx: &Ctx) -> anyhow::Result<()> {
    let path = match &ctx.project {
        Some(p) => format!("/api/projects/{}/targets", crate::transport::encode(p)),
        None => "/api/targets".into(),
    };
    let items = ctx.api.list(&path, vec![], None).await?;
    let v = Value::Array(items);
    ctx.emit(&v, |c, v| {
        print_table(c, as_items(v), TARGET_COLS, "no targets")
    })
}

pub async fn admin_target(ctx: &Ctx, cmd: &AdminTargetCmd) -> anyhow::Result<()> {
    match cmd {
        AdminTargetCmd::Ls => targets(ctx).await,
        AdminTargetCmd::New {
            name,
            kind,
            address,
            global,
        } => {
            let mut body = json!({"name": name, "kind": kind});
            set_opt(&mut body, "address", address.clone());
            if !global {
                body["project"] = ctx.project_or(None).await?.into();
            }
            let t = ctx.api.post("/api/targets", Some(body)).await?;
            ctx.emit(&t, |c, t| {
                c.line(format!("created target {}", field(t, "name")));
            })
        }
    }
}

pub async fn hosts(ctx: &Ctx) -> anyhow::Result<()> {
    let v = Value::Array(ctx.api.list("/api/hosts", vec![], None).await?);
    ctx.emit(&v, |c, v| print_generic(c, v, "no host agents"))
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

pub async fn audit(ctx: &Ctx, a: &AuditArgs) -> anyhow::Result<()> {
    let q = query([
        ("since", a.since.clone()),
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
