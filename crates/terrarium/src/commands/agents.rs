//! `agent` (also `admin agent`), `admin role`, and `status`.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    agent_path, persona_value, print_object_with_text, print_table, set_opt, system_prompt_value,
};
use crate::api::{CliError, query};
use crate::cli::{AgentCmd, AgentNewArgs, AgentSetArgs, AnswerArgs, RoleCmd, RolesCmd, StatusCmd};
use crate::commands::projects::{AGENT_COLS, agent_rows, project_path};
use crate::commands::system::{as_items, show_events};
use crate::ctx::Ctx;
use crate::output::field;

/// The line that names one attached image in a turn. The dashboard uses
/// the same words (`dashboard/src/features/agents/conversation/attachments.ts`).
pub fn image_line(path: &str) -> String {
    format!("Attached image: {path} (open this file with your image or file-read tool to see it)")
}

/// The turn text: the text, then one line for each image path.
pub fn turn_with_images(text: &str, paths: &[String]) -> String {
    let text = text.trim();
    let lines: Vec<String> = paths.iter().map(|p| image_line(p)).collect();
    if text.is_empty() {
        lines.join("\n")
    } else {
        format!("{text}\n\n{}", lines.join("\n"))
    }
}

/// Upload each image to `POST /api/agents/{id}/attachments` (the JSON
/// form), in order. Returns the paths that the agent sees. The first
/// failure stops the send.
async fn attach_images(
    ctx: &Ctx,
    agent: &str,
    images: &[std::path::PathBuf],
) -> anyhow::Result<Vec<String>> {
    let mut paths = Vec::with_capacity(images.len());
    for file in images {
        let data = std::fs::read(file)
            .map_err(|e| CliError::usage(format!("cannot read {}: {e}", file.display())))?;
        let r = ctx
            .api
            .post(
                &format!("{}/attachments", agent_path(agent)),
                Some(json!({"data_base64": super::artifacts::base64(&data)})),
            )
            .await?;
        let path = r["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("the daemon answered no path for {}", file.display()))?;
        paths.push(path.to_owned());
    }
    Ok(paths)
}

pub async fn agent(ctx: &Ctx, cmd: &AgentCmd) -> anyhow::Result<()> {
    match cmd {
        AgentCmd::Ls { all_projects } => ls(ctx, *all_projects).await,
        AgentCmd::Show { agent } => {
            let a = ctx.api.get(&agent_path(agent), vec![]).await?;
            ctx.emit(&a, |c, v| print_object_with_text(c, v, "system_prompt"))
        }
        AgentCmd::New(a) => new(ctx, a).await,
        AgentCmd::Set(a) => set(ctx, a).await,
        AgentCmd::Swap {
            agent,
            subscription,
        } => {
            let r = ctx
                .api
                .post(
                    &format!("{}/subscription", agent_path(agent)),
                    Some(json!({"subscription": subscription})),
                )
                .await?;
            ctx.emit(&r, |c, r| {
                let how = match r["applied"].as_str().unwrap_or_default() {
                    "live" => "live; the harness reads the new login at its next request",
                    "restart_scheduled" => "the agent restarts with resume when its turn ends",
                    "restarted" => "the agent restarted with resume",
                    _ => "the next start uses it",
                };
                c.line(format!(
                    "{agent}: {} -> {} ({how})",
                    r["from"].as_str().unwrap_or("none"),
                    field(r, "to")
                ));
            })
        }
        AgentCmd::Rm {
            agent,
            confirm,
            keep_workspace,
        } => {
            let mut q: Vec<(String, String)> = Vec::new();
            if *confirm {
                q.push(("confirm".into(), "true".into()));
            }
            if *keep_workspace {
                q.push(("keep_workspace".into(), "true".into()));
            }
            ctx.api.delete(&agent_path(agent), q).await?;
            ctx.emit(&json!({"deleted": agent}), |c, _| {
                c.line(format!("deleted agent {agent}"));
            })
        }
        AgentCmd::Start { agent, no_wait } => lifecycle(ctx, agent, "start", *no_wait).await,
        AgentCmd::Stop { agent, no_wait } => lifecycle(ctx, agent, "stop", *no_wait).await,
        AgentCmd::Restart { agent, no_wait } => lifecycle(ctx, agent, "restart", *no_wait).await,
        AgentCmd::Pause { agent } => pause(ctx, agent, "pause").await,
        AgentCmd::Unpause { agent } => pause(ctx, agent, "unpause").await,
        AgentCmd::Send {
            agent,
            text,
            raw,
            image,
        } => {
            let text = match text {
                Some(t) => ctx.text(t)?,
                None => String::new(),
            };
            let text = if image.is_empty() {
                text
            } else {
                let paths = attach_images(ctx, agent, image).await?;
                turn_with_images(&text, &paths)
            };
            let mut body = json!({"text": text});
            if *raw {
                body["raw"] = json!(true);
            }
            let r = ctx
                .api
                .post(&format!("{}/send", agent_path(agent)), Some(body))
                .await?;
            let what = if *raw {
                "typed raw text into"
            } else {
                "sent a turn to"
            };
            ctx.emit(&r, |c, _| c.line(format!("{what} {agent}")))
        }
        AgentCmd::Keys { agent, keys } => {
            let r = ctx
                .api
                .post(
                    &format!("{}/keys", agent_path(agent)),
                    Some(json!({"keys": keys})),
                )
                .await?;
            ctx.emit(&r, |c, v| {
                let names: Vec<&str> = v["keys"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                c.line(format!("sent {} to {agent}", names.join(" ")))
            })
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
        AgentCmd::Read(a) => super::inspect::read(ctx, a).await,
        AgentCmd::Screen { agent } => super::inspect::screen(ctx, agent).await,
        AgentCmd::Leases { agent, all } => {
            let q = crate::api::query([("all", all.then(|| "true".to_owned()))]);
            let v = Value::Array(
                ctx.api
                    .list(&format!("{}/leases", agent_path(agent)), q, None)
                    .await?,
            );
            ctx.emit(&v, super::system::print_leases)
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
        print_table(c, &agent_rows(as_items(v)), AGENT_COLS, "no agents")
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
    if !a.pool.is_empty() {
        def["subscription_pool"] = json!(a.pool);
    }
    set_opt(&mut def, "model", a.model.clone());
    set_opt(&mut def, "prompt", a.prompt.clone());
    set_opt(
        &mut def,
        "system_prompt",
        system_prompt_value(ctx, &a.system_prompt)?,
    );
    set_opt(&mut def, "persona", persona_value(&a.system_prompt));
    set_opt(&mut def, "restart", a.restart.clone());
    set_opt(&mut def, "idle_stop", a.idle_stop.clone());
    set_opt(&mut def, "template", a.template.clone());
    if !a.roles.is_empty() {
        def["roles"] = json!(a.roles);
    }
    if !a.skills.is_empty() {
        def["skills"] = json!(a.skills);
    }
    if !(a.targets.is_empty() && a.mail_to.is_empty() && a.cross_project.is_empty()) {
        // With a template, the daemon merges `policy` key by key: send
        // only the keys that the flags give.
        let empty = if a.template.is_some() {
            json!({})
        } else {
            json!({"targets": [], "mail_to": ["*"]})
        };
        let policy = def.get("policy").cloned().unwrap_or(empty);
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
    if def.get("harness").is_none() && a.template.is_none() {
        return Err(CliError::usage(
            "give --harness (claude, codex, copilot, or pi), --template, or --from-file",
        ));
    }
    Ok(def)
}

async fn new(ctx: &Ctx, a: &AgentNewArgs) -> anyhow::Result<()> {
    let def = definition(ctx, a).await?;
    let r = ctx.api.post("/api/agents", Some(def)).await?;
    ctx.emit(&r, |c, r| {
        let from = match r["template"]["name"].as_str() {
            Some(t) => format!(
                " from template {t} (revision {})",
                r["template"]["revision"]
            ),
            None => String::new(),
        };
        c.line(format!(
            "created agent {}/{} ({}) {}{from}",
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
    if let Some(p) = &a.pool {
        let pool: Vec<&String> = p.iter().filter(|s| !s.trim().is_empty()).collect();
        body["subscription_pool"] = json!(pool);
    }
    set_opt(&mut body, "model", a.model.clone());
    set_opt(&mut body, "prompt", a.prompt.clone());
    set_opt(
        &mut body,
        "system_prompt",
        system_prompt_value(ctx, &a.system_prompt)?,
    );
    set_opt(&mut body, "persona", persona_value(&a.system_prompt));
    set_opt(&mut body, "restart", a.restart.clone());
    // `default` goes back to the daemon's idle stop.
    match a.idle_stop.as_deref() {
        Some("default") => body["idle_stop"] = Value::Null,
        Some(v) => body["idle_stop"] = v.into(),
        None => {}
    }
    if let Some(p) = &a.policy {
        let p: Value = serde_json::from_str(p)
            .map_err(|e| CliError::usage(format!("--policy is not JSON: {e}")))?;
        body["policy"] = p;
    }
    if let Some(b) = a.new_session_on_prompt_change {
        body["new_session_on_prompt_change"] = b.into();
    }
    if a.no_skills {
        body["skills"] = json!([]);
    } else if !a.skills.is_empty() {
        body["skills"] = json!(a.skills);
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

/// `agent pause|unpause`: the daemon answers with the agent.
async fn pause(ctx: &Ctx, agent: &str, action: &str) -> anyhow::Result<()> {
    let r = ctx
        .api
        .post(&format!("{}/{action}", agent_path(agent)), None)
        .await?;
    ctx.emit(&r, |c, r| {
        let queued = r["queued"].as_u64().unwrap_or(0);
        let state = if r["paused"].as_bool() == Some(true) {
            "paused"
        } else {
            "not paused"
        };
        c.line(format!(
            "{}/{} is {state} ({}, {queued} queued)",
            field(r, "project"),
            field(r, "name"),
            field(r, "status")
        ));
    })
}

/// How long `agent start|stop|restart` waits for the new status.
const LIFECYCLE_WAIT: Duration = Duration::from_secs(10);

/// True when `status` is where `action` leads: a live, settled status
/// after a start or a restart, `stopped` after a stop. `error` ends both.
fn settled(action: &str, status: &str) -> bool {
    match action {
        "stop" => matches!(status, "stopped" | "error"),
        _ => matches!(status, "idle" | "waiting" | "working" | "blocked" | "error"),
    }
}

/// The daemon answers `202` with the status it had when the request came
/// (the start or stop goes on after the answer). So the CLI polls the
/// agent until the status settles, for at most [`LIFECYCLE_WAIT`].
async fn lifecycle(ctx: &Ctx, agent: &str, action: &str, no_wait: bool) -> anyhow::Result<()> {
    let r = ctx
        .api
        .post(&format!("{}/{action}", agent_path(agent)), None)
        .await?;
    let was = r.get("status").and_then(Value::as_str).map(str::to_owned);
    let Some(was) = was else {
        // No status in the answer: there is nothing to compare.
        return ctx.emit(&r, |c, _| c.line(format!("{action} requested for {agent}")));
    };
    let mut out = r.clone();
    out["was"] = json!(was);
    if no_wait {
        out["waited"] = json!(false);
        return ctx.emit(&out, |c, _| {
            c.line(format!("{action} requested; was {was}"));
        });
    }
    let deadline = Instant::now() + LIFECYCLE_WAIT;
    let interval = ctx.poll_floor.min(Duration::from_millis(250));
    let mut now = r;
    // A restart answers before the old harness ends, so its answer can show
    // the old live status. It is done after a status that is not settled.
    let mut moved = action != "restart" || !settled(action, &was);
    let mut done = moved && settled(action, &was);
    while !done && Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        let v = ctx.api.get(&agent_path(agent), vec![]).await?;
        let Some(st) = v.get("status").and_then(Value::as_str) else {
            break;
        };
        if !settled(action, st) {
            moved = true;
        }
        done = moved && settled(action, st);
        now = v;
    }
    let mut out = now;
    out["was"] = json!(was);
    out["waited"] = json!(true);
    out["settled"] = json!(done);
    ctx.emit(&out, |c, r| {
        let (who, st) = (
            format!("{}/{}", field(r, "project"), field(r, "name")),
            field(r, "status"),
        );
        let tail = if st == was {
            String::new()
        } else {
            format!(" (was {was})")
        };
        if done {
            c.line(format!("{action}: {who} is {st}{tail}"));
        } else {
            c.line(format!(
                "{action} requested; {who} is {st} after {} s{tail}",
                LIFECYCLE_WAIT.as_secs()
            ));
        }
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
