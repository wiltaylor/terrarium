//! `question`, `notify`, `admin question`, and `admin inbox` (docs/api.md
//! §6.8, §6.9, PLAN §12e). A leader asks the operator, asks a bundle of
//! related questions, or sends a notice; the answer or the acknowledgement
//! comes as mail. The text, the options, the items, and the context of a
//! question are the agent's own words: `show` prints them between data
//! markers.

use anyhow::Context as _;
use serde_json::{Value, json};

use super::{agent_path, delimited, print_object, print_table};
use crate::api::{CliError, query};
use crate::cli::{AdminInboxCmd, AdminQuestionCmd, NotifyArgs, QuestionCmd, QuestionContext};
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

const COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("KIND", "kind"),
    ("STATUS", "status"),
    ("PRIORITY", "priority"),
    ("CARD", "card"),
    ("ASKED", "created_at"),
    ("QUESTION", "short"),
];

const ADMIN_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("KIND", "kind"),
    ("STATUS", "status"),
    ("PRIORITY", "priority"),
    ("AGENT", "agent_name"),
    ("CARD", "card"),
    ("ASKED", "created_at"),
    ("QUESTION", "short"),
];

fn path(id: &str) -> String {
    format!("/api/questions/{}", encode(id))
}

/// The first line of the text, at most 60 characters, for a table.
fn short(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut out: String = first.trim().chars().take(60).collect();
    if first.trim().chars().count() > 60 || text.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

/// The rows with a `short` column of the text.
fn rows(v: &Value) -> Vec<Value> {
    as_items(v)
        .iter()
        .map(|q| {
            let mut q = q.clone();
            q["short"] = json!(short(q["text"].as_str().unwrap_or_default()));
            if q["kind"].is_null() {
                q["kind"] = json!("question");
            }
            q
        })
        .collect()
}

pub(super) fn context_value(ctx: &Ctx, c: &QuestionContext) -> anyhow::Result<Option<String>> {
    if let Some(text) = &c.context {
        return Ok(Some(ctx.text(text)?));
    }
    if let Some(p) = &c.context_file {
        return Ok(Some(
            std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?,
        ));
    }
    Ok(None)
}

/// The text of a file argument: a path, or `-` for stdin.
fn file_arg(ctx: &Ctx, arg: &str, what: &str) -> anyhow::Result<String> {
    if arg == "-" {
        return ctx.text("-");
    }
    std::fs::read_to_string(arg).with_context(|| format!("cannot read the {what} file {arg}"))
}

/// The JSON array of a file argument; `{"<key>": [...]}` is the same.
fn json_list(ctx: &Ctx, arg: &str, what: &str, key: &str) -> anyhow::Result<Vec<Value>> {
    let text = file_arg(ctx, arg, what)?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| CliError::usage(format!("the {what} file is not JSON: {e}")))?;
    match v {
        Value::Array(a) => Ok(a),
        Value::Object(mut o) if o.get(key).is_some_and(Value::is_array) => {
            Ok(as_items(&o.remove(key).unwrap_or_default()).to_vec())
        }
        _ => Err(CliError::usage(format!(
            "the {what} file needs a JSON array, or an object with `{key}`"
        ))),
    }
}

/// The items of a bundle from `--items-file`: a plain string is an item
/// with no options. The daemon checks the rest.
pub(super) fn items_value(ctx: &Ctx, arg: &str) -> anyhow::Result<Value> {
    let items = json_list(ctx, arg, "items", "items")?
        .into_iter()
        .map(|i| match i {
            Value::String(t) => json!({"text": t}),
            other => other,
        })
        .collect();
    Ok(Value::Array(items))
}

/// The `items` of a bundle answer from `--answers-file`. An option is a
/// label of its item, or a number from 1, as `--option` takes it.
fn answers_value(given: Vec<Value>, bundle: &Value) -> anyhow::Result<Value> {
    if bundle["kind"] != "bundle" {
        return Err(CliError::usage(
            "--answers-file is for a bundle; answer a question with a text or --option",
        ));
    }
    let items = as_items(&bundle["items"]);
    let mut out = Vec::with_capacity(given.len());
    for (i, a) in given.into_iter().enumerate() {
        let item = items.get(i).cloned().unwrap_or_default();
        let mut entry = json!({});
        match a {
            Value::String(t) => entry["answer"] = json!(t),
            Value::Object(o) => {
                if let Some(t) = o.get("answer").and_then(Value::as_str) {
                    entry["answer"] = json!(t);
                }
                let opt = match o.get("option") {
                    Some(Value::String(l)) => Some(l.clone()),
                    Some(Value::Number(n)) => Some(n.to_string()),
                    _ => None,
                };
                if let Some(o) = opt {
                    entry["option"] = option_ref(&item, &o)
                        .map_err(|e| CliError::usage(format!("item {}: {e}", i + 1)))?;
                }
                if let Some(options) = o.get("options") {
                    let options = options.as_array().ok_or_else(|| {
                        CliError::usage(format!("item {}: options must be an array", i + 1))
                    })?;
                    let selected = options
                        .iter()
                        .map(|o| {
                            let label = match o {
                                Value::String(s) => s.clone(),
                                Value::Number(n) => n.to_string(),
                                _ => {
                                    return Err(CliError::usage(
                                        "a checked option must be a label or number",
                                    ));
                                }
                            };
                            option_ref(&item, &label)
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    entry["options"] = json!(selected);
                }
            }
            _ => {
                return Err(CliError::usage(format!(
                    "answer {} is not a string or an object",
                    i + 1
                )));
            }
        }
        out.push(entry);
    }
    Ok(Value::Array(out))
}

/// The items of a bundle: each item's words between markers, then its
/// answer.
fn print_items(c: &Ctx, v: &Value, items: &[Value]) {
    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
        let mut block = item["text"].as_str().unwrap_or_default().to_owned();
        let options: Vec<&str> = as_items(&item["options"])
            .iter()
            .filter_map(Value::as_str)
            .collect();
        if !options.is_empty() {
            block.push_str("\n\nOptions:\n");
            for (k, o) in options.iter().enumerate() {
                block.push_str(&format!("{}. {o}\n", k + 1));
            }
        }
        if let Some(ctx) = item["context"].as_str() {
            block.push_str(&format!("\n\nContext:\n{ctx}"));
        }
        delimited(c, &format!("ITEM {n} OF {}", items.len()), &block);
        if let Some(k) = item["answer_option"].as_u64() {
            let label = usize::try_from(k)
                .ok()
                .and_then(|k| options.get(k))
                .copied()
                .unwrap_or_default();
            c.line(format!("item {n} chosen option: {}. {label}", k + 1));
        }
        if let Some(a) = item["answer"].as_str() {
            print_selected(c, item);
            c.line(format!(
                "item {n} answer by {}:",
                field(v, "answered_by_name")
            ));
            c.out.write_str(a);
            if !a.ends_with('\n') {
                c.line("");
            }
        }
    }
}

/// One question: the fields, then the agent's words between markers,
/// then the answer.
pub(super) fn print_question(c: &Ctx, v: &Value) {
    print_selected(c, v);
    let mut head = v.clone();
    let mut take = |k: &str| head.as_object_mut().and_then(|o| o.remove(k));
    let text = take("text");
    let options = take("options");
    let context = take("context");
    let answer = take("answer");
    let option = take("answer_option");
    let items = take("items");
    let notice = v["kind"] == "notice";
    let bundle = v["kind"] == "bundle";
    print_object(c, &head);
    if let Some(t) = text.as_ref().and_then(Value::as_str) {
        let what = match (notice, bundle) {
            (true, _) => "NOTICE",
            (_, true) => "BUNDLE",
            _ => "QUESTION",
        };
        delimited(c, what, t);
    }
    let options: Vec<String> = options
        .as_ref()
        .map(as_items)
        .unwrap_or_default()
        .iter()
        .filter_map(|o| o.as_str().map(str::to_owned))
        .collect();
    if !options.is_empty() {
        let list: String = options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("{}. {o}\n", i + 1))
            .collect();
        delimited(c, "OPTIONS", &list);
    }
    if let Some(t) = context.as_ref().and_then(Value::as_str) {
        delimited(c, "CONTEXT", t);
    }
    if let Some(items) = items.as_ref() {
        print_items(c, v, as_items(items));
    }
    if let Some(i) = option.as_ref().and_then(Value::as_u64) {
        let label = usize::try_from(i)
            .ok()
            .and_then(|i| options.get(i))
            .cloned()
            .unwrap_or_default();
        c.line(format!("chosen option: {}. {label}", i + 1));
    }
    if let Some(a) = answer.as_ref().and_then(Value::as_str) {
        let what = if notice { "reply" } else { "answer" };
        c.line(format!("{what} by {}:", field(v, "answered_by_name")));
        c.out.write_str(a);
        if !a.ends_with('\n') {
            c.line("");
        }
    }
}

fn print_selected(c: &Ctx, v: &Value) {
    for index in as_items(&v["answer_options"]) {
        if let Some(i) = index.as_u64().and_then(|i| usize::try_from(i).ok())
            && let Some(label) = v["options"].get(i).and_then(Value::as_str)
        {
            c.line(format!("chosen option: {}. {label}", i + 1));
        }
    }
}

/// The agent whose questions to list: `--agent`, else the caller.
async fn subject(ctx: &Ctx, agent: &Option<String>) -> anyhow::Result<String> {
    match agent {
        Some(a) => Ok(a.clone()),
        None if ctx.is_agent() => ctx.me().await,
        None => Err(CliError::usage(
            "the operator names the agent (--agent <agent>), or runs `terrarium admin question ls`",
        )),
    }
}

pub async fn question(ctx: &Ctx, cmd: &QuestionCmd) -> anyhow::Result<()> {
    match cmd {
        QuestionCmd::Ask {
            text,
            options,
            multi_select,
            context,
            card,
            urgent,
            critical,
            wait,
            timeout,
        } => {
            if !ctx.is_agent() {
                return Err(CliError::usage(
                    "an agent asks questions; the operator answers them with \
                     `terrarium admin question answer`",
                ));
            }
            let me = ctx.me().await?;
            let mut body = json!({"text": ctx.text(text)?});
            if *multi_select {
                body["multi_select"] = json!(true);
            }
            super::set_list(&mut body, "options", options);
            super::set_opt(&mut body, "context", context_value(ctx, context)?);
            super::set_opt(&mut body, "card", card.clone());
            set_priority(&mut body, *urgent, *critical);
            if *wait {
                body["wait"] = json!(true);
            }
            super::set_opt(&mut body, "timeout", timeout.clone());
            let v = ctx
                .api
                .post(&format!("{}/questions", agent_path(&me)), Some(body))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "question {} asked ({}). The answer comes as mail on thread {}.",
                    field(v, "id"),
                    field(v, "priority"),
                    field(v, "thread_id")
                ));
                if v["wait"].is_object() {
                    c.line(format!(
                        "wait {} registered: {}",
                        field(&v["wait"], "id"),
                        field(&v["wait"], "summary")
                    ));
                    c.line(
                        "End your turn now. The answer mail wakes you; do not poll and do not \
                         ask again.",
                    );
                } else {
                    c.line(
                        "Go on with other work. Do not ask it again: `terrarium question ls` \
                         lists it.",
                    );
                }
            })
        }
        QuestionCmd::Bundle {
            text,
            items,
            items_file,
            context,
            card,
            urgent,
            critical,
            wait,
            timeout,
        } => {
            if !ctx.is_agent() {
                return Err(CliError::usage(
                    "an agent asks questions; the operator answers them with \
                     `terrarium admin question answer`",
                ));
            }
            if text == "-" && items_file.as_deref() == Some("-") {
                return Err(CliError::usage(
                    "only one of the summary and --items-file can read stdin",
                ));
            }
            let items = match items_file {
                Some(f) => items_value(ctx, f)?,
                None => items.iter().map(|t| json!({"text": t})).collect(),
            };
            let me = ctx.me().await?;
            let mut body = json!({"kind": "bundle", "text": ctx.text(text)?, "items": items});
            super::set_opt(&mut body, "context", context_value(ctx, context)?);
            super::set_opt(&mut body, "card", card.clone());
            set_priority(&mut body, *urgent, *critical);
            if *wait {
                body["wait"] = json!(true);
            }
            super::set_opt(&mut body, "timeout", timeout.clone());
            let v = ctx
                .api
                .post(&format!("{}/questions", agent_path(&me)), Some(body))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "bundle {} asked ({}): {} questions. The answers come as one mail on \
                     thread {}.",
                    field(v, "id"),
                    field(v, "priority"),
                    as_items(&v["items"]).len(),
                    field(v, "thread_id")
                ));
                if v["wait"].is_object() {
                    c.line(format!(
                        "wait {} registered: {}",
                        field(&v["wait"], "id"),
                        field(&v["wait"], "summary")
                    ));
                    c.line(
                        "End your turn now. The answer mail wakes you; do not poll and do not \
                         ask again.",
                    );
                } else {
                    c.line(
                        "Go on with other work. Do not ask it again: `terrarium question ls` \
                         lists it.",
                    );
                }
            })
        }
        QuestionCmd::Ls {
            all,
            status,
            kind,
            agent,
        } => {
            let who = subject(ctx, agent).await?;
            let q = query([
                ("all", all.then(|| "true".to_owned())),
                ("status", status.clone()),
                ("kind", kind.clone()),
            ]);
            let v = ctx
                .api
                .get(&format!("{}/questions", agent_path(&who)), q)
                .await?;
            ctx.emit(&v, |c, v| {
                print_table(
                    c,
                    &rows(v),
                    COLS,
                    "no open or recently answered questions or notices",
                )
            })
        }
        QuestionCmd::Show { id } => {
            let v = ctx.api.get(&path(id), vec![]).await?;
            ctx.emit(&v, print_question)
        }
        QuestionCmd::Update {
            id,
            text,
            options,
            clear_options,
            items_file,
            context,
            card,
            no_card,
            urgent,
            critical,
            normal,
        } => {
            let mut body = json!({});
            if text.as_deref() == Some("-") && items_file.as_deref() == Some("-") {
                return Err(CliError::usage(
                    "only one of --text and --items-file can read stdin",
                ));
            }
            if let Some(t) = text {
                body["text"] = json!(ctx.text(t)?);
            }
            if let Some(f) = items_file {
                body["items"] = items_value(ctx, f)?;
            }
            if *clear_options {
                body["options"] = json!([]);
            } else {
                super::set_list(&mut body, "options", options);
            }
            super::set_opt(&mut body, "context", context_value(ctx, context)?);
            card_value(&mut body, card, *no_card);
            set_priority(&mut body, *urgent, *critical);
            if *normal {
                body["priority"] = json!("normal");
            }
            if body.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(CliError::usage(
                    "nothing to change: give --text, --option, --clear-options, --items-file, \
                     --context, --card, --no-card, --urgent, --critical, or --normal",
                ));
            }
            let v = ctx.api.patch(&path(id), body).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{} {} updated (revision {}).",
                    kind_of(v),
                    field(v, "id"),
                    field(v, "revision")
                ));
            })
        }
        QuestionCmd::Retract { id, reason } => retract(ctx, id, reason).await,
        QuestionCmd::ToThread { id, message } => to_thread(ctx, id, message).await,
    }
}

async fn retract(ctx: &Ctx, id: &str, reason: &Option<String>) -> anyhow::Result<()> {
    let mut body = json!({});
    super::set_opt(&mut body, "reason", reason.clone());
    let v = ctx
        .api
        .post(&format!("{}/retract", path(id)), Some(body))
        .await?;
    ctx.emit(&v, |c, v| {
        c.line(format!("{} {} retracted.", kind_of(v), field(v, "id")));
    })
}

/// `question to-thread <id> [-m <message>]` (GitHub #30): the operator
/// moves the question into a new Inbox thread with its agent.
async fn to_thread(ctx: &Ctx, id: &str, message: &Option<String>) -> anyhow::Result<()> {
    let mut body = json!({});
    if let Some(m) = message {
        body["message"] = json!(ctx.text(m)?);
    }
    let v = ctx
        .api
        .post(&format!("{}/to-thread", path(id)), Some(body))
        .await?;
    ctx.emit(&v, |c, v| {
        let q = &v["question"];
        c.line(format!(
            "{} {} moved into thread {} with {}: `terrarium thread show {}`.",
            kind_of(q),
            field(q, "id"),
            field(&v["thread"], "id"),
            field(q, "agent_name"),
            field(&v["thread"], "id")
        ));
    })
}

/// `question`, `notice`, or `bundle`, for a message.
fn kind_of(v: &Value) -> &str {
    match v["kind"].as_str() {
        Some(k @ ("notice" | "bundle")) => k,
        _ => "question",
    }
}

/// The `option` of an answer: a label of the question as it is, else a
/// number from 1 as the 0-based index.
fn option_ref(question: &Value, arg: &str) -> anyhow::Result<Value> {
    let arg = arg.trim();
    let labels: Vec<&str> = as_items(&question["options"])
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if labels.contains(&arg) {
        return Ok(json!(arg));
    }
    match arg.parse::<usize>() {
        Ok(n) if n >= 1 && n <= labels.len() => Ok(json!(n - 1)),
        _ if labels.is_empty() => Err(CliError::usage(
            "the question has no options; give the answer as text",
        )),
        _ => Err(CliError::usage(format!(
            "no option `{arg}`: give a label or a number from 1 to {}",
            labels.len()
        ))),
    }
}

pub async fn admin(ctx: &Ctx, cmd: &AdminQuestionCmd) -> anyhow::Result<()> {
    match cmd {
        AdminQuestionCmd::Ls {
            agent,
            open: _,
            status,
            all,
            kind,
        } => {
            let status = if *all {
                Some("all".to_owned())
            } else {
                status.clone()
            };
            let q = query([
                ("status", status),
                ("project", ctx.project.clone()),
                ("agent", agent.clone()),
                ("kind", kind.clone()),
            ]);
            let v = ctx.api.get("/api/questions", q).await?;
            ctx.emit(&v, |c, v| {
                print_table(c, &rows(v), ADMIN_COLS, "no questions")
            })
        }
        AdminQuestionCmd::Show { id } => {
            let v = ctx.api.get(&path(id), vec![]).await?;
            ctx.emit(&v, print_question)
        }
        AdminQuestionCmd::Answer {
            id,
            text,
            option,
            selected,
            select_none,
            answers_file,
        } => {
            if let Some(f) = answers_file {
                let given = json_list(ctx, f, "answers", "items")?;
                for (i, entry) in given.iter().enumerate() {
                    if entry
                        .get("options")
                        .is_some_and(|options| !options.is_array())
                    {
                        return Err(CliError::usage(format!(
                            "item {}: options must be an array",
                            i + 1
                        )));
                    }
                }
                let q = ctx.api.get(&path(id), vec![]).await?;
                // The revision that the answers are for: a change of the
                // agent meanwhile is a conflict.
                let body = json!({"items": answers_value(given, &q)?, "revision": q["revision"]});
                let v = ctx
                    .api
                    .post(&format!("{}/answer", path(id)), Some(body))
                    .await?;
                return ctx.emit(&v, |c, v| {
                    c.line(format!(
                        "bundle {} answered: {} answers went to {} in one mail.",
                        field(v, "id"),
                        as_items(&v["items"]).len(),
                        field(v, "agent_name")
                    ));
                });
            }
            if text.is_none() && option.is_none() && selected.is_empty() && !select_none {
                return Err(CliError::usage(
                    "give the answer as text, --option <label|n>, or both; answer a bundle \
                     with --answers-file",
                ));
            }
            let q = if !selected.is_empty() {
                ctx.api.get(&path(id), vec![]).await?
            } else {
                Value::Null
            };
            let mut body = json!({});
            if *select_none || !selected.is_empty() {
                body["options"] = json!(
                    selected
                        .iter()
                        .map(|o| option_ref(&q, o))
                        .collect::<anyhow::Result<Vec<_>>>()?
                );
            }
            if let Some(t) = text {
                body["answer"] = json!(ctx.text(t)?);
            }
            if let Some(o) = option {
                let q = ctx.api.get(&path(id), vec![]).await?;
                body["option"] = option_ref(&q, o)?;
            }
            let v = ctx
                .api
                .post(&format!("{}/answer", path(id)), Some(body))
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "question {} answered; the answer went to {} by mail.",
                    field(v, "id"),
                    field(v, "agent_name")
                ));
            })
        }
        AdminQuestionCmd::Update { id, card, no_card } => {
            let mut body = json!({});
            card_value(&mut body, card, *no_card);
            let v = ctx.api.patch(&path(id), body).await?;
            ctx.emit(&v, |c, v| match v["card"].as_str() {
                Some(card) => c.line(format!(
                    "{} {} linked to {card}.",
                    kind_of(v),
                    field(v, "id")
                )),
                None => c.line(format!(
                    "{} {} has no card now.",
                    kind_of(v),
                    field(v, "id")
                )),
            })
        }
        AdminQuestionCmd::Retract { id, reason } => retract(ctx, id, reason).await,
        AdminQuestionCmd::ToThread { id, message } => to_thread(ctx, id, message).await,
    }
}

/// `--card <id>` as `card`, `--no-card` as `card: null`.
fn card_value(body: &mut Value, card: &Option<String>, no_card: bool) {
    if no_card {
        body["card"] = Value::Null;
    } else {
        super::set_opt(body, "card", card.clone());
    }
}

/// `--urgent` or `--critical` (clap lets only one through) as `priority`.
pub(super) fn set_priority(body: &mut Value, urgent: bool, critical: bool) {
    if critical {
        body["priority"] = json!("critical");
    } else if urgent {
        body["priority"] = json!("urgent");
    }
}

/// `terrarium notify`: a leader sends a notice to the operator.
pub async fn notify(ctx: &Ctx, a: &NotifyArgs) -> anyhow::Result<()> {
    if a.card.is_none() && !a.critical {
        return Err(CliError::usage(
            "notify requires --card unless --critical: otherwise post in an open thread on the topic with `terrarium thread post`",
        ));
    }
    if !ctx.is_agent() {
        return Err(CliError::usage(
            "an agent sends notices; the operator acknowledges them with `terrarium admin \
             inbox ack`",
        ));
    }
    let me = ctx.me().await?;
    let mut body = json!({"kind": "notice", "text": ctx.text(&a.text)?});
    super::set_opt(&mut body, "context", context_value(ctx, &a.context)?);
    super::set_opt(&mut body, "card", a.card.clone());
    super::set_opt(&mut body, "reply_to", a.reply_to.clone());
    set_priority(&mut body, a.urgent, a.critical);
    if a.wait {
        body["wait"] = json!(true);
    }
    super::set_opt(&mut body, "timeout", a.timeout.clone());
    let v = ctx
        .api
        .post(&format!("{}/questions", agent_path(&me)), Some(body))
        .await?;
    ctx.emit(&v, |c, v| {
        c.line(format!(
            "notice {} sent ({}). The acknowledgement comes as mail on thread {}.",
            field(v, "id"),
            field(v, "priority"),
            field(v, "thread_id")
        ));
        if v["wait"].is_object() {
            c.line(format!(
                "wait {} registered: {}",
                field(&v["wait"], "id"),
                field(&v["wait"], "summary")
            ));
            c.line("End your turn now. The acknowledgement wakes you; do not poll.");
        } else {
            c.line("Go on with your work. Do not send it again.");
        }
    })
}

const INBOX_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("KIND", "kind"),
    ("STATUS", "status"),
    ("URGENT", "urgent"),
    ("PROJECT", "project"),
    ("FROM", "from"),
    ("AT", "at"),
    ("WHAT", "short"),
];

/// The rows of the Inbox table: one line for each item.
fn inbox_rows(v: &Value) -> Vec<Value> {
    as_items(v)
        .iter()
        .map(|i| {
            let mut row = json!({
                "id": i["id"], "kind": i["kind"],
                "urgent": if i["critical"] == true { json!("critical") } else { i["urgent"].clone() },
                "project": i["project"], "at": i["at"],
                "status": if i["open"] == true { "open" } else { "done" },
            });
            match i["kind"].as_str() {
                Some("question" | "notice") => {
                    let q = &i["question"];
                    row["from"] = q["agent_name"].clone();
                    row["status"] = q["status"].clone();
                    row["short"] = json!(short(q["text"].as_str().unwrap_or_default()));
                }
                Some("mail") => {
                    let m = &i["message"];
                    row["from"] = m["from_name"].clone();
                    row["short"] = json!(short(m["subject"].as_str().unwrap_or_default()));
                }
                Some("system") => {
                    let s = &i["system"];
                    row["from"] = json!("system");
                    let held = as_items(&s["held"]).len();
                    let paused = if s["paused"] == true {
                        "paused"
                    } else {
                        "not paused"
                    };
                    row["short"] = json!(format!(
                        "thread {} {paused}, {held} held",
                        s["thread_id"].as_str().unwrap_or_default()
                    ));
                }
                _ => {}
            }
            row
        })
        .collect()
}

/// The question or message ID of an Inbox ID: `notice:<id>` gives
/// `(Some("notice"), id)`; a bare ID gives `(None, id)`.
fn item_ref(id: &str) -> (Option<&str>, &str) {
    match id.split_once(':') {
        Some((k @ ("question" | "bundle" | "notice" | "mail" | "thread"), rest)) => (Some(k), rest),
        _ => (None, id),
    }
}

/// `terrarium admin inbox ...`.
pub async fn inbox(ctx: &Ctx, cmd: &AdminInboxCmd) -> anyhow::Result<()> {
    match cmd {
        AdminInboxCmd::Ls { all, done, kind } => {
            let status = if *all {
                Some("all".to_owned())
            } else if *done {
                Some("done".to_owned())
            } else {
                None
            };
            let q = query([
                ("status", status),
                ("kind", kind.clone()),
                ("project", ctx.project.clone()),
            ]);
            let v = ctx.api.get("/api/inbox", q).await?;
            ctx.emit(&v, |c, v| {
                print_table(c, &inbox_rows(v), INBOX_COLS, "nothing waits for you")
            })
        }
        AdminInboxCmd::Ack { id, text } => {
            let text = text.as_deref().map(|t| ctx.text(t)).transpose()?;
            let (kind, key) = item_ref(id);
            let notice = match kind {
                Some("notice") => true,
                Some("mail") => false,
                Some(other) => {
                    return Err(CliError::usage(format!(
                        "`{other}` items have no ack: answer a question or a bundle with \
                         `terrarium admin question answer`, resume a thread with `terrarium \
                         admin inbox resume`"
                    )));
                }
                // A bare ID: a notice when it is a question ID, else a mail.
                None => match ctx.api.get(&path(key), vec![]).await {
                    Ok(q) if q["kind"] == "notice" => true,
                    Ok(q) => {
                        return Err(CliError::usage(format!(
                            "{key} is a {}: answer it with `terrarium admin question answer \
                             {key}`",
                            kind_of(&q)
                        )));
                    }
                    Err(e)
                        if e.downcast_ref::<crate::api::ServerError>()
                            .is_some_and(|s| s.status == 404) =>
                    {
                        false
                    }
                    Err(e) => return Err(e),
                },
            };
            if notice {
                let mut body = json!({});
                super::set_opt(&mut body, "text", text);
                let v = ctx
                    .api
                    .post(&format!("{}/ack", path(key)), Some(body))
                    .await?;
                ctx.emit(&v, |c, v| {
                    c.line(format!(
                        "notice {} acknowledged; {} gets a mail.",
                        field(v, "id"),
                        field(v, "agent_name")
                    ));
                })
            } else {
                let mut body = json!({});
                super::set_opt(&mut body, "reply", text);
                let v = ctx
                    .api
                    .post(&format!("/api/inbox/mail/{}/ack", encode(key)), Some(body))
                    .await?;
                ctx.emit(&v, |c, v| {
                    let n = as_items(&v["replies"]).len();
                    let m = &v["message"];
                    if n == 0 {
                        c.line(format!("mail {} acknowledged.", field(m, "id")));
                    } else {
                        c.line(format!(
                            "mail {} acknowledged; the reply went on thread {}.",
                            field(m, "id"),
                            field(m, "thread_id")
                        ));
                    }
                })
            }
        }
        AdminInboxCmd::Release { id } => {
            let (_, key) = item_ref(id);
            let v = ctx
                .api
                .post(&format!("/api/mail/{}/release", encode(key)), None)
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "message {} released; {} gets it now.",
                    field(v, "id"),
                    field(v, "to_name")
                ));
            })
        }
        AdminInboxCmd::Resume { thread, note } => {
            let (_, key) = item_ref(thread);
            let mut body = json!({});
            super::set_opt(
                &mut body,
                "note",
                note.as_deref().map(|n| ctx.text(n)).transpose()?,
            );
            let v = ctx
                .api
                .post(
                    &format!("/api/mail/threads/{}/resume", encode(key)),
                    Some(body),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "thread {} resumed: {} held messages released, {} notes sent.",
                    field(v, "thread_id"),
                    as_items(&v["released"]).len(),
                    as_items(&v["note"]).len()
                ));
            })
        }
    }
}
