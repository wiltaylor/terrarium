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
        Some("on") => {
            let mut t = format!("on {}", strings(&spec["kinds"]).join(","));
            if let Some(n) = spec["match"].as_array().map(Vec::len).filter(|n| *n > 0) {
                t.push_str(&format!(" ({n} rules)"));
            }
            t
        }
        _ => spec.to_string(),
    }
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
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
        Some("card") => format!("card {} {}", field(a, "op"), field(a, "key")),
        Some("webhook") => {
            let to = match a["url_secret"].as_str() {
                Some(s) => format!("(secret {s})"),
                None => field(a, "url"),
            };
            format!(
                "webhook {} {to} ({})",
                a["method"].as_str().unwrap_or("POST"),
                a["format"].as_str().unwrap_or("json")
            )
        }
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

const DELIVERY_COLS: &[(&str, &str)] = &[
    ("DELIVERY", "id"),
    ("CREATED", "created_at"),
    ("EVENT", "event_text"),
    ("STATUS", "status"),
    ("HTTP", "http_status"),
    ("TRY", "attempt"),
    ("MS", "duration_ms"),
    ("NOTE", "note"),
];

/// A delivery with `event_text` and `note` (the error, the next retry,
/// or the response).
fn delivery_row(d: &Value) -> Value {
    let mut d = d.clone();
    let mut event = match d["event_seq"].as_i64() {
        Some(seq) => format!("{seq} {}", field(&d, "event_kind")),
        None => field(&d, "event_kind"),
    };
    if d["test"] == true {
        event = format!("test {event}");
    }
    let mut note = d["error"].as_str().unwrap_or_default().to_owned();
    if d["status"] == "pending"
        && let Some(n) = d["next_attempt_at"].as_str()
    {
        note = format!("{note} (next {})", short_time(n)).trim().to_owned();
    }
    if note.is_empty() {
        note = d["response"]
            .as_str()
            .unwrap_or_default()
            .replace('\n', " ")
            .chars()
            .take(60)
            .collect();
    }
    d["event_text"] = json!(event.trim());
    d["note"] = json!(note);
    d["created_at"] = json!(short_time(&field(&d, "created_at")));
    d
}

fn print_delivery(ctx: &Ctx, d: &Value) {
    ctx.line(format!(
        "delivery {}  {} {}: {}{}",
        field(d, "id"),
        field(d, "method"),
        field(d, "url"),
        field(d, "status"),
        d["http_status"]
            .as_i64()
            .map(|h| format!(" ({h})"))
            .unwrap_or_default()
    ));
    if let Some(e) = d["error"].as_str() {
        ctx.line(format!("error:     {e}"));
    }
    if let Some(n) = d["next_attempt_at"].as_str() {
        ctx.line(format!("next try:  {}", short_time(n)));
    }
    if let Some(r) = d["response"].as_str().filter(|r| !r.is_empty()) {
        ctx.line(format!("response:  {r}"));
    }
}

const RUN_COLS: &[(&str, &str)] = &[
    ("RUN", "id"),
    ("STARTED", "started_at"),
    ("TRIGGER", "trigger"),
    ("EVENT", "event_text"),
    ("STATUS", "status"),
    ("SUMMARY", "summary"),
];

/// A run with `event_text`: the `seq` and kind of a trigger run's event.
fn run_row(r: &Value) -> Value {
    let mut r = r.clone();
    let text = match (r["event_seq"].as_i64(), r["event"]["kind"].as_str()) {
        (Some(seq), Some(kind)) => format!("{seq} {kind}"),
        (Some(seq), None) => seq.to_string(),
        _ => String::new(),
    };
    r["event_text"] = json!(text);
    r
}

fn print_schedule(ctx: &Ctx, s: &Value) {
    ctx.line(format!("schedule {}  {}", key_of(s), field(s, "id")));
    ctx.line(format!("when:      {}", spec_text(&s["spec"])));
    ctx.line(format!("action:    {}", action_text(&s["action"])));
    let spec = &s["spec"];
    if spec["kind"] == "on" {
        for m in spec["match"].as_array().into_iter().flatten() {
            ctx.line(format!(
                "match:     {} {} {}",
                field(m, "path"),
                m["op"].as_str().unwrap_or("eq"),
                m["value"]
            ));
        }
        if spec["dedupe"].is_object() {
            ctx.line(format!(
                "dedupe:    {} in {}",
                field(&spec["dedupe"], "key"),
                field(&spec["dedupe"], "window")
            ));
        }
        let e = &spec["escalate"];
        if e.is_object() {
            ctx.line(format!(
                "escalate:  {} after {} to {} (reset on {})",
                field(e, "key"),
                field(e, "after"),
                field(e, "to"),
                strings(&e["reset_on"]).join(",")
            ));
        }
        ctx.line(format!("max/hour:  {}", field(spec, "max_per_hour")));
    }
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
    if let Some(seq) = r["event_seq"].as_i64() {
        ctx.line(format!(
            "event {seq}: {} {}",
            field(&r["event"], "kind"),
            field(&r["event"], "summary")
        ));
    }
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

fn json_arg(flag: &str, text: &str) -> anyhow::Result<Value> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| CliError::usage(format!("bad {flag}: {e}")))?;
    if !v.is_object() {
        return Err(CliError::usage(format!("{flag} takes a JSON object")));
    }
    Ok(v)
}

/// One `--match` rule: `PATH=VALUE`, `PATH!=VALUE`, `PATH=glob:PATTERN`,
/// or `PATH=in:A,B`.
fn match_rule(rule: &str) -> anyhow::Result<Value> {
    let bad = || {
        CliError::usage(format!(
            "bad --match `{rule}`: use PATH=VALUE, PATH!=VALUE, PATH=glob:PATTERN, or PATH=in:A,B"
        ))
    };
    if let Some((path, value)) = rule.split_once("!=") {
        return Ok(json!({"path": path.trim(), "op": "ne", "value": value}));
    }
    let (path, value) = rule.split_once('=').ok_or_else(bad)?;
    let path = path.trim();
    if path.is_empty() {
        return Err(bad());
    }
    Ok(if let Some(p) = value.strip_prefix("glob:") {
        json!({"path": path, "op": "glob", "value": p})
    } else if let Some(list) = value.strip_prefix("in:") {
        json!({"path": path, "op": "in", "value": list.split(',').collect::<Vec<_>>()})
    } else {
        json!({"path": path, "op": "eq", "value": value})
    })
}

/// The `on` spec of the trigger flags.
fn trigger_spec(a: &ScheduleArgs) -> anyhow::Result<Value> {
    let mut spec = json!({"kind": "on", "kinds": a.on});
    if !a.matches.is_empty() {
        spec["match"] = a
            .matches
            .iter()
            .map(|m| match_rule(m))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into();
    }
    if let Some(d) = &a.dedupe {
        let (key, window) = d.rsplit_once('/').ok_or_else(|| {
            CliError::usage(format!(
                "bad --dedupe `{d}`: use KEY/WINDOW, as '{{{{event.data.repo}}}}/10m'"
            ))
        })?;
        spec["dedupe"] = json!({"key": key, "window": window});
    }
    if let Some(key) = &a.escalate {
        let mut e = json!({"key": key, "after": a.escalate_after.unwrap_or_default()});
        if !a.reset_on.is_empty() {
            e["reset_on"] = json!(a.reset_on);
        }
        if let Some(to) = &a.escalate_to {
            e["to"] = json!(to);
        }
        spec["escalate"] = e;
    }
    if let Some(n) = a.max_per_hour {
        spec["max_per_hour"] = json!(n);
    }
    Ok(spec)
}

/// The spec of the flags, if any.
fn spec_of(a: &ScheduleArgs) -> anyhow::Result<Option<Value>> {
    if let Some(j) = &a.spec_json {
        return json_arg("--spec-json", j).map(Some);
    }
    if !a.on.is_empty() {
        return trigger_spec(a).map(Some);
    }
    if !a.matches.is_empty()
        || a.dedupe.is_some()
        || a.escalate.is_some()
        || a.max_per_hour.is_some()
    {
        return Err(CliError::usage(
            "--match, --dedupe, --escalate, and --max-per-hour go with --on <kind>",
        ));
    }
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
    if let Some(j) = &a.action_json {
        return json_arg("--action-json", j).map(Some);
    }
    if let Some(op) = &a.card {
        if !matches!(op.as_str(), "open" | "close") {
            return Err(CliError::usage(format!(
                "bad --card `{op}`: use open or close"
            )));
        }
        let mut v = json!({"type": "card", "op": op, "key": a.key.clone().unwrap_or_default()});
        if let Some(t) = &a.title {
            v["title"] = json!(t);
        }
        if let Some(b) = &a.body {
            v["body"] = json!(ctx.text(b)?);
        }
        if let Some(c) = &a.column {
            v["column"] = json!(c);
        }
        return Ok(Some(v));
    }
    if a.body.is_some() && a.mail.is_none() {
        return Err(CliError::usage("--body goes with --mail or --card"));
    }
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
        if let Some(t) = &a.timeout {
            v["timeout_secs"] = json!(seconds("--timeout", t)?);
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
    if a.webhook.is_some() || a.webhook_url_secret.is_some() {
        return webhook_action(ctx, a).map(Some);
    }
    if a.format.is_some()
        || a.template_file.is_some()
        || !a.headers.is_empty()
        || !a.header_secrets.is_empty()
        || a.sign_secret.is_some()
        || a.method.is_some()
        || a.content_type.is_some()
    {
        return Err(CliError::usage(
            "--format, --template-file, --header, --header-secret, --sign-secret, --method, and \
             --content-type go with --webhook <url> or --webhook-url-secret <secret>",
        ));
    }
    if !a.cmd.is_empty() {
        return Err(CliError::usage(
            "a command after `--` needs --exec <target>",
        ));
    }
    Ok(None)
}

/// A time limit: seconds (`600`) or a duration (`10s`, `5m`).
fn seconds(flag: &str, s: &str) -> anyhow::Result<u64> {
    let bad = || CliError::usage(format!("bad {flag} `{s}`: use seconds or 10s, 5m"));
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.parse().map_err(|_| bad())?;
    match unit {
        "s" => Ok(n),
        "m" => Ok(n * 60),
        "h" => Ok(n * 3600),
        _ => Err(bad()),
    }
}

/// The `webhook` action of the flags.
fn webhook_action(ctx: &Ctx, a: &ScheduleArgs) -> anyhow::Result<Value> {
    let mut v = json!({"type": "webhook"});
    match (&a.webhook, &a.webhook_url_secret) {
        (Some(u), _) => v["url"] = json!(u),
        (_, Some(s)) => v["url_secret"] = json!(s),
        _ => {}
    }
    if let Some(m) = &a.method {
        let m = m.to_ascii_uppercase();
        if !matches!(m.as_str(), "POST" | "PUT") {
            return Err(CliError::usage(format!(
                "bad --method `{m}`: use POST or PUT"
            )));
        }
        v["method"] = json!(m);
    }
    let format = a.format.as_deref().unwrap_or(if a.template_file.is_some() {
        "template"
    } else {
        "json"
    });
    if !matches!(format, "json" | "slack" | "discord" | "ntfy" | "template") {
        return Err(CliError::usage(format!(
            "bad --format `{format}`: use json, slack, discord, ntfy, or template"
        )));
    }
    v["format"] = json!(format);
    match (&a.template_file, format) {
        (Some(f), "template") => {
            let text = if f == "-" {
                ctx.text("-")?
            } else {
                std::fs::read_to_string(f)
                    .map_err(|e| CliError::usage(format!("cannot read {f}: {e}")))?
            };
            v["template"] = json!(text);
        }
        (None, "template") => {
            return Err(CliError::usage(
                "--format template needs --template-file <file|->",
            ));
        }
        (Some(_), _) => {
            return Err(CliError::usage(
                "--template-file goes with --format template",
            ));
        }
        (None, _) => {}
    }
    if let Some(c) = &a.content_type {
        v["content_type"] = json!(c);
    }
    let mut headers = Vec::new();
    for h in &a.headers {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| CliError::usage(format!("bad --header `{h}`: use 'Name: value'")))?;
        headers.push(json!({"name": name.trim(), "value": value.trim()}));
    }
    for h in &a.header_secrets {
        let (name, secret) = h.split_once('=').ok_or_else(|| {
            CliError::usage(format!("bad --header-secret `{h}`: use 'Name=secretname'"))
        })?;
        headers.push(json!({"name": name.trim(), "secret": secret.trim()}));
    }
    if !headers.is_empty() {
        v["headers"] = json!(headers);
    }
    if let Some(s) = &a.sign_secret {
        let mut sig = json!({"secret": s});
        if let Some(h) = &a.sign_header {
            sig["header"] = json!(h);
        }
        v["signing"] = sig;
    }
    if let Some(t) = &a.timeout {
        v["timeout_secs"] = json!(seconds("--timeout", t)?);
    }
    Ok(v)
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
            triggers,
            time,
        } => {
            let project = if *all { None } else { ctx.project.clone() };
            let state = match (enabled, disabled_only) {
                (true, _) => Some("true".to_owned()),
                (_, true) => Some("false".to_owned()),
                _ => None,
            };
            let kind = match (triggers, time) {
                (true, _) => Some("trigger".to_owned()),
                (_, true) => Some("time".to_owned()),
                _ => None,
            };
            let q = query([("project", project), ("enabled", state), ("kind", kind)]);
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
                CliError::usage(
                    "give the time: --cron \"<expr>\", --every <dur>, or --at <time>; \
                     or a trigger: --on <kind>",
                )
            })?;
            let action = action_of(ctx, args)?.ok_or_else(|| {
                CliError::usage(
                    "give the action: --mail, --turn, --exec, --agent with --verb, --probe, \
                     --card, or --webhook",
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
            ctx.emit(&v, |c, v| {
                let rows: Vec<Value> = as_items(v).iter().map(run_row).collect();
                print_table(c, &rows, RUN_COLS, "no runs");
            })
        }
        ScheduleCmd::Deliveries { schedule, limit } => {
            let items = ctx
                .api
                .list(
                    &format!("{}/deliveries", path(schedule)),
                    Vec::new(),
                    Some(*limit),
                )
                .await?;
            let v = Value::Array(items);
            ctx.emit(&v, |c, v| {
                let rows: Vec<Value> = as_items(v).iter().map(delivery_row).collect();
                print_table(c, &rows, DELIVERY_COLS, "no deliveries");
            })
        }
        ScheduleCmd::Redeliver { schedule, delivery } => {
            let d = ctx
                .api
                .post(
                    &format!(
                        "{}/deliveries/{}/redeliver",
                        path(schedule),
                        encode(delivery)
                    ),
                    None,
                )
                .await?;
            ctx.emit(&d, |c, d| {
                c.line(format!(
                    "queued delivery {} (a copy of {})",
                    field(d, "id"),
                    field(d, "redelivery_of")
                ));
            })
        }
        ScheduleCmd::Test { schedule } => {
            let r = ctx
                .api
                .post(&format!("{}/test", path(schedule)), None)
                .await?;
            ctx.emit(&r, |c, r| {
                let q = &r["request"];
                c.line(format!("{} {}", field(q, "method"), field(q, "url")));
                for h in q["headers"].as_array().into_iter().flatten() {
                    c.line(format!(
                        "{}: {}",
                        h[0].as_str().unwrap_or_default(),
                        h[1].as_str().unwrap_or_default()
                    ));
                }
                c.line("");
                c.line(field(q, "body"));
                c.line("");
                print_delivery(c, &r["delivery"]);
            })?;
            if r["delivery"]["status"] != "ok" {
                return Err(CliError::RemoteExit(1).into());
            }
            Ok(())
        }
    }
}
