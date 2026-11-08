//! Resource reservations and operator-owned host capacity budgets.

use std::collections::BTreeMap;
use axum::{Json, Router, extract::{Path, State}, routing::{get, post}};
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_core::resources::IoAmounts;
use crate::{actor::Actor, error::ApiResult, policy, reservations};
use crate::store::{hosts, reservations as store};
use super::{ApiJson, Shared};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/hosts/{host}/capacity", get(capacity).put(configure))
        .route("/hosts/{host}/reservations", get(list).post(create))
        .route("/reservations/{id}", get(show).patch(extend).delete(retract))
        .route("/reservations/{id}/cancel", post(cancel))
}

async fn capacity(State(state): State<Shared>, Path(host): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(state.db.read(move |c| {
        hosts::get(c, &host)?;
        let (physical, budget, percentage) = store::capacity(c, &host)?;
        let rows = store::list(c, &host)?;
        let charged = rows.iter().filter(|r| matches!(r.state, store::State::Starting | store::State::Active | store::State::Ending));
        let (cpu, memory) = charged.fold((0_u64, 0_u64), |(cpu, memory), r| (
            cpu.saturating_add(r.amounts.cpu_millis), memory.saturating_add(r.amounts.memory_bytes)
        ));
        Ok(json!({"host":host,"physical":physical,"reservable":budget,"percentage":percentage,
            "used":{"cpu_millis":cpu,"memory_bytes":memory}}))
    }).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityBody { percentage: u8, #[serde(default)] io: BTreeMap<String, IoAmounts> }

async fn configure(State(state): State<Shared>, actor: Actor, Path(host): Path<String>, ApiJson(body): ApiJson<CapacityBody>) -> ApiResult<Json<Value>> {
    policy::sysadmin(&actor, "configure reservable host capacity")?;
    let _guard = state.executor.resource_lock.lock().await;
    state.db.write(actor.label(), move |t| store::configure(t, &host, body.percentage, body.io)).await?;
    Ok(Json(json!({"ok":true})))
}

async fn list(State(state): State<Shared>, actor: Actor, Path(host): Path<String>) -> ApiResult<Json<Vec<store::Reservation>>> {
    Ok(Json(state.db.read(move |c| {
        hosts::get(c, &host)?;
        Ok(store::list(c, &host)?.into_iter().filter(|r| policy::read_project(&actor, &r.project).is_ok()).collect())
    }).await?))
}

async fn show(State(state): State<Shared>, actor: Actor, Path(id): Path<String>) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(state.db.read(move |c| {
        let r = store::get(c, &id)?;
        policy::read_project(&actor, &r.project)?;
        Ok(r)
    }).await?))
}

async fn create(State(state): State<Shared>, actor: Actor, Path(host): Path<String>, ApiJson(body): ApiJson<reservations::NewBody>) -> ApiResult<(axum::http::StatusCode, Json<store::Reservation>)> {
    Ok((axum::http::StatusCode::CREATED, Json(reservations::grant(&state, &actor, &host, body).await?)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendBody { until: String }

async fn extend(State(state): State<Shared>, actor: Actor, Path(id): Path<String>, ApiJson(body): ApiJson<ExtendBody>) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(reservations::extend(&state, &actor, &id, &body.until).await?))
}

async fn retract(State(state): State<Shared>, actor: Actor, Path(id): Path<String>) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(reservations::retract(&state, &actor, &id).await?))
}

async fn cancel(State(state): State<Shared>, actor: Actor, Path(id): Path<String>) -> ApiResult<Json<store::Reservation>> {
    Ok(Json(reservations::cancel(&state, &actor, &id).await?))
}
