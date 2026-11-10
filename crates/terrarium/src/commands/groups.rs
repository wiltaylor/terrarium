//! Explicit version group actions for the lead. No Git operations.
use serde_json::{Value, json};

use crate::{cli::GroupCmd, ctx::Ctx, transport::encode};

pub async fn group(ctx: &Ctx, cmd: &GroupCmd) -> anyhow::Result<()> {
    let project = ctx.project_or(None).await?;
    let calls: Vec<(&String, &str, Value)> = match cmd {
        GroupCmd::Status {
            key,
            step,
            state,
            text,
            eta,
        } => vec![(
            key,
            "status",
            json!({"step":step,"state":state,"text":text,"eta":eta}),
        )],
        // Several groups can hold the cards of one version. Built is
        // idempotent, so a run that stops at a failed key can be repeated.
        GroupCmd::Built { keys_and_version } => {
            let (version, keys) = keys_and_version
                .split_last()
                .ok_or_else(|| anyhow::anyhow!("give one or more keys, then the version"))?;
            keys.iter()
                .map(|key| (key, "built", json!({"version":version})))
                .collect()
        }
        GroupCmd::Remove { key, card } => vec![(key, "remove", json!({"card":card}))],
        GroupCmd::Add { key, card } => vec![(key, "add", json!({"card":card}))],
    };
    let many = calls.len() > 1;
    let mut results = Vec::new();
    for (key, action, body) in calls {
        let path = format!(
            "/api/projects/{}/version-groups/{}/{action}",
            encode(&project),
            encode(key)
        );
        let result = ctx.api.post(&path, Some(body)).await?;
        if many && ctx.json {
            results.push(result);
            continue;
        }
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
            // `built` also moves accepted cards labelled with the version.
            if let Some(ids) = v["also_done"].as_array().filter(|a| !a.is_empty()) {
                let ids: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
                ctx.line(format!("also done by version label: {}", ids.join(", ")));
            }
        })?;
    }
    if many && ctx.json {
        ctx.emit(&Value::Array(results), |_, _| {})?;
    }
    Ok(())
}
