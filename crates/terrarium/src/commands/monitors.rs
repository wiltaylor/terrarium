//! `monitor` and `dashboard` (docs/api.md §30): scripts that run on a
//! target on a schedule, and the project dashboard that shows them.

use anyhow::Context;
use serde_json::{Map, Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::{DashboardCmd, MonitorArgs, MonitorCmd};
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

/// The path of a monitor: an ID or `project/name` as it is, a bare name
/// in the project of `--project` (or the agent's own).
async fn monitor_path(ctx: &Ctx, key: &str) -> anyhow::Result<String> {
    let is_ulid = key.len() == 26 && key.chars().all(|c| c.is_ascii_alphanumeric());
    let key = if key.contains('/') || is_ulid {
        key.to_owned()
    } else {
        format!("{}/{key}", ctx.project_or(None).await?)
    };
    Ok(format!("/api/monitors/{}", encode(&key)))
}

/// `42.5 %`, `0.42`, or `-`.
pub fn value_text(v: &Value, unit: Option<&str>) -> String {
    match v.as_f64() {
        None => "-".into(),
        Some(n) => {
            let n = if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", n as i64)
            } else {
                format!("{}", (n * 100.0).round() / 100.0)
            };
            match unit.filter(|u| !u.is_empty()) {
                Some(u) => format!("{n} {u}"),
                None => n,
            }
        }
    }
}

/// The schedule as text: `every 1m`, `cron */5 * * * * (UTC)`.
pub fn schedule_text(s: &Value) -> String {
    match s["kind"].as_str() {
        Some("interval") => format!("every {}", field(s, "every")),
        Some("cron") => format!("cron {} ({})", field(s, "expr"), field(s, "tz")),
        _ => "-".into(),
    }
}

/// The status, the value, and the message of a result.
fn result_line(r: &Value, unit: Option<&str>) -> String {
    let mut parts = vec![field(r, "status").to_uppercase()];
    if !r["value"].is_null() {
        let u = r["json"]["unit"].as_str().or(unit);
        parts.push(value_text(&r["value"], u));
    }
    if let Some(t) = r["text"].as_str().and_then(|t| t.lines().next()) {
        parts.push(t.to_owned());
    }
    if let Some(e) = r["error"].as_str() {
        parts.push(format!("({e})"));
    }
    parts.join("  ")
}

/// The body of `monitor new` and `monitor edit` from the flags.
pub fn monitor_body(ctx: &Ctx, a: &MonitorArgs) -> anyhow::Result<Map<String, Value>> {
    let mut b = Map::new();
    if let Some(t) = &a.target {
        b.insert("target".into(), json!(t));
    }
    if let Some(s) = &a.script {
        b.insert("script".into(), json!(ctx.text(s)?));
    }
    if let Some(p) = &a.script_file {
        let s =
            std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
        b.insert("script".into(), json!(s));
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
    for (k, v) in [
        ("kind", a.kind.as_ref()),
        ("unit", a.unit.as_ref()),
        ("direction", a.direction.as_ref()),
        ("description", a.description.as_ref()),
    ] {
        if let Some(v) = v {
            b.insert(k.into(), json!(v));
        }
    }
    if let Some(w) = a.warn {
        b.insert("warn".into(), json!(w));
    }
    if let Some(c) = a.crit {
        b.insert("crit".into(), json!(c));
    }
    if let Some(t) = a.timeout {
        b.insert("timeout_secs".into(), json!(t));
    }
    if let Some(r) = a.retention {
        b.insert("retention_days".into(), json!(r));
    }
    if a.disabled {
        b.insert("enabled".into(), json!(false));
    }
    Ok(b)
}

fn print_monitor(c: &Ctx, m: &Value) {
    let unit = m["unit"].as_str();
    c.line(format!(
        "{} {}  [{}]{}",
        field(m, "id"),
        field(m, "name"),
        field(m, "kind"),
        if m["enabled"] == false {
            "  disabled"
        } else {
            ""
        }
    ));
    if let Some(d) = m["description"].as_str() {
        c.line(d);
    }
    c.line(format!(
        "target: {}  schedule: {}  timeout: {} s  retention: {} d",
        field(m, "target"),
        schedule_text(&m["schedule"]),
        field(m, "timeout_secs"),
        field(m, "retention_days")
    ));
    if !m["warn"].is_null() || !m["crit"].is_null() {
        c.line(format!(
            "thresholds: warn {}  crit {}  ({})",
            value_text(&m["warn"], unit),
            value_text(&m["crit"], unit),
            field(m, "direction")
        ));
    }
    c.line(format!(
        "next run: {}  skipped: {}  runs as: {}",
        field(m, "next_run_at"),
        field(m, "skipped"),
        field(m, "run_as")
    ));
    match m["last"].as_object() {
        Some(_) => c.line(format!(
            "last: {}  at {}",
            result_line(&m["last"], unit),
            field(&m["last"], "at")
        )),
        None => c.line("last: no result yet"),
    }
    c.line("script:");
    for l in m["script"].as_str().unwrap_or_default().lines() {
        c.line(format!("  {l}"));
    }
}

fn print_run(c: &Ctx, v: &Value, unit: Option<&str>) {
    let r = &v["result"];
    c.line(format!(
        "{}  in {} ms",
        result_line(r, unit),
        field(r, "duration_ms")
    ));
    if let Some(rows) = r["json"]["rows"].as_array() {
        c.line(format!("{} rows", rows.len()));
    }
    let o = &v["output"];
    if o.is_object() {
        c.line(format!("exit code: {}", field(o, "exit_code")));
        for (name, key) in [("stdout", "stdout"), ("stderr", "stderr")] {
            let s = o[key].as_str().unwrap_or_default();
            if !s.trim().is_empty() {
                c.line(format!("{name}:"));
                for l in s.lines().take(40) {
                    c.line(format!("  {l}"));
                }
            }
        }
    }
}

pub async fn monitor(ctx: &Ctx, cmd: &MonitorCmd) -> anyhow::Result<()> {
    match cmd {
        MonitorCmd::Ls => {
            let p = ctx.project_or(None).await?;
            let v = ctx
                .api
                .get(&format!("{}/monitors", project_path(&p)), vec![])
                .await?;
            let rows: Vec<Value> = as_items(&v)
                .iter()
                .map(|m| {
                    let mut r = m.clone();
                    r["status"] = m["last"]["status"].clone();
                    r["value"] = json!(value_text(
                        &m["last"]["value"],
                        m["last"]["json"]["unit"].as_str().or(m["unit"].as_str())
                    ));
                    r["every"] = json!(schedule_text(&m["schedule"]));
                    r["last_at"] = m["last"]["at"].clone();
                    if m["enabled"] == false {
                        r["status"] = json!("disabled");
                    }
                    r
                })
                .collect();
            ctx.emit(&v, |c, _| {
                print_table(
                    c,
                    &rows,
                    &[
                        ("NAME", "name"),
                        ("KIND", "kind"),
                        ("STATUS", "status"),
                        ("VALUE", "value"),
                        ("TARGET", "target"),
                        ("SCHEDULE", "every"),
                        ("LAST", "last_at"),
                        ("ID", "id"),
                    ],
                    "no monitors",
                );
            })
        }
        MonitorCmd::Show { monitor } => {
            let v = ctx
                .api
                .get(&monitor_path(ctx, monitor).await?, vec![])
                .await?;
            ctx.emit(&v, print_monitor)
        }
        MonitorCmd::New { name, args } => {
            let p = ctx.project_or(None).await?;
            let mut b = monitor_body(ctx, args)?;
            for (flag, key) in [
                ("--target", "target"),
                ("--script or --script-file", "script"),
                ("--kind", "kind"),
            ] {
                if !b.contains_key(key) {
                    return Err(CliError::usage(format!("give {flag}")));
                }
            }
            if !b.contains_key("schedule") {
                return Err(CliError::usage("give --every or --cron"));
            }
            b.insert("name".into(), json!(name));
            let v = ctx
                .api
                .post(
                    &format!("{}/monitors", project_path(&p)),
                    Some(Value::Object(b)),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "created monitor {} ({}) on {}, {}",
                    field(v, "name"),
                    field(v, "id"),
                    field(v, "target"),
                    schedule_text(&v["schedule"])
                ));
            })
        }
        MonitorCmd::Edit {
            monitor,
            rename,
            clear_thresholds,
            enable,
            args,
        } => {
            let mut b = monitor_body(ctx, args)?;
            if let Some(n) = rename {
                b.insert("name".into(), json!(n));
            }
            if *clear_thresholds {
                b.insert("warn".into(), Value::Null);
                b.insert("crit".into(), Value::Null);
            }
            if *enable {
                b.insert("enabled".into(), json!(true));
            }
            if b.is_empty() {
                return Err(CliError::usage("give a flag to change"));
            }
            let v = ctx
                .api
                .patch(&monitor_path(ctx, monitor).await?, Value::Object(b))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("updated monitor {}", field(v, "name")));
            })
        }
        MonitorCmd::Rm { monitor } => {
            ctx.api
                .delete(&monitor_path(ctx, monitor).await?, vec![])
                .await?;
            ctx.emit(&json!({"deleted": monitor}), |c, _| {
                c.line(format!("deleted monitor {monitor}"));
            })
        }
        MonitorCmd::Run { monitor } => {
            let v = ctx
                .api
                .post(&format!("{}/run", monitor_path(ctx, monitor).await?), None)
                .await?;
            ctx.emit(&v, |c, v| print_run(c, v, None))
        }
        MonitorCmd::Results {
            monitor,
            since,
            limit,
            bucket,
        } => {
            let q = query([
                ("since", since.clone()),
                ("limit", Some(limit.to_string())),
                ("bucket", bucket.clone()),
            ]);
            let v = ctx
                .api
                .get(&format!("{}/results", monitor_path(ctx, monitor).await?), q)
                .await?;
            let rows: Vec<Value> = as_items(&v)
                .iter()
                .map(|r| {
                    let mut x = r.clone();
                    x["value"] = json!(value_text(&r["value"], r["json"]["unit"].as_str()));
                    if !r["min"].is_null() {
                        x["range"] = json!(format!(
                            "{}..{}",
                            value_text(&r["min"], None),
                            value_text(&r["max"], None)
                        ));
                    }
                    x["message"] = r["error"].clone();
                    if x["message"].is_null() {
                        x["message"] = json!(r["text"].as_str().and_then(|t| t.lines().next()));
                    }
                    x
                })
                .collect();
            let cols: &[(&str, &str)] = if bucket.is_some() {
                &[
                    ("AT", "at"),
                    ("STATUS", "status"),
                    ("MEAN", "value"),
                    ("RANGE", "range"),
                    ("COUNT", "count"),
                ]
            } else {
                &[
                    ("AT", "at"),
                    ("STATUS", "status"),
                    ("VALUE", "value"),
                    ("MS", "duration_ms"),
                    ("MESSAGE", "message"),
                ]
            };
            ctx.emit(&v, |c, _| print_table(c, &rows, cols, "no results"))
        }
    }
}

pub async fn dashboard(ctx: &Ctx, cmd: &DashboardCmd) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    let path = format!("{}/dashboard", project_path(&p));
    match cmd {
        DashboardCmd::Show => {
            let v = ctx.api.get(&path, vec![]).await?;
            ctx.emit(&v, |c, v| {
                let monitors = as_items(&v["monitors"]);
                if monitors.is_empty() {
                    c.line(format!("project {p} has no monitors"));
                    return;
                }
                let rows: Vec<Value> = monitors
                    .iter()
                    .map(|m| {
                        let last = &m["last"];
                        let unit = last["json"]["unit"].as_str().or(m["unit"].as_str());
                        let message = last["error"]
                            .as_str()
                            .or_else(|| last["text"].as_str().and_then(|t| t.lines().next()))
                            .unwrap_or_default();
                        let status = if m["enabled"] == false {
                            "disabled".to_owned()
                        } else if last.is_null() {
                            "pending".to_owned()
                        } else {
                            field(last, "status")
                        };
                        json!({"name": m["name"], "status": status,
                               "value": value_text(&last["value"], unit),
                               "message": message, "at": last["at"]})
                    })
                    .collect();
                print_table(
                    c,
                    &rows,
                    &[
                        ("MONITOR", "name"),
                        ("STATUS", "status"),
                        ("VALUE", "value"),
                        ("MESSAGE", "message"),
                        ("AT", "at"),
                    ],
                    "",
                );
                let bad: Vec<&Value> = rows
                    .iter()
                    .filter(|r| matches!(r["status"].as_str(), Some("warn" | "crit" | "unknown")))
                    .collect();
                c.line(format!(
                    "{} monitors, {} not ok; {} dashboard blocks",
                    rows.len(),
                    bad.len(),
                    as_items(&v["blocks"]).len()
                ));
            })
        }
        DashboardCmd::Set { from_file } => {
            let text = if from_file == "-" {
                ctx.text("-")?
            } else {
                std::fs::read_to_string(from_file)
                    .with_context(|| format!("cannot read {from_file}"))?
            };
            let body: Value = serde_json::from_str(&text)
                .map_err(|e| CliError::usage(format!("{from_file} is not JSON: {e}")))?;
            let v = ctx.api.put(&path, body).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "saved the dashboard of {p}: {} blocks",
                    as_items(&v["blocks"]).len()
                ));
            })
        }
    }
}
