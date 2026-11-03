//! `pipeline` (docs/api.md §31): the CI/CD pipelines of a project and
//! their runs.

use anyhow::Context;
use serde_json::{Map, Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::{PipelineArgs, PipelineCmd};
use crate::commands::monitors::schedule_text;
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

/// The path of a pipeline: an ID or `project/name` as it is, a bare name
/// in the project of `--project` (or the agent's own).
async fn pipeline_path(ctx: &Ctx, key: &str) -> anyhow::Result<String> {
    let is_ulid = key.len() == 26 && key.chars().all(|c| c.is_ascii_alphanumeric());
    let key = if key.contains('/') || is_ulid {
        key.to_owned()
    } else {
        format!("{}/{key}", ctx.project_or(None).await?)
    };
    Ok(format!("/api/pipelines/{}", encode(&key)))
}

/// The source as text: `github o/r ci.yml@main`, `script on wsl`.
pub fn source_text(s: &Value) -> String {
    match s["kind"].as_str() {
        Some("github") => {
            let mut t = format!("github {}", field(s, "repo"));
            if let Some(w) = s["workflow"].as_str() {
                t.push_str(&format!(" {w}"));
            }
            if let Some(b) = s["branch"].as_str() {
                t.push_str(&format!("@{b}"));
            }
            t
        }
        Some("script") => format!("script on {}", field(s, "target")),
        _ => "-".into(),
    }
}

/// The status of a pipeline for a table: `disabled`, `error`, the status
/// of the newest run, or `pending` before the first poll.
fn status_text(p: &Value) -> String {
    if p["enabled"] == false {
        return "disabled".into();
    }
    if p["error"].is_string() {
        return format!("error ({})", p["status"].as_str().unwrap_or("-"));
    }
    match p["status"].as_str() {
        Some(s) => s.to_owned(),
        None if p["polled_at"].is_null() => "pending".into(),
        None => "no runs".into(),
    }
}

/// The body of `pipeline new`, `edit`, and `test` from the flags. `kind`
/// comes only with a flag that picks the adapter (`--github`,
/// `--target`, a script), so an edit of a branch keeps the source.
pub fn pipeline_body(ctx: &Ctx, a: &PipelineArgs) -> anyhow::Result<Map<String, Value>> {
    let mut b = Map::new();
    let mut src = Map::new();
    for (k, v) in [
        ("repo", a.github.as_ref()),
        ("workflow", a.workflow.as_ref()),
        ("branch", a.branch.as_ref()),
        ("token_secret", a.token_secret.as_ref()),
        ("api_url", a.api_url.as_ref()),
        ("target", a.target.as_ref()),
    ] {
        if let Some(v) = v {
            src.insert(k.into(), json!(v));
        }
    }
    if let Some(s) = &a.script {
        src.insert("script".into(), json!(ctx.text(s)?));
    }
    if let Some(p) = &a.script_file {
        let s =
            std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
        src.insert("script".into(), json!(s));
    }
    if let Some(t) = a.timeout {
        src.insert("timeout_secs".into(), json!(t));
    }
    if !a.env_secret.is_empty() {
        let mut env = Map::new();
        for e in &a.env_secret {
            let Some((k, v)) = e
                .split_once('=')
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            else {
                return Err(CliError::usage(format!(
                    "--env-secret {e}: give VAR=secret, for example OCTOPUS_API_KEY=octopus-key"
                )));
            };
            env.insert(k.into(), json!(v));
        }
        src.insert("env_secrets".into(), Value::Object(env));
    }
    if a.github.is_some() {
        src.insert("kind".into(), json!("github"));
    } else if a.target.is_some() || src.contains_key("script") {
        src.insert("kind".into(), json!("script"));
    }
    if !src.is_empty() {
        b.insert("source".into(), Value::Object(src));
    }
    if let Some(e) = &a.every {
        b.insert("schedule".into(), json!({"kind": "interval", "every": e}));
    }
    if let Some(c) = &a.cron {
        b.insert(
            "schedule".into(),
            json!({"kind": "cron", "expr": c, "tz": a.tz.as_deref().unwrap_or("UTC")}),
        );
    }
    if let Some(n) = a.max_runs {
        b.insert("max_runs".into(), json!(n));
    }
    if let Some(d) = &a.description {
        b.insert("description".into(), json!(d));
    }
    if a.disabled {
        b.insert("enabled".into(), json!(false));
    }
    Ok(b)
}

/// One run as one line: `#12 failed  main abc1234  2026-11-03T10:00:00Z`.
fn run_line(r: &Value) -> String {
    let mut parts = vec![
        r["name"]
            .as_str()
            .or(r["id"].as_str())
            .unwrap_or("-")
            .to_owned(),
        field(r, "status").to_uppercase(),
    ];
    for k in ["environment", "branch"] {
        if let Some(v) = r[k].as_str() {
            parts.push(v.to_owned());
        }
    }
    if let Some(c) = r["commit"].as_str() {
        parts.push(c.chars().take(10).collect());
    }
    if let Some(t) = r["started_at"].as_str() {
        parts.push(t.to_owned());
    }
    parts.join("  ")
}

fn print_pipeline(c: &Ctx, p: &Value) {
    c.line(format!(
        "{} {}  [{}]  {}",
        field(p, "id"),
        field(p, "name"),
        source_text(&p["source"]),
        status_text(p)
    ));
    if let Some(d) = p["description"].as_str() {
        c.line(d);
    }
    let s = &p["source"];
    if let Some(t) = s["token_secret"].as_str() {
        c.line(format!("token secret: {t}"));
    }
    if let Some(u) = s["api_url"].as_str() {
        c.line(format!("api: {u}"));
    }
    if let Some(env) = s["env_secrets"].as_object() {
        let list: Vec<String> = env
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
            .collect();
        c.line(format!("env secrets: {}", list.join(", ")));
    }
    c.line(format!(
        "schedule: {}  keeps: {} runs  next poll: {}",
        schedule_text(&p["schedule"]),
        field(p, "max_runs"),
        field(p, "next_run_at"),
    ));
    c.line(format!(
        "last poll: {}  last good poll: {}  polls as: {}",
        field(p, "polled_at"),
        field(p, "ok_at"),
        field(p, "run_as")
    ));
    if let Some(e) = p["error"].as_str() {
        c.line(format!("error: {e}"));
    }
    match p["latest"].as_object() {
        Some(_) => {
            c.line(format!("latest: {}", run_line(&p["latest"])));
            if let Some(u) = p["latest"]["url"].as_str() {
                c.line(format!("        {u}"));
            }
        }
        None => c.line("latest: no runs yet"),
    }
    if let Some(script) = s["script"].as_str() {
        c.line("script:");
        for l in script.lines() {
            c.line(format!("  {l}"));
        }
    }
}

fn print_run(c: &Ctx, r: &Value) {
    c.line(run_line(r));
    for (label, k) in [
        ("id", "id"),
        ("url", "url"),
        ("actor", "actor"),
        ("event", "event"),
        ("finished", "finished_at"),
    ] {
        if let Some(v) = r[k].as_str() {
            c.line(format!("{label}: {v}"));
        }
    }
    if let Some(s) = r["summary"].as_str() {
        c.line(format!("summary: {s}"));
    }
    match r["steps"].as_array() {
        Some(steps) if !steps.is_empty() => {
            let rows: Vec<Value> = steps.to_vec();
            print_table(
                c,
                &rows,
                &[
                    ("STEP", "name"),
                    ("STATUS", "status"),
                    ("STARTED", "started_at"),
                    ("FINISHED", "finished_at"),
                ],
                "",
            );
        }
        _ => c.line("no steps"),
    }
    if let Some(e) = r["steps_error"].as_str() {
        c.line(format!("the steps did not load: {e}"));
    }
}

fn print_output(c: &Ctx, o: &Value) {
    if !o.is_object() {
        return;
    }
    c.line(format!("exit code: {}", field(o, "exit_code")));
    for key in ["stdout", "stderr"] {
        let s = o[key].as_str().unwrap_or_default();
        if !s.trim().is_empty() {
            c.line(format!("{key}:"));
            for l in s.lines().take(40) {
                c.line(format!("  {l}"));
            }
        }
    }
}

fn need_source(b: &Map<String, Value>) -> anyhow::Result<()> {
    let src = b.get("source").cloned().unwrap_or(Value::Null);
    match src["kind"].as_str() {
        Some("github") => Ok(()),
        Some("script") => {
            for (flag, key) in [
                ("--target", "target"),
                ("--script or --script-file", "script"),
            ] {
                if src.get(key).is_none() {
                    return Err(CliError::usage(format!("a script pipeline needs {flag}")));
                }
            }
            Ok(())
        }
        _ => Err(CliError::usage(
            "give --github owner/repo, or --target and --script for a script pipeline",
        )),
    }
}

pub async fn pipeline(ctx: &Ctx, cmd: &PipelineCmd) -> anyhow::Result<()> {
    match cmd {
        PipelineCmd::Ls => {
            let p = ctx.project_or(None).await?;
            let v = ctx
                .api
                .get(&format!("{}/pipelines", project_path(&p)), vec![])
                .await?;
            let rows: Vec<Value> = as_items(&v)
                .iter()
                .map(|x| {
                    let mut r = x.clone();
                    r["status"] = json!(status_text(x));
                    r["src"] = json!(source_text(&x["source"]));
                    r["run"] = json!(
                        x["latest"]["name"]
                            .as_str()
                            .or(x["latest"]["id"].as_str())
                            .unwrap_or("-")
                    );
                    r["at"] = x["latest"]["started_at"].clone();
                    r
                })
                .collect();
            ctx.emit(&v, |c, _| {
                print_table(
                    c,
                    &rows,
                    &[
                        ("NAME", "name"),
                        ("STATUS", "status"),
                        ("LATEST", "run"),
                        ("STARTED", "at"),
                        ("SOURCE", "src"),
                        ("ID", "id"),
                    ],
                    "no pipelines",
                );
            })
        }
        PipelineCmd::Show { pipeline } => {
            let v = ctx
                .api
                .get(&pipeline_path(ctx, pipeline).await?, vec![])
                .await?;
            ctx.emit(&v, print_pipeline)
        }
        PipelineCmd::New { name, args } => {
            let p = ctx.project_or(None).await?;
            let mut b = pipeline_body(ctx, args)?;
            need_source(&b)?;
            b.insert("name".into(), json!(name));
            let v = ctx
                .api
                .post(
                    &format!("{}/pipelines", project_path(&p)),
                    Some(Value::Object(b)),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "created pipeline {} ({}): {}, {}",
                    field(v, "name"),
                    field(v, "id"),
                    source_text(&v["source"]),
                    schedule_text(&v["schedule"])
                ));
            })
        }
        PipelineCmd::Edit {
            pipeline,
            rename,
            enable,
            args,
        } => {
            let mut b = pipeline_body(ctx, args)?;
            if let Some(n) = rename {
                b.insert("name".into(), json!(n));
            }
            if *enable {
                b.insert("enabled".into(), json!(true));
            }
            if b.is_empty() {
                return Err(CliError::usage("give a flag to change"));
            }
            let v = ctx
                .api
                .patch(&pipeline_path(ctx, pipeline).await?, Value::Object(b))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("updated pipeline {}", field(v, "name")));
            })
        }
        PipelineCmd::Rm { pipeline } => {
            ctx.api
                .delete(&pipeline_path(ctx, pipeline).await?, vec![])
                .await?;
            ctx.emit(&json!({"deleted": pipeline}), |c, _| {
                c.line(format!("deleted pipeline {pipeline}"));
            })
        }
        PipelineCmd::Refresh { pipeline } => {
            let v = ctx
                .api
                .post(
                    &format!("{}/refresh", pipeline_path(ctx, pipeline).await?),
                    None,
                )
                .await?;
            ctx.emit(&v, |c, v| {
                let p = &v["pipeline"];
                let poll = &v["poll"];
                match p["error"].as_str() {
                    Some(e) => c.line(format!("{}: the poll failed: {e}", field(p, "name"))),
                    None => c.line(format!(
                        "{}: {}  ({} runs, {} new, {} changed)",
                        field(p, "name"),
                        status_text(p),
                        field(poll, "runs"),
                        field(poll, "new"),
                        field(poll, "changed")
                    )),
                }
                if p["latest"].is_object() {
                    c.line(format!("latest: {}", run_line(&p["latest"])));
                }
                if p["error"].is_string() {
                    print_output(c, &v["output"]);
                }
            })
        }
        PipelineCmd::Runs { pipeline, limit } => {
            let q = query([("limit", Some(limit.to_string()))]);
            let v = ctx
                .api
                .get(&format!("{}/runs", pipeline_path(ctx, pipeline).await?), q)
                .await?;
            let rows: Vec<Value> = as_items(&v)
                .iter()
                .map(|r| {
                    let mut x = r.clone();
                    x["where"] = r["environment"].clone();
                    if x["where"].is_null() {
                        x["where"] = r["branch"].clone();
                    }
                    x["title"] = r["name"].clone();
                    if x["title"].is_null() {
                        x["title"] = r["id"].clone();
                    }
                    x
                })
                .collect();
            ctx.emit(&v, |c, _| {
                print_table(
                    c,
                    &rows,
                    &[
                        ("RUN", "title"),
                        ("STATUS", "status"),
                        ("WHERE", "where"),
                        ("STARTED", "started_at"),
                        ("SUMMARY", "summary"),
                        ("ID", "id"),
                    ],
                    "no runs",
                );
            })
        }
        PipelineCmd::Run { pipeline, run } => {
            let v = ctx
                .api
                .get(
                    &format!(
                        "{}/runs/{}",
                        pipeline_path(ctx, pipeline).await?,
                        encode(run)
                    ),
                    vec![],
                )
                .await?;
            ctx.emit(&v, print_run)
        }
        PipelineCmd::Test { args } => {
            let p = ctx.project_or(None).await?;
            let b = pipeline_body(ctx, args)?;
            need_source(&b)?;
            let v = ctx
                .api
                .post(
                    &format!("{}/pipelines/test", project_path(&p)),
                    Some(Value::Object(b)),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                match v["error"].as_str() {
                    Some(e) => c.line(format!("the poll failed: {e}")),
                    None => {
                        let runs = as_items(&v["runs"]);
                        c.line(format!(
                            "{} runs in {} ms",
                            runs.len(),
                            field(v, "duration_ms")
                        ));
                        for r in runs.iter().take(20) {
                            c.line(format!("  {}", run_line(r)));
                        }
                    }
                }
                print_output(c, &v["output"]);
            })
        }
    }
}
