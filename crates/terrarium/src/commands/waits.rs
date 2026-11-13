//! `wait`, `run`, and `watch` (docs/api.md §6.7, PLAN §12d). None of them
//! blocks: each registers, prints what it did, and returns, so that the
//! agent ends its turn. The daemon mails the agent when the wait ends.

use serde_json::{Value, json};

use super::{agent_path, print_object, print_table};
use crate::api::{CliError, query};
use crate::cli::{RunArgs, RunCmd, WaitCmd, WaitOpts, WaiterArg, WatchArgs, WatchCmd};
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

const WAIT_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("KIND", "kind"),
    ("STATE", "state"),
    ("SUMMARY", "summary"),
    ("CREATED", "created_at"),
    ("TIMEOUT", "timeout_at"),
];

const WATCH_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("AGENT", "agent_name"),
    ("ON", "on"),
    ("ONCE", "once"),
    ("WATCHER", "watcher_name"),
    ("FIRED", "fired"),
];

/// The line that tells an agent what to do next.
const END_TURN: &str = "End your turn now. The daemon mails you when the wait ends; do not poll.";

/// The agent that waits: the caller in agent mode, `--waiter` for the
/// operator.
async fn waiter(ctx: &Ctx, w: &WaiterArg) -> anyhow::Result<String> {
    match (&w.waiter, ctx.is_agent()) {
        (Some(_), true) => Err(CliError::usage(
            "--waiter is for the operator; an agent waits for itself",
        )),
        (Some(a), false) => Ok(a.clone()),
        (None, true) => ctx.me().await,
        (None, false) => Err(CliError::usage(
            "the operator names the agent that waits: --waiter <agent>",
        )),
    }
}

/// The agent whose list to show: `--agent`, else the caller (an agent).
async fn subject(ctx: &Ctx, agent: &Option<String>) -> anyhow::Result<String> {
    match agent {
        Some(a) => Ok(a.clone()),
        None if ctx.is_agent() => ctx.me().await,
        None => Err(CliError::usage(
            "the operator names the agent: --agent <agent>",
        )),
    }
}

fn opts_into(body: &mut Value, o: &WaitOpts) {
    if let Some(t) = &o.timeout {
        body["timeout"] = json!(t);
    }
    if let Some(r) = &o.reason {
        body["reason"] = json!(r);
    }
}

fn registered(c: &Ctx, v: &Value) {
    c.line(format!(
        "wait {} registered: {}",
        field(v, "id"),
        field(v, "summary")
    ));
    if v["timeout_at"].is_string() {
        c.line(format!("It times out at {}.", field(v, "timeout_at")));
    }
    c.line(END_TURN);
}

pub async fn wait(ctx: &Ctx, cmd: &WaitCmd) -> anyhow::Result<()> {
    let (who, body) = match cmd {
        WaitCmd::Agent { agent, until, opts } => {
            let mut b = json!({"kind": "agent", "agent": agent, "until": until});
            opts_into(&mut b, opts);
            (waiter(ctx, &opts.waiter).await?, b)
        }
        WaitCmd::Time {
            duration,
            until,
            reason,
            waiter: w,
        } => {
            let mut b = match (duration, until) {
                (Some(d), None) => json!({"kind": "time", "for": d}),
                (None, Some(u)) => json!({"kind": "time", "at": u}),
                _ => {
                    return Err(CliError::usage(
                        "give one of --for <duration> or --until <time>",
                    ));
                }
            };
            if let Some(r) = reason {
                b["reason"] = json!(r);
            }
            (waiter(ctx, w).await?, b)
        }
        WaitCmd::Process { session, opts } => {
            let mut b = json!({"kind": "process", "session": session});
            opts_into(&mut b, opts);
            (waiter(ctx, &opts.waiter).await?, b)
        }
        WaitCmd::Card { card, column, opts } => {
            let mut b = json!({"kind": "card", "card": card, "column": column});
            opts_into(&mut b, opts);
            (waiter(ctx, &opts.waiter).await?, b)
        }
        WaitCmd::Question { id, opts } => {
            let mut b = json!({"kind": "question", "question": id});
            opts_into(&mut b, opts);
            (waiter(ctx, &opts.waiter).await?, b)
        }
        WaitCmd::Reservation { id, opts } => {
            let mut b = json!({"kind":"reservation", "reservation":id});
            opts_into(&mut b, opts);
            (waiter(ctx, &opts.waiter).await?, b)
        }
        WaitCmd::Ls { all, agent } => {
            let who = subject(ctx, agent).await?;
            let q = query([("all", all.then(|| "true".to_owned()))]);
            let v = ctx
                .api
                .get(&format!("{}/waits", agent_path(&who)), q)
                .await?;
            return ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), WAIT_COLS, "no open waits")
            });
        }
        WaitCmd::Show { id } => {
            let v = ctx
                .api
                .get(&format!("/api/waits/{}", encode(id)), vec![])
                .await?;
            return ctx.emit(&v, print_wait);
        }
        WaitCmd::Cancel { id, kill } => {
            let q = query([("kill", kill.then(|| "true".to_owned()))]);
            let v = ctx
                .api
                .delete(&format!("/api/waits/{}", encode(id)), q)
                .await?;
            if *kill {
                ctx.emit(&v, |c, v| {
                    c.line(format!("wait {}: {}", field(v, "id"), field(v, "state")));
                    c.line(format!(
                        "cleanup: {} (operation {})",
                        cleanup_status(v),
                        field(&v["cleanup"], "operation_id")
                    ));
                })?;
                if !cleanup_verified(&v) {
                    anyhow::bail!(
                        "termination is not verified (cleanup: {}); inspect the same wait operation, do not start a fresh kill",
                        cleanup_status(&v)
                    );
                }
                return Ok(());
            }
            return ctx.emit(&v, |c, v| {
                c.line(format!(
                    "wait {} cancelled: {}",
                    field(v, "id"),
                    field(v, "summary")
                ));
            });
        }
    };
    let v = ctx
        .api
        .post(&format!("{}/waits", agent_path(&who)), Some(body))
        .await?;
    ctx.emit(&v, registered)
}

/// One wait, with its result. Process output is untrusted: it prints
/// between markers.
fn print_wait(c: &Ctx, v: &Value) {
    let mut head = v.clone();
    let result = head.as_object_mut().and_then(|o| o.remove("result"));
    print_object(c, &head);
    if let Some(mut r) = result {
        for (k, label) in [("stdout_tail", "STDOUT"), ("stderr_tail", "STDERR")] {
            let text = r
                .as_object_mut()
                .and_then(|o| o.remove(k))
                .and_then(|t| t.as_str().map(str::to_owned))
                .unwrap_or_default();
            if !text.is_empty() {
                super::delimited(c, label, &text);
            }
        }
        // A bundle: one line for each item, then the rest of the result.
        let answers = r.as_object_mut().and_then(|o| o.remove("answers"));
        for a in answers.as_ref().map(as_items).unwrap_or_default() {
            let option = a["option"]
                .as_str()
                .map(|o| format!(" [option: {o}]"))
                .unwrap_or_default();
            c.line(format!(
                "answer {}{option}: {}",
                field(a, "item"),
                field(a, "answer")
            ));
        }
        c.line(format!("result  {r}"));
    }
}

pub async fn run(ctx: &Ctx, cmd: Option<&RunCmd>, a: &RunArgs) -> anyhow::Result<()> {
    match cmd {
        None => start_run(ctx, a).await,
        Some(RunCmd::Ls { all, agent }) => {
            let who = subject(ctx, agent).await?;
            let q = query([
                ("all", all.then(|| "true".to_owned())),
                ("kind", Some("process".to_owned())),
            ]);
            let v = ctx
                .api
                .get(&format!("{}/waits", agent_path(&who)), q)
                .await?;
            ctx.emit(&v, |c, v| print_table(c, as_items(v), WAIT_COLS, "no runs"))
        }
        Some(RunCmd::Log { id, bytes }) => {
            let q = query([("max", bytes.map(|b| b.to_string()))]);
            let v = ctx
                .api
                .get(&format!("/api/waits/{}/log", encode(id)), q)
                .await?;
            ctx.emit(&v, |c, v| {
                let state = if v["running"] == true {
                    "running"
                } else {
                    "ended"
                };
                c.line(format!(
                    "run {} on {} ({state})",
                    field(v, "wait_id"),
                    field(v, "target")
                ));
                super::delimited(c, "RUN OUTPUT", v["output"].as_str().unwrap_or_default());
            })
        }
        Some(RunCmd::Kill { id }) => {
            let v = ctx
                .api
                .delete(
                    &format!("/api/waits/{}", encode(id)),
                    vec![("kill".into(), "true".into())],
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("wait {}: {}", field(v, "id"), field(v, "state")));
                c.line(format!(
                    "cleanup: {} (operation {})",
                    cleanup_status(v),
                    field(&v["cleanup"], "operation_id")
                ));
            })?;
            // Emit JSON first, so callers retain the durable operation ID.
            if !cleanup_verified(&v) {
                anyhow::bail!(
                    "termination is not verified (cleanup: {}); inspect the same wait operation, do not start a fresh kill",
                    cleanup_status(&v)
                );
            }
            Ok(())
        }
    }
}

fn cleanup_status(v: &Value) -> &str {
    v["cleanup"]["status"].as_str().unwrap_or("unknown")
}

fn cleanup_verified(v: &Value) -> bool {
    cleanup_status(v) == "verified_terminated"
        && v["cleanup"]["operation_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        && v["cleanup"]["evidence_kind"].as_str() == Some("bound_complete_scope")
        && v["cleanup"]["observed_at"]
            .as_str()
            .is_some_and(|at| !at.is_empty())
}

async fn start_run(ctx: &Ctx, a: &RunArgs) -> anyhow::Result<()> {
    let Some(target) = &a.target else {
        return Err(CliError::usage(
            "name the target: terrarium run <target> -- <cmd>",
        ));
    };
    if a.cmd.is_empty() {
        return Err(CliError::usage(
            "name the command after `--`: terrarium run <target> -- <cmd>",
        ));
    }
    let who = waiter(ctx, &a.waiter).await?;
    let mut body = json!({"target": target, "cmd": a.cmd});
    super::set_opt(&mut body, "name", a.name.clone());
    super::set_opt(&mut body, "timeout", a.timeout.clone());
    super::set_opt(&mut body, "cwd", a.cwd.clone());
    super::set_opt(&mut body, "reason", a.reason.clone());
    if !a.env.is_empty() {
        let mut env = serde_json::Map::new();
        for p in &a.env {
            let (k, v) = p
                .split_once('=')
                .ok_or_else(|| CliError::usage(format!("bad --env `{p}`: use KEY=VALUE")))?;
            env.insert(k.to_owned(), json!(v));
        }
        body["env"] = Value::Object(env);
    }
    let v = ctx
        .api
        .post(&format!("{}/run", agent_path(&who)), Some(body))
        .await?;
    ctx.emit(&v, |c, v| {
        let w = &v["wait"];
        c.line(format!(
            "run started: {} (wait {}, session {})",
            field(w, "summary"),
            field(w, "id"),
            field(v, "session_id")
        ));
        c.line(format!(
            "`terrarium run log {}` shows the output so far.",
            field(w, "id")
        ));
        c.line(
            "End your turn now. The daemon mails you the exit code and the output tail \
             when the run ends; do not poll.",
        );
    })
}

pub async fn watch(ctx: &Ctx, cmd: Option<&WatchCmd>, a: &WatchArgs) -> anyhow::Result<()> {
    match cmd {
        None => {
            let Some(agent) = &a.agent else {
                return Err(CliError::usage(
                    "name the agent: terrarium watch <agent> --on ...",
                ));
            };
            let on: Vec<String> =
                a.on.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            let mut body = json!({"on": on, "once": a.once, "debounce": !a.no_debounce});
            if let Some(w) = &a.watcher {
                if ctx.is_agent() {
                    return Err(CliError::usage(
                        "--watcher is for the operator; an agent watches for itself",
                    ));
                }
                body["watcher"] = json!(w);
            }
            let v = ctx
                .api
                .post(&format!("{}/watches", agent_path(agent)), Some(body))
                .await?;
            ctx.emit(&v, |c, v| {
                let on: Vec<String> = as_items(&v["on"])
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect();
                c.line(format!(
                    "watch {} on {}: a mail goes to {} when it is {}{}.",
                    field(v, "id"),
                    field(v, "agent_name"),
                    field(v, "watcher_name"),
                    on.join(" or "),
                    if v["once"] == true { " (once)" } else { "" }
                ));
            })
        }
        Some(WatchCmd::Ls { watcher }) => {
            let q = query([("watcher", watcher.clone())]);
            let v = ctx.api.get("/api/watches", q).await?;
            ctx.emit(&v, |c, v| {
                print_table(c, as_items(v), WATCH_COLS, "no watches")
            })
        }
        Some(WatchCmd::Rm { id }) => {
            ctx.api
                .delete(&format!("/api/watches/{}", encode(id)), vec![])
                .await?;
            ctx.emit(&json!({"removed": id}), |c, _| {
                c.line(format!("removed watch {id}"));
            })
        }
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn only_complete_bound_evidence_can_allow_success() {
        for status in [
            "not_requested",
            "pending",
            "unknown",
            "unavailable",
            "remaining",
            "unrecognized",
            "verified_terminated",
        ] {
            assert!(!cleanup_verified(
                &json!({"cleanup":{"operation_id":"same-op","status":status}})
            ));
        }
        for legacy in [
            json!({}),
            json!({"kill":{"pid":null,"left":[]}}),
            json!({"kill":{"pid":1,"left":[]}}),
        ] {
            assert!(!cleanup_verified(&legacy));
            assert_eq!(cleanup_status(&legacy), "unknown");
        }
        assert!(cleanup_verified(
            &json!({"cleanup":{"operation_id":"op","status":"verified_terminated","evidence_kind":"bound_complete_scope","observed_at":"2026-11-12T00:00:00Z"}})
        ));
    }
}
