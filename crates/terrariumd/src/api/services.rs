//! Services C1 (TER-1264): the definitions and the request queue.
//!
//! | Route | Who |
//! |---|---|
//! | `GET /api/services`, `GET /api/services/{name}` | the operator and sysadmins: all; an agent: its project's services and those it may call |
//! | `POST /api/services`, `PATCH`, `DELETE /api/services/{name}` | a global service: sysadmins and the operator; a project service: also its project_admin |
//! | `POST /api/services/{name}/requests` | a caller in the service's `callers` (empty: its project), or an admin of the service |
//! | `GET /api/services/{name}/requests` | an admin of the service: all; others: their own |
//! | `GET /api/service-requests/{id}`, `POST /api/service-requests/{id}/cancel`, `GET /api/service-requests/{id}/payload` | the caller of the request or an admin of the service |
//!
//! A call is `application/json` `{"request": <any JSON>, "payload_base64":
//! "<the file as base64>"}` (the CLI's form), or `multipart/form-data` with
//! a `request` part (JSON text) and a `payload` part (the file). Each
//! field and part is optional. The answer is `201`
//! `{id, service, state, position, status_url}` at once.
//!
//! Each change writes an audit row: `service.create`, `service.update`,
//! `service.delete`, `service.call`, `service.cancel`.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest;
use terrarium_core::service::{self as core, ServiceDef};

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::store::services::{self as store, Caller, RequestFilter, RequestRow, ServiceRow};

pub fn routes() -> super::link_registry::RegisteredRouter {
    use super::link_registry::Access;
    use axum::http::Method;
    super::link_registry::RegisteredRouter::new()
        .route(
            "/services",
            get(list).post(create),
            &[(Method::GET, Access::Link), (Method::POST, Access::Link)],
        )
        .route(
            "/services/{name}",
            get(show).patch(update).delete(remove),
            &[
                (Method::GET, Access::Link),
                (Method::PATCH, Access::Link),
                (Method::DELETE, Access::Link),
            ],
        )
        .route(
            "/services/{name}/requests",
            get(requests_of).post(call),
            &[(Method::GET, Access::Link), (Method::POST, Access::Link)],
        )
        .route(
            "/service-requests/{id}",
            get(request_show),
            &[(Method::GET, Access::Link)],
        )
        .route(
            "/service-requests/{id}/cancel",
            post(request_cancel),
            &[(Method::POST, Access::Link)],
        )
        .route(
            "/service-requests/{id}/payload",
            get(request_payload),
            &[(Method::GET, Access::Link)],
        )
}

/// May the actor define, change, and remove a service of `project`
/// (`None`: global)? Design Q6 A: a sysadmin or the project lead.
fn manage(actor: &Actor, project: Option<&str>, what: &str) -> ApiResult<()> {
    match project {
        None => policy::sysadmin(actor, what),
        Some(p) => policy::admin_project(actor, p, what),
    }
}

fn manages(actor: &Actor, project: Option<&str>) -> bool {
    manage(actor, project, "").is_ok()
}

fn caller_of(actor: &Actor) -> Caller {
    match actor.agent() {
        None => Caller::Operator,
        Some(a) => Caller::Agent {
            id: a.id.clone(),
            name: a.name.clone(),
            project: a.project.clone(),
        },
    }
}

/// May the actor see `row`? Its admins, its project, and its callers.
fn may_see(actor: &Actor, row: &ServiceRow) -> bool {
    if manages(actor, row.def.project()) {
        return true;
    }
    let me = caller_of(actor);
    let own = match &me {
        Caller::Agent { project, .. } => row.def.project() == Some(project.as_str()),
        Caller::Operator => false,
    };
    own || store::in_callers(&row.def, &me)
}

/// May the actor see and cancel request `r`? Its caller and the admins of
/// its service.
fn may_touch(actor: &Actor, r: &RequestRow) -> bool {
    if manages(actor, r.project.as_deref()) {
        return true;
    }
    match actor.agent() {
        Some(a) => r.caller_agent.as_deref() == Some(a.id.as_str()),
        None => r.caller_agent.is_none(),
    }
}

fn no_service(name: &str) -> ApiError {
    ApiError::not_found(format!("no service `{name}`"))
}

fn no_request(id: &str) -> ApiError {
    ApiError::not_found(format!("no service request `{id}`"))
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<ServiceRow>>> {
    let rows = state.db.read(store::list).await?;
    Ok(Json(
        rows.into_iter().filter(|r| may_see(&actor, r)).collect(),
    ))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let n = name.clone();
    let (row, open) = state
        .db
        .read(move |c| Ok((store::find(c, &n)?, store::open_requests(c, &n)?)))
        .await?;
    // A service that the actor may not see is 404, so names do not leak.
    let row = row
        .filter(|r| may_see(&actor, r))
        .ok_or_else(|| no_service(&name))?;
    let mut v = serde_json::to_value(&row)?;
    v["open_requests"] = json!(open);
    v["may_call"] = json!(
        manages(&actor, row.def.project()) || store::in_callers(&row.def, &caller_of(&actor))
    );
    Ok(Json(v))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(def): ApiJson<ServiceDef>,
) -> ApiResult<(StatusCode, Json<ServiceRow>)> {
    manage(&actor, def.project(), "defining a service")?;
    let row = state
        .db
        .write(actor.label(), move |t| {
            let row = store::create(t, &def)?;
            t.audit(
                "service.create",
                &row.def.name,
                row.def.project(),
                serde_json::to_value(&row.def)?,
            )?;
            Ok(row)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// The project of service `name`, for the rights check before a write.
async fn project_of(state: &Shared, actor: &Actor, name: &str) -> ApiResult<Option<String>> {
    let n = name.to_owned();
    let row = state.db.read(move |c| store::find(c, &n)).await?;
    let row = row
        .filter(|r| may_see(actor, r))
        .ok_or_else(|| no_service(name))?;
    Ok(row.def.project)
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<ServiceRow>> {
    let project = project_of(&state, &actor, &name).await?;
    manage(&actor, project.as_deref(), "changing a service")?;
    let row = state
        .db
        .write(actor.label(), move |t| {
            let row = store::update(t, &name, &patch)?;
            t.audit("service.update", &name, row.def.project(), patch)?;
            Ok(row)
        })
        .await?;
    Ok(Json(row))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let project = project_of(&state, &actor, &name).await?;
    manage(&actor, project.as_deref(), "deleting a service")?;
    state
        .db
        .write(actor.label(), move |t| {
            let row = store::delete(t, &name)?;
            t.audit("service.delete", &name, row.def.project(), json!({}))
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- calls

/// The JSON body of a call.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallBody {
    #[serde(default)]
    request: Option<Value>,
    /// The payload as standard base64.
    #[serde(default)]
    payload_base64: Option<String>,
}

/// The parsed call: the request JSON and the payload bytes.
struct Call {
    request: Value,
    payload: Option<Vec<u8>>,
}

/// Room for the multipart framing and the request part.
const BODY_SLACK: u64 = core::REQUEST_JSON_MAX as u64 + 64 * 1024;

fn parse_call(headers: &HeaderMap, body: &[u8]) -> ApiResult<Call> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let check_json = |text: &[u8]| -> ApiResult<Value> {
        if text.len() > core::REQUEST_JSON_MAX {
            return Err(ApiError::new(
                Code::TooLarge,
                format!(
                    "the request JSON has {} bytes; a request takes at most {}",
                    text.len(),
                    core::REQUEST_JSON_MAX
                ),
            ));
        }
        serde_json::from_slice(text)
            .map_err(|e| ApiError::bad_request(format!("the request is not JSON: {e}")))
    };
    if ct.starts_with("multipart/form-data") {
        let mut parts = super::artifacts::parse_multipart(ct, body)?;
        if let Some(p) = parts
            .iter()
            .find(|p| p.name != "request" && p.name != "payload")
        {
            return Err(ApiError::bad_request(format!(
                "unknown part `{}`: a call has the parts `request` and `payload`",
                p.name
            )));
        }
        let request = match parts.iter().find(|p| p.name == "request") {
            Some(p) if !p.data.is_empty() => check_json(&p.data)?,
            _ => json!({}),
        };
        let payload = parts
            .iter()
            .position(|p| p.name == "payload")
            .map(|i| parts.swap_remove(i).data);
        return Ok(Call { request, payload });
    }
    if body.is_empty() {
        return Ok(Call {
            request: json!({}),
            payload: None,
        });
    }
    let b: CallBody = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("bad call body: {e}")))?;
    let request = b.request.unwrap_or_else(|| json!({}));
    let text = serde_json::to_vec(&request)?;
    check_json(&text)?;
    let payload = b
        .payload_base64
        .map(|p| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(p.trim())
                .map_err(|e| ApiError::bad_request(format!("`payload_base64` is not base64: {e}")))
        })
        .transpose()?;
    Ok(Call { request, payload })
}

/// The free space of the daemon's state filesystem.
fn free_space(dir: &std::path::Path) -> Option<store::FreeSpace> {
    #[cfg(unix)]
    {
        let s = nix::sys::statvfs::statvfs(dir).ok()?;
        #[allow(clippy::unnecessary_cast)]
        let frag = s.fragment_size() as u64;
        #[allow(clippy::unnecessary_cast)]
        Some((
            (s.blocks_available() as u64).saturating_mul(frag),
            (s.blocks() as u64).saturating_mul(frag),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

fn write_payload(dir: &std::path::Path, id: &str, data: &[u8]) -> std::io::Result<()> {
    let d = store::payload_dir(dir, id);
    std::fs::create_dir_all(&d)?;
    let tmp = d.join("request.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, store::payload_file(dir, id))
}

fn remove_payload(dir: &std::path::Path, id: &str) {
    let d = store::payload_dir(dir, id);
    if let Err(e) = std::fs::remove_dir_all(&d)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %d.display(), "cannot remove a service payload: {e}");
    }
}

/// `POST /api/services/{name}/requests`: queue a request (design D2).
async fn call(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let n = name.clone();
    let row = state
        .db
        .read(move |c| store::find(c, &n))
        .await?
        .filter(|r| may_see(&actor, r))
        .ok_or_else(|| no_service(&name))?;
    let me = caller_of(&actor);
    if !manages(&actor, row.def.project()) && !store::in_callers(&row.def, &me) {
        return Err(ApiError::forbidden(format!(
            "calling service `{name}` needs a place in its callers list, or project_admin of its project, sysadmin, or the operator"
        )));
    }
    // The body limit of this service, before the bytes are read: the
    // payload as base64 (4/3) and room for the request JSON.
    let max = row.def.max_payload_bytes();
    let limit = max
        .saturating_add(max.div_ceil(3))
        .saturating_add(BODY_SLACK);
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && len > limit
    {
        return Err(too_large(&row, len));
    }
    let bytes = axum::body::to_bytes(body, usize::try_from(limit).unwrap_or(usize::MAX))
        .await
        .map_err(|_| too_large(&row, limit.saturating_add(1)))?;
    let parsed = parse_call(&headers, &bytes)?;
    drop(bytes);
    let size = parsed.payload.as_ref().map_or(0, |p| p.len() as u64);
    let free = free_space(&state.state_dir);
    if free.is_none() && size > 0 {
        tracing::warn!(
            "cannot read the free space of the state directory; the 20% floor is not checked"
        );
    }
    // The caps before the file is written; the insert checks them again.
    let r2 = row.clone();
    state
        .db
        .read(move |c| store::check_payload_room(c, &r2, size, free))
        .await?;
    let id = ulid::Ulid::new().to_string();
    let sha = match &parsed.payload {
        Some(data) => {
            let (dir, id2, data) = (state.state_dir.clone(), id.clone(), data.clone());
            let sha = tokio::task::spawn_blocking(move || {
                write_payload(&dir, &id2, &data)?;
                Ok::<_, std::io::Error>(hex(&sha2::Sha256::digest(&data)))
            })
            .await
            .map_err(|e| ApiError::internal(format!("payload task failed: {e}")))?
            .map_err(|e| ApiError::internal(format!("cannot store the payload: {e}")))?;
            Some(sha)
        }
        None => None,
    };
    let (id2, request) = (id.clone(), parsed.request);
    let res = state
        .db
        .write(actor.label(), move |t| {
            let row = store::get(t, &row.def.name)?;
            store::check_payload_room(t, &row, size, free)?;
            let r = store::insert_request(
                t,
                &store::NewRequest {
                    id: &id2,
                    service: &row,
                    caller: &me,
                    request: &request,
                    payload_size: size,
                    payload_sha256: sha.as_deref(),
                },
            )?;
            t.audit(
                "service.call",
                &row.def.name,
                row.def.project(),
                json!({"request": r.id, "caller": r.caller, "payload_size": size}),
            )?;
            Ok(r)
        })
        .await;
    let r = match res {
        Ok(r) => r,
        Err(e) => {
            if size > 0 {
                remove_payload(&state.state_dir, &id);
            }
            return Err(e);
        }
    };
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": r.id,
            "service": r.service,
            "state": r.state,
            "position": r.position,
            "wait_until": r.wait_until,
            "status_url": format!("/api/service-requests/{}", r.id),
        })),
    ))
}

fn too_large(row: &ServiceRow, len: u64) -> ApiError {
    ApiError::new(
        Code::TooLarge,
        format!(
            "the call has {len} bytes; service `{}` takes a payload of at most {} bytes",
            row.def.name,
            row.def.max_payload_bytes()
        ),
    )
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The query of `GET /api/services/{name}/requests`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestsQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn requests_of(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<RequestsQuery>,
) -> ApiResult<Json<Vec<RequestRow>>> {
    let st = q
        .state
        .as_deref()
        .map(|s| {
            core::State::parse(s).ok_or_else(|| {
                ApiError::bad_request(format!(
                    "bad state `{s}`: use queued, waiting, running, done, failed, cancelled, or timed_out"
                ))
            })
        })
        .transpose()?;
    let n = name.clone();
    let row = state
        .db
        .read(move |c| store::find(c, &n))
        .await?
        .filter(|r| may_see(&actor, r))
        .ok_or_else(|| no_service(&name))?;
    let admin = manages(&actor, row.def.project());
    let f = RequestFilter {
        service: Some(name),
        caller_agent: if admin {
            None
        } else {
            actor.agent().map(|a| a.id.clone())
        },
        operator_only: !admin && actor.agent().is_none(),
        state: st,
        limit: q.limit.unwrap_or(100),
    };
    Ok(Json(
        state.db.read(move |c| store::list_requests(c, &f)).await?,
    ))
}

async fn visible_request(state: &Shared, actor: &Actor, id: &str) -> ApiResult<RequestRow> {
    let i = id.to_owned();
    state
        .db
        .read(move |c| store::find_request(c, &i))
        .await?
        .filter(|r| may_touch(actor, r))
        .ok_or_else(|| no_request(id))
}

async fn request_show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<RequestRow>> {
    Ok(Json(visible_request(&state, &actor, &id).await?))
}

async fn request_cancel(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<RequestRow>> {
    visible_request(&state, &actor, &id).await?;
    let why = format!("cancelled by {}", caller_of(&actor).label());
    let r = state
        .db
        .write(actor.label(), move |t| {
            let r = store::cancel(t, &id, &why)?;
            t.audit(
                "service.cancel",
                &r.service,
                r.project.as_deref(),
                json!({"request": r.id}),
            )?;
            Ok(r)
        })
        .await?;
    Ok(Json(r))
}

/// `GET /api/service-requests/{id}/payload`: the request payload as
/// `application/octet-stream`. `404` when the request has none, or when
/// the retention removed it.
async fn request_payload(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    use tokio::io::AsyncReadExt as _;
    let r = visible_request(&state, &actor, &id).await?;
    if r.payload_sha256.is_none() {
        return Err(ApiError::not_found(format!(
            "service request `{id}` has no payload"
        )));
    }
    if r.payload_deleted_at.is_some() {
        return Err(ApiError::not_found(format!(
            "the payload of service request `{id}` is past its retention"
        )));
    }
    let path = store::payload_file(&state.state_dir, &id);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ApiError::internal(format!("cannot open the payload: {e}")))?;
    let stream = futures_util::stream::unfold(Some(file), |file| async move {
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
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&r.payload_size.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Some(sha) = r.payload_sha256.as_deref()
        && let Ok(v) = HeaderValue::from_str(sha)
    {
        h.insert("x-terrarium-sha256", v);
    }
    Ok(resp)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn headers(ct: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
        h
    }

    #[test]
    fn a_call_body_parses_as_json_or_multipart() {
        let c = parse_call(
            &headers("application/json"),
            br#"{"request":{"seconds":5}}"#,
        )
        .unwrap();
        assert_eq!(c.request, json!({"seconds": 5}));
        assert!(c.payload.is_none());
        let c = parse_call(
            &headers("application/json"),
            br#"{"request":{},"payload_base64":"QllURVM="}"#,
        )
        .unwrap();
        assert_eq!(c.payload.as_deref(), Some(&b"BYTES"[..]));
        assert!(parse_call(&headers("application/json"), br#"{"payload_base64":"%%"}"#).is_err());
        let c = parse_call(&headers("application/json"), b"").unwrap();
        assert_eq!(c.request, json!({}));
        assert!(parse_call(&headers("application/json"), br#"{"req":1}"#).is_err());
        let body = b"--XX\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\"a\":1}\r\n\
--XX\r\nContent-Disposition: form-data; name=\"payload\"; filename=\"in.bin\"\r\n\r\nBYTES\r\n--XX--\r\n";
        let c = parse_call(&headers("multipart/form-data; boundary=XX"), body).unwrap();
        assert_eq!(c.request, json!({"a": 1}));
        assert_eq!(c.payload.as_deref(), Some(&b"BYTES"[..]));
        let bad = b"--XX\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nx\r\n--XX--\r\n";
        assert!(parse_call(&headers("multipart/form-data; boundary=XX"), bad).is_err());
    }

    #[test]
    fn hex_is_lower_case() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
    }
}
