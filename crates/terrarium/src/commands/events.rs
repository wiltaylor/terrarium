//! `event emit` and `event source ...`: docs/api.md §20.

use serde_json::{Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::{EventCmd, SourceCmd};
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::{field, short_time};
use crate::transport::encode;

/// `/api/event-sources/{key}` for a ULID, `project/name`, or a name.
fn path(key: &str) -> String {
    format!("/api/event-sources/{}", encode(key))
}

/// `project/name`, or `_/name` for a global source.
fn key_of(s: &Value) -> String {
    format!(
        "{}/{}",
        s["project"].as_str().unwrap_or("_"),
        field(s, "name")
    )
}

/// One flat row of `event source ls`.
fn row(s: &Value) -> Value {
    json!({
        "id": s["id"],
        "key": key_of(s),
        "kind": s["kind"],
        "enabled": if s["enabled"] == true { "yes" } else { "no" },
        "received": s["received"],
        "last_seen": s["last_seen"],
        "error": if s["last_error"].is_string() { "yes" } else { "" },
    })
}

const COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("SOURCE", "key"),
    ("KIND", "kind"),
    ("ON", "enabled"),
    ("EVENTS", "received"),
    ("LAST SEEN", "last_seen"),
    ("ERROR", "error"),
];

fn print_source(ctx: &Ctx, s: &Value) {
    ctx.line(format!("source {}  {}", key_of(s), field(s, "id")));
    ctx.line(format!("kind:      {}", field(s, "kind")));
    ctx.line(format!(
        "enabled:   {}",
        if s["enabled"] == true { "yes" } else { "no" }
    ));
    ctx.line(format!(
        "hook URL:  {}",
        s["hook_url"]
            .as_str()
            .unwrap_or_else(|| s["hook_path"].as_str().unwrap_or("-"))
    ));
    if let Some(note) = s["hook_note"].as_str() {
        ctx.line(format!("           ({note})"));
    }
    ctx.line(format!(
        "events:    {}   last seen: {}",
        field(s, "received"),
        short_time(&field(s, "last_seen"))
    ));
    if let Some(e) = s["last_error"].as_str() {
        ctx.line(format!("last error: {e}"));
    }
    ctx.line(format!("made by:   {}", field(s, "created_by")));
}

/// The answer of `add` and `rotate`: the source, the secret one time, and
/// the setup steps.
fn print_secret(ctx: &Ctx, s: &Value) {
    print_source(ctx, s);
    ctx.line(format!("secret:    {}", field(s, "secret")));
    ctx.line("           (shown this one time; `event source rotate` makes a new one)");
    if let Some(steps) = s["setup"].as_array().filter(|a| !a.is_empty()) {
        ctx.line("setup:");
        for step in steps.iter().filter_map(Value::as_str) {
            ctx.line(format!("  - {step}"));
        }
    }
}

pub async fn event(ctx: &Ctx, cmd: &EventCmd) -> anyhow::Result<()> {
    match cmd {
        EventCmd::Emit {
            kind,
            summary,
            data,
        } => {
            let mut b = json!({"kind": kind});
            if let Some(s) = summary {
                b["summary"] = json!(s);
            }
            if let Some(d) = data {
                let text = ctx.text(d)?;
                let v: Value = serde_json::from_str(&text)
                    .map_err(|e| CliError::usage(format!("bad --data: {e}; use a JSON object")))?;
                if !v.is_object() {
                    return Err(CliError::usage("bad --data: use a JSON object"));
                }
                b["data"] = v;
            }
            if let Some(p) = &ctx.project {
                b["project"] = json!(p);
            }
            let e = ctx.api.post("/api/events", Some(b)).await?;
            ctx.emit(&e, |c, e| {
                c.line(format!(
                    "event {} {} ({})",
                    field(e, "seq"),
                    field(e, "kind"),
                    e["project"].as_str().unwrap_or("no project")
                ));
            })
        }
        EventCmd::Source { command } => source(ctx, command).await,
    }
}

async fn source(ctx: &Ctx, cmd: &SourceCmd) -> anyhow::Result<()> {
    match cmd {
        SourceCmd::Ls => {
            let q = query([("project", ctx.project.clone())]);
            let v = ctx.api.get("/api/event-sources", q).await?;
            ctx.emit(&v, |c, v| {
                let rows: Vec<Value> = as_items(v).iter().map(row).collect();
                print_table(c, &rows, COLS, "no event sources");
            })
        }
        SourceCmd::Show { source } => {
            let v = ctx.api.get(&path(source), Vec::new()).await?;
            ctx.emit(&v, print_source)
        }
        SourceCmd::Add {
            name,
            kind,
            global,
            disabled,
        } => {
            if !matches!(kind.as_str(), "github" | "forgejo" | "generic") {
                return Err(CliError::usage(format!(
                    "bad --kind `{kind}`: use github, forgejo, or generic"
                )));
            }
            let project = if *global {
                if ctx.project.is_some() {
                    return Err(CliError::usage("use --project or --global, not both"));
                }
                Value::Null
            } else {
                match ctx.project_or(None).await {
                    Ok(p) => json!(p),
                    Err(_) => {
                        return Err(CliError::usage(
                            "name the project with --project <name>, or make a global source with --global",
                        ));
                    }
                }
            };
            let mut b = json!({"name": name, "project": project, "kind": kind});
            if *disabled {
                b["enabled"] = json!(false);
            }
            let s = ctx.api.post("/api/event-sources", Some(b)).await?;
            ctx.emit(&s, print_secret)
        }
        SourceCmd::Rm { source } => {
            ctx.api.delete(&path(source), Vec::new()).await?;
            let v = json!({"deleted": source});
            ctx.emit(&v, |c, _| c.line(format!("deleted event source {source}")))
        }
        SourceCmd::Rotate { source } => {
            let s = ctx
                .api
                .post(&format!("{}/rotate", path(source)), None)
                .await?;
            ctx.emit(&s, print_secret)
        }
        SourceCmd::Enable { source } | SourceCmd::Disable { source } => {
            let on = matches!(cmd, SourceCmd::Enable { .. });
            let s = ctx.api.patch(&path(source), json!({"enabled": on})).await?;
            ctx.emit(&s, |c, s| {
                c.line(format!(
                    "event source {} {}",
                    key_of(s),
                    if s["enabled"] == true {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ));
            })
        }
        SourceCmd::Rename { source, name } => {
            let s = ctx.api.patch(&path(source), json!({"name": name})).await?;
            ctx.emit(&s, |c, s| {
                c.line(format!(
                    "event source renamed to {}; hook URL {}",
                    key_of(s),
                    s["hook_url"]
                        .as_str()
                        .unwrap_or_else(|| s["hook_path"].as_str().unwrap_or("-"))
                ));
            })
        }
    }
}
