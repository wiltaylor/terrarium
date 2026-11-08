//! Resource reservations and operator-owned host capacity budgets.

use super::{ApiJson, Shared};
use crate::store::{hosts, reservations as store};
use crate::{actor::Actor, error::ApiResult, policy, reservations};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use terrarium_core::resources::IoAmounts;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/hosts/{host}/capacity", get(capacity).put(configure))
        .route("/hosts/{host}/reservations", get(list).post(create))
        .route(
            "/reservations/{id}",
            get(show).patch(extend).delete(retract),
        )
        .route("/reservations/{id}/cancel", post(cancel))
}

async fn capacity(State(state): State<Shared>, Path(host): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(state.db.read(move |c| {
        hosts::get(c, &host)?;
        let (physical, budget, percentage) = store::capacity(c, &host)?;
        let rows = store::list(c, &host)?;
        let charged = rows.iter().filter(|r| matches!(r.state, store::State::Starting | store::State::Active | store::State::Ending));
        let mut used = terrarium_core::resources::Amounts::default();
        let mut cores = std::collections::BTreeSet::new();
        let mut gpus = Vec::new();
        for r in charged {
            for gpu in &r.gpus { gpus.push(json!({"reservation":r.id,"holder":r.holder,"claim":gpu})); }
            used.cpu_millis = used.cpu_millis.saturating_add(r.amounts.cpu_millis);
            used.memory_bytes = used.memory_bytes.saturating_add(r.amounts.memory_bytes);
            cores.extend(r.cores.iter().copied());
            for (device, amount) in &r.amounts.io {
                let io = used.io.entry(device.clone()).or_default();
                io.read_bps = io.read_bps.saturating_add(amount.read_bps);
                io.write_bps = io.write_bps.saturating_add(amount.write_bps);
                io.read_iops = io.read_iops.saturating_add(amount.read_iops);
                io.write_iops = io.write_iops.saturating_add(amount.write_iops);
            }
        }
        let mut used = serde_json::to_value(used)?;
        used["exclusive_cores"] = json!(cores);
        used["gpus"] = json!(gpus);
        let inventory = hosts::get(c, &host)?.capabilities.get("gpus").cloned().unwrap_or_else(|| json!([]));
        Ok(json!({"host":host,"physical":physical,"reservable":budget,"percentage":percentage,
            "used":used,"gpus":inventory,"gpu_enforcement":terrarium_core::resources::GPU_ENFORCEMENT}))
    }).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityBody {
    percentage: u8,
    #[serde(default)]
    io: BTreeMap<String, IoAmounts>,
}

async fn configure(
    State(state): State<Shared>,
    actor: Actor,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<CapacityBody>,
) -> ApiResult<Json<Value>> {
    policy::sysadmin(&actor, "configure reservable host capacity")?;
    let _guard = state.executor.resource_lock(&host).lock_owned().await;
    state.executor.resource_verb_idle(&host)?;
    state
        .db
        .write(actor.label(), move |t| {
            store::configure(t, &host, body.percentage, body.io)
        })
        .await?;
    Ok(Json(json!({"ok":true})))
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(host): Path<String>,
) -> ApiResult<Json<Vec<store::Reservation>>> {
    Ok(Json(
        state
            .db
            .read(move |c| {
                hosts::get(c, &host)?;
                Ok(store::list(c, &host)?
                    .into_iter()
                    .filter(|r| policy::read_project(&actor, &r.project).is_ok())
                    .collect())
            })
            .await?,
    ))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(
        state
            .db
            .read(move |c| {
                let r = store::get(c, &id)?;
                policy::read_project(&actor, &r.project)?;
                Ok(r)
            })
            .await?,
    ))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<reservations::NewBody>,
) -> ApiResult<(axum::http::StatusCode, Json<store::Reservation>)> {
    Ok((
        axum::http::StatusCode::CREATED,
        Json(reservations::grant(&state, &actor, &host, body).await?),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendBody {
    until: String,
}

async fn extend(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ExtendBody>,
) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(
        reservations::extend(&state, &actor, &id, &body.until).await?,
    ))
}

async fn retract(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(reservations::retract(&state, &actor, &id).await?))
}

async fn cancel(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(reservations::cancel(&state, &actor, &id).await?))
}
