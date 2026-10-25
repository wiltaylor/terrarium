//! `mail` and `inbox`.

use std::time::Instant;

use serde_json::{Value, json};

use super::{delimited, print_table, set_opt};
use crate::api::query;
use crate::cli::{InboxCmd, MailCmd};
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::{Ctx, Waiter};
use crate::output::{field, short_time};
use crate::transport::encode;

const MAIL_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("FROM", "from_name"),
    ("TO", "to_name"),
    ("SUBJECT", "subject"),
    ("STATE", "state"),
    ("AT", "created_at"),
];

/// Print one message with its body between markers (PLAN §12).
pub fn print_message(ctx: &Ctx, m: &Value) {
    ctx.line(format!(
        "message {}  thread {}",
        field(m, "id"),
        field(m, "thread_id")
    ));
    ctx.line(format!("from:    {}", field(m, "from_name")));
    ctx.line(format!("to:      {}", field(m, "to_name")));
    ctx.line(format!("subject: {}", field(m, "subject")));
    ctx.line(format!(
        "date:    {}  state: {}",
        short_time(&field(m, "created_at")),
        field(m, "state")
    ));
    delimited(ctx, "MAIL BODY", m["body"].as_str().unwrap_or_default());
}

fn print_messages(ctx: &Ctx, v: &Value) {
    let items = as_items(v);
    if items.is_empty() {
        ctx.line("no messages");
    }
    for (i, m) in items.iter().enumerate() {
        if i > 0 {
            ctx.line("");
        }
        print_message(ctx, m);
    }
}

fn unread(m: &Value) -> bool {
    matches!(m["state"].as_str(), Some("queued" | "delivered"))
}

pub async fn mail(ctx: &Ctx, cmd: &MailCmd) -> anyhow::Result<()> {
    match cmd {
        MailCmd::Ls {
            to,
            from,
            thread,
            state,
            unread,
        } => {
            let state = if *unread {
                Some("unread".to_owned())
            } else {
                state.clone()
            };
            let q = query([
                ("to", to.clone()),
                ("from", from.clone()),
                ("thread", thread.clone()),
                ("state", state),
            ]);
            let v = Value::Array(ctx.api.list("/api/mail", q, None).await?);
            ctx.emit(&v, |c, v| print_table(c, as_items(v), MAIL_COLS, "no mail"))
        }
        MailCmd::Check => {
            let me = ctx.me().await?;
            let unread = ctx
                .api
                .list(
                    "/api/mail",
                    query([("to", Some(me.clone())), ("state", Some("unread".into()))]),
                    None,
                )
                .await?;
            let total = ctx
                .api
                .list("/api/mail", query([("to", Some(me))]), None)
                .await?
                .len();
            let v = json!({"unread": unread.len(), "total": total, "messages": unread});
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{} unread, {} in total",
                    field(v, "unread"),
                    field(v, "total")
                ));
                if !as_items(&v["messages"]).is_empty() {
                    print_table(c, as_items(&v["messages"]), MAIL_COLS, "");
                    c.line("Run `terrarium mail read` to read them.");
                }
            })
        }
        MailCmd::Read {
            id,
            thread,
            mark,
            no_mark,
        } => {
            let how = match (mark, no_mark) {
                (_, true) => Mark::No,
                (true, _) => Mark::Asked,
                _ => Mark::Mine,
            };
            read(ctx, id.as_deref(), thread.clone(), how).await
        }
        MailCmd::Send {
            to,
            body,
            body_flag,
            subject,
            thread,
            from,
        } => {
            let mut b = json!({"to": to, "body": body_arg(ctx, body, body_flag)?});
            set_opt(&mut b, "subject", subject.clone());
            set_opt(&mut b, "thread_id", thread.clone());
            set_opt(&mut b, "from", from.clone());
            let m = ctx.api.post("/api/mail", Some(b)).await?;
            ctx.emit(&m, |c, m| {
                c.line(format!(
                    "sent {} to {} (thread {})",
                    field(m, "id"),
                    field(m, "to_name"),
                    field(m, "thread_id")
                ));
            })
        }
        MailCmd::Reply {
            id,
            body,
            body_flag,
            subject,
        } => {
            let mut b = json!({"body": body_arg(ctx, body, body_flag)?});
            set_opt(&mut b, "subject", subject.clone());
            let r = ctx
                .api
                .post(&format!("/api/mail/{}/reply", encode(id)), Some(b))
                .await?;
            ctx.emit(&r, print_sent)
        }
        MailCmd::Wait { timeout } => {
            let me = ctx.me().await?;
            let w = Waiter::new(*timeout, ctx.poll_floor);
            while let Some(secs) = w.next() {
                let started = Instant::now();
                let q = query([
                    ("to", Some(me.clone())),
                    ("state", Some("unread".into())),
                    ("wait", Some(secs.to_string())),
                ]);
                let page = ctx.api.page("/api/mail", q).await?;
                if !page.items.is_empty() {
                    let v = Value::Array(page.items);
                    return ctx.emit(&v, |c, v| {
                        print_table(c, as_items(v), MAIL_COLS, "");
                        c.line("Run `terrarium mail read` to read them.");
                    });
                }
                w.pause(started).await;
            }
            ctx.emit(&json!([]), |c, _| {
                c.line(format!("no mail after {timeout}s"));
            })
        }
    }
}

/// The body from the positional or from `--body`; `-` reads stdin. Clap
/// makes sure that exactly one is there.
fn body_arg(ctx: &Ctx, pos: &Option<String>, flag: &Option<String>) -> anyhow::Result<String> {
    match pos.as_deref().or(flag.as_deref()) {
        Some(b) => ctx.text(b),
        None => Err(crate::api::CliError::usage(
            "give the body, or `--body <text|->`",
        )),
    }
}

fn print_sent(ctx: &Ctx, v: &Value) {
    for m in as_items(v) {
        ctx.line(format!(
            "sent {} to {} (thread {})",
            field(m, "id"),
            field(m, "to_name"),
            field(m, "thread_id")
        ));
    }
}

/// What `mail read` marks read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// The default: the unread messages to the reader. The agents and the
    /// dashboard take "read" as "seen".
    Mine,
    /// `--mark`: also one message named by ID, as the daemon allows.
    Asked,
    /// `--no-mark`: nothing.
    No,
}

async fn read(
    ctx: &Ctx,
    id: Option<&str>,
    thread: Option<String>,
    how: Mark,
) -> anyhow::Result<()> {
    let me = ctx.me().await?;
    if let Some(id) = id {
        let path = format!("/api/mail/{}", encode(id));
        let mut m = ctx.api.get(&path, vec![]).await?;
        let mine = m["to"].as_str() == Some(me.as_str());
        if unread(&m) && (how == Mark::Asked || (how == Mark::Mine && mine)) {
            m = ctx.api.post(&format!("{path}/read"), None).await?;
        }
        return ctx.emit(&m, print_message);
    }
    let q = match &thread {
        Some(t) => query([("thread", Some(t.clone()))]),
        None => query([("to", Some(me.clone())), ("state", Some("unread".into()))]),
    };
    let mut items = ctx.api.list("/api/mail", q, None).await?;
    if how != Mark::No {
        for m in &mut items {
            if unread(m) && m["to"].as_str() == Some(me.as_str()) {
                let path = format!("/api/mail/{}/read", encode(&field(m, "id")));
                *m = ctx.api.post(&path, None).await?;
            }
        }
    }
    let v = Value::Array(items);
    ctx.emit(&v, print_messages)
}

pub async fn inbox(ctx: &Ctx, cmd: &InboxCmd) -> anyhow::Result<()> {
    let project = ctx.project_or(None).await?;
    let base = format!("{}/inbox", project_path(&project));
    match cmd {
        InboxCmd::Ls {
            unread,
            state,
            thread,
        } => {
            let q = query([
                ("unread", unread.then(|| "true".to_owned())),
                ("state", state.clone()),
                ("thread", thread.clone()),
            ]);
            let v = Value::Array(ctx.api.list(&base, q, None).await?);
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), MAIL_COLS, "the inbox is empty");
            })
        }
        InboxCmd::Read { id, mark } => {
            let path = format!("{base}/{}", encode(id));
            let mut m = ctx.api.get(&path, vec![]).await?;
            if *mark && unread(&m) {
                m = ctx.api.post(&format!("{path}/read"), None).await?;
            }
            ctx.emit(&m, print_message)
        }
        InboxCmd::Reply {
            id,
            body,
            body_flag,
            subject,
        } => {
            let mut b = json!({"body": body_arg(ctx, body, body_flag)?});
            set_opt(&mut b, "subject", subject.clone());
            let r = ctx
                .api
                .post(&format!("{base}/{}/reply", encode(id)), Some(b))
                .await?;
            ctx.emit(&r, print_sent)
        }
        InboxCmd::Send {
            body,
            body_flag,
            subject,
            thread,
        } => {
            let mut b = json!({"body": body_arg(ctx, body, body_flag)?});
            set_opt(&mut b, "subject", subject.clone());
            set_opt(&mut b, "thread_id", thread.clone());
            let m = ctx.api.post(&base, Some(b)).await?;
            ctx.emit(&m, |c, m| print_sent(c, &json!([m])))
        }
    }
}
