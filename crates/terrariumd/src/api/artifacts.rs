//! `/api/projects/{name}/artifacts`, `/api/artifacts/...`, and the serving
//! routes `/a/{id}/{path}` (PLAN §15.2).
//!
//! The serving routes check a short-lived signed URL and need no token or
//! actor. [`artifact_router`] serves them alone, for a listener on its own
//! port (a separate origin from the dashboard). The dashboard port mounts
//! them only when that listener is off (`TERRARIUM_ARTIFACT_LISTEN` empty);
//! the Unix socket always does. A signed URL then names the artifact
//! origin ([`artifact_origin`]).

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::store::Page;
use crate::store::artifacts::{
    self, Artifact, ArtifactCtx, NewArtifactLink, PruneReport, StoredBlob, Upload, Usage,
};

/// The request body limit of an upload: 32 MiB of data as base64 in JSON,
/// or as multipart, with room for the rest of the body.
pub const UPLOAD_BODY_LIMIT: usize = 48 * 1024 * 1024;

/// The CSP of every served artifact: an opaque origin, so an uploaded page
/// cannot reach the dashboard or the API. Scripts may run inside it.
pub const ARTIFACT_CSP: &str = "sandbox allow-scripts allow-forms allow-popups";

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/projects/{name}/artifacts",
            get(list)
                .post(upload)
                .layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/projects/{name}/artifacts/usage", get(usage))
        .route("/artifacts/prune", post(prune))
        .route("/artifacts/{id}", get(show).delete(remove))
        .route("/artifacts/{id}/links", post(link))
        .route("/artifacts/{id}/url", post(mint_url))
}

/// The serving routes, to merge into a router that has the state.
pub fn serving_routes() -> Router<Shared> {
    Router::new()
        .route("/a/{id}", get(serve_root))
        .route("/a/{id}/", get(serve_root))
        .route("/a/{id}/{*path}", get(serve_path))
}

/// The serving routes alone, for a listener of their own (port 8081).
pub fn artifact_router(state: Shared) -> Router {
    serving_routes().with_state(state)
}

fn ctx(state: &Shared) -> ApiResult<std::sync::Arc<ArtifactCtx>> {
    artifacts::context(&state.state_dir, &state.config_dir)
}

// ---- upload ----

/// The JSON form of an upload.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonUpload {
    name: String,
    #[serde(default)]
    content_type: Option<String>,
    data_base64: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    attach: Option<NewArtifactLink>,
}

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Vec<u8>,
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn header_param(value: &str, key: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        (k.trim().eq_ignore_ascii_case(key)).then(|| v.trim().trim_matches('"').to_owned())
    })
}

/// A small `multipart/form-data` parser over a buffered body.
fn parse_multipart(content_type: &str, body: &[u8]) -> ApiResult<Vec<Part>> {
    let boundary = header_param(content_type, "boundary")
        .ok_or_else(|| ApiError::bad_request("multipart body without a boundary"))?;
    let delim = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut pos = find_bytes(body, &delim, 0)
        .ok_or_else(|| ApiError::bad_request("multipart body without its boundary"))?;
    loop {
        pos += delim.len();
        if body[pos..].starts_with(b"--") {
            break;
        }
        // Skip the line end after the boundary.
        if body[pos..].starts_with(b"\r\n") {
            pos += 2;
        } else if body[pos..].starts_with(b"\n") {
            pos += 1;
        }
        let head_end = find_bytes(body, b"\r\n\r\n", pos)
            .ok_or_else(|| ApiError::bad_request("multipart part without headers"))?;
        let head = String::from_utf8_lossy(&body[pos..head_end]).into_owned();
        let data_start = head_end + 4;
        let mut next_delim = b"\r\n".to_vec();
        next_delim.extend_from_slice(&delim);
        let data_end = find_bytes(body, &next_delim, data_start)
            .ok_or_else(|| ApiError::bad_request("multipart part without an end"))?;
        let mut part = Part {
            name: String::new(),
            filename: None,
            content_type: None,
            data: body[data_start..data_end].to_vec(),
        };
        for line in head.lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            if k.trim().eq_ignore_ascii_case("content-disposition") {
                part.name = header_param(v, "name").unwrap_or_default();
                part.filename = header_param(v, "filename");
            } else if k.trim().eq_ignore_ascii_case("content-type") {
                part.content_type = Some(v.trim().to_owned());
            }
        }
        parts.push(part);
        pos = data_end + 2;
    }
    Ok(parts)
}

fn too_large(size: u64, max: u64) -> ApiError {
    ApiError::new(
        Code::TooLarge,
        format!("the upload is {size} bytes; the limit is {max} bytes"),
    )
    .with_details(serde_json::json!({"size": size, "max_file_bytes": max}))
}

fn parse_upload(headers: &HeaderMap, body: &[u8], max: u64) -> ApiResult<Upload> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let kind_of = |k: Option<String>| k.unwrap_or_else(|| "file".into());
    if ct.starts_with("multipart/form-data") {
        let parts = parse_multipart(ct, body)?;
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
            .or_else(|| file.filename.clone())
            .ok_or_else(|| ApiError::invalid("the upload needs a name"))?;
        let attach = match (text("attach"), text("attach_kind"), text("attach_ref")) {
            (Some(json), _, _) => Some(
                serde_json::from_str::<NewArtifactLink>(&json)
                    .map_err(|e| ApiError::bad_request(format!("bad `attach`: {e}")))?,
            ),
            (None, Some(kind), Some(reference)) => Some(NewArtifactLink { kind, reference }),
            _ => None,
        };
        if file.data.len() as u64 > max {
            return Err(too_large(file.data.len() as u64, max));
        }
        return Ok(Upload {
            name,
            claimed_type: text("content_type").or_else(|| file.content_type.clone()),
            data: file.data.clone(),
            kind: kind_of(text("kind")),
            attach,
        });
    }
    if ct.starts_with("application/json") {
        let u: JsonUpload = serde_json::from_slice(body)
            .map_err(|e| ApiError::bad_request(format!("bad upload JSON: {e}")))?;
        let b64: String = u
            .data_base64
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let approx = (b64.len() as u64 / 4) * 3;
        if approx > max + 3 {
            return Err(too_large(approx, max));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| ApiError::bad_request(format!("bad data_base64: {e}")))?;
        return Ok(Upload {
            name: u.name,
            claimed_type: u.content_type,
            data,
            kind: kind_of(u.kind),
            attach: u.attach,
        });
    }
    Err(ApiError::bad_request(
        "send the upload as multipart/form-data or as application/json with data_base64",
    ))
}

async fn upload(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Artifact>)> {
    policy::read_project(&actor, &name)?;
    let p = name.clone();
    state
        .db
        .read(move |c| crate::store::projects::get(c, &p).map(|_| ()))
        .await?;
    let ctx = ctx(&state)?;
    let up = parse_upload(&headers, &body, ctx.limits.max_file_bytes)?;
    drop(body);
    let (c2, u2) = (ctx.clone(), up.clone());
    let blob: StoredBlob = tokio::task::spawn_blocking(move || artifacts::store_blob(&c2, &u2))
        .await
        .map_err(|e| ApiError::internal(format!("upload task failed: {e}")))??;
    let limits = ctx.limits.clone();
    let a = state
        .db
        .write(actor.label(), move |t| {
            artifacts::create(t, &actor, &name, &up, &blob, &limits)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(a)))
}

// ---- reads and links ----

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let rows = state
        .db
        .read(move |c| artifacts::list(c, &name, &page))
        .await?;
    Ok(paged(rows))
}

async fn usage(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Usage>> {
    policy::read_project(&actor, &name)?;
    let limits = ctx(&state)?.limits.clone();
    Ok(Json(
        state
            .db
            .read(move |c| artifacts::usage(c, &name, &limits))
            .await?,
    ))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Artifact>> {
    Ok(Json(
        state
            .db
            .read(move |c| artifacts::get(c, &actor, &id))
            .await?,
    ))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let blob = state
        .db
        .write(actor.label(), move |t| artifacts::delete(t, &actor, &id))
        .await?;
    let ctx = ctx(&state)?;
    state
        .db
        .read(move |c| {
            artifacts::remove_blobs(&ctx, c, &[blob]);
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn link(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(l): ApiJson<NewArtifactLink>,
) -> ApiResult<(StatusCode, Json<Artifact>)> {
    let a = state
        .db
        .write(actor.label(), move |t| {
            artifacts::add_link(t, &actor, &id, &l)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(a)))
}

// ---- signed URLs ----

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MintBody {
    /// Seconds. Default from the config (300), at most a day.
    #[serde(default)]
    ttl: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SignedUrl {
    /// A path on the artifact origin. For a bundle the signature is in the
    /// path, so the bundle's relative links keep working.
    pub url: String,
    pub exp: i64,
    pub expires_at: String,
}

fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The origin that signed URLs point at, with no trailing `/`. Empty: the
/// dashboard port serves `/a/...` itself, so the URL stays relative.
/// Else `TERRARIUM_ARTIFACT_PUBLIC_URL`, or the host of the request with
/// the port of `TERRARIUM_ARTIFACT_LISTEN` (default 8081), and `https`
/// when a proxy says `X-Forwarded-Proto: https`.
pub fn artifact_origin(headers: &HeaderMap) -> String {
    origin_from(
        &crate::settings::artifact_listen(),
        crate::settings::artifact_public_url(),
        headers,
    )
}

/// [`artifact_origin`] with the settings given.
fn origin_from(listen: &str, public: Option<String>, headers: &HeaderMap) -> String {
    if listen.is_empty() {
        return String::new();
    }
    if let Some(url) = public {
        return url;
    }
    let port = listen.rsplit_once(':').map_or("8081", |(_, p)| p);
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|p| p.trim().eq_ignore_ascii_case("https"))
        .map_or("http", |_| "https");
    format!("{scheme}://{}:{port}", host_without_port(host))
}

/// `example.com:8080` -> `example.com`; `[::1]:8080` -> `[::1]`.
fn host_without_port(host: &str) -> &str {
    if let Some(end) = host.find(']') {
        return &host[..=end];
    }
    match host.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

async fn mint_url(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<SignedUrl>> {
    let b: MintBody = if body.is_empty() {
        MintBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad body: {e}")))?
    };
    let a = state
        .db
        .read(move |c| artifacts::get(c, &actor, &id))
        .await?;
    let ctx = ctx(&state)?;
    let ttl = b.ttl.unwrap_or(ctx.limits.url_ttl_secs).clamp(1, 24 * 3600);
    let exp = Utc::now().timestamp() + ttl;
    let sig = ctx.sign(&a.uid, exp);
    let origin = artifact_origin(&headers);
    let url = if a.kind == "bundle" {
        format!("{origin}/a/{}/_s/{exp}/{sig}/", a.uid)
    } else {
        format!(
            "{origin}/a/{}/{}?exp={exp}&sig={sig}",
            a.uid,
            encode_segment(&a.name)
        )
    };
    let expires_at = DateTime::<Utc>::from_timestamp(exp, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default();
    Ok(Json(SignedUrl {
        url,
        exp,
        expires_at,
    }))
}

// ---- serving ----

#[derive(Debug, Default, Deserialize)]
struct SigQuery {
    exp: Option<i64>,
    sig: Option<String>,
}

async fn serve_root(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<SigQuery>,
) -> Response {
    serve(state, id, String::new(), q)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn serve_path(
    State(state): State<Shared>,
    Path((id, path)): Path<(String, String)>,
    Query(q): Query<SigQuery>,
) -> Response {
    serve(state, id, path, q)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

fn forbidden_url() -> ApiError {
    ApiError::forbidden("the artifact URL is not signed, or the signature is wrong or expired")
}

fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn respond(data: Vec<u8>, content_type: &str, attachment: Option<&str>) -> Response {
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    let set = |h: &mut HeaderMap, k: header::HeaderName, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(k, v);
        }
    };
    set(h, header::CONTENT_TYPE, content_type);
    set(h, header::CONTENT_SECURITY_POLICY, ARTIFACT_CSP);
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::REFERRER_POLICY, "no-referrer");
    set(h, header::CACHE_CONTROL, "private, max-age=60");
    if let Some(name) = attachment {
        set(
            h,
            header::CONTENT_DISPOSITION,
            &format!("attachment; filename=\"{}\"", safe_filename(name)),
        );
    }
    resp
}

async fn serve(state: Shared, id: String, path: String, q: SigQuery) -> ApiResult<Response> {
    let ctx = ctx(&state)?;
    let (exp, sig, rest) = match path.strip_prefix("_s/") {
        Some(signed) => {
            let mut it = signed.splitn(3, '/');
            let exp = it.next().and_then(|e| e.parse::<i64>().ok());
            let sig = it.next().map(str::to_owned);
            (exp, sig, it.next().unwrap_or("").to_owned())
        }
        None => (q.exp, q.sig, path),
    };
    let (Some(exp), Some(sig)) = (exp, sig) else {
        return Err(forbidden_url());
    };
    if !ctx.verify(&id, exp, &sig, Utc::now().timestamp()) {
        return Err(forbidden_url());
    }
    let uid = id.clone();
    let row = state
        .db
        .read(move |c| artifacts::by_uid(c, &uid))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no artifact `{id}`")))?;
    if row.kind == "file" {
        let data = tokio::fs::read(ctx.blob_path(&row.sha256))
            .await
            .map_err(|e| ApiError::not_found(format!("the blob is gone: {e}")))?;
        let attach = (!artifacts::renders(&row.content_type)).then_some(row.name.as_str());
        return Ok(respond(data, &row.content_type, attach));
    }
    // A bundle: a path inside its directory, `index.html` for a directory.
    let mut rel = std::path::PathBuf::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(ApiError::not_found("no such file in the bundle")),
            s => rel.push(s),
        }
    }
    let root = ctx.bundle_dir(&row.sha256);
    let mut file = root.join(&rel);
    if file.is_dir() {
        file = file.join("index.html");
    }
    let data = tokio::fs::read(&file)
        .await
        .map_err(|_| ApiError::not_found("no such file in the bundle"))?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ct = artifacts::bundle_file_type(&name, &data);
    let attach = ct
        .starts_with("application/octet-stream")
        .then_some(name.as_str());
    Ok(respond(data, &ct, attach))
}

// ---- retention ----

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PruneBody {
    /// Default: true. Set false to prune.
    #[serde(default)]
    dry_run: Option<bool>,
    /// Apply the rule as of this time. Default: now.
    #[serde(default)]
    as_of: Option<String>,
}

/// Apply the retention rule, then remove the blobs no one uses. The daemon
/// wiring may call this on a timer.
pub async fn run_prune(
    state: &Shared,
    now: DateTime<Utc>,
    dry_run: bool,
    actor: String,
) -> ApiResult<PruneReport> {
    let ctx = ctx(state)?;
    let days = ctx.limits.retention_days;
    let report = state
        .db
        .write(actor, move |t| artifacts::prune(t, now, days, dry_run))
        .await?;
    if !report.blobs.is_empty() {
        let blobs = report.blobs.clone();
        state
            .db
            .read(move |c| {
                artifacts::remove_blobs(&ctx, c, &blobs);
                Ok(())
            })
            .await?;
    }
    Ok(report)
}

async fn prune(
    State(state): State<Shared>,
    actor: Actor,
    body: Bytes,
) -> ApiResult<Json<PruneReport>> {
    policy::prune_artifacts(&actor)?;
    let b: PruneBody = if body.is_empty() {
        PruneBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad body: {e}")))?
    };
    let now = match &b.as_of {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map_err(|e| ApiError::bad_request(format!("bad as_of: {e}")))?
            .with_timezone(&Utc),
        None => Utc::now(),
    };
    Ok(Json(
        run_prune(&state, now, b.dry_run.unwrap_or(true), actor.label()).await?,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    /// F5: a signed URL names the artifact origin, not the dashboard.
    #[test]
    fn signed_urls_point_at_the_artifact_origin() {
        let h = headers(&[("host", "terrarium.lan:8080")]);
        assert_eq!(
            origin_from("0.0.0.0:8081", None, &h),
            "http://terrarium.lan:8081"
        );
        assert_eq!(
            origin_from("127.0.0.1:9000", None, &h),
            "http://terrarium.lan:9000"
        );
        // The explicit setting wins.
        assert_eq!(
            origin_from("0.0.0.0:8081", Some("https://art.example".into()), &h),
            "https://art.example"
        );
        // No listener of its own: the dashboard serves /a/, the URL stays
        // relative.
        assert_eq!(origin_from("", Some("https://art.example".into()), &h), "");
        // Behind a TLS proxy.
        let h = headers(&[
            ("host", "inner:8080"),
            ("x-forwarded-host", "t.example.com"),
            ("x-forwarded-proto", "https"),
        ]);
        assert_eq!(
            origin_from("0.0.0.0:8081", None, &h),
            "https://t.example.com:8081"
        );
        // IPv6 and no host.
        let h = headers(&[("host", "[::1]:8080")]);
        assert_eq!(origin_from(":8081", None, &h), "http://[::1]:8081");
        assert_eq!(
            origin_from("0.0.0.0:8081", None, &HeaderMap::new()),
            "http://localhost:8081"
        );
    }
}
