//! `thread` (docs/api.md §6.13, TER-93): Inbox threads. A thread is a
//! conversation between the operator and one agent on one topic. An agent
//! with the right `thread:open` opens one with a template that does not
//! exceed its own rights (the daemon checks it); the attached agent posts
//! and asks questions in it; the operator responds and confirms the close.
//! Messages, questions, the response, and the handoff are the words of the
//! operator or an agent: `show` prints them between data markers.

use serde_json::{Value, json};

use super::questions::{context_value, items_value, set_priority};
use super::{agent_path, delimited, print_object, print_table};
use crate::api::{CliError, query};
use crate::cli::ThreadCmd;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

const COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("STATUS", "status"),
    ("SCOPE", "scope"),
    ("AGENT", "agent_name"),
    ("MSGS", "message_count"),
    ("OPEN_Q", "open_questions"),
    ("LAST", "last_at"),
    ("TOPIC", "topic"),
];

/// The handoff link kinds of the API.
const LINK_KINDS: &[&str] = &["card", "mail", "wiki", "memory", "url", "other"];

fn path(id: &str) -> String {
    format!("/api/threads/{}", encode(id))
}

/// A `--link <kind>:<ref>` as `{kind, ref}`.
fn link(arg: &str) -> anyhow::Result<Value> {
    match arg.split_once(':') {
        Some((k, r)) if LINK_KINDS.contains(&k.trim()) && !r.trim().is_empty() => {
            Ok(json!({"kind": k.trim(), "ref": r.trim()}))
        }
        _ => Err(CliError::usage(format!(
            "--link `{arg}` is not `<kind>:<ref>` with kind {}",
            LINK_KINDS.join(", ")
        ))),
    }
}

/// The first line of a text, at most 60 characters.
fn short(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut out: String = first.trim().chars().take(60).collect();
    if first.trim().chars().count() > 60 || text.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

/// One thread: the fields, then each message, question, the response,
/// and the handoff between data markers.
fn print_thread(c: &Ctx, v: &Value) {
    let mut head = v.clone();
    let mut take = |k: &str| head.as_object_mut().and_then(|o| o.remove(k));
    let messages = take("messages");
    let questions = take("questions");
    let response = take("response");
    let handoff = take("handoff");
    print_object(c, &head);
    let messages = messages.as_ref().map(as_items).unwrap_or_default();
    for (i, m) in messages.iter().enumerate() {
        let what = match m["kind"].as_str() {
            Some("response") => "RESPONSE",
            _ => "MESSAGE",
        };
        delimited(
            c,
            &format!(
                "{what} {} OF {} BY {} AT {}",
                i + 1,
                messages.len(),
                field(m, "author_name"),
                field(m, "at")
            ),
            m["text"].as_str().unwrap_or_default(),
        );
    }
    for q in questions.as_ref().map(as_items).unwrap_or_default() {
        c.line(format!(
            "{} {} ({}): {}",
            field(q, "kind"),
            field(q, "id"),
            field(q, "status"),
            short(q["text"].as_str().unwrap_or_default())
        ));
    }
    if let Some(r) = response.as_ref().filter(|r| r.is_object()) {
        c.line(format!(
            "response by {} at {} (the approval record):",
            field(r, "by"),
            field(r, "at")
        ));
        delimited(c, "RESPONSE", r["text"].as_str().unwrap_or_default());
    }
    if let Some(h) = handoff.as_ref().filter(|h| h.is_object()) {
        c.line(format!("handoff by {} at {}:", field(h, "by"), field(h, "at")));
        delimited(c, "HANDOFF", h["summary"].as_str().unwrap_or_default());
        for l in as_items(&h["links"]) {
            c.line(format!("link {}: {}", field(l, "kind"), field(l, "ref")));
        }
    }
}

pub async fn thread(ctx: &Ctx, cmd: &ThreadCmd) -> anyhow::Result<()> {
    match cmd {
        ThreadCmd::New {
            template,
            topic,
            scope,
            agent,
            message,
        } => {
            let mut body = json!({"topic": topic});
            super::set_opt(&mut body, "template", template.clone());
            super::set_opt(&mut body, "scope", scope.clone());
            super::set_opt(&mut body, "agent", agent.clone());
            if let Some(m) = message {
                body["text"] = json!(ctx.text(m)?);
            }
            let v = ctx.api.post("/api/threads", Some(body)).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "thread {} opened ({}, reports to {}, {}).",
                    field(v, "id"),
                    field(v, "scope"),
                    field(v, "report_to_name"),
                    field(v, "status")
                ));
            })
        }
        ThreadCmd::Post { id, message } => {
            let v = ctx
                .api
                .post(
                    &format!("{}/messages", path(id)),
                    Some(json!({"text": ctx.text(message)?})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "posted to thread {} ({}).",
                    field(v, "id"),
                    field(v, "status")
                ));
            })
        }
        ThreadCmd::Ask {
            id,
            text,
            options,
            bundle,
            items,
            items_file,
            context,
            urgent,
            critical,
        } => {
            if !ctx.is_agent() {
                return Err(CliError::usage(
                    "the thread's agent asks questions; the operator answers them with \
                     `terrarium admin question answer`",
                ));
            }
            if text == "-" && items_file.as_deref() == Some("-") {
                return Err(CliError::usage(
                    "only one of the summary and --items-file can read stdin",
                ));
            }
            let mut body = json!({"text": ctx.text(text)?, "thread": id});
            if *bundle {
                body["kind"] = json!("bundle");
                body["items"] = match items_file {
                    Some(f) => items_value(ctx, f)?,
                    None if !items.is_empty() => {
                        items.iter().map(|t| json!({"text": t})).collect()
                    }
                    None => {
                        return Err(CliError::usage(
                            "a bundle needs its questions: --item <text>... or --items-file",
                        ));
                    }
                };
            }
            super::set_list(&mut body, "options", options);
            super::set_opt(&mut body, "context", context_value(ctx, context)?);
            set_priority(&mut body, *urgent, *critical);
            let me = ctx.me().await?;
            let v = ctx
                .api
                .post(&format!("{}/questions", agent_path(&me)), Some(body))
                .await?;
            ctx.emit(&v, |c, v| {
                let what = if v["kind"] == "bundle" {
                    format!("bundle of {} questions", as_items(&v["items"]).len())
                } else {
                    "question".to_owned()
                };
                c.line(format!(
                    "{what} {} asked in thread {} ({}). The operator answers in the thread.",
                    field(v, "id"),
                    field(v, "thread"),
                    field(v, "priority")
                ));
            })
        }
        ThreadCmd::Respond { id, message } => {
            let v = ctx
                .api
                .post(
                    &format!("{}/respond", path(id)),
                    Some(json!({"text": ctx.text(message)?})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "response sent on thread {} ({}).",
                    field(v, "id"),
                    field(v, "status")
                ));
            })
        }
        ThreadCmd::Close { id, handoff, links } => {
            let mut body = json!({});
            if let Some(h) = handoff {
                body["summary"] = json!(ctx.text(h)?);
            }
            if !links.is_empty() {
                body["links"] = links
                    .iter()
                    .map(|l| link(l))
                    .collect::<anyhow::Result<Vec<_>>>()?
                    .into();
            }
            let v = ctx
                .api
                .post(&format!("{}/close", path(id)), Some(body))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "thread {} {}.",
                    field(v, "id"),
                    match v["status"].as_str() {
                        Some("closed") => "closed".to_owned(),
                        _ => format!("close proposed ({})", field(v, "status")),
                    }
                ));
            })
        }
        ThreadCmd::Ls { status, scope } => {
            let q = query([("status", status.clone()), ("scope", scope.clone())]);
            let v = ctx.api.get("/api/threads", q).await?;
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), COLS, "no threads");
            })
        }
        ThreadCmd::Show { id } => {
            let v = ctx.api.get(&path(id), vec![]).await?;
            ctx.emit(&v, print_thread)
        }
    }
}
