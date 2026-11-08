//! `board` and `card`.

use std::time::Instant;

use serde_json::{Value, json};

use super::{delimited, event_line, print_table, set_list, set_opt, set_replace_list};
use crate::api::{CliError, query};
use crate::cli::{BoardCmd, CardCmd, CardEditArgs, CardFilesCmd, CardNewArgs, ColumnCmd};
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::{Ctx, Waiter};
use crate::output::{field, short_time};
use crate::transport::encode;

const CARD_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("COLUMN", "column"),
    ("POS", "position"),
    ("TITLE", "title"),
    ("ASSIGNEE", "assignee_name"),
    ("LABELS", "labels"),
    ("BLOCKED BY", "blocked_by"),
    ("IDLE", "idle"),
];

/// The card agent of a card (TER-75): the name of the ephemeral agent
/// bound to it, when it is not the assignee already.
pub fn card_agent(k: &Value) -> Option<&str> {
    let agent = k["agent_name"].as_str()?;
    let assignee = k["assignee_name"].as_str().unwrap_or_default();
    let same = assignee == agent || assignee.rsplit('/').next() == Some(agent);
    (!same).then_some(agent)
}

/// The idle time of a card with an idle mark (TER-67): `25m` since
/// `idle_since`. `None` for a card with no mark.
pub fn idle_for(k: &Value, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let since = chrono::DateTime::parse_from_rfc3339(k["idle_since"].as_str()?).ok()?;
    let secs = (now - since.with_timezone(&chrono::Utc)).num_seconds();
    Some(super::inspect::duration(secs))
}

/// The cards of a list, each with `idle` for the table.
fn card_rows(items: &[Value], now: chrono::DateTime<chrono::Utc>) -> Vec<Value> {
    items
        .iter()
        .map(|k| {
            let mut k = k.clone();
            if let Some(d) = idle_for(&k, now) {
                k["idle"] = d.into();
            }
            k
        })
        .collect()
}

fn card_path(id: &str) -> String {
    format!("/api/cards/{}", encode(id))
}

pub async fn board(ctx: &Ctx, cmd: Option<&BoardCmd>) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    let path = format!("{}/board", project_path(&p));
    let cols = match cmd {
        None => return show_board(ctx, &path).await,
        Some(BoardCmd::Columns { from_file: None }) => {
            let b = ctx.api.get(&path, vec![]).await?;
            return ctx.emit(&b["columns"], print_columns);
        }
        Some(BoardCmd::Columns { from_file: Some(f) }) => columns_from(ctx, f)?,
        Some(BoardCmd::Column { command }) => {
            let b = ctx.api.get(&path, vec![]).await?;
            edit_columns(as_items(&b["columns"]), command)?
        }
    };
    let b = ctx.api.patch(&path, json!({ "columns": cols })).await?;
    ctx.emit(&b["columns"], print_columns)
}

const COLUMN_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("TITLE", "title"),
    ("ROLE", "role"),
    ("WIP", "wip_limit"),
    ("CARDS", "count"),
];

fn print_columns(ctx: &Ctx, cols: &Value) {
    print_table(ctx, as_items(cols), COLUMN_COLS, "no columns");
}

/// The default title of a column, as the daemon makes it.
fn default_title(name: &str) -> String {
    name.replace('_', " ")
}

/// The column list of `--from-file`: a JSON list, or `{"columns": [...]}`.
fn columns_from(ctx: &Ctx, arg: &str) -> anyhow::Result<Vec<Value>> {
    let text = if arg == "-" {
        ctx.text("-")?
    } else {
        std::fs::read_to_string(arg)
            .map_err(|e| CliError::usage(format!("cannot read {arg}: {e}")))?
    };
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| CliError::usage(format!("{arg} is not JSON: {e}")))?;
    let list = match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove("columns") {
            Some(Value::Array(a)) => a,
            _ => return Err(CliError::usage(format!("{arg} has no `columns` list"))),
        },
        _ => return Err(CliError::usage(format!("{arg}: give a list of columns"))),
    };
    Ok(list)
}

/// The `PATCH .../board` list of the columns that the daemon returned:
/// the name, the role (null for none), the WIP limit, and the title when
/// it is not the default. A title left out keeps the stored one, so a
/// rename takes the default title of the new name.
fn column_specs(cols: &[Value]) -> Vec<Value> {
    cols.iter()
        .map(|c| {
            let name = c["name"].as_str().unwrap_or_default();
            let mut s =
                json!({"name": name, "role": c.get("role").cloned().unwrap_or(Value::Null)});
            if let Some(w) = c.get("wip_limit").filter(|w| !w.is_null()) {
                s["wip_limit"] = w.clone();
            }
            if let Some(t) = c["title"].as_str().filter(|t| *t != default_title(name)) {
                s["title"] = t.into();
            }
            s
        })
        .collect()
}

fn index_of(specs: &[Value], name: &str) -> anyhow::Result<usize> {
    specs.iter().position(|s| s["name"] == name).ok_or_else(|| {
        let names: Vec<&str> = specs.iter().filter_map(|s| s["name"].as_str()).collect();
        CliError::usage(format!("no column `{name}`; columns: {}", names.join(", ")))
    })
}

/// The place of a column put `--after` or `--before` another, or before
/// `done` when neither is given.
fn place(specs: &[Value], after: Option<&str>, before: Option<&str>) -> anyhow::Result<usize> {
    match (after, before) {
        (Some(a), _) => Ok(index_of(specs, a)? + 1),
        (_, Some(b)) => index_of(specs, b),
        _ => Ok(specs
            .iter()
            .position(|s| s["name"] == "done")
            .unwrap_or(specs.len())),
    }
}

/// Give `role` to the column at `at`: each other column with it loses it,
/// because one column has each role.
fn give_role(specs: &mut [Value], at: usize, role: &str) {
    for s in specs.iter_mut() {
        if s["role"] == role {
            s["role"] = Value::Null;
        }
    }
    specs[at]["role"] = role.into();
}

/// The whole column list after one `board column` change. The daemon
/// checks the rules (the fixed columns, the roles, cards in a removed
/// column).
pub fn edit_columns(cols: &[Value], cmd: &ColumnCmd) -> anyhow::Result<Vec<Value>> {
    let mut specs = column_specs(cols);
    match cmd {
        ColumnCmd::Add {
            name,
            title,
            wip,
            role,
            after,
            before,
        } => {
            if specs.iter().any(|s| s["name"] == name.as_str()) {
                return Err(CliError::usage(format!("column `{name}` exists")));
            }
            let at = place(&specs, after.as_deref(), before.as_deref())?;
            let mut s = json!({"name": name, "role": null});
            set_opt(&mut s, "title", title.clone());
            set_opt(&mut s, "wip_limit", *wip);
            specs.insert(at, s);
            if let Some(r) = role {
                give_role(&mut specs, at, r);
            }
        }
        ColumnCmd::Set {
            name,
            title,
            wip,
            no_wip,
            role,
            no_role,
            rename,
        } => {
            if title.is_none()
                && wip.is_none()
                && !no_wip
                && role.is_none()
                && !no_role
                && rename.is_none()
            {
                return Err(CliError::usage(
                    "give --title, --wip, --no-wip, --role, --no-role, or --rename",
                ));
            }
            let at = index_of(&specs, name)?;
            if let Some(t) = title {
                specs[at]["title"] = if t.trim().is_empty() {
                    Value::Null
                } else {
                    t.as_str().into()
                };
            }
            if let Some(w) = wip {
                specs[at]["wip_limit"] = (*w).into();
            }
            if *no_wip && let Some(o) = specs[at].as_object_mut() {
                o.remove("wip_limit");
            }
            if let Some(r) = role {
                give_role(&mut specs, at, r);
            }
            if *no_role {
                specs[at]["role"] = Value::Null;
            }
            if let Some(new) = rename.as_deref().filter(|n| *n != name.as_str()) {
                specs[at]["rename_from"] = name.as_str().into();
                specs[at]["name"] = new.into();
            }
        }
        ColumnCmd::Rm { name } => {
            let at = index_of(&specs, name)?;
            specs.remove(at);
        }
        ColumnCmd::Move {
            name,
            after,
            before,
        } => {
            let s = specs.remove(index_of(&specs, name)?);
            let at = place(&specs, after.as_deref(), before.as_deref())?;
            specs.insert(at, s);
        }
    }
    Ok(specs)
}

async fn show_board(ctx: &Ctx, path: &str) -> anyhow::Result<()> {
    let b = ctx.api.get(path, vec![]).await?;
    ctx.emit(&b, |c, b| {
        c.line(format!(
            "board {} (cards {}-N)",
            field(b, "project"),
            field(b, "card_prefix")
        ));
        let cards = as_items(&b["cards"]);
        for col in as_items(&b["columns"]) {
            let name = col["name"].as_str().unwrap_or_default();
            let title = col["title"]
                .as_str()
                .filter(|t| *t != default_title(name))
                .map(|t| format!(" \"{t}\""))
                .unwrap_or_default();
            let wip = col["wip_limit"]
                .as_u64()
                .map(|w| format!(", wip {w}"))
                .unwrap_or_default();
            let role = col["role"]
                .as_str()
                .map(|r| format!(", {r}"))
                .unwrap_or_default();
            c.line(format!(
                "\n== {name}{title} ({}{wip}{role})",
                field(col, "count")
            ));
            for card in cards.iter().filter(|k| k["column"] == name) {
                let who = match field(card, "assignee_name").as_str() {
                    "-" => String::new(),
                    a => format!("  @{a}"),
                };
                let who = match card_agent(card) {
                    Some(a) => format!("{who}  agent {a}"),
                    None => who,
                };
                let idle = idle_for(card, chrono::Utc::now())
                    .map(|d| format!("  (idle {d})"))
                    .unwrap_or_default();
                c.line(format!(
                    "  {:<9} {}{who}{idle}",
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
        "assignee: {}  created by: {}{}",
        field(k, "assignee_name"),
        field(k, "created_by_name"),
        k["agent_name"]
            .as_str()
            .map(|a| format!("  agent: {a}"))
            .unwrap_or_default()
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
    if let Some(eta) = eta_line(k, chrono::Utc::now()) {
        ctx.line(eta);
    }
    if let Some(d) = idle_for(k, chrono::Utc::now()) {
        ctx.line(format!(
            "idle: the assignee rests with no wait since {} ({d}); the leads were told",
            field(k, "idle_since")
        ));
    }
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

/// Check an ETA value here, for a usage error before the call: a duration
/// from now (`45m`, `2h30m`, `1d`) or an RFC 3339 time, in the future and
/// at most 30 days away. The daemon resolves the value again on its own
/// clock, so the value goes as it is.
pub fn eta_value(v: &str) -> anyhow::Result<String> {
    terrarium_core::eta::resolve(v, chrono::Utc::now()).map_err(CliError::usage)?;
    Ok(v.trim().to_owned())
}

/// The ETA line of `card show`: the time, the time left (or `overdue`, or
/// the done time), and who set it. `None` for a card with no ETA.
pub fn eta_line(k: &Value, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let at = k["eta_at"].as_str()?;
    let t = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    let secs = (t.with_timezone(&chrono::Utc) - now).num_seconds();
    let state = if k["column"] == "done" || k["archived"] == true {
        "done".to_owned()
    } else if secs >= 0 {
        format!("{} left", super::inspect::duration(secs))
    } else {
        format!("overdue {}", super::inspect::duration(-secs))
    };
    Some(format!(
        "eta: {at} ({state}), set by {}",
        field(k, "eta_set_by_name")
    ))
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
                let rows = card_rows(as_items(v), chrono::Utc::now());
                print_table(c, &rows, CARD_COLS, "no cards")
            })
        }
        CardCmd::Show { id } => {
            let k = ctx.api.get(&card_path(id), vec![]).await?;
            ctx.emit(&k, print_card)
        }
        CardCmd::New(a) => new(ctx, a).await,
        CardCmd::Edit(a) => edit(ctx, a).await,
        CardCmd::Claim { id, eta } => {
            let eta = eta.as_deref().map(eta_value).transpose()?;
            let mut k = ctx
                .api
                .post(&format!("{}/claim", card_path(id)), None)
                .await?;
            if let Some(eta) = eta {
                k = ctx
                    .api
                    .patch(&card_path(id), json!({"eta_at": eta}))
                    .await?;
            }
            ctx.emit(&k, |c, k| card_line(c, "claimed", k))
        }
        CardCmd::Move {
            id,
            column,
            position,
            comment,
            eta,
        } => {
            let mut b = json!({"column": column});
            set_opt(&mut b, "position", *position);
            set_opt(&mut b, "comment", ctx.opt_text(comment.as_deref())?);
            // With an ETA, one PATCH moves the card and sets it together.
            let k = match eta.as_deref().map(eta_value).transpose()? {
                Some(eta) => {
                    b["eta_at"] = eta.into();
                    ctx.api.patch(&card_path(id), b).await?
                }
                None => {
                    ctx.api
                        .post(&format!("{}/move", card_path(id)), Some(b))
                        .await?
                }
            };
            ctx.emit(&k, |c, k| card_line(c, "moved", k))
        }
        CardCmd::Eta { id, when, clear } => {
            let eta = match when.as_deref() {
                Some(w) if !*clear => Value::String(eta_value(w)?),
                _ => Value::Null,
            };
            let k = ctx
                .api
                .patch(&card_path(id), json!({"eta_at": eta}))
                .await?;
            ctx.emit(&k, |c, k| match k["eta_at"].as_str() {
                Some(at) => c.line(format!("{} ETA {at}", field(k, "id"))),
                None => c.line(format!("{} has no ETA", field(k, "id"))),
            })
        }
        CardCmd::Validate {
            id,
            commit,
            repo,
            branch,
            comment,
            eta,
        } => super::jobs::validate(ctx, &card_path(id), commit, repo, branch, comment, eta).await,
        CardCmd::Merge {
            ids,
            repo,
            branch,
            eta,
        } => match ids.as_slice() {
            [id] => super::jobs::merge(ctx, &card_path(id), repo, branch, eta).await,
            _ => super::jobs::merge_batch(ctx, ids, repo, branch, eta).await,
        },
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
            reply_to,
            text,
            message,
            body,
        } => {
            let text = message
                .as_deref()
                .or(body.as_deref())
                .or(text.as_deref())
                .unwrap_or_default();
            let mut request = json!({"body": ctx.text(text)?});
            if let Some(reply) = reply_to {
                request["reply_to"] = json!(reply);
            }
            let k = ctx
                .api
                .post(&format!("{}/comments", card_path(id)), Some(request))
                .await?;
            ctx.emit(&k, |c, k| {
                c.line(format!("commented on {}", field(k, "id")))
            })
        }
        CardCmd::Link {
            id,
            inbox_thread: Some(th),
            ..
        } => {
            let v = ctx
                .api
                .post(
                    &format!("/api/threads/{}/cards", encode(th)),
                    Some(json!({"card": id})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("linked {id} to Inbox thread {}", field(v, "id")));
            })
        }
        CardCmd::Link {
            id,
            thread,
            branch,
            card,
            link_url,
            inbox_thread: None,
        } => {
            let (kind, r) = match (thread, branch, card, link_url) {
                (Some(t), ..) => ("thread", t),
                (_, Some(b), ..) => ("branch", b),
                (_, _, Some(k), _) => ("card", k),
                (.., Some(u)) => ("url", u),
                _ => {
                    return Err(CliError::usage(
                        "give --thread, --inbox-thread, --branch, --card, or --link-url",
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
        CardCmd::Attach {
            id,
            paths,
            replace,
            name,
        } => attach(ctx, id, paths, *replace, name.as_deref()).await,
        CardCmd::Files { command } => files(ctx, command).await,
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
    set_replace_list(&mut b, "labels", "--label", &a.labels)?;
    set_replace_list(&mut b, "depends_on", "--depends-on", &a.depends_on)?;
    set_opt(&mut b, "assignee", a.assignee.clone());
    set_opt(&mut b, "repo", a.repo.clone());
    set_opt(&mut b, "branch", a.branch.clone());
    set_opt(&mut b, "from_mail", a.from_mail.clone());
    set_opt(&mut b, "github_author", a.github_author.clone());
    set_opt(&mut b, "inbox_thread", a.inbox_thread.clone());
    set_opt(&mut b, "comment", ctx.opt_text(a.comment.as_deref())?);
    set_opt(
        &mut b,
        "eta_at",
        a.eta.as_deref().map(eta_value).transpose()?,
    );
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
    set_replace_list(&mut b, "labels", "--label", &a.labels)?;
    set_list(&mut b, "add_labels", &a.add_labels);
    set_list(&mut b, "remove_labels", &a.rm_labels);
    set_replace_list(&mut b, "depends_on", "--depends-on", &a.depends_on)?;
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

// ---- card files (docs/api.md §8.4) ----

const FILE_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("VERSION", "version"),
    ("BYTES", "size"),
    ("TYPE", "media_type"),
    ("BY", "created_by_name"),
    ("AT", "created_at"),
];

const VERSION_COLS: &[(&str, &str)] = &[
    ("VERSION", "version"),
    ("BYTES", "size"),
    ("SHA256", "sha256"),
    ("BY", "created_by_name"),
    ("AT", "created_at"),
];

fn files_path(id: &str) -> String {
    format!("{}/files", card_path(id))
}

fn file_path(id: &str, name: &str) -> String {
    format!("{}/{}", files_path(id), encode(name))
}

/// Upload each file in order (the JSON form: every transport carries it).
/// The first failure stops the rest.
async fn attach(
    ctx: &Ctx,
    id: &str,
    paths: &[std::path::PathBuf],
    replace: bool,
    name: Option<&str>,
) -> anyhow::Result<()> {
    if name.is_some() && paths.len() > 1 {
        return Err(CliError::usage("`--name` takes one path only"));
    }
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let data = std::fs::read(path)
            .map_err(|e| CliError::usage(format!("cannot read {}: {e}", path.display())))?;
        let n = match name {
            Some(n) => n.to_owned(),
            None => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| CliError::usage(format!("{} has no file name", path.display())))?,
        };
        let mut body = json!({
            "name": n,
            "data_base64": super::artifacts::base64(&data),
        });
        if replace {
            body["replace"] = json!(true);
        }
        out.push(ctx.api.post(&files_path(id), Some(body)).await?);
    }
    let v = Value::Array(out);
    ctx.emit(&v, |c, v| {
        for f in as_items(v) {
            c.line(format!(
                "{} {} v{} ({} bytes) on {id}",
                field(f, "outcome"),
                field(f, "name"),
                field(f, "version"),
                field(f, "size")
            ));
        }
    })
}

async fn files(ctx: &Ctx, cmd: &CardFilesCmd) -> anyhow::Result<()> {
    match cmd {
        CardFilesCmd::Ls { id } => {
            let v = ctx.api.get(&files_path(id), vec![]).await?;
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), FILE_COLS, "no files")
            })
        }
        CardFilesCmd::Versions { id, name } => {
            let v = ctx
                .api
                .get(&format!("{}/versions", file_path(id, name)), vec![])
                .await?;
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), VERSION_COLS, "no versions")
            })
        }
        CardFilesCmd::Rm { id, name } => {
            ctx.api.delete(&file_path(id, name), vec![]).await?;
            ctx.emit(&json!({"card": id, "removed": name}), |c, _| {
                c.line(format!("removed {name} from {id}"));
            })
        }
        CardFilesCmd::Get {
            id,
            name,
            version,
            output,
            dir,
            force,
        } => {
            let names: Vec<String> = match name {
                Some(n) => vec![n.clone()],
                None => as_items(&ctx.api.get(&files_path(id), vec![]).await?)
                    .iter()
                    .filter_map(|f| f["name"].as_str().map(str::to_owned))
                    .collect(),
            };
            let dir = match (name, dir) {
                (_, Some(d)) => d.clone(),
                (Some(_), None) => std::path::PathBuf::from("."),
                (None, None) => std::path::PathBuf::from(id),
            };
            let mut got = Vec::new();
            for n in &names {
                let q = query([
                    ("format", Some("json".to_owned())),
                    ("version", version.map(|v| v.to_string())),
                ]);
                let f = ctx.api.get(&file_path(id, n), q).await?;
                let data = {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD
                        .decode(f["data_base64"].as_str().unwrap_or(""))
                        .map_err(|e| {
                            anyhow::anyhow!("the daemon answered bad base64 for {n}: {e}")
                        })?
                };
                // The daemon checks the names; check again before a write.
                let safe = f["name"]
                    .as_str()
                    .filter(|s| !s.is_empty() && !s.contains('/') && !s.starts_with('.'))
                    .ok_or_else(|| anyhow::anyhow!("the daemon answered a bad name for {n}"))?;
                let to = output.clone().unwrap_or_else(|| dir.join(safe));
                if to.as_os_str() == "-" {
                    use std::io::Write as _;
                    let mut so = std::io::stdout().lock();
                    so.write_all(&data)?;
                    so.flush()?;
                    return Ok(());
                }
                if to.exists() && !force {
                    return Err(CliError::usage(format!(
                        "{} is there already; add --force to overwrite it",
                        to.display()
                    )));
                }
                if let Some(parent) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| anyhow::anyhow!("cannot make {}: {e}", parent.display()))?;
                }
                std::fs::write(&to, &data)
                    .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", to.display()))?;
                got.push(json!({
                    "name": safe, "version": f["version"], "size": data.len(),
                    "path": to.display().to_string(),
                }));
            }
            let v = Value::Array(got);
            ctx.emit(&v, |c, v| {
                if as_items(v).is_empty() {
                    c.line(format!("{id} has no files"));
                }
                for f in as_items(v) {
                    c.line(format!(
                        "{} v{} -> {}",
                        field(f, "name"),
                        field(f, "version"),
                        field(f, "path")
                    ));
                }
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod eta_tests {
    use super::*;

    #[test]
    fn eta_values_are_checked_before_the_call() {
        assert_eq!(eta_value(" 2h30m ").unwrap(), "2h30m");
        assert_eq!(eta_value("45m").unwrap(), "45m");
        assert_eq!(eta_value("1d").unwrap(), "1d");
        let later = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        assert_eq!(eta_value(&later).unwrap(), later);
        for bad in ["45", "2000-01-01T00:00:00Z", "31d", "soon", "0m", "14:30"] {
            assert!(eta_value(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_eta_line_counts_down_or_says_overdue() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-30T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let k = json!({"eta_at": "2026-10-30T11:12:00.000Z", "column": "in_progress",
                       "eta_set_by_name": "proj/w1"});
        let line = eta_line(&k, now).unwrap();
        assert!(line.contains("left") && line.contains("proj/w1"), "{line}");
        let k = json!({"eta_at": "2026-10-30T09:48:00.000Z", "column": "review"});
        assert!(eta_line(&k, now).unwrap().contains("overdue"));
        let k = json!({"eta_at": "2026-10-30T09:48:00.000Z", "column": "done"});
        assert!(eta_line(&k, now).unwrap().contains("(done)"));
        assert_eq!(eta_line(&json!({"column": "done"}), now), None);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod label_tests {
    use super::*;
    use crate::transport::mock::MockTransport;

    fn args(add: &[&str], rm: &[&str], set: &[&str]) -> CardEditArgs {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect();
        CardEditArgs {
            id: "TER-1".into(),
            title: None,
            body: None,
            labels: v(set),
            add_labels: v(add),
            rm_labels: v(rm),
            depends_on: Vec::new(),
            repo: None,
            branch: None,
        }
    }

    /// TER-36: `--add-label` and `--rm-label` send `add_labels` and
    /// `remove_labels`, not a replacement of every label.
    #[tokio::test]
    async fn edit_adds_and_removes_labels() {
        let mock = MockTransport::new();
        mock.reply(
            200,
            json!({"id": "TER-1", "labels": ["bug", "version:0.1.0"]}),
        );
        let c = Ctx::for_test(std::sync::Arc::new(mock.clone()), None);
        edit(&c, &args(&["version:0.1.0"], &["version:vnext"], &[]))
            .await
            .unwrap();
        let r = mock.requests();
        assert_eq!(
            r[0].body,
            Some(json!({"add_labels": ["version:0.1.0"], "remove_labels": ["version:vnext"]}))
        );
        assert!(r[0].body.as_ref().unwrap().get("labels").is_none());
    }
}
