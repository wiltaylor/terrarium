//! `agent` (also `admin agent`), `admin role`, and `status`.

use serde_json::{Value, json};

use super::{agent_path, print_object, print_table, set_opt};
use crate::api::{CliError, query};
use crate::cli::{AgentCmd, AgentNewArgs, AgentSetArgs, AnswerArgs, RoleCmd, RolesCmd, StatusCmd};
use crate::commands::projects::{AGENT_COLS, project_path};
use crate::commands::system::{as_items, show_events};
use crate::ctx::Ctx;
use crate::output::field;

pub async fn agent(ctx: &Ctx, cmd: &AgentCmd) -> anyhow::Result<()> {
    match cmd {
        AgentCmd::Ls { all_projects } => ls(ctx, *all_projects).await,
        AgentCmd::Show { agent } => {
            let a = ctx.api.get(&agent_path(agent), vec![]).await?;
            ctx.emit(&a, print_object)
        }
        AgentCmd::New(a) => new(ctx, a).await,
        AgentCmd::Set(a) => set(ctx, a).await,
        AgentCmd::Rm { agent, confirm } => {
            let q = if *confirm {
                vec![("confirm".into(), "true".into())]
            } else {
                vec![]
            };
            ctx.api.delete(&agent_path(agent), q).await?;
            ctx.emit(&json!({"deleted": agent}), |c, _| {
                c.line(format!("deleted agent {agent}"));
            })
        }
        AgentCmd::Start { agent } => lifecycle(ctx, agent, "start").await,
        AgentCmd::Stop { agent } => lifecycle(ctx, agent, "stop").await,
        AgentCmd::Restart { agent } => lifecycle(ctx, agent, "restart").await,
        AgentCmd::Send { agent, text } => {
            let text = ctx.text(text)?;
            let r = ctx
                .api
                .post(
                    &format!("{}/send", agent_path(agent)),
                    Some(json!({"text": text})),
                )
                .await?;
            ctx.emit(&r, |c, _| c.line(format!("sent a turn to {agent}")))
        }
        AgentCmd::Answer(a) => answer(ctx, a).await,
        AgentCmd::Logs {
            agent,
            since,
            kind,
            follow,
            limit,
        } => {
            let q = query([("kind", kind.clone())]);
            show_events(
                ctx,
                &format!("{}/events", agent_path(agent)),
                q,
                since.clone(),
                *limit,
                *follow,
            )
            .await
        }
        AgentCmd::Attach { agent } => {
            let Some(http) = &ctx.http else {
                return Err(CliError::not_available(
                    "attach needs the daemon socket: the runner tunnel does not carry the PTY WebSocket",
                ));
            };
            crate::attach::attach(http, &format!("{}/pty", agent_path(agent)), agent).await
        }
        AgentCmd::Sessions { agent } => {
            let v = Value::Array(
                ctx.api
                    .list(&format!("{}/sessions", agent_path(agent)), vec![], None)
                    .await?,
            );
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    as_items(v),
                    &[
                        ("ID", "id"),
                        ("CURRENT", "current"),
                        ("STARTED", "started_at"),
                        ("ENDED", "ended_at"),
                        ("OUTCOME", "outcome"),
                        ("TURNS", "turns"),
                        ("HARNESS SESSION", "harness_session_id"),
                    ],
                    "no sessions",
                );
            })
        }
        AgentCmd::Roles { command } => match command {
            RolesCmd::Get { agent } => show_roles(ctx, agent).await,
            RolesCmd::Set { agent, roles } => put_roles(ctx, agent, roles.clone()).await,
        },
    }
}

async fn ls(ctx: &Ctx, all_projects: bool) -> anyhow::Result<()> {
    let items = if all_projects && ctx.is_agent() && ctx.project.is_none() {
        // The daemon defaults an agent to its own project. Ask for each
        // project the agent can see.
        let mut out = Vec::new();
        for p in ctx.api.list("/api/projects", vec![], None).await? {
            let path = format!("{}/agents", project_path(&field(&p, "name")));
            out.extend(ctx.api.list(&path, vec![], None).await?);
        }
        out
    } else {
        let q = query([("project", ctx.project.clone())]);
        ctx.api.list("/api/agents", q, None).await?
    };
    let v = Value::Array(items);
    ctx.emit(&v, |c, v| {
        print_table(c, as_items(v), AGENT_COLS, "no agents")
    })
}

/// The agent definition as JSON: the file first, then the flags on top.
pub async fn definition(ctx: &Ctx, a: &AgentNewArgs) -> anyhow::Result<Value> {
    let mut def = match &a.from_file {
        Some(path) => {
            let d: terrarium_core::config::AgentDef = terrarium_core::config::load_file(path)?;
            serde_json::to_value(d)?
        }
        None => json!({}),
    };
    set_opt(&mut def, "name", a.name.clone());
    if let Some(p) = &ctx.project {
        def["project"] = p.clone().into();
    }
    if def.get("project").is_none() {
        def["project"] = ctx.project_or(None).await?.into();
    }
    set_opt(&mut def, "harness", a.harness.clone());
    set_opt(&mut def, "subscription", a.subscription.clone());
    set_opt(&mut def, "model", a.model.clone());
    set_opt(&mut def, "prompt", a.prompt.clone());
    set_opt(&mut def, "restart", a.restart.clone());
    if !a.roles.is_empty() {
        def["roles"] = json!(a.roles);
    }
    if !(a.targets.is_empty() && a.mail_to.is_empty() && a.cross_project.is_empty()) {
        let policy = def
            .get("policy")
            .cloned()
            .unwrap_or_else(|| json!({"targets": [], "mail_to": ["*"]}));
        let mut policy = policy;
        if !a.targets.is_empty() {
            policy["targets"] = json!(a.targets);
        }
        if !a.mail_to.is_empty() {
            policy["mail_to"] = json!(a.mail_to);
        }
        if !a.cross_project.is_empty() {
            policy["cross_project"] = json!(a.cross_project);
        }
        def["policy"] = policy;
    }
    if def
        .get("name")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(CliError::usage("give the agent a name, or use --from-file"));
    }
    if def.get("harness").is_none() {
        return Err(CliError::usage(
            "give --harness (claude, codex, copilot, or pi), or use --from-file",
        ));
    }
    Ok(def)
}

async fn new(ctx: &Ctx, a: &AgentNewArgs) -> anyhow::Result<()> {
    let def = definition(ctx, a).await?;
    let r = ctx.api.post("/api/agents", Some(def)).await?;
    ctx.emit(&r, |c, r| {
        c.line(format!(
            "created agent {}/{} ({}) {}",
            field(r, "project"),
            field(r, "name"),
            field(r, "harness"),
            field(r, "id")
        ));
    })
}

async fn set(ctx: &Ctx, a: &AgentSetArgs) -> anyhow::Result<()> {
    let mut body = json!({});
    set_opt(&mut body, "harness", a.harness.clone());
    set_opt(&mut body, "subscription", a.subscription.clone());
    set_opt(&mut body, "model", a.model.clone());
    set_opt(&mut body, "prompt", a.prompt.clone());
    set_opt(&mut body, "restart", a.restart.clone());
    if let Some(p) = &a.policy {
        let p: Value = serde_json::from_str(p)
            .map_err(|e| CliError::usage(format!("--policy is not JSON: {e}")))?;
        body["policy"] = p;
    }
    if body.as_object().is_some_and(serde_json::Map::is_empty) {
        return Err(CliError::usage("give at least one field to change"));
    }
    let r = ctx.api.patch(&agent_path(&a.agent), body).await?;
    ctx.emit(&r, |c, r| {
        c.line(format!(
            "updated agent {}/{}",
            field(r, "project"),
            field(r, "name")
        ));
    })
}

async fn lifecycle(ctx: &Ctx, agent: &str, action: &str) -> anyhow::Result<()> {
    let r = ctx
        .api
        .post(&format!("{}/{action}", agent_path(agent)), None)
        .await?;
    ctx.emit(&r, |c, r| {
        c.line(format!(
            "{action}: {}/{} is {}",
            field(r, "project"),
            field(r, "name"),
            field(r, "status")
        ));
    })
}

async fn answer(ctx: &Ctx, a: &AnswerArgs) -> anyhow::Result<()> {
    let body = if let Some(text) = &a.text {
        json!({"text": ctx.text(text)?})
    } else {
        let answer = match (a.choice, a.allow, a.deny) {
            (Some(i), _, _) => json!({"kind": "choice", "value": i}),
            (_, true, _) => json!({"kind": "allow"}),
            (_, _, true) => json!({"kind": "deny"}),
            _ => {
                return Err(CliError::usage(
                    "give --choice N, --allow, --deny, or --text T",
                ));
            }
        };
        let mut b = json!({"answer": answer});
        set_opt(&mut b, "request_id", a.request_id.clone());
        b
    };
    let r = ctx
        .api
        .post(&format!("{}/answer", agent_path(&a.agent)), Some(body))
        .await?;
    ctx.emit(&r, |c, _| c.line(format!("answered {}", a.agent)))
}

fn roles_of(v: &Value) -> Vec<String> {
    let list = v.get("roles").unwrap_or(v);
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(str::to_owned))
        .collect()
}

async fn show_roles(ctx: &Ctx, agent: &str) -> anyhow::Result<()> {
    let r = ctx
        .api
        .get(&format!("{}/roles", agent_path(agent)), vec![])
        .await?;
    ctx.emit(&r, |c, r| {
        let roles = roles_of(r);
        c.line(if roles.is_empty() {
            "member".to_owned()
        } else {
            roles.join(", ")
        });
    })
}

async fn put_roles(ctx: &Ctx, agent: &str, roles: Vec<String>) -> anyhow::Result<()> {
    let r = ctx
        .api
        .put(
            &format!("{}/roles", agent_path(agent)),
            json!({"roles": roles}),
        )
        .await?;
    ctx.emit(&r, |c, r| {
        let roles = roles_of(r);
        c.line(format!(
            "{agent}: {}",
            if roles.is_empty() {
                "member".to_owned()
            } else {
                roles.join(", ")
            }
        ));
    })
}

pub async fn role(ctx: &Ctx, cmd: &RoleCmd) -> anyhow::Result<()> {
    match cmd {
        RoleCmd::Ls { agent } => show_roles(ctx, agent).await,
        RoleCmd::Grant { agent, role } | RoleCmd::Revoke { agent, role } => {
            let current = ctx
                .api
                .get(&format!("{}/roles", agent_path(agent)), vec![])
                .await?;
            let mut roles = roles_of(&current);
            roles.retain(|r| r != role);
            if matches!(cmd, RoleCmd::Grant { .. }) {
                roles.push(role.clone());
            }
            put_roles(ctx, agent, roles).await
        }
    }
}

pub async fn status(ctx: &Ctx, cmd: &StatusCmd) -> anyhow::Result<()> {
    let StatusCmd::Set { status, note } = cmd;
    let me = ctx.me().await?;
    let mut body = json!({"status": status});
    set_opt(&mut body, "note", note.clone());
    let r = ctx
        .api
        .post(&format!("{}/status", agent_path(&me)), Some(body))
        .await?;
    ctx.emit(&r, |c, _| c.line(format!("status: {status}")))
}
