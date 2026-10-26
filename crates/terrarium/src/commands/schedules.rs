//! `schedule` (and `admin schedule`): docs/api.md §19.

use serde_json::{Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::{ScheduleArgs, ScheduleCmd};
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::{field, short_time};
use crate::transport::encode;

/// `/api/schedules/{key}` for a ULID, `project/name`, or a name.
fn path(key: &str) -> String {
    format!("/api/schedules/{}", encode(key))
}

/// `project/name`, or the name of a global schedule.
fn key_of(s: &Value) -> String {
    match s["project"].as_str() {
        Some(p) => format!("{p}/{}", field(s, "name")),
        None => field(s, "name"),
    }
}

fn spec_text(spec: &Value) -> String {
    match spec["kind"].as_str() {
        Some("cron") => match spec["tz"].as_str() {
            Some(tz) if tz != "UTC" => format!("cron {} ({tz})", field(spec, "expr")),
            _ => format!("cron {}", field(spec, "expr")),
        },
        Some("interval") => format!("every {}", field(spec, "every")),
        Some("at") => format!("at {}", short_time(&field(spec, "at"))),
        _ => spec.to_string(),
    }
}

fn action_text(a: &Value) -> String {
    match a["type"].as_str() {
        Some("mail") => format!("mail {}", field(a, "to")),
        Some("turn") => format!("turn {}", field(a, "agent")),
        Some("exec") => format!(
            "exec {}: {}",
            field(a, "target"),
            a["cmd"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default()
        ),
        Some("agent") => format!("{} {}", field(a, "verb"), field(a, "agent")),
        Some("probe") => format!("probe {}", field(a, "subscription")),
        _ => a.to_string(),
    }
}

/// One flat row of `schedule ls`.
fn row(s: &Value) -> Value {
    json!({
        "id": s["id"],
        "key": key_of(s),
        "spec": spec_text(&s["spec"]),
        "action": action_text(&s["action"]),
        "enabled": if s["enabled"] == true { "yes" } else { "no" },
        "next_run_at": s["next_run_at"],
        "last": s["last_run"]["status"],
    })
}

const COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("SCHEDULE", "key"),
    ("WHEN", "spec"),
    ("ACTION", "action"),
    ("ON", "enabled"),
    ("NEXT", "next_run_at"),
    ("LAST", "last"),
];

const RUN_COLS: &[(&str, &str)] = &[
    ("RUN", "id"),
    ("STARTED", "started_at"),
    ("TRIGGER", "trigger"),
    ("STATUS", "status"),
    ("SUMMARY", "summary"),
];

fn print_schedule(ctx: &Ctx, s: &Value) {
    ctx.line(format!("schedule {}  {}", key_of(s), field(s, "id")));
    ctx.line(format!("when:      {}", spec_text(&s["spec"])));
    ctx.line(format!("action:    {}", action_text(&s["action"])));
    ctx.line(format!(
        "enabled:   {}{}",
        if s["enabled"] == true { "yes" } else { "no" },
        s["disabled_reason"]
            .as_str()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default()
    ));
    ctx.line(format!(
        "next run:  {}",
        short_time(&field(s, "next_run_at"))
    ));
    if s["last_run"].is_object() {
        let l = &s["last_run"];
        ctx.line(format!(
            "last run:  {} {}: {}",
            short_time(&field(l, "at")),
            field(l, "status"),
            field(l, "summary")
        ));
    }
    ctx.line(format!(
        "runs as:   {}  (made by {}, source {})",
        field(s, "run_as"),
        field(s, "created_by"),
        field(s, "source")
    ));
    ctx.line(format!(
        "catch-up:  {}   runs: {}",
        field(s, "catch_up"),
        field(s, "runs_count")
    ));
    ctx.line(format!("detail:    {}", s["action"]));
}

fn print_run(ctx: &Ctx, r: &Value) {
    ctx.line(format!(
        "run {}  {}  {}: {}",
        field(r, "id"),
        field(r, "trigger"),
        field(r, "status"),
        field(r, "summary")
    ));
    let o = &r["output"];
    for (k, label) in [("stdout", "stdout"), ("stderr", "stderr")] {
        if let Some(t) = o[k].as_str().filter(|t| !t.is_empty()) {
            ctx.line(format!("--- {label} (tail) ---"));
            ctx.out.write_str(t);
            if !t.ends_with('\n') {
                ctx.line("");
            }
        }
    }
}

/// `--at`: RFC 3339, or `+<duration>` from now.
fn at_time(s: &str) -> anyhow::Result<String> {
    let Some(rel) = s.strip_prefix('+') else {
        return Ok(s.to_owned());
    };
    let mut total = 0i64;
    let mut num = String::new();
    for c in rel.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => {
                return Err(CliError::usage(format!(
                    "bad --at `{s}`: use +30m, +2h, +1d"
                )));
            }
        };
        let n: i64 = num
            .parse()
            .map_err(|_| CliError::usage(format!("bad --at `{s}`")))?;
        total += n * unit;
        num.clear();
    }
    if !num.is_empty() || total == 0 {
        return Err(CliError::usage(format!(
            "bad --at `{s}`: use +30m, +2h, +1d"
        )));
    }
    Ok((chrono::Utc::now() + chrono::Duration::seconds(total))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// The spec of the flags, if any.
fn spec_of(a: &ScheduleArgs) -> anyhow::Result<Option<Value>> {
    Ok(match (&a.cron, &a.every, &a.at) {
        (Some(expr), _, _) => Some(json!({
            "kind": "cron", "expr": expr, "tz": a.tz.as_deref().unwrap_or("UTC"),
        })),
        (_, Some(every), _) => Some(json!({"kind": "interval", "every": every})),
        (_, _, Some(at)) => Some(json!({"kind": "at", "at": at_time(at)?})),
        _ => None,
    })
}

/// The action of the flags, if any.
fn action_of(ctx: &Ctx, a: &ScheduleArgs) -> anyhow::Result<Option<Value>> {
    if let Some(to) = &a.mail {
        let body = a
            .body
            .as_deref()
            .ok_or_else(|| CliError::usage("--mail needs --body <text|->"))?;
        let mut v = json!({"type": "mail", "to": to, "subject": a.subject.clone().unwrap_or_default(),
                           "body": ctx.text(body)?});
        if let Some(t) = &a.thread {
            v["thread"] = json!(t);
        }
        return Ok(Some(v));
    }
    if let Some(agent) = &a.turn {
        let text = a
            .text
            .as_deref()
            .ok_or_else(|| CliError::usage("--turn needs --text <text|->"))?;
        return Ok(Some(
            json!({"type": "turn", "agent": agent, "text": ctx.text(text)?,
                              "start_if_stopped": a.start_if_stopped}),
        ));
    }
    if let Some(target) = &a.exec {
        if a.cmd.is_empty() {
            return Err(CliError::usage(
                "--exec needs the command after `--`: --exec box -- make test",
            ));
        }
        let mut v = json!({"type": "exec", "target": target, "cmd": a.cmd});
        if let Some(c) = &a.cwd {
            v["cwd"] = json!(c);
        }
        if let Some(t) = a.timeout {
            v["timeout_secs"] = json!(t);
        }
        return Ok(Some(v));
    }
    if let Some(agent) = &a.agent {
        let verb = a.verb.as_deref().unwrap_or_default();
        if !matches!(verb, "start" | "stop" | "restart") {
            return Err(CliError::usage(
                "--agent needs --verb start, stop, or restart",
            ));
        }
        return Ok(Some(json!({"type": "agent", "agent": agent, "verb": verb})));
    }
    if let Some(sub) = &a.probe {
        return Ok(Some(json!({"type": "probe", "subscription": sub})));
    }
    if !a.cmd.is_empty() {
        return Err(CliError::usage(
            "a command after `--` needs --exec <target>",
        ));
    }
    Ok(None)
}

fn catch_up_of(a: &ScheduleArgs) -> anyhow::Result<Option<Value>> {
    match a.catch_up.as_deref() {
        None => Ok(None),
        Some(c @ ("skip" | "once")) => Ok(Some(json!(c))),
        Some(other) => Err(CliError::usage(format!(
            "bad --catch-up `{other}`: use skip or once"
        ))),
    }
}

fn print_saved(ctx: &Ctx, s: &Value, what: &str) {
    ctx.line(format!(
        "{what} schedule {} ({}); next run {}",
        key_of(s),
        field(s, "id"),
        short_time(&field(s, "next_run_at"))
    ));
}

pub async fn schedule(ctx: &Ctx, cmd: &ScheduleCmd) -> anyhow::Result<()> {
    match cmd {
        ScheduleCmd::Ls {
            all,
            enabled,
            disabled_only,
        } => {
            let project = if *all { None } else { ctx.project.clone() };
            let state = match (enabled, disabled_only) {
                (true, _) => Some("true".to_owned()),
                (_, true) => Some("false".to_owned()),
                _ => None,
            };
            let q = query([("project", project), ("enabled", state)]);
            let v = ctx.api.get("/api/schedules", q).await?;
            ctx.emit(&v, |c, v| {
                let rows: Vec<Value> = as_items(v).iter().map(row).collect();
                print_table(c, &rows, COLS, "no schedules");
            })
        }
        ScheduleCmd::Show { schedule } => {
            let v = ctx.api.get(&path(schedule), Vec::new()).await?;
            ctx.emit(&v, print_schedule)
        }
        ScheduleCmd::New { name, args } => {
            let spec = spec_of(args)?.ok_or_else(|| {
                CliError::usage("give the time: --cron \"<expr>\", --every <dur>, or --at <time>")
            })?;
            let action = action_of(ctx, args)?.ok_or_else(|| {
                CliError::usage(
                    "give the action: --mail, --turn, --exec, --agent with --verb, or --probe",
                )
            })?;
            let project = if args.global {
                if ctx.project.is_some() {
                    return Err(CliError::usage("use --project or --global, not both"));
                }
                Value::Null
            } else {
                match ctx.project_or(None).await {
                    Ok(p) => json!(p),
                    Err(_) => {
                        return Err(CliError::usage(
                            "name the project with --project <name>, or make a global schedule with --global",
                        ));
                    }
                }
            };
            let mut b = json!({"name": name, "project": project, "spec": spec, "action": action});
            if let Some(c) = catch_up_of(args)? {
                b["catch_up"] = c;
            }
            if args.disabled {
                b["enabled"] = json!(false);
            }
            let s = ctx.api.post("/api/schedules", Some(b)).await?;
            ctx.emit(&s, |c, s| print_saved(c, s, "made"))
        }
        ScheduleCmd::Set {
            schedule,
            rename,
            enabled,
            args,
        } => {
            if args.global {
                return Err(CliError::usage(
                    "the scope of a schedule does not change; make a new one",
                ));
            }
            let mut b = json!({});
            if let Some(n) = rename {
                b["name"] = json!(n);
            }
            if let Some(spec) = spec_of(args)? {
                b["spec"] = spec;
            }
            if let Some(action) = action_of(ctx, args)? {
                b["action"] = action;
            }
            if let Some(c) = catch_up_of(args)? {
                b["catch_up"] = c;
            }
            if *enabled {
                b["enabled"] = json!(true);
            }
            if args.disabled {
                b["enabled"] = json!(false);
            }
            if b.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(CliError::usage("nothing to change: give a flag"));
            }
            let s = ctx.api.patch(&path(schedule), b).await?;
            ctx.emit(&s, |c, s| print_saved(c, s, "changed"))
        }
        ScheduleCmd::Rm { schedule } => {
            ctx.api.delete(&path(schedule), Vec::new()).await?;
            let v = json!({"deleted": schedule});
            ctx.emit(&v, |c, _| c.line(format!("deleted schedule {schedule}")))
        }
        ScheduleCmd::Run { schedule } => {
            let r = ctx
                .api
                .post(&format!("{}/run", path(schedule)), None)
                .await?;
            ctx.emit(&r, print_run)?;
            if r["status"] == "failed" {
                return Err(CliError::RemoteExit(1).into());
            }
            Ok(())
        }
        ScheduleCmd::Enable { schedule } | ScheduleCmd::Disable { schedule } => {
            let verb = if matches!(cmd, ScheduleCmd::Enable { .. }) {
                "enable"
            } else {
                "disable"
            };
            let s = ctx
                .api
                .post(&format!("{}/{verb}", path(schedule)), None)
                .await?;
            ctx.emit(&s, |c, s| {
                c.line(format!(
                    "schedule {} {}; next run {}",
                    key_of(s),
                    if s["enabled"] == true {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    short_time(&field(s, "next_run_at"))
                ));
            })
        }
        ScheduleCmd::Runs { schedule, limit } => {
            let items = ctx
                .api
                .list(
                    &format!("{}/runs", path(schedule)),
                    Vec::new(),
                    Some(*limit),
                )
                .await?;
            let v = Value::Array(items);
            ctx.emit(&v, |c, v| print_table(c, as_items(v), RUN_COLS, "no runs"))
        }
    }
}
