//! `tool`: the tool proxies (GitHub #21, docs/api.md §17.11). P1a: list,
//! show, grant, and revoke. A definition comes from `tools/<name>.toml` or
//! `POST /api/tools`; the run path comes later.

use anyhow::Context as _;
use base64::Engine as _;
use serde_json::{Value, json};

use super::remote::{self, B64};
use super::secrets::grantee;
use super::{print_object, print_table};
use crate::api::{CliError, ServerError, query};
use crate::cli::ToolCmd;
use crate::ctx::Ctx;
use crate::transport::encode;

fn path(name: &str) -> String {
    format!("/api/tools/{}", encode(name))
}

/// One row of `tool ls`: the command as one line, and the grant count.
fn ls_row(mut t: Value) -> Value {
    let cmd = t["command"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    t["cmd"] = cmd.into();
    t["owner"] = t["project"].as_str().unwrap_or("global").into();
    if let Some(g) = t["grants"].as_array() {
        t["granted"] = g
            .iter()
            .filter_map(|x| x["grantee_label"].as_str())
            .collect::<Vec<_>>()
            .join(", ")
            .into();
    }
    t
}

pub async fn tool(ctx: &Ctx, cmd: &ToolCmd) -> anyhow::Result<()> {
    match cmd {
        ToolCmd::Ls => {
            let v = ctx.api.get("/api/tools", vec![]).await?;
            ctx.emit(&v, |c, v| {
                let rows: Vec<Value> = v
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(ls_row)
                    .collect();
                print_table(
                    c,
                    &rows,
                    &[
                        ("NAME", "name"),
                        ("TARGET", "target"),
                        ("OWNER", "owner"),
                        ("SOURCE", "source"),
                        ("GRANTED", "granted"),
                        ("COMMAND", "cmd"),
                    ],
                    "no tools",
                );
            })
        }
        ToolCmd::Show { name } => {
            let v = ctx.api.get(&path(name), vec![]).await?;
            ctx.emit(&v, print_object)
        }
        ToolCmd::Grant { name, grantee: who } => {
            let g = grantee(who);
            let v = ctx
                .api
                .post(
                    &format!("{}/grants", path(name)),
                    Some(json!({"grantee": g})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "granted tool {name} to {}",
                    v["grantee_label"].as_str().unwrap_or(&g)
                ));
            })
        }
        ToolCmd::Revoke { name, grantee: who } => {
            let g = grantee(who);
            ctx.api
                .delete(
                    &format!("{}/grants", path(name)),
                    query([("grantee", Some(g.clone()))]),
                )
                .await?;
            ctx.emit(&json!({"revoked": name, "grantee": g}), |c, _| {
                c.line(format!("revoked tool {name} from {g}"));
            })
        }
        ToolCmd::Run { name, args } => run(ctx, name, args).await,
    }
}

/// The exit code of a run that the operator refused, or whose approval was
/// retracted or timed out (design Q5).
const EXIT_REFUSED: u8 = 77;

/// Wait until the operator answers the approval question `q` of a run of
/// tool `name`. `Ok` when it is approved. A refuse, a retract, or a timeout
/// prints the reason and exits [`EXIT_REFUSED`]. Ctrl-C retracts the
/// question and exits 130.
async fn wait_approval(
    ctx: &Ctx,
    name: &str,
    q: &str,
    expires_at: Option<&str>,
) -> anyhow::Result<()> {
    eprintln!(
        "terrarium: tool {name} waits for the operator's approval (question {q}{})",
        expires_at
            .map(|e| format!(", until {e}"))
            .unwrap_or_default()
    );
    let qpath = format!("/api/questions/{}", encode(q));
    let wait = async {
        // The first poll after `poll_floor` (2 s; 0 in the tests), then up
        // to 5 s between polls.
        let mut pause = ctx.poll_floor.min(std::time::Duration::from_millis(500));
        loop {
            let v = ctx.api.get(&qpath, vec![]).await?;
            match v["status"].as_str().unwrap_or("") {
                "open" => {}
                "answered" if v["answer_option"] == 0 => return Ok::<_, anyhow::Error>(true),
                "answered" => {
                    eprintln!("terrarium: the operator refused this run of tool {name}");
                    return Ok(false);
                }
                _ => {
                    let why = v["retract_reason"].as_str().unwrap_or("it was retracted");
                    eprintln!("terrarium: the approval of tool {name} ended: {why}");
                    return Ok(false);
                }
            }
            if !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
            pause = (pause * 2).min(std::time::Duration::from_secs(5));
        }
    };
    let ok = tokio::select! {
        r = wait => r?,
        _ = tokio::signal::ctrl_c() => {
            let _ = ctx
                .api
                .post(&format!("{qpath}/retract"), Some(json!({"reason": "the agent stopped waiting"})))
                .await;
            eprintln!("terrarium: stopped waiting; the approval question {q} is retracted");
            return Err(CliError::RemoteExit(130).into());
        }
    };
    if ok {
        Ok(())
    } else {
        Err(CliError::RemoteExit(EXIT_REFUSED).into())
    }
}

/// A 409 (the run ended) or a 422 (the tool takes no stdin) on an input
/// call: stop sending, and read the end from the output.
fn input_refused(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ServerError>()
        .is_some_and(|s| s.status == 409 || s.status == 422)
}

/// `terrarium tool run <name> -- <args>` (and a tool link, design D3): the
/// daemon runs the tool on its target (`POST /api/tools/{name}/run`), the
/// output streams to stdout and stderr (`/api/tools/sessions/{id}/output`),
/// and the CLI exits with the remote exit code. Stdin goes to a tool that
/// takes it (`stdin = true`), when stdin is not a terminal. Ctrl-C ends the
/// run (`DELETE /api/tools/sessions/{id}`) and exits 130.
async fn run(ctx: &Ctx, name: &str, args: &[String]) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    let wants_stdin =
        !std::io::stdin().is_terminal() && ctx.api.get(&path(name), vec![]).await?["stdin"] == true;
    let mut r = ctx
        .api
        .post(&format!("{}/run", path(name)), Some(json!({"argv": args})))
        .await?;
    // A tool that asks the operator first answers 202 {pending, question}
    // (design D5): wait for the answer, then run with the approval.
    if r["pending"] == true {
        let q = r["question"]
            .as_str()
            .context("the daemon answered `pending` without `question`")?
            .to_owned();
        wait_approval(ctx, name, &q, r["expires_at"].as_str()).await?;
        r = match ctx
            .api
            .post(
                &format!("{}/run", path(name)),
                Some(json!({"argv": args, "approval": q})),
            )
            .await
        {
            Ok(v) => v,
            Err(e)
                if e.downcast_ref::<ServerError>()
                    .is_some_and(|s| s.status == 409) =>
            {
                eprintln!("terrarium: tool {name} was not run: {e}");
                return Err(CliError::RemoteExit(EXIT_REFUSED).into());
            }
            Err(e) => return Err(e),
        };
    }
    let sid = r["session_id"]
        .as_str()
        .context("the daemon answered without `session_id`")?
        .to_owned();
    let session = format!("/api/tools/sessions/{}", encode(&sid));
    if wants_stdin {
        let input = ctx.text("-")?;
        let mut open = true;
        for chunk in input.as_bytes().chunks(remote::CHUNK) {
            match remote::send_at(ctx, &session, json!({"data": B64.encode(chunk)})).await {
                Ok(()) => {}
                Err(e) if input_refused(&e) => {
                    open = false;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if open {
            match remote::send_at(ctx, &session, json!({"eof": true})).await {
                Err(e) if !input_refused(&e) => return Err(e),
                _ => {}
            }
        }
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let output = async {
        let mut since = 0;
        loop {
            let p = remote::poll_at(ctx, &session, since).await?;
            for (stream, bytes) in &p.chunks {
                let is_err = stream == "stderr";
                if ctx.json {
                    if is_err { &mut err } else { &mut out }.extend_from_slice(bytes);
                } else {
                    ctx.out.write_bytes(is_err, bytes);
                }
            }
            since = p.next;
            if let Some(end) = p.end {
                return Ok::<_, anyhow::Error>(end);
            }
        }
    };
    let end = tokio::select! {
        r = output => r?,
        _ = tokio::signal::ctrl_c() => {
            let _ = ctx.api.delete(&session, vec![]).await;
            eprintln!("terrarium: the run of tool {name} was stopped");
            return Err(CliError::RemoteExit(130).into());
        }
    };
    if ctx.json {
        ctx.line(serde_json::to_string_pretty(&json!({
            "session_id": sid,
            "tool": name,
            "stdout": String::from_utf8_lossy(&out),
            "stderr": String::from_utf8_lossy(&err),
            "end": end,
        }))?);
    }
    remote::exit_of(&end)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::api::exit_code_of;
    use crate::transport::mock::MockTransport;

    fn ctx(mock: &MockTransport) -> Ctx {
        Ctx::for_test(Arc::new(mock.clone()), None)
    }

    /// `run` reads the tool (for its `stdin`) only when stdin is not a
    /// terminal: queue that answer only then.
    fn tool_reply(mock: &MockTransport) {
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            mock.reply(200, json!({"name": "t", "stdin": false}));
        }
    }

    /// TER-31: an approved run polls the question, posts again with the
    /// approval and the same argv, then reads the session as before.
    #[tokio::test]
    async fn an_approved_run_posts_again_with_the_approval() {
        let mock = MockTransport::new();
        tool_reply(&mock);
        mock.reply(
            202,
            json!({"pending": true, "question": "Q1", "expires_at": "x"}),
        )
        .reply(200, json!({"id": "Q1", "status": "open"}))
        .reply(
            200,
            json!({"id": "Q1", "status": "answered", "answer_option": 0}),
        )
        .reply(
            201,
            json!({"session_id": "S1", "tool": "t", "target": "box"}),
        )
        .reply(
            200,
            json!({"since": 0, "next": 3, "done": true,
                       "chunks": [{"stream": "stdout", "data": "b2sK"}],
                       "end": {"type": "exit", "code": 0, "reason": "exited"}}),
        );
        let c = ctx(&mock);
        run(&c, "t", &["show".into(), "T-1".into()]).await.unwrap();
        let reqs = mock.requests();
        let posts: Vec<_> = reqs
            .iter()
            .filter(|r| r.path.ends_with("/run"))
            .map(|r| r.body.clone().unwrap())
            .collect();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0], json!({"argv": ["show", "T-1"]}));
        assert_eq!(posts[1], json!({"argv": ["show", "T-1"], "approval": "Q1"}));
        assert!(reqs.iter().any(|r| r.path == "/api/questions/Q1"));
    }

    /// A refuse, a retract (the timeout), or a 409 on the second post exits
    /// 77, and nothing runs.
    #[tokio::test]
    async fn a_refused_or_expired_run_exits_77() {
        for last in [
            json!({"id": "Q1", "status": "answered", "answer_option": 1}),
            json!({"id": "Q1", "status": "retracted", "retract_reason": "the approval timed out"}),
        ] {
            let mock = MockTransport::new();
            tool_reply(&mock);
            mock.reply(202, json!({"pending": true, "question": "Q1"}))
                .reply(200, last.clone());
            let e = run(&ctx(&mock), "t", &[]).await.unwrap_err();
            assert_eq!(exit_code_of(&e), 77, "{last}");
            assert_eq!(
                mock.requests()
                    .iter()
                    .filter(|r| r.path.ends_with("/run"))
                    .count(),
                1,
                "{last}"
            );
        }
        let mock = MockTransport::new();
        tool_reply(&mock);
        mock.reply(202, json!({"pending": true, "question": "Q1"}))
            .reply(200, json!({"id": "Q1", "status": "answered", "answer_option": 0}))
            .reply(
                409,
                json!({"error": {"code": "conflict", "message": "approval Q1 does not allow this run: it was used already"}}),
            );
        let e = run(&ctx(&mock), "t", &[]).await.unwrap_err();
        assert_eq!(exit_code_of(&e), 77);
    }
}
