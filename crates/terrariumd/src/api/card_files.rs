//! `/api/cards/{id}/files...` (docs/api.md §8.4): the files of a card.
//!
//! - `GET /api/cards/{id}/files`: the live files, the latest version of
//!   each.
//! - `POST /api/cards/{id}/files`: add a file, or a version with
//!   `replace`. The body is `multipart/form-data` (a `file` part, and
//!   optional `name` and `replace` parts), JSON `{"name", "data_base64",
//!   "replace"?}` (the CLI: its transports carry JSON only), or the raw
//!   bytes with `?name=` (any other `Content-Type`).
//! - `GET /api/cards/{id}/files/{name}?version=&format=`: the bytes, as a
//!   download; `format=json` answers the version with `data_base64`.
//! - `GET /api/cards/{id}/files/{name}/versions`: the versions, newest
//!   first.
//! - `DELETE /api/cards/{id}/files/{name}`: remove the file (all its
//!   versions; a soft delete).
//!
//! A download never renders on the daemon origin: `Content-Disposition:
//! attachment`, `X-Content-Type-Options: nosniff`, and a CSP with
//! `sandbox` and `default-src 'none'`. An `<img>` of the dashboard still
//! shows an image (an SVG in an `<img>` runs no script).

use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json};
use base64::Engine;
use serde::{Deserialize, Serialize};

use super::{ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::board;
use crate::store::card_files::{self, AddOutcome, CardFile, FileVersion, Limits};

/// The CSP of a downloaded card file.
pub const FILE_CSP: &str =
    "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'; frame-ancestors 'none'";

pub fn routes() -> super::link_registry::RegisteredRouter {
    let limits = Limits::from_env();
    // The largest file as base64 in JSON or in a multipart body, with room
    // for the rest. A bigger body is a `413` from the handler.
    let body_limit = usize::try_from(limits.max_file / 3 * 4 + 1024 * 1024).unwrap_or(usize::MAX);
    super::link_registry::RegisteredRouter::new()
        .route(
            "/cards/{id}/files",
            get(list)
                .post(upload)
                .layer(DefaultBodyLimit::max(body_limit)), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/cards/{id}/files/{name}", get(download).delete(remove), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::DELETE, super::link_registry::Access::Link)])
        .route("/cards/{id}/files/{name}/versions", get(versions), &[(axum::http::Method::GET, super::link_registry::Access::Link)])
}

/// The card, after the read right of its project.
async fn readable(state: &Shared, actor: &Actor, id: String) -> ApiResult<board::CardRef> {
    let card = state.db.read(move |c| board::card_ref(c, &id)).await?;
    policy::read_project(actor, &card.project)?;
    Ok(card)
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<CardFile>>> {
    let card = readable(&state, &actor, id).await?;
    Ok(Json(
        state
            .db
            .read(move |c| card_files::list(c, &card.id))
            .await?,
    ))
}

async fn versions(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<Vec<FileVersion>>> {
    let card = readable(&state, &actor, id).await?;
    Ok(Json(
        state
            .db
            .read(move |c| card_files::versions(c, &card.id, &name))
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
struct UploadQuery {
    name: Option<String>,
    replace: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonUpload {
    name: String,
    data_base64: String,
    #[serde(default)]
    replace: bool,
}

/// One upload after its body is read.
struct Upload {
    name: String,
    data: Vec<u8>,
    replace: bool,
}

fn too_large(max: u64) -> ApiError {
    card_files::too_large_file(max + 1, max)
}

fn parse_upload(
    headers: &HeaderMap,
    q: UploadQuery,
    body: Bytes,
    limits: Limits,
) -> ApiResult<Upload> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let base = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let flag = |v: &str| matches!(v.trim(), "1" | "true" | "yes" | "on");
    if base == "multipart/form-data" {
        let parts = super::artifacts::parse_multipart(ct, &body)?;
        let text = |name: &str| {
            parts
                .iter()
                .find(|p| p.name == name)
                .map(|p| String::from_utf8_lossy(&p.data).trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let file = parts
            .iter()
            .find(|p| p.name == "file")
            .ok_or_else(|| ApiError::bad_request("the multipart upload needs a `file` part"))?;
        let name = text("name")
            .or_else(|| q.name.clone())
            .or_else(|| file.filename.clone())
            .ok_or_else(|| ApiError::invalid("the upload needs a file name"))?;
        let replace = text("replace").is_some_and(|v| flag(&v)) || q.replace.unwrap_or(false);
        return Ok(Upload {
            name,
            data: file.data.clone(),
            replace,
        });
    }
    if base == "application/json" {
        let u: JsonUpload = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad upload JSON: {e}")))?;
        let b64: String = u
            .data_base64
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        if (b64.len() as u64 / 4) * 3 > limits.max_file + 3 {
            return Err(too_large(limits.max_file));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| ApiError::bad_request(format!("`data_base64` is not base64: {e}")))?;
        return Ok(Upload {
            name: u.name,
            data,
            replace: u.replace || q.replace.unwrap_or(false),
        });
    }
    let name = q.name.ok_or_else(|| {
        ApiError::bad_request(
            "a raw upload needs `?name=`; or send multipart/form-data, or JSON with `name` and `data_base64`",
        )
    })?;
    Ok(Upload {
        name,
        data: body.to_vec(),
        replace: q.replace.unwrap_or(false),
    })
}

#[derive(Debug, Serialize)]
struct Uploaded {
    #[serde(flatten)]
    file: CardFile,
    /// `added`, `replaced`, or `unchanged` (the latest version has the
    /// same bytes).
    outcome: AddOutcome,
}

async fn upload(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<UploadQuery>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<Uploaded>)> {
    let limits = Limits::from_env();
    // The rights before the body: a refused caller learns nothing more.
    let check = (actor.clone(), id.clone());
    let card = state
        .db
        .read(move |c| board::check_file_edit(c, &check.0, &check.1))
        .await?;
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_large(limits.max_file)
        } else {
            ApiError::bad_request(format!("cannot read the body: {e}"))
        }
    })?;
    let up = parse_upload(&headers, q, body, limits)?;
    let redactor = state.db.redactor().get();
    let view = crate::secrets::redact::view_of(&actor);
    let state_dir = state.state_dir.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let p = card_files::prepare(&up.name, up.data, limits, &redactor, &view)?;
        card_files::store_blob(&state_dir, &p)?;
        Ok::<_, ApiError>((p, up.replace))
    })
    .await
    .map_err(|e| ApiError::internal(format!("the upload task failed: {e}")))??;
    let (p, replace) = prepared;
    let (file, outcome) = state
        .db
        .write(actor.label(), move |t| {
            card_files::add(t, &actor, &card.id, &p, replace, limits)
        })
        .await?;
    let status = match outcome {
        AddOutcome::Unchanged => StatusCode::OK,
        _ => StatusCode::CREATED,
    };
    Ok((status, Json(Uploaded { file, outcome })))
}

#[derive(Debug, Default, Deserialize)]
struct DownloadQuery {
    version: Option<i64>,
    /// `json`: the version with `data_base64`.
    format: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonDownload {
    card: String,
    #[serde(flatten)]
    version: FileVersion,
    data_base64: String,
}

async fn download(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, name)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<DownloadQuery>,
) -> ApiResult<Response> {
    let json = match q.format.as_deref() {
        None | Some("raw") => false,
        Some("json") => true,
        Some(other) => {
            return Err(ApiError::invalid(format!(
                "bad `format` `{other}`: use raw or json"
            )));
        }
    };
    let card = readable(&state, &actor, id).await?;
    let cid = card.id.clone();
    let v = state
        .db
        .read(move |c| card_files::get(c, &cid, &name, q.version))
        .await?;
    let (dir, sha) = (state.state_dir.clone(), v.sha256.clone());
    let data = tokio::task::spawn_blocking(move || card_files::read_blob(&dir, &sha))
        .await
        .map_err(|e| ApiError::internal(format!("the read task failed: {e}")))??;
    if json {
        return Ok(Json(JsonDownload {
            card: card.id,
            version: v,
            data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
        })
        .into_response());
    }
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    let set = |h: &mut HeaderMap, k: header::HeaderName, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(k, v);
        }
    };
    set(h, header::CONTENT_TYPE, &v.media_type);
    // The name has only safe characters (`check_name`): no quote, no `\`.
    set(
        h,
        header::CONTENT_DISPOSITION,
        &format!("attachment; filename=\"{}\"", v.name),
    );
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::CONTENT_SECURITY_POLICY, FILE_CSP);
    set(h, header::CACHE_CONTROL, "private, no-cache");
    set(h, header::ETAG, &format!("\"{}\"", v.sha256));
    set(
        h,
        header::HeaderName::from_static("x-terrarium-file-version"),
        &v.version.to_string(),
    );
    Ok(resp)
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    state
        .db
        .write(actor.label(), move |t| {
            card_files::remove(t, &actor, &id, &name)
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The prune of the card files (the prune timer): delete the rows of the
/// files removed more than `TERRARIUM_TRASH_DAYS` ago, then the blobs that
/// no row names. Returns the rows and the blobs.
pub async fn prune(state: &Shared, now: std::time::SystemTime) -> ApiResult<(usize, usize)> {
    let days = i64::try_from(crate::trash::trash_days()).unwrap_or(i64::MAX / 86_400);
    let cutoff = chrono::DateTime::<chrono::Utc>::from(now) - chrono::Duration::days(days);
    let cutoff = cutoff.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (rows, used) = state
        .db
        .write("system", move |t| {
            let n = card_files::gc_rows(t, &cutoff)?;
            Ok((n, card_files::used_blobs(t)?))
        })
        .await?;
    let dir = state.state_dir.clone();
    let blobs = tokio::task::spawn_blocking(move || card_files::gc_blobs(&dir, &used, now))
        .await
        .map_err(|e| ApiError::internal(format!("the prune task failed: {e}")))?;
    Ok((rows, blobs))
}
