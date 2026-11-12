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
    ("HOOKS", "hooks_dir"),
    ("VIA", "via"),
];

pub const AGENT_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("NAME", "name"),
    ("PROJECT", "project"),
    ("HARNESS", "harness"),
    ("STATUS", "state"),
    ("QUEUED", "queued"),
    ("ROLES", "roles"),
    ("CARD", "current_card"),
    ("WAITING ON", "waiting_on"),
];

/// The agents of a list, each with `state` for the table: the status,
/// `paused`, or `stopped (idle)` after an idle stop (PLAN §18).
pub fn agent_rows(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|a| {
            let mut a = a.clone();
            let status = field(&a, "status");
            let state = if a["paused"].as_bool() == Some(true) {
                "paused".to_owned()
            } else if status == "stopped" && a["status_reason"] == "idle" {
                "stopped (idle)".to_owned()
            } else {
                status
            };
            a["state"] = state.into();
            a
        })
        .collect()
}

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
            let grants = ctx
                .api
                .get(&format!("{base}/target-grants"), vec![])
                .await?;
            let v = json!({"project": project, "agents": agents, "repos": repos,
                           "targets": targets, "target_grants": grants});
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
                c.line(format!(
                    "idle alert: {}",
                    p["idle_alert"].as_str().unwrap_or("15m (default)")
                ));
                let authors: Vec<&str> = p["github_authors"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                c.line(format!(
                    "github authors: {}",
                    if authors.is_empty() {
                        "none (no gate)".to_owned()
                    } else {
                        authors.join(", ")
                    }
                ));
                c.line("\nagents:");
                print_table(c, &agent_rows(as_items(&v["agents"])), AGENT_COLS, "  none");
                c.line("\nrepos:");
                print_table(c, as_items(&v["repos"]), REPO_COLS, "  none");
                c.line("\ntargets:");
                print_table(
                    c,
                    as_items(&v["targets"]),
                    &[("NAME", "name"), ("KIND", "kind"), ("SCOPE", "scope")],
                    "  none",
                );
                c.line("\ntarget grants (global targets for project-owned items):");
                print_table(
                    c,
                    as_items(&v["target_grants"]),
                    &[
                        ("TARGET", "target"),
                        ("FOR", "for"),
                        ("BY", "granted_by"),
                        ("AT", "granted_at"),
                    ],
                    "  none",
                );
            })
        }
        ProjectCmd::Grant { target, purpose } => {
            let name = ctx.project_or(None).await?;
            let v = ctx
                .api
                .post(
                    &format!("{}/target-grants", project_path(&name)),
                    Some(json!({"target": target, "for": purpose})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "granted target {} to project {} for {}",
                    field(v, "target"),
                    field(v, "project"),
                    field(v, "for")
                ));
            })
        }
        ProjectCmd::Ungrant { target, purpose } => {
            let name = ctx.project_or(None).await?;
            ctx.api
                .delete(
                    &format!(
                        "{}/target-grants/{}/{}",
                        project_path(&name),
                        encode(target),
                        encode(purpose)
                    ),
                    vec![],
                )
                .await?;
            ctx.emit(
                &json!({"target": target, "project": name, "for": purpose}),
                |c, v| {
                    c.line(format!(
                        "revoked target {} from project {} for {}",
                        field(v, "target"),
                        field(v, "project"),
                        field(v, "for")
                    ));
                },
            )
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
            provenance,
            git_name,
            git_email,
            name,
            description,
            prefix,
            idle_alert,
            github_author,
            github_authors_clear,
            home_board_columns,
        } => {
            let name = ctx.project_or(name.as_deref()).await?;
            if provenance.is_some() || git_name.is_some() || git_email.is_some() {
                let mut git = json!({});
                if let Some(p) = provenance {
                    git["provenance"] = (p == "on").into();
                }
                set_opt(&mut git, "name", git_name.clone());
                set_opt(&mut git, "email", git_email.clone());
                let v = ctx
                    .api
                    .patch(&format!("{}/git", project_path(&name)), git)
                    .await?;
                if description.is_none()
                    && prefix.is_none()
                    && idle_alert.is_none()
                    && github_author.is_empty()
                    && !github_authors_clear
                    && home_board_columns.is_none()
                {
                    return ctx.emit(&v, |c, _| {
                        c.line(format!(
                            "updated git settings of project {name}; refresh checkouts"
                        ))
                    });
                }
            }
            let mut body = json!({});
            set_opt(&mut body, "description", description.clone());
            set_opt(&mut body, "card_prefix", prefix.clone());
            if let Some(columns) = home_board_columns {
                body["home_board_columns"] = serde_json::from_str(columns)?;
            }
            match idle_alert.as_deref().map(str::trim) {
                Some("default") => body["idle_alert"] = Value::Null,
                Some(v) => body["idle_alert"] = v.into(),
                None => {}
            }
            if *github_authors_clear {
                body["github_authors"] = json!([]);
            } else if !github_author.is_empty() {
                body["github_authors"] = json!(github_author);
            }
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
        ProjectCmd::Delivery { command } => super::jobs::delivery(ctx, command).await,
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
    set_opt(&mut body, "hooks_dir", a.hooks_dir.clone());
    if !a.push_deny.is_empty() {
        body["push_deny"] = json!(a.push_deny);
    }
    set_opt(&mut body, "push_via", a.push_via.clone());
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
        RepoCmd::PushChecked { repo } => {
            use std::io::Read as _;
            let mut updates = String::new();
            std::io::stdin().take(65537).read_to_string(&mut updates)?;
            ctx.api
                .post(
                    &format!("/api/repos/{}/push-checked", encode(repo)),
                    Some(json!({"updates": updates})),
                )
                .await?;
            Ok(())
        }
        RepoCmd::Set {
            repo,
            provenance,
            git_name,
            git_email,
            warn_only,
        } => {
            let name = ctx.project_or(None).await?;
            let all = ctx.api.list(&format!("{}/repos", project_path(&name)), vec![], None).await?;
            let id = all.iter().find(|r| r["name"] == repo.as_str() || r["id"] == repo.as_str())
                .map_or_else(|| repo.clone(), |r| field(r, "id"));
            let mut body = json!({});
            if let Some(p) = provenance {
                body["provenance"] = (p == "on").into();
            }
            set_opt(&mut body, "name", git_name.clone());
            set_opt(&mut body, "email", git_email.clone());
            if let Some(warn) = warn_only {
                body["warn_only"] = (*warn).into();
            }
            let v = ctx
                .api
                .patch(&format!("/api/repos/{}/git", encode(&id)), body)
                .await?;
            ctx.emit(&v, |c, _| {
                c.line(format!(
                    "updated git settings of repo {repo}; refresh checkouts"
                ))
            })
        }
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
        RepoCmd::Rm { repo } => repo_rm(ctx, repo).await,
    }
}

/// `repo rm` (TER-658): `DELETE /api/repos/{id}`. No confirm prompt; the
/// daemon checks project_admin and the uses of the repo.
async fn repo_rm(ctx: &Ctx, repo: &str) -> anyhow::Result<()> {
    let name = ctx.project_or(None).await?;
    let all = ctx
        .api
        .list(&format!("{}/repos", project_path(&name)), vec![], None)
        .await?;
    let id = all
        .iter()
        .find(|r| r["name"] == repo || r["id"] == repo)
        .map_or_else(|| repo.to_owned(), |r| field(r, "id"));
    let r = ctx
        .api
        .delete(&format!("/api/repos/{}", encode(&id)), vec![])
        .await?;
    ctx.emit(&r, |c, r| {
        c.line(format!(
            "removed repo {} from {} (the clones stay)",
            field(r, "name"),
            field(r, "project")
        ));
    })
}

/// The first line of the answer of `repo checkout` (TER-37):
///
/// - in an agent workspace: `cloned <repo> in work/<path> of <agent>`;
/// - on a target with an absolute path: `cloned <repo> on <target> at
///   <path>`;
/// - on a target with a path under `$HOME/work/`: `cloned <repo> on
///   <target> in work/<path> (<absolute path>)`.
fn checkout_place(r: &Value) -> String {
    let verb = match r["action"].as_str() {
        Some("cloned") => "cloned",
        Some("updated") => "updated",
        Some("up_to_date") => "up to date:",
        Some("not_updated") => "fetched, not updated:",
        _ => "checked out",
    };
    let repo = field(r, "repo");
    let Some(target) = r["target"].as_str().filter(|t| !t.is_empty()) else {
        return format!(
            "{verb} {repo} in work/{} of {}",
            field(r, "path"),
            field(r, "agent")
        );
    };
    let path = r["path"].as_str().unwrap_or_default();
    let remote = r["remote_path"].as_str().filter(|p| !p.is_empty());
    if path.starts_with('/') || path.is_empty() {
        let at = remote.unwrap_or(path);
        return format!(
            "{verb} {repo} on {target} at {}",
            if at.is_empty() { "-" } else { at }
        );
    }
    match remote {
        Some(abs) => format!("{verb} {repo} on {target} in work/{path} ({abs})"),
        None => format!("{verb} {repo} on {target} in work/{path}"),
    }
}

/// The text answer of `repo checkout`.
fn checkout_lines(c: &Ctx, r: &Value) {
    c.line(checkout_place(r));
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::checkout_place;

    /// TER-37: an agent workspace keeps its line; a target names the target
    /// and the absolute path, also for a relative path under work/, for
    /// each target kind (the answer is the same for docker and ssh).
    #[test]
    fn the_checkout_line_names_the_target_and_the_path() {
        assert_eq!(
            checkout_place(
                &json!({"repo": "app", "path": "app", "agent": "terra/w1", "action": "cloned"})
            ),
            "cloned app in work/app of terra/w1"
        );
        // An absolute --path on an ssh target (the vmlab report).
        assert_eq!(
            checkout_place(&json!({"repo": "vmlab", "target": "vmlab-helios-vmlab",
                "path": "/home/vmlab/src/lead/vmlab",
                "remote_path": "/home/vmlab/src/lead/vmlab", "action": "cloned"})),
            "cloned vmlab on vmlab-helios-vmlab at /home/vmlab/src/lead/vmlab"
        );
        // A relative path on a docker target: under work/, with the
        // absolute path that the target gave.
        assert_eq!(
            checkout_place(&json!({"repo": "app", "target": "box", "path": "app",
                "remote_path": "/home/build/work/app", "action": "updated"})),
            "updated app on box in work/app (/home/build/work/app)"
        );
        // No remote_path (an older daemon): the relative form alone.
        assert_eq!(
            checkout_place(&json!({"repo": "app", "target": "box", "path": "src/app",
                "action": "up_to_date"})),
            "up to date: app on box in work/src/app"
        );
        assert_eq!(
            checkout_place(&json!({"repo": "app", "target": "box", "path": "/srv/app",
                "action": "not_updated"})),
            "fetched, not updated: app on box at /srv/app"
        );
        // Never the workspace prefix with an absolute path, never `of -`.
        let line = checkout_place(&json!({"repo": "r", "target": "t", "path": "/abs",
            "remote_path": "/abs", "action": "cloned"}));
        assert!(!line.contains("work//") && !line.contains(" of "), "{line}");
    }
}
