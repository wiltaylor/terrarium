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
    let r = ctx
        .api
        .post(&format!("{}/run", path(name)), Some(json!({"argv": args})))
        .await?;
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
