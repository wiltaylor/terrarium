//! `project` and `repo`.

use serde_json::{Value, json};

use super::{print_table, set_opt};
use crate::api::{CliError, ServerError};
use crate::cli::{ProjectCmd, RepoAddArgs, RepoCmd};
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

pub fn project_path(name: &str) -> String {
    format!("/api/projects/{}", encode(name))
}

const PROJECT_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("PREFIX", "card_prefix"),
    ("SOURCE", "source"),
    ("DESCRIPTION", "description"),
];

const REPO_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("URL", "url"),
    ("BRANCH", "default_branch"),
    ("PATH", "path"),
    ("CREDENTIAL", "credential"),
];

pub const AGENT_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("NAME", "name"),
    ("PROJECT", "project"),
    ("HARNESS", "harness"),
    ("STATUS", "status"),
    ("ROLES", "roles"),
    ("CARD", "current_card"),
];

pub async fn project(ctx: &Ctx, cmd: &ProjectCmd) -> anyhow::Result<()> {
    match cmd {
        ProjectCmd::Ls => {
            let v = Value::Array(ctx.api.list("/api/projects", vec![], None).await?);
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), PROJECT_COLS, "no projects");
            })
        }
        ProjectCmd::Show { name } => {
            let name = ctx.project_or(name.as_deref()).await?;
            let base = project_path(&name);
            let project = ctx.api.get(&base, vec![]).await?;
            let agents = ctx
                .api
                .list(&format!("{base}/agents"), vec![], None)
                .await?;
            let repos = ctx.api.list(&format!("{base}/repos"), vec![], None).await?;
            let targets = ctx
                .api
                .list(&format!("{base}/targets"), vec![], None)
                .await?;
            let v =
                json!({"project": project, "agents": agents, "repos": repos, "targets": targets});
            ctx.emit(&v, |c, v| {
                let p = &v["project"];
                c.line(format!(
                    "project: {}  prefix: {}  source: {}",
                    field(p, "name"),
                    field(p, "card_prefix"),
                    field(p, "source")
                ));
                if let Some(d) = p["description"].as_str().filter(|d| !d.is_empty()) {
                    c.line(d);
                }
                c.line("\nagents:");
                print_table(c, as_items(&v["agents"]), AGENT_COLS, "  none");
                c.line("\nrepos:");
                print_table(c, as_items(&v["repos"]), REPO_COLS, "  none");
                c.line("\ntargets:");
                print_table(
                    c,
                    as_items(&v["targets"]),
                    &[("NAME", "name"), ("KIND", "kind"), ("SCOPE", "scope")],
                    "  none",
                );
            })
        }
        ProjectCmd::New {
            name,
            description,
            prefix,
        } => {
            let mut body = json!({"name": name});
            set_opt(&mut body, "description", description.clone());
            set_opt(&mut body, "card_prefix", prefix.clone());
            let p = ctx.api.post("/api/projects", Some(body)).await?;
            ctx.emit(&p, |c, p| {
                c.line(format!(
                    "created project {} (cards {}-N)",
                    field(p, "name"),
                    field(p, "card_prefix")
                ));
            })
        }
        ProjectCmd::Set {
            name,
            description,
            prefix,
        } => {
            let name = ctx.project_or(name.as_deref()).await?;
            let mut body = json!({});
            set_opt(&mut body, "description", description.clone());
            set_opt(&mut body, "card_prefix", prefix.clone());
            let p = ctx.api.patch(&project_path(&name), body).await?;
            ctx.emit(&p, |c, p| {
                c.line(format!("updated project {}", field(p, "name")))
            })
        }
        ProjectCmd::Rm { name, confirm } => {
            let q = if *confirm {
                vec![("confirm".into(), "true".into())]
            } else {
                vec![]
            };
            ctx.api.delete(&project_path(name), q).await?;
            ctx.emit(&json!({"deleted": name}), |c, _| {
                c.line(format!("deleted project {name}"));
            })
        }
        ProjectCmd::Repos { name } => repos(ctx, name.as_deref()).await,
        ProjectCmd::RepoAdd(a) => repo_add(ctx, a).await,
    }
}

async fn repos(ctx: &Ctx, name: Option<&str>) -> anyhow::Result<()> {
    let name = ctx.project_or(name).await?;
    let v = Value::Array(
        ctx.api
            .list(&format!("{}/repos", project_path(&name)), vec![], None)
            .await?,
    );
    ctx.emit(&v, |c, v| {
        print_table(c, as_items(v), REPO_COLS, "no repos")
    })
}

async fn repo_add(ctx: &Ctx, a: &RepoAddArgs) -> anyhow::Result<()> {
    let name = ctx.project_or(None).await?;
    let mut body = json!({"url": a.url});
    set_opt(&mut body, "name", a.name.clone());
    set_opt(&mut body, "default_branch", a.branch.clone());
    set_opt(&mut body, "credential", a.credential.clone());
    set_opt(&mut body, "path", a.path.clone());
    let r = ctx
        .api
        .post(&format!("{}/repos", project_path(&name)), Some(body))
        .await?;
    ctx.emit(&r, |c, r| {
        c.line(format!(
            "attached repo {} to {}",
            field(r, "name"),
            field(r, "project")
        ));
    })
}

pub async fn repo(ctx: &Ctx, cmd: &RepoCmd) -> anyhow::Result<()> {
    match cmd {
        RepoCmd::Ls => repos(ctx, None).await,
        RepoCmd::Add(a) => repo_add(ctx, a).await,
        RepoCmd::Checkout {
            repo,
            target,
            branch,
            path,
            depth,
            agent,
        } => {
            let name = ctx.project_or(None).await?;
            let all = ctx
                .api
                .list(&format!("{}/repos", project_path(&name)), vec![], None)
                .await?;
            let id = all
                .iter()
                .find(|r| r["name"] == repo.as_str() || r["id"] == repo.as_str())
                .map_or_else(|| repo.clone(), |r| field(r, "id"));
            let mut body = json!({});
            set_opt(&mut body, "target", target.clone());
            set_opt(&mut body, "branch", branch.clone());
            set_opt(&mut body, "path", path.clone());
            set_opt(&mut body, "depth", *depth);
            set_opt(&mut body, "agent", agent.clone());
            let r = match ctx
                .api
                .post(&format!("/api/repos/{}/checkout", encode(&id)), Some(body))
                .await
            {
                Err(e)
                    if e.downcast_ref::<ServerError>()
                        .is_some_and(ServerError::is_missing_route) =>
                {
                    return Err(CliError::not_available(format!(
                        "awaiting server: the daemon does not check out repos yet (POST /api/repos/{id}/checkout)"
                    )));
                }
                Err(e)
                    if e.downcast_ref::<ServerError>()
                        .is_some_and(|s| s.status == 501) =>
                {
                    let msg = e
                        .downcast_ref::<ServerError>()
                        .map(|s| s.message.clone())
                        .unwrap_or_default();
                    return Err(CliError::not_available(msg));
                }
                r => r?,
            };
            ctx.emit(&r, checkout_lines)
        }
        RepoCmd::Credential { operation } => credential(ctx, operation).await,
    }
}

/// The text answer of `repo checkout`.
fn checkout_lines(c: &Ctx, r: &Value) {
    let verb = match r["action"].as_str() {
        Some("cloned") => "cloned",
        Some("updated") => "updated",
        Some("up_to_date") => "up to date:",
        Some("not_updated") => "fetched, not updated:",
        _ => "checked out",
    };
    c.line(format!(
        "{verb} {} in work/{} of {}",
        field(r, "repo"),
        field(r, "path"),
        field(r, "agent")
    ));
    c.line(format!(
        "HEAD {} ({})",
        field(r, "head"),
        field(r, "branch")
    ));
    if let Some(note) = r["note"].as_str() {
        c.line(note);
    }
}

/// The fields of a git credential helper request: `key=value` lines up to
/// the first blank line.
pub fn parse_helper_input(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// `terrarium repo credential <operation>`: git's credential helper. Only
/// `get` answers, through the daemon (`POST /api/repos/credential`). A
/// `404` prints nothing, so git falls back to its next helper or fails
/// cleanly. A secret never goes to a log or to stderr.
async fn credential(ctx: &Ctx, operation: &str) -> anyhow::Result<()> {
    if operation != "get" {
        return Ok(());
    }
    let input = parse_helper_input(&ctx.stdin_until_blank()?);
    let (Some(protocol), Some(host)) = (input.get("protocol"), input.get("host")) else {
        return Ok(());
    };
    let mut body = json!({"protocol": protocol, "host": host});
    set_opt(&mut body, "path", input.get("path").cloned());
    match ctx.api.post("/api/repos/credential", Some(body)).await {
        Ok(r) => {
            let (Some(user), Some(pass)) = (r["username"].as_str(), r["password"].as_str()) else {
                return Ok(());
            };
            if user.contains('\n') || pass.contains('\n') {
                return Ok(());
            }
            ctx.out
                .write_str(&format!("username={user}\npassword={pass}\n"));
            Ok(())
        }
        Err(e)
            if e.downcast_ref::<ServerError>()
                .is_some_and(|s| s.status == 404) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}
