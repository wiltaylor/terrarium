//! `question` and `admin question` (docs/api.md §6.8, PLAN §12e). A
//! leader asks the operator; the answer comes as mail. The text, the
//! options, and the context of a question are the agent's own words:
//! `show` prints them between data markers.

use anyhow::Context as _;
use serde_json::{Value, json};

use super::{agent_path, delimited, print_object, print_table};
use crate::api::{CliError, query};
use crate::cli::{AdminQuestionCmd, QuestionCmd, QuestionContext};
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

const COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("STATUS", "status"),
    ("PRIORITY", "priority"),
    ("CARD", "card"),
    ("ASKED", "created_at"),
    ("QUESTION", "short"),
];

const ADMIN_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
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
            q
        })
        .collect()
}

fn context_value(ctx: &Ctx, c: &QuestionContext) -> anyhow::Result<Option<String>> {
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

/// One question: the fields, then the agent's words between markers,
/// then the answer.
fn print_question(c: &Ctx, v: &Value) {
    let mut head = v.clone();
    let mut take = |k: &str| head.as_object_mut().and_then(|o| o.remove(k));
    let text = take("text");
    let options = take("options");
    let context = take("context");
    let answer = take("answer");
    let option = take("answer_option");
    print_object(c, &head);
    if let Some(t) = text.as_ref().and_then(Value::as_str) {
        delimited(c, "QUESTION", t);
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
    if let Some(i) = option.as_ref().and_then(Value::as_u64) {
        let label = usize::try_from(i)
            .ok()
            .and_then(|i| options.get(i))
            .cloned()
            .unwrap_or_default();
        c.line(format!("chosen option: {}. {label}", i + 1));
    }
    if let Some(a) = answer.as_ref().and_then(Value::as_str) {
        c.line(format!("answer by {}:", field(v, "answered_by_name")));
        c.out.write_str(a);
        if !a.ends_with('\n') {
            c.line("");
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
            context,
            card,
            urgent,
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
            super::set_list(&mut body, "options", options);
            super::set_opt(&mut body, "context", context_value(ctx, context)?);
            super::set_opt(&mut body, "card", card.clone());
            if *urgent {
                body["priority"] = json!("urgent");
            }
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
        QuestionCmd::Ls { all, status, agent } => {
            let who = subject(ctx, agent).await?;
            let q = query([
                ("all", all.then(|| "true".to_owned())),
                ("status", status.clone()),
            ]);
            let v = ctx
                .api
                .get(&format!("{}/questions", agent_path(&who)), q)
                .await?;
            ctx.emit(&v, |c, v| {
                print_table(c, &rows(v), COLS, "no open or recently answered questions")
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
            context,
            card,
            urgent,
            normal,
        } => {
            let mut body = json!({});
            if let Some(t) = text {
                body["text"] = json!(ctx.text(t)?);
            }
            if *clear_options {
                body["options"] = json!([]);
            } else {
                super::set_list(&mut body, "options", options);
            }
            super::set_opt(&mut body, "context", context_value(ctx, context)?);
            super::set_opt(&mut body, "card", card.clone());
            if *urgent {
                body["priority"] = json!("urgent");
            } else if *normal {
                body["priority"] = json!("normal");
            }
            if body.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(CliError::usage(
                    "nothing to change: give --text, --option, --clear-options, --context, \
                     --card, --urgent, or --normal",
                ));
            }
            let v = ctx.api.patch(&path(id), body).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "question {} updated (revision {}).",
                    field(v, "id"),
                    field(v, "revision")
                ));
            })
        }
        QuestionCmd::Retract { id, reason } => retract(ctx, id, reason).await,
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
        c.line(format!("question {} retracted.", field(v, "id")));
    })
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
        AdminQuestionCmd::Answer { id, text, option } => {
            if text.is_none() && option.is_none() {
                return Err(CliError::usage(
                    "give the answer as text, --option <label|n>, or both",
                ));
            }
            let mut body = json!({});
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
        AdminQuestionCmd::Retract { id, reason } => retract(ctx, id, reason).await,
    }
}
