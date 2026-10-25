//! `board` and `card`.

use std::time::Instant;

use serde_json::{Value, json};

use super::{delimited, event_line, print_table, set_list, set_opt};
use crate::api::{CliError, query};
use crate::cli::{CardCmd, CardEditArgs, CardNewArgs};
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::{Ctx, Waiter};
use crate::output::{field, short_time};
use crate::transport::encode;

const CARD_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("COLUMN", "column"),
    ("TITLE", "title"),
    ("ASSIGNEE", "assignee_name"),
    ("LABELS", "labels"),
    ("BLOCKED BY", "blocked_by"),
];

fn card_path(id: &str) -> String {
    format!("/api/cards/{}", encode(id))
}

pub async fn board(ctx: &Ctx) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    let b = ctx
        .api
        .get(&format!("{}/board", project_path(&p)), vec![])
        .await?;
    ctx.emit(&b, |c, b| {
        c.line(format!(
            "board {} (cards {}-N)",
            field(b, "project"),
            field(b, "card_prefix")
        ));
        let cards = as_items(&b["cards"]);
        for col in as_items(&b["columns"]) {
            let name = col["name"].as_str().unwrap_or_default();
            let wip = col["wip_limit"]
                .as_u64()
                .map(|w| format!(", wip {w}"))
                .unwrap_or_default();
            c.line(format!("\n== {name} ({}{wip})", field(col, "count")));
            for card in cards.iter().filter(|k| k["column"] == name) {
                let who = match field(card, "assignee_name").as_str() {
                    "-" => String::new(),
                    a => format!("  @{a}"),
                };
                c.line(format!(
                    "  {:<9} {}{who}",
                    field(card, "id"),
                    field(card, "title")
                ));
            }
        }
    })
}

pub fn print_card(ctx: &Ctx, k: &Value) {
    ctx.line(format!(
        "{}  {}  [{}]{}",
        field(k, "id"),
        field(k, "title"),
        field(k, "column"),
        if k["archived"] == true {
            "  (archived)"
        } else {
            ""
        }
    ));
    ctx.line(format!(
        "assignee: {}  created by: {}",
        field(k, "assignee_name"),
        field(k, "created_by_name")
    ));
    ctx.line(format!(
        "labels: {}  repo: {}  branch: {}",
        field(k, "labels"),
        field(k, "repo"),
        field(k, "branch")
    ));
    ctx.line(format!(
        "depends on: {}  blocked by: {}",
        field(k, "depends_on"),
        field(k, "blocked_by")
    ));
    if let Some(body) = k["body"].as_str().filter(|b| !b.is_empty()) {
        delimited(ctx, "CARD BODY", body);
    }
    let links = as_items(&k["links"]);
    if !links.is_empty() {
        ctx.line("links:");
        for l in links {
            ctx.line(format!("  {}: {}", field(l, "kind"), field(l, "ref")));
        }
    }
    let comments = as_items(&k["comments"]);
    if !comments.is_empty() {
        ctx.line("comments:");
        for cm in comments {
            ctx.line(format!(
                "  {} {}:",
                short_time(&field(cm, "created_at")),
                field(cm, "author_name")
            ));
            for line in cm["body"].as_str().unwrap_or_default().lines() {
                ctx.line(format!("    {line}"));
            }
        }
    }
}

fn card_line(ctx: &Ctx, verb: &str, k: &Value) {
    ctx.line(format!(
        "{verb} {}: {} [{}]",
        field(k, "id"),
        field(k, "title"),
        field(k, "column")
    ));
}

pub async fn card(ctx: &Ctx, cmd: &CardCmd) -> anyhow::Result<()> {
    match cmd {
        CardCmd::Ls {
            column,
            mine,
            assignee,
            unassigned,
            label,
            archived,
        } => {
            let p = ctx.project_or(None).await?;
            let assignee = if *mine {
                Some("me".to_owned())
            } else {
                assignee.clone()
            };
            let q = query([
                ("column", column.clone()),
                ("assignee", assignee),
                ("unassigned", unassigned.then(|| "true".to_owned())),
                ("label", label.clone()),
                ("archived", archived.then(|| "true".to_owned())),
            ]);
            let v = Value::Array(
                ctx.api
                    .list(&format!("{}/cards", project_path(&p)), q, None)
                    .await?,
            );
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), CARD_COLS, "no cards")
            })
        }
        CardCmd::Show { id } => {
            let k = ctx.api.get(&card_path(id), vec![]).await?;
            ctx.emit(&k, print_card)
        }
        CardCmd::New(a) => new(ctx, a).await,
        CardCmd::Edit(a) => edit(ctx, a).await,
        CardCmd::Claim { id } => {
            let k = ctx
                .api
                .post(&format!("{}/claim", card_path(id)), None)
                .await?;
            ctx.emit(&k, |c, k| card_line(c, "claimed", k))
        }
        CardCmd::Move {
            id,
            column,
            position,
            comment,
        } => {
            let mut b = json!({"column": column});
            set_opt(&mut b, "position", *position);
            set_opt(&mut b, "comment", ctx.opt_text(comment.as_deref())?);
            let k = ctx
                .api
                .post(&format!("{}/move", card_path(id)), Some(b))
                .await?;
            ctx.emit(&k, |c, k| card_line(c, "moved", k))
        }
        CardCmd::Assign { id, agent } => {
            let who = match agent.as_str() {
                "none" | "-" => Value::Null,
                a => a.into(),
            };
            let k = ctx
                .api
                .post(
                    &format!("{}/assign", card_path(id)),
                    Some(json!({"assignee": who})),
                )
                .await?;
            ctx.emit(&k, |c, k| {
                c.line(format!(
                    "{} assigned to {}",
                    field(k, "id"),
                    field(k, "assignee_name")
                ));
            })
        }
        CardCmd::Comment {
            id,
            text,
            message,
            body,
        } => {
            let text = message
                .as_deref()
                .or(body.as_deref())
                .or(text.as_deref())
                .unwrap_or_default();
            let k = ctx
                .api
                .post(
                    &format!("{}/comments", card_path(id)),
                    Some(json!({"body": ctx.text(text)?})),
                )
                .await?;
            ctx.emit(&k, |c, k| {
                c.line(format!("commented on {}", field(k, "id")))
            })
        }
        CardCmd::Link {
            id,
            thread,
            branch,
            card,
            link_url,
        } => {
            let (kind, r) = match (thread, branch, card, link_url) {
                (Some(t), ..) => ("thread", t),
                (_, Some(b), ..) => ("branch", b),
                (_, _, Some(k), _) => ("card", k),
                (.., Some(u)) => ("url", u),
                _ => {
                    return Err(CliError::usage(
                        "give --thread, --branch, --card, or --link-url",
                    ));
                }
            };
            let k = ctx
                .api
                .post(
                    &format!("{}/links", card_path(id)),
                    Some(json!({"kind": kind, "ref": r})),
                )
                .await?;
            ctx.emit(&k, |c, k| {
                c.line(format!("linked {kind} {r} to {}", field(k, "id")));
            })
        }
        CardCmd::Rm { id } => {
            ctx.api.delete(&card_path(id), vec![]).await?;
            ctx.emit(&json!({"archived": id}), |c, _| {
                c.line(format!("archived {id}"));
            })
        }
        CardCmd::Events { id } => {
            let v = Value::Array(
                ctx.api
                    .list(&format!("{}/events", card_path(id)), vec![], None)
                    .await?,
            );
            ctx.emit(&v, |c, v| {
                for e in as_items(v) {
                    c.line(event_line(e));
                }
            })
        }
        CardCmd::Wait { mine, timeout } => wait(ctx, *mine, *timeout).await,
    }
}

async fn new(ctx: &Ctx, a: &CardNewArgs) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    let mut b = json!({});
    set_opt(&mut b, "title", a.title.clone());
    set_opt(&mut b, "body", ctx.opt_text(a.body.as_deref())?);
    set_opt(&mut b, "column", a.column.clone());
    set_list(&mut b, "labels", &a.labels);
    set_list(&mut b, "depends_on", &a.depends_on);
    set_opt(&mut b, "assignee", a.assignee.clone());
    set_opt(&mut b, "repo", a.repo.clone());
    set_opt(&mut b, "branch", a.branch.clone());
    set_opt(&mut b, "from_mail", a.from_mail.clone());
    set_opt(&mut b, "comment", ctx.opt_text(a.comment.as_deref())?);
    if a.title.is_none() && a.from_mail.is_none() {
        return Err(CliError::usage("give a title, or --from-mail <message id>"));
    }
    let k = ctx
        .api
        .post(&format!("{}/cards", project_path(&p)), Some(b))
        .await?;
    ctx.emit(&k, |c, k| card_line(c, "created", k))
}

async fn edit(ctx: &Ctx, a: &CardEditArgs) -> anyhow::Result<()> {
    let mut b = json!({});
    set_opt(&mut b, "title", a.title.clone());
    set_opt(&mut b, "body", ctx.opt_text(a.body.as_deref())?);
    set_list(&mut b, "labels", &a.labels);
    set_list(&mut b, "depends_on", &a.depends_on);
    for (key, v) in [("repo", &a.repo), ("branch", &a.branch)] {
        if let Some(v) = v {
            b[key] = if v == "none" {
                Value::Null
            } else {
                v.clone().into()
            };
        }
    }
    if b.as_object().is_some_and(serde_json::Map::is_empty) {
        return Err(CliError::usage("give at least one field to change"));
    }
    let k = ctx.api.patch(&card_path(&a.id), b).await?;
    ctx.emit(&k, |c, k| card_line(c, "updated", k))
}

/// Wait for a card change.
///
/// - `--mine` with no card assigned: long-poll the card list with
///   `assignee=me&wait=`. It answers when a card is assigned.
/// - Else: watch the card events of the project after now. The firehose
///   has no `?wait=`, so this polls every few seconds.
async fn wait(ctx: &Ctx, mine: bool, timeout: u64) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    let me = ctx.me().await?;
    let cards = format!("{}/cards", project_path(&p));
    let w = Waiter::new(timeout, ctx.poll_floor);
    if mine
        && ctx
            .api
            .page(&cards, query([("assignee", Some("me".into()))]))
            .await?
            .items
            .is_empty()
    {
        while let Some(secs) = w.next() {
            let started = Instant::now();
            let q = query([
                ("assignee", Some("me".into())),
                ("wait", Some(secs.to_string())),
            ]);
            let page = ctx.api.page(&cards, q).await?;
            if !page.items.is_empty() {
                let v = Value::Array(page.items);
                return ctx.emit(&v, |c, v| {
                    for k in as_items(v) {
                        card_line(c, "assigned to you:", k);
                    }
                });
            }
            w.pause(started).await;
        }
        return ctx.emit(&json!([]), |c, _| {
            c.line(format!("no card changes after {timeout}s"));
        });
    }
    let base = query([("kind", Some("card".into())), ("project", Some(p))]);
    // Find the newest card event, so that only later events count.
    let past = ctx.api.list("/api/events", base.clone(), None).await?;
    let mut since = past
        .last()
        .map_or_else(|| "0".to_owned(), |e| field(e, "seq"));
    let matches = |e: &Value| {
        !mine
            || e["agent_id"] == me.as_str()
            || e["data"]["assignee"] == me.as_str()
            || (me == "operator" && e["data"]["assignee"] == "operator")
    };
    while let Some(secs) = w.next() {
        let started = Instant::now();
        let mut q = base.clone();
        q.push(("since".into(), since.clone()));
        q.push(("wait".into(), secs.to_string()));
        let page = ctx.api.page("/api/events", q).await?;
        if let Some(last) = page.items.last() {
            since = field(last, "seq");
        }
        let hits: Vec<Value> = page.items.into_iter().filter(matches).collect();
        if !hits.is_empty() {
            let v = Value::Array(hits);
            return ctx.emit(&v, |c, v| {
                for e in as_items(v) {
                    c.line(event_line(e));
                }
            });
        }
        w.pause(started).await;
    }
    ctx.emit(&json!([]), |c, _| {
        c.line(format!("no card changes after {timeout}s"));
    })
}
