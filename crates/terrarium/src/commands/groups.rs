//! Explicit version group actions for the lead. No Git operations.
use serde_json::{Value, json};

use crate::{cli::GroupCmd, ctx::Ctx, transport::encode};

pub async fn group(ctx: &Ctx, cmd: &GroupCmd) -> anyhow::Result<()> {
    let project = ctx.project_or(None).await?;
    let (key, action, body) = match cmd {
        GroupCmd::Status {
            key,
            step,
            state,
            text,
            eta,
        } => (
            key,
            "status",
            json!({"step":step,"state":state,"text":text,"eta":eta}),
        ),
        GroupCmd::Built { key, version } => (key, "built", json!({"version":version})),
        GroupCmd::Remove { key, card } => (key, "remove", json!({"card":card})),
    };
    let path = format!(
        "/api/projects/{}/version-groups/{}/{action}",
        encode(&project),
        encode(key)
    );
    let result = ctx.api.post(&path, Some(body)).await?;
    ctx.emit(&result, |ctx, v: &Value| {
        ctx.line(format!(
            "group {}: {} ({})",
            v["group"]["key"].as_str().unwrap_or(key),
            action,
            if v["changed"] == true {
                "changed"
            } else {
                "unchanged"
            }
        ));
    })
}
