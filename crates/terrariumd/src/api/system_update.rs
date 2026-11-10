//! `/api/system/update`: the self-update (docs/api.md §34).
//!
//! The operator and a sysadmin (an agent with the `sysadmin` role too)
//! read the state, check for a release, update, and roll back. The drain
//! and its end are for the updater only: the operator (the Unix socket in
//! the container, where the updater runs `terrarium admin update drain`).

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use terrarium_updater::protocol::Request;

use super::Shared;
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::self_update::{self, DrainInfo, Report};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/system/update", get(status).post(update))
        .route("/system/update/check", post(check))
        .route("/system/update/rollback", post(rollback))
        .route("/system/update/drain", post(drain))
        .route("/system/update/resume", post(resume))
}

/// An optional JSON body: no body is the default.
fn parse_optional<T: DeserializeOwned + Default>(raw: &axum::body::Bytes) -> ApiResult<T> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(raw).map_err(|e| ApiError::bad_request(e.to_string()))
}

const WHAT: &str = "the self-update";

/// `GET /api/system/update`.
async fn status(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Report>> {
    policy::read_system(&actor, "reading the self-update")?;
    Ok(Json(self_update::report(&state).await))
}

/// `POST /api/system/update/check`: read the releases now.
async fn check(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Report>> {
    policy::sysadmin(&actor, "checking for a Terrarium release")?;
    self_update::refresh(&state, std::time::Duration::ZERO).await;
    // The new-release mail goes out now, not at the next tick.
    self_update::tick(&state, chrono::Utc::now(), false).await;
    Ok(Json(self_update::report(&state).await))
}

/// The body of `POST /api/system/update`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateBody {
    /// A version (`0.3.0`), `latest`, or `pre`. Default: the newest
    /// release of the channel.
    #[serde(default)]
    version: Option<String>,
}

/// `POST /api/system/update`.
async fn update(
    State(state): State<Shared>,
    actor: Actor,
    raw: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    policy::sysadmin(&actor, "updating Terrarium")?;
    let body: UpdateBody = parse_optional(&raw)?;
    let target = match body.version.as_deref().map(str::trim) {
        Some("") => return Err(ApiError::invalid("the version is empty")),
        Some(v) => v.to_owned(),
        None => self_update::default_target(&state),
    };
    if terrarium_updater::machine::tag_of(&target).is_none() {
        return Err(ApiError::invalid(format!(
            "`{target}` is not a version, `latest`, or `pre`"
        )));
    }
    let label = actor.label();
    let run = self_update::request(
        &state,
        Request::Update {
            target: target.clone(),
            actor: label.clone(),
        },
        &label,
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"run": run, "target": target})),
    ))
}

/// `POST /api/system/update/rollback`.
async fn rollback(
    State(state): State<Shared>,
    actor: Actor,
) -> ApiResult<(StatusCode, Json<Value>)> {
    policy::sysadmin(&actor, "rolling back Terrarium")?;
    let label = actor.label();
    let run = self_update::request(
        &state,
        Request::Rollback {
            actor: label.clone(),
        },
        &label,
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"run": run}))))
}

/// The body of `POST /api/system/update/drain`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainBody {
    /// Drain the job slots only (TER-614 R4): no job starts, the running
    /// jobs finish, and the agents go on.
    #[serde(default)]
    jobs_only: bool,
}

/// `POST /api/system/update/drain`: no new turns. The caller polls
/// `GET /api/system/update` (`drain.busy`) for the end of the running
/// turns. With `{"jobs_only": true}`: no new jobs only (`drain.jobs`).
async fn drain(
    State(state): State<Shared>,
    actor: Actor,
    raw: axum::body::Bytes,
) -> ApiResult<Json<DrainInfo>> {
    policy::operator_only(&actor, WHAT)?;
    let body: DrainBody = parse_optional(&raw)?;
    if body.jobs_only {
        self_update::start_jobs_drain(&state, &actor.label());
    } else {
        self_update::start_drain(&state, &actor.label());
    }
    let detail = json!({"jobs_only": body.jobs_only});
    let r = state
        .db
        .write(actor.label(), move |t| {
            t.audit("system.drain", "daemon", None, detail)
        })
        .await;
    if let Err(e) = r {
        tracing::warn!("cannot write the audit row of system.drain: {e}");
    }
    Ok(Json(self_update::drain_info(&state).await))
}

/// `POST /api/system/update/resume`: end the drain.
async fn resume(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<DrainInfo>> {
    policy::operator_only(&actor, WHAT)?;
    self_update::end_drain(&state).await;
    Ok(Json(self_update::drain_info(&state).await))
}
