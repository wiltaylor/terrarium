//! `terrarium admin config ...`: the config repo (PLAN §8.3, docs/api.md
//! §25). `scan` runs on this machine with no daemon: the `pre-commit` hook
//! of the repo calls it.

use serde_json::{Value, json};

use crate::api::CliError;
use crate::cli::{ConfigCmd, ConfigDefaultsCmd, ConfigRemoteCmd};
use crate::ctx::Ctx;
use crate::output::field;

pub async fn config(ctx: &Ctx, cmd: &ConfigCmd) -> anyhow::Result<()> {
    match cmd {
        ConfigCmd::Status => {
            let v = ctx.api.get("/api/config/status", vec![]).await?;
            ctx.emit(&v, print_status)
        }
        ConfigCmd::Diff { path } => {
            let q = path
                .iter()
                .map(|p| ("path".to_owned(), p.clone()))
                .collect();
            let v = ctx.api.get("/api/config/diff", q).await?;
            ctx.emit(&v, |c, v| {
                let d = v["diff"].as_str().unwrap_or_default();
                if d.is_empty() {
                    c.line("no changes");
                } else {
                    c.out.write_str(d);
                }
            })
        }
        ConfigCmd::Commit {
            message,
            no_export,
            force_export,
        } => {
            let body = json!({"message": message, "export": !no_export,
                              "force_export": force_export});
            let v = ctx.api.post("/api/config/commit", Some(body)).await?;
            ctx.emit(&v, print_commit)
        }
        ConfigCmd::Log { limit } => {
            let v = ctx
                .api
                .get("/api/config/log", vec![("limit".into(), limit.to_string())])
                .await?;
            ctx.emit(&v, |c, v| {
                for e in v.as_array().into_iter().flatten() {
                    let commit = field(e, "commit");
                    c.line(format!(
                        "{}  {}  {:<12} {}",
                        commit.get(..10).unwrap_or(&commit),
                        crate::output::short_time(&field(e, "at")),
                        field(e, "author"),
                        field(e, "subject")
                    ));
                }
            })
        }
        ConfigCmd::Push => {
            let v = ctx.api.post("/api/config/push", None).await?;
            ctx.emit(&v, |c, v| {
                c.line(if v["pushed"] == true {
                    "pushed"
                } else {
                    "nothing to push"
                });
            })
        }
        ConfigCmd::Pull {
            apply,
            approve_grants,
        } => {
            let v = ctx
                .api
                .post(
                    "/api/config/pull",
                    Some(json!({"apply": apply, "approve_grants": approve_grants})),
                )
                .await?;
            ctx.emit(&v, print_merge)
        }
        ConfigCmd::Sync {
            message,
            apply,
            approve_grants,
        } => {
            let mut body = json!({"apply": apply, "approve_grants": approve_grants});
            if let Some(m) = message {
                body["message"] = m.clone().into();
            }
            let v = ctx.api.post("/api/config/sync", Some(body)).await?;
            ctx.emit(&v, |c, v| {
                print_commit(c, &v["committed"]);
                print_merge(c, v);
                c.line(if v["pushed"] == true {
                    "pushed"
                } else {
                    "nothing to push"
                });
            })
        }
        ConfigCmd::Abort => {
            let v = ctx.api.post("/api/config/abort", None).await?;
            ctx.emit(&v, |c, _| {
                c.line("merge aborted: the repo is back at your last commit")
            })
        }
        ConfigCmd::Remote { command } => match command {
            ConfigRemoteCmd::Set {
                url,
                credential,
                branch,
            } => {
                let mut body = json!({"url": url});
                if let Some(c) = credential {
                    body["credential"] = c.clone().into();
                }
                if let Some(b) = branch {
                    body["branch"] = b.clone().into();
                }
                let v = ctx.api.put("/api/config/remote", body).await?;
                ctx.emit(&v, print_status)
            }
            ConfigRemoteCmd::Rm => {
                let v = ctx.api.delete("/api/config/remote", vec![]).await?;
                ctx.emit(&v, print_status)
            }
        },
        ConfigCmd::Defaults { command } => {
            let v = match command.as_ref().unwrap_or(&ConfigDefaultsCmd::Ls) {
                ConfigDefaultsCmd::Ls => ctx.api.get("/api/config/defaults", vec![]).await?,
                ConfigDefaultsCmd::Accept { path } => {
                    ctx.api
                        .post("/api/config/defaults/accept", Some(json!({"path": path})))
                        .await?
                }
                ConfigDefaultsCmd::Keep { path } => {
                    ctx.api
                        .post("/api/config/defaults/keep", Some(json!({"path": path})))
                        .await?
                }
            };
            ctx.emit(&v, print_pending)
        }
        ConfigCmd::Scan {
            staged,
            repo,
            files,
        } => scan(ctx, *staged, repo.as_deref(), files),
    }
}

/// The secret scan on this machine. A finding prints the file, the line,
/// and the rule, and exits 1.
fn scan(
    ctx: &Ctx,
    staged: bool,
    repo: Option<&std::path::Path>,
    files: &[std::path::PathBuf],
) -> anyhow::Result<()> {
    use terrarium_core::secret_scan;
    if !staged && files.is_empty() {
        return Err(CliError::usage("give --staged or files to scan"));
    }
    let mut found = Vec::new();
    if staged {
        let repo = repo.unwrap_or(std::path::Path::new("."));
        found.extend(secret_scan::scan_staged(repo).map_err(anyhow::Error::msg)?);
    }
    for f in files {
        let bytes =
            std::fs::read(f).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", f.display()))?;
        found.extend(secret_scan::scan(&f.to_string_lossy(), &bytes));
    }
    if ctx.json {
        ctx.line(json!({"findings": found}).to_string());
    }
    if found.is_empty() {
        return Ok(());
    }
    if !ctx.json {
        for f in &found {
            eprintln!("secret scan: {f}");
        }
    }
    anyhow::bail!(
        "the secret scan found {} secret(s); the commit is blocked. Remove each one, or for a \
         false hit add `{}` to the line",
        found.len(),
        secret_scan::ALLOW_MARKER
    )
}

fn print_status(c: &Ctx, v: &Value) {
    c.line(format!("repo:     {}", field(v, "dir")));
    if v["initialized"] != true {
        c.line("state:    not a git repo yet (the next commit makes it)");
        return;
    }
    let head = field(v, "head");
    c.line(format!(
        "branch:   {} at {}",
        field(v, "branch"),
        head.get(..10).unwrap_or(&head)
    ));
    match v.get("remote").filter(|r| r.is_object()) {
        Some(r) => {
            let cred = r["credential"]
                .as_str()
                .map(|c| format!(" (credential {c})"))
                .unwrap_or_default();
            c.line(format!("remote:   {}{cred}", field(r, "url")));
            c.line(format!(
                "          {} ahead, {} behind (at the last fetch)",
                field(v, "ahead"),
                field(v, "behind")
            ));
        }
        None => c.line("remote:   none (`terrarium admin config remote set <url>`)"),
    }
    c.line(format!("auto-sync: {}", field(v, "auto_sync")));
    if v["merging"] == true {
        c.line(format!(
            "MERGE WAITS: resolve {} and commit, or `terrarium admin config abort`",
            list(&v["conflicts"])
        ));
    }
    let changes = v["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() {
        c.line("changes:  none");
    } else {
        c.line("changes:");
        for ch in &changes {
            c.line(format!(
                "  {:<10} {}",
                field(ch, "status"),
                field(ch, "path")
            ));
        }
    }
    let pending = v["pending_defaults"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !pending.is_empty() {
        c.line(format!(
            "new defaults wait: {} (`terrarium admin config defaults`)",
            list(&v["pending_defaults"])
        ));
    }
    if v["export_pending"] == true {
        c.line("export/ has pulled changes that are not applied: `terrarium admin config pull --apply`");
    }
}

fn list(v: &Value) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_commit(c: &Ctx, v: &Value) {
    let export = v["export"].as_array().map_or(0, Vec::len);
    if export > 0 {
        c.line(format!("export: wrote {export} file(s)"));
    }
    match v["commit"].as_str() {
        Some(commit) => c.line(format!(
            "committed {} ({} file(s)): {}",
            commit.get(..10).unwrap_or(commit),
            v["files"].as_array().map_or(0, Vec::len),
            list(&v["files"])
        )),
        None => c.line("nothing to commit"),
    }
}

fn print_merge(c: &Ctx, v: &Value) {
    if v["fetched"] != true {
        c.line("the remote has no branch yet");
    } else if v["updated"] == true {
        c.line(format!("merged: {}", list(&v["changed"])));
        c.line("prompts and harness settings apply at the next start of each agent");
    } else {
        c.line("up to date with the remote");
    }
    for e in v["export"].as_array().into_iter().flatten() {
        let path = field(e, "path");
        if let Some(err) = e["error"].as_str() {
            c.line(format!("{path}: {err}"));
        } else if e["applied"].is_object() {
            c.line(format!("{path}: applied"));
        } else if let Some(p) = e.get("plan") {
            let s = &p["summary"];
            c.line(format!(
                "{path}: plan {} create, {} update, {} unchanged (apply with --apply)",
                field(s, "create"),
                field(s, "update"),
                field(s, "unchanged")
            ));
            for entry in p["entries"].as_array().into_iter().flatten() {
                let action = field(entry, "action");
                if action != "unchanged" {
                    c.line(format!(
                        "  {action:<9} {} {}",
                        field(entry, "kind"),
                        field(entry, "key")
                    ));
                }
            }
        }
    }
    print_secrets(c, v);
}

/// The secrets of a pulled export (docs/api.md §25.3): the changes of the
/// import (or its plan), and the warnings. No value.
fn print_secrets(c: &Ctx, v: &Value) {
    let s = &v["secrets"];
    if !s.is_object() {
        return;
    }
    if let Some(err) = s["error"].as_str() {
        c.line(format!("secrets: {err}"));
    }
    for ch in s["changes"].as_array().into_iter().flatten() {
        let grantee = ch["grantee"]
            .as_str()
            .map(|g| format!(" {g}"))
            .unwrap_or_default();
        // A new or wider grant of a secret with a value (docs/api.md §25.3).
        let approval = match ch["approval"].as_str() {
            Some("needed") => " (needs the operator: --apply --approve-grants)",
            Some("held") => " (held: not made; the operator applies it with --approve-grants)",
            Some("approved") => " (approved)",
            _ => "",
        };
        c.line(format!(
            "secrets: {:<7} {}{grantee}{approval}",
            field(ch, "action"),
            field(ch, "secret")
        ));
    }
    for w in s["warnings"].as_array().into_iter().flatten() {
        if let Some(w) = w.as_str() {
            c.line(format!("secrets: warning: {w}"));
        }
    }
}

fn print_pending(c: &Ctx, v: &Value) {
    let items = v.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        c.line("no new defaults wait");
        return;
    }
    for p in &items {
        c.line(format!(
            "{} (accept or keep: `terrarium admin config defaults accept|keep {}`)",
            field(p, "path"),
            field(p, "path")
        ));
        c.out.write_str(p["diff"].as_str().unwrap_or_default());
    }
}
