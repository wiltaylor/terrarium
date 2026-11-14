//! Services C5 (TER-1268, design D6): fetch the result of an ended request.
//!
//! | Route | What |
//! |---|---|
//! | `GET /api/service-requests/{id}/result` | the state, exit code, error, log tail, response JSON, and the list of response files with their sizes and sha256 |
//! | `GET /api/service-requests/{id}/result/{*path}` | one response file: the bytes (`application/octet-stream`), or with `?format=json` one chunk of at most 8 MiB as `data_base64` from `?offset=` |
//!
//! Who: the caller of the request or an admin of its service, as for the
//! other request routes (`api::services`); any other actor gets `404`. A
//! request that has not ended is `409`. The files are kept 7 days from the
//! end; after that the list is empty, `files_deleted_at` is set, and a
//! file is `404`.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use super::card_files::FILE_CSP;
use super::{ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::service_result as result;
use crate::store::services::{self as store, RequestRow};

/// The largest chunk of `?format=json`.
pub const CHUNK: u64 = 8 * 1024 * 1024;

pub fn routes() -> super::link_registry::RegisteredRouter {
    use super::link_registry::Access;
    use axum::http::Method;
    super::link_registry::RegisteredRouter::new()
        .route(
            "/service-requests/{id}/result",
            get(show),
            &[(Method::GET, Access::Link)],
        )
        .route(
            "/service-requests/{id}/result/{*path}",
            get(file),
            &[(Method::GET, Access::Link)],
        )
}

/// The ended request `id` that the actor may see.
async fn ended(state: &Shared, actor: &Actor, id: &str) -> ApiResult<RequestRow> {
    let r = super::services::visible_request(state, actor, id).await?;
    if !r.state.is_final() {
        return Err(ApiError::conflict(format!(
            "service request `{id}` is {}: it has no result until it ends",
            r.state
        )));
    }
    Ok(r)
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let r = ended(&state, &actor, &id).await?;
    let (files, cut) = if r.payload_deleted_at.is_some() {
        (Vec::new(), false)
    } else {
        let dir = store::response_dir(&state.state_dir, &id);
        tokio::task::spawn_blocking(move || result::files(&dir, true))
            .await
            .map_err(|e| ApiError::internal(format!("the list task failed: {e}")))?
            .map_err(|e| ApiError::internal(format!("cannot list the response files: {e}")))?
    };
    Ok(Json(json!({
        "id": r.id,
        "service": r.service,
        "state": r.state,
        "exit_code": r.exit_code,
        "error": r.error,
        "log_tail": r.log_tail,
        "response": r.response,
        "started_at": r.started_at,
        "ended_at": r.ended_at,
        "files_deleted_at": r.payload_deleted_at,
        "files": files,
        "files_cut": cut,
    })))
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    format: Option<String>,
    offset: Option<u64>,
    length: Option<u64>,
}

async fn file(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, path)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<FileQuery>,
) -> ApiResult<Response> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    let json = match q.format.as_deref() {
        None | Some("raw") => false,
        Some("json") => true,
        Some(other) => {
            return Err(ApiError::invalid(format!(
                "bad `format` `{other}`: use raw or json"
            )));
        }
    };
    let r = ended(&state, &actor, &id).await?;
    if r.payload_deleted_at.is_some() {
        return Err(ApiError::not_found(format!(
            "the result of service request `{id}` is past its retention"
        )));
    }
    let dir = store::response_dir(&state.state_dir, &id);
    let rel = path.clone();
    let found = tokio::task::spawn_blocking(move || result::resolve(&dir, &rel))
        .await
        .map_err(|e| ApiError::internal(format!("the lookup task failed: {e}")))?;
    let Some(p) = found else {
        return Err(ApiError::not_found(format!(
            "service request `{id}` has no response file `{path}`"
        )));
    };
    let mut f = tokio::fs::File::open(&p)
        .await
        .map_err(|e| ApiError::internal(format!("cannot open the response file: {e}")))?;
    let size = f
        .metadata()
        .await
        .map_err(|e| ApiError::internal(format!("cannot read the response file: {e}")))?
        .len();
    let offset = q.offset.unwrap_or(0);
    if offset > size {
        return Err(ApiError::invalid(format!(
            "`offset` {offset} is past the end of `{path}` ({size} bytes)"
        )));
    }
    f.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| ApiError::internal(format!("cannot read the response file: {e}")))?;
    if json {
        let want = q.length.unwrap_or(CHUNK).clamp(1, CHUNK).min(size - offset);
        let mut buf = vec![0u8; usize::try_from(want).unwrap_or(0)];
        f.read_exact(&mut buf)
            .await
            .map_err(|e| ApiError::internal(format!("cannot read the response file: {e}")))?;
        return Ok(Json(json!({
            "path": path,
            "size": size,
            "offset": offset,
            "length": want,
            "eof": offset + want >= size,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&buf),
        }))
        .into_response());
    }
    let stream = futures_util::stream::unfold(Some(f), |file| async move {
        let mut file = file?;
        let mut buf = vec![0u8; 64 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    let mut resp = Body::from_stream(stream).into_response();
    let h = resp.headers_mut();
    let set = |h: &mut HeaderMap, k: header::HeaderName, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(k, v);
        }
    };
    set(h, header::CONTENT_TYPE, "application/octet-stream");
    set(h, header::CONTENT_LENGTH, &(size - offset).to_string());
    set(h, header::CONTENT_DISPOSITION, "attachment");
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::CONTENT_SECURITY_POLICY, FILE_CSP);
    Ok(resp)
}
