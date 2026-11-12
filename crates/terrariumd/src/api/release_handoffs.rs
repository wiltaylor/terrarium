//! Authenticated release handoffs. These routes never mint public URLs.
use axum::{Json, extract::{DefaultBodyLimit, Path, State}, routing::{get, post}};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};

use super::{ApiJson, ApiQuery, Shared, link_registry::{Access, RegisteredRouter}};
use crate::{actor::Actor, error::{ApiError, ApiResult}, policy, release_handoff as handoff, store::jobs::{self, Job, JobKind}};

pub fn routes() -> RegisteredRouter {
    RegisteredRouter::new()
        .route("/jobs/{id}/artifacts", get(list), &[(axum::http::Method::GET, Access::Link)])
        .route("/jobs/{id}/artifacts/{name}", get(download), &[(axum::http::Method::GET, Access::Link)])
        .route("/jobs/{id}/attachment", post(attach).layer(DefaultBodyLimit::max(2 * handoff::ATTACHMENT_LIMIT)), &[(axum::http::Method::POST, Access::Link)])
}

pub(crate) fn authorize(actor: &Actor, job: &Job) -> ApiResult<()> {
    policy::admin_project(actor, &job.project, "accessing release handoffs")?;
    if let Actor::Agent(agent) = actor {
        if agent.project != job.project {
            return Err(ApiError::forbidden("release handoffs belong to the project's own lead"));
        }
    }
    if job.kind != JobKind::Release { return Err(ApiError::invalid("handoffs require a release job")); }
    Ok(())
}

async fn list(State(state): State<Shared>, actor: Actor, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let job = state.db.read(move |c| { let job = jobs::get(c, &id)?; authorize(&actor, &job)?; Ok(job) }).await?;
    let outputs = handoff::artifacts(&state.state_dir, &job.id)?;
    let id = job.id.clone();
    let link = state.db.read(move |c| handoff::successor(c, &id)).await?;
    Ok(Json(json!({"job":job.id,"artifacts":outputs,"phase":if link.is_some(){2}else{1},"handoff":link})))
}

#[derive(Deserialize)]
pub struct DownloadQuery { #[serde(default)] offset: u64 }

async fn download(State(state): State<Shared>, actor: Actor, Path((id, name)): Path<(String, String)>, ApiQuery(query): ApiQuery<DownloadQuery>) -> ApiResult<Json<Value>> {
    let job = state.db.read(move |c| { let job = jobs::get(c, &id)?; authorize(&actor, &job)?; Ok(job) }).await?;
    let path = handoff::output_path(&state.state_dir, &job.id, &name)?;
    let mut file = std::fs::File::open(path).map_err(|e| ApiError::internal(e.to_string()))?;
    let size = file.metadata().map_err(|e| ApiError::internal(e.to_string()))?.len();
    if query.offset > size { return Err(ApiError::invalid("artifact offset exceeds size")); }
    file.seek(SeekFrom::Start(query.offset)).map_err(|e| ApiError::internal(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(handoff::CHUNK_SIZE as u64).read_to_end(&mut bytes).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({"offset":query.offset,"size":size,"data":base64::engine::general_purpose::STANDARD.encode(&bytes),"next_offset":query.offset + bytes.len() as u64})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachBody { data: String }

async fn attach(State(state): State<Shared>, actor: Actor, Path(id): Path<String>, ApiJson(body): ApiJson<AttachBody>) -> ApiResult<Json<Value>> {
    if body.data.len() > handoff::ATTACHMENT_LIMIT.div_ceil(3) * 4 {
        return Err(ApiError::invalid("release attachment exceeds 1 MiB"));
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(body.data).map_err(|_| ApiError::invalid("attachment data is not base64"))?;
    let dir = state.state_dir.clone();
    let result = state.db.write(actor.label(), move |t| {
        let job = jobs::get(t, &id)?;
        authorize(&actor, &job)?;
        handoff::check_predecessor(t, &job)?;
        let artifact = handoff::attachment(&dir, &id, &bytes)?;
        t.execute("INSERT INTO release_attachments(job,size,sha256) VALUES (?1,?2,?3) ON CONFLICT(job) DO UPDATE SET size=excluded.size, sha256=excluded.sha256", rusqlite::params![id, artifact.size, artifact.sha256])?;
        Ok(artifact)
    }).await?;
    Ok(Json(json!({"attachment":result})))
}
