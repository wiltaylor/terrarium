//! `calendar` (docs/api.md §33): the events of the project calendar and
//! of the global calendar, the skips, the assignees, the watchers, and
//! the write grants of a lead.

use serde_json::{Map, Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::{CalendarArgs, CalendarCmd};
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::{field, short_time};
use crate::transport::encode;

fn event_path(id: &str) -> String {
    format!("/api/calendar/events/{}", encode(id))
}

/// `2026-11-10 09:00 – 10:00`, or the days of an all-day occurrence.
pub fn when_text(o: &Value) -> String {
    let start = o["start"].as_str().unwrap_or_default();
    let end = o["end"].as_str().unwrap_or_default();
    if o["all_day"] == true {
        // The API sends the instants; the event has the local days.
        if let (Some(a), Some(b)) = (o["start_date"].as_str(), o["end_date"].as_str()) {
            return if a == b {
                format!("{a} (all day)")
            } else {
                format!("{a} to {b} (all day)")
            };
        }
        return format!("{} (all day)", short_time(start));
    }
    let (a, b) = (short_time(start), short_time(end));
    match (a.split_once(' '), b.split_once(' ')) {
        (Some((da, _)), Some((db, tb))) if da == db => format!("{a} – {tb} UTC"),
        _ => format!("{a} – {b} UTC"),
    }
}

/// The recurrence of the flags: a rule object, or the RRULE text as it is.
fn rule_of(a: &CalendarArgs) -> anyhow::Result<Option<Value>> {
    let Some(r) = &a.repeat else {
        return Ok(None);
    };
    let freq = r.trim().to_ascii_lowercase();
    if !matches!(freq.as_str(), "daily" | "weekly" | "monthly") {
        if a.interval.is_some() || !a.on.is_empty() || a.month_day.is_some() {
            return Err(CliError::usage(
                "--interval, --on, and --month-day go with --repeat daily|weekly|monthly; \
                 put them in the RRULE text instead",
            ));
        }
        let mut text = r.clone();
        if let Some(u) = &a.until {
            text.push_str(&format!(";UNTIL={}", u.replace('-', "")));
        }
        if let Some(c) = a.count {
            text.push_str(&format!(";COUNT={c}"));
        }
        return Ok(Some(json!(text)));
    }
    let mut m = Map::new();
    m.insert("freq".into(), json!(freq));
    if let Some(i) = a.interval {
        m.insert("interval".into(), json!(i));
    }
    if !a.on.is_empty() {
        m.insert("weekdays".into(), json!(a.on));
    }
    if let Some(d) = a.month_day {
        m.insert("month_day".into(), json!(d));
    }
    if let Some(u) = &a.until {
        m.insert("until".into(), json!(u));
    }
    if let Some(c) = a.count {
        m.insert("count".into(), json!(c));
    }
    Ok(Some(Value::Object(m)))
}

/// The body of `calendar add` and `calendar edit` from the flags.
pub fn calendar_body(ctx: &Ctx, a: &CalendarArgs) -> anyhow::Result<Map<String, Value>> {
    let mut b = Map::new();
    if let Some(e) = &a.end {
        b.insert("end".into(), json!(e));
    }
    if a.all_day {
        b.insert("all_day".into(), json!(true));
    }
    for (k, v) in [("tz", &a.tz), ("kind", &a.kind)] {
        if let Some(v) = v {
            b.insert(k.into(), json!(v));
        }
    }
    if let Some(d) = &a.description {
        b.insert("description".into(), json!(ctx.text(d)?));
    }
    if let Some(r) = rule_of(a)? {
        b.insert("recurrence".into(), r);
    }
    if !a.assignees.is_empty() {
        b.insert("assignees".into(), json!(a.assignees));
    }
    let mut links = Map::new();
    for (k, v) in [
        ("card", &a.card),
        ("target", &a.target),
        ("repo", &a.repo),
        ("url", &a.link_url),
    ] {
        if let Some(v) = v {
            links.insert(k.into(), json!(v));
        }
    }
    if !links.is_empty() {
        b.insert("links".into(), Value::Object(links));
    }
    Ok(b)
}

fn print_event(c: &Ctx, e: &Value) {
    let scope = if e["global"] == true {
        "global".to_owned()
    } else {
        field(e, "project")
    };
    c.line(format!(
        "{} {}  [{}]  {scope}",
        field(e, "id"),
        field(e, "title"),
        field(e, "kind")
    ));
    c.line(format!("when: {}  zone: {}", when_text(e), field(e, "tz")));
    if let Some(r) = e["repeat"].as_str() {
        c.line(format!("repeats: {r}  ({})", field(e, "rrule")));
    }
    let skipped: Vec<String> = as_items(&e["exceptions"])
        .iter()
        .filter_map(|x| x.as_str().map(short_time))
        .collect();
    if !skipped.is_empty() {
        c.line(format!("skipped: {}", skipped.join(", ")));
    }
    let names: Vec<String> = as_items(&e["assignees"])
        .iter()
        .map(|a| format!("{}/{}", field(a, "project"), field(a, "name")))
        .collect();
    c.line(format!(
        "assignees: {}",
        if names.is_empty() {
            "none".into()
        } else {
            names.join(", ")
        }
    ));
    if let Some(l) = e["links"].as_object().filter(|l| !l.is_empty()) {
        let parts: Vec<String> = l
            .iter()
            .map(|(k, v)| format!("{k} {}", v.as_str().unwrap_or_default()))
            .collect();
        c.line(format!("links: {}", parts.join(", ")));
    }
    c.line(format!(
        "made by {} at {}; changed {}",
        field(e, "created_by_name"),
        short_time(e["created_at"].as_str().unwrap_or_default()),
        short_time(e["updated_at"].as_str().unwrap_or_default())
    ));
    if let Some(d) = e["description"].as_str().filter(|d| !d.trim().is_empty()) {
        c.line("description:");
        for l in d.lines() {
            c.line(format!("  {l}"));
        }
    }
}

/// The calendar path of the scope: the project of `--project` (or the
/// agent's own), or the global calendar.
async fn scope_path(ctx: &Ctx, global: bool) -> anyhow::Result<String> {
    if global {
        Ok("/api/calendar/global".into())
    } else {
        Ok(format!(
            "{}/calendar",
            project_path(&ctx.project_or(None).await?)
        ))
    }
}

pub async fn calendar(ctx: &Ctx, cmd: &CalendarCmd) -> anyhow::Result<()> {
    match cmd {
        CalendarCmd::Ls {
            from,
            to,
            days,
            global,
        } => {
            let q = query([
                ("from", from.clone()),
                ("to", to.clone()),
                ("days", days.map(|d| d.to_string())),
            ]);
            // The operator with no project: the combined view.
            let path = if *global {
                "/api/calendar/global".to_owned()
            } else if !ctx.is_agent() && ctx.project.is_none() {
                "/api/calendar".to_owned()
            } else {
                scope_path(ctx, false).await?
            };
            let v = ctx.api.get(&path, q).await?;
            let events = as_items(&v["events"]);
            let rows: Vec<Value> = as_items(&v["occurrences"])
                .iter()
                .map(|o| {
                    let mut r = o.clone();
                    if let Some(e) = events.iter().find(|e| e["id"] == o["event_id"]) {
                        r["start_date"] = e["start_date"].clone();
                        r["end_date"] = e["end_date"].clone();
                        if o["all_day"] == true && e["recurrence"].is_object() {
                            // A repeating all-day occurrence: its own day.
                            r["start_date"] = Value::Null;
                        }
                    }
                    r["when"] = json!(when_text(&r));
                    r["calendar"] = if o["global"] == true {
                        json!("global")
                    } else {
                        o["project"].clone()
                    };
                    r["who"] = json!(
                        as_items(&o["assignees"])
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    r
                })
                .collect();
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    &rows,
                    &[
                        ("WHEN", "when"),
                        ("KIND", "kind"),
                        ("TITLE", "title"),
                        ("CALENDAR", "calendar"),
                        ("ASSIGNEES", "who"),
                        ("ID", "event_id"),
                    ],
                    &format!(
                        "no events from {} to {}",
                        short_time(v["from"].as_str().unwrap_or_default()),
                        short_time(v["to"].as_str().unwrap_or_default())
                    ),
                );
                if v["truncated"] == true {
                    c.line("(the list is cut; give a shorter window)");
                }
            })
        }
        CalendarCmd::Show { id } => {
            let v = ctx.api.get(&event_path(id), vec![]).await?;
            ctx.emit(&v, print_event)
        }
        CalendarCmd::Add {
            title,
            start,
            global,
            args,
        } => {
            let mut b = calendar_body(ctx, args)?;
            b.insert("title".into(), json!(title));
            b.insert("start".into(), json!(start));
            let path = format!("{}/events", scope_path(ctx, *global).await?);
            let v = ctx.api.post(&path, Some(Value::Object(b))).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "added {} \"{}\" ({}) at {}",
                    field(v, "kind"),
                    field(v, "title"),
                    field(v, "id"),
                    when_text(v)
                ));
            })
        }
        CalendarCmd::Edit {
            id,
            title,
            start,
            no_repeat,
            clear_links,
            timed,
            args,
        } => {
            let mut b = calendar_body(ctx, args)?;
            if let Some(t) = title {
                b.insert("title".into(), json!(t));
            }
            if let Some(s) = start {
                b.insert("start".into(), json!(s));
            }
            if *no_repeat {
                b.insert("recurrence".into(), Value::Null);
            }
            if *clear_links {
                b.insert("links".into(), Value::Null);
            }
            if *timed {
                b.insert("all_day".into(), json!(false));
            }
            if b.is_empty() {
                return Err(CliError::usage("give a flag to change"));
            }
            let v = ctx.api.patch(&event_path(id), Value::Object(b)).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "updated {} \"{}\": {}",
                    field(v, "kind"),
                    field(v, "title"),
                    when_text(v)
                ));
            })
        }
        CalendarCmd::Rm { id } => {
            ctx.api.delete(&event_path(id), vec![]).await?;
            ctx.emit(&json!({"deleted": id}), |c, _| {
                c.line(format!("removed calendar event {id}"));
            })
        }
        CalendarCmd::Skip {
            id,
            occurrence,
            undo,
        } => {
            let verb = if *undo { "unskip" } else { "skip" };
            let v = ctx
                .api
                .post(
                    &format!("{}/{verb}", event_path(id)),
                    Some(json!({"occurrence": occurrence})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                let n = as_items(&v["exceptions"]).len();
                c.line(format!(
                    "{} the occurrence {occurrence} of \"{}\"; {n} skipped",
                    if *undo { "restored" } else { "skipped" },
                    field(v, "title")
                ));
            })
        }
        CalendarCmd::Assign { id, agents } | CalendarCmd::Unassign { id, agents } => {
            let on = matches!(cmd, CalendarCmd::Assign { .. });
            let mut last = Value::Null;
            for a in agents {
                let path = format!("{}/assignees/{}", event_path(id), encode(a));
                last = if on {
                    ctx.api.put(&path, json!({})).await?
                } else {
                    ctx.api.delete(&path, vec![]).await?
                };
            }
            ctx.emit(&last, |c, v| {
                let names: Vec<String> = as_items(&v["assignees"])
                    .iter()
                    .map(|a| format!("{}/{}", field(a, "project"), field(a, "name")))
                    .collect();
                c.line(format!(
                    "assignees of \"{}\": {}",
                    field(v, "title"),
                    if names.is_empty() {
                        "none".into()
                    } else {
                        names.join(", ")
                    }
                ));
            })
        }
        CalendarCmd::Watch { global, watcher } => {
            let path = format!("{}/watches", scope_path(ctx, *global).await?);
            let body = watcher.as_ref().map(|w| json!({"watcher": w}));
            let v = ctx.api.post(&path, body).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{} watches the calendar {}{}",
                    field(v, "watcher"),
                    field(v, "calendar"),
                    if v["changed"] == false {
                        " (already)"
                    } else {
                        ""
                    }
                ));
            })
        }
        CalendarCmd::Unwatch { global, watcher } => {
            let who = watcher.clone().unwrap_or_else(|| "me".into());
            let path = format!(
                "{}/watches/{}",
                scope_path(ctx, *global).await?,
                encode(&who)
            );
            let v = ctx.api.delete(&path, vec![]).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{} does not watch the calendar {}",
                    field(v, "watcher"),
                    field(v, "calendar")
                ));
            })
        }
        CalendarCmd::Watchers { global } => {
            let path = format!("{}/watches", scope_path(ctx, *global).await?);
            let v = ctx.api.get(&path, vec![]).await?;
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    as_items(v),
                    &[
                        ("WATCHER", "name"),
                        ("SINCE", "created_at"),
                        ("BY", "created_by"),
                    ],
                    "no watchers",
                );
            })
        }
        CalendarCmd::Grant { agent } | CalendarCmd::Revoke { agent } => {
            let on = matches!(cmd, CalendarCmd::Grant { .. });
            let p = ctx.project_or(None).await?;
            let path = format!("{}/calendar/grants/{}", project_path(&p), encode(agent));
            let v = if on {
                ctx.api.put(&path, json!({})).await?
            } else {
                ctx.api.delete(&path, vec![]).await?
            };
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{}/{} {} write the calendar{}",
                    field(v, "project"),
                    field(v, "name"),
                    if on { "may" } else { "may not" },
                    if v["changed"] == false {
                        " (no change)"
                    } else {
                        ""
                    }
                ));
            })
        }
        CalendarCmd::Grants => {
            let p = ctx.project_or(None).await?;
            let v = ctx
                .api
                .get(&format!("{}/calendar/grants", project_path(&p)), vec![])
                .await?;
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    as_items(v),
                    &[("AGENT", "name"), ("ID", "id")],
                    "no agent has the calendar grant; leads (project_admin) write it",
                );
            })
        }
    }
}
