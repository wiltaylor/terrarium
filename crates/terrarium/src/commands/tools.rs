//! `tool`: the tool proxies (GitHub #21, docs/api.md §17.11). P1a: list,
//! show, grant, and revoke. A definition comes from `tools/<name>.toml` or
//! `POST /api/tools`; the run path comes later.

use serde_json::{Value, json};

use super::secrets::grantee;
use super::{print_object, print_table};
use crate::api::query;
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
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
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
                let rows: Vec<Value> = v.as_array().cloned().unwrap_or_default().into_iter().map(ls_row).collect();
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
                .post(&format!("{}/grants", path(name)), Some(json!({"grantee": g})))
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
                .delete(&format!("{}/grants", path(name)), query([("grantee", Some(g.clone()))]))
                .await?;
            ctx.emit(&json!({"revoked": name, "grantee": g}), |c, _| {
                c.line(format!("revoked tool {name} from {g}"));
            })
        }
    }
}
