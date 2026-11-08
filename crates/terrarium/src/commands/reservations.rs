//! Resource reservations. The daemon owns admission and authorization.
use crate::{cli::ReserveCmd, ctx::Ctx, transport::encode};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use terrarium_core::{limits::TargetLimits, resources::IoAmounts};

fn device_map(value: &Option<String>) -> anyhow::Result<BTreeMap<String, IoAmounts>> {
    Ok(match value {
        Some(s) => serde_json::from_str(s)?,
        None => BTreeMap::new(),
    })
}

pub async fn run(ctx: &Ctx, cmd: &ReserveCmd) -> anyhow::Result<()> {
    let result = match cmd {
        ReserveCmd::New {
            host,
            target,
            agent,
            cpu,
            memory,
            io,
            until,
            exclusive,
            reason,
            queue,
            queue_timeout,
        } => {
            let limits: TargetLimits = serde_json::from_value(json!({"cpu":cpu,"memory":memory}))?;
            let limits = limits.resolve().map_err(crate::api::CliError::usage)?;
            let amounts = terrarium_core::resources::Amounts {
                cpu_millis: u64::from(limits.cpu_millis.unwrap_or_default()),
                memory_bytes: limits.memory_bytes.unwrap_or_default(),
                io: device_map(io)?,
            };
            amounts.validate().map_err(crate::api::CliError::usage)?;
            ctx.api.post(&format!("/api/hosts/{}/reservations", encode(host)), Some(json!({
                "target":target,"agent":agent,"amounts":amounts,"until":until,
                "exclusive":exclusive,"reason":reason,"queue":queue,"queue_timeout":queue_timeout
            }))).await?
        }
        ReserveCmd::Extend { id, until } => {
            ctx.api
                .patch(
                    &format!("/api/reservations/{}", encode(id)),
                    json!({"until":until}),
                )
                .await?
        }
        ReserveCmd::Retract { id } => {
            ctx.api
                .delete(&format!("/api/reservations/{}", encode(id)), vec![])
                .await?
        }
        ReserveCmd::Cancel { id } => {
            ctx.api
                .post(
                    &format!("/api/reservations/{}/cancel", encode(id)),
                    Some(json!({})),
                )
                .await?
        }
        ReserveCmd::Ls { host } => {
            ctx.api
                .get(&format!("/api/hosts/{}/reservations", encode(host)), vec![])
                .await?
        }
        ReserveCmd::Capacity {
            host,
            percentage,
            io,
        } => {
            let path = format!("/api/hosts/{}/capacity", encode(host));
            if let Some(percentage) = percentage {
                ctx.api
                    .put(&path, json!({"percentage":percentage,"io":device_map(io)?}))
                    .await?;
            }
            ctx.api.get(&path, vec![]).await?
        }
    };
    // A structured result retains amounts, holder, timestamps, and blockers
    // without losing fields to a narrow terminal table.
    ctx.line(serde_json::to_string_pretty(&result)?);
    if !ctx.json && result["state"] == Value::String("queued".into()) {
        ctx.line(format!(
            "Wait for admission with: terrarium wait reservation {}",
            result["id"].as_str().unwrap_or_default()
        ));
    }
    Ok(())
}
