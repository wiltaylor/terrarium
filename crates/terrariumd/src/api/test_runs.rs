//! `/api/projects/{name}/test-runs` and `/api/test-runs/...`: test result
//! files (PLAN §15.5, docs/api.md §29).
//!
//! An upload is one of:
//!
//! - JSON with `content` (the file as text) or `data_base64`, and the
//!   fields `file_name`, `name`, `format`, `cards`, `labels`, `commit`,
//!   `branch` (the CLI sends this);
//! - `multipart/form-data` with a `file` part and the same fields as text
//!   parts (`card` and `label` can repeat);
//! - any other body: the file itself, with the fields in the query
//!   (`?name=&format=&card=&card=&label=&commit=&branch=&file_name=`).
//!
//! A run links to a version label with `label`.
//!
//! The parse runs off the async threads, before the write.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use serde::Deserialize;

use super::artifacts::parse_multipart;
use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::store::test_runs::{
    self, CaseQuery, Compare, LinksPatch, ListQuery, NewRun, TestRunDetail, TestRunSummary,
};
use crate::test_results::{self, Format};

/// The request body limit: a 20 MiB file as base64 in JSON, with room.
pub const UPLOAD_BODY_LIMIT: usize = 30 * 1024 * 1024;

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/projects/{name}/test-runs",
            get(list)
                .post(upload)
                .layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/test-runs/{id}",
            get(show).delete(remove),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (
                    axum::http::Method::DELETE,
                    super::link_registry::Access::Link,
                ),
            ],
        )
        .route(
            "/test-runs/{id}/raw",
            get(raw),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/test-runs/{id}/compare",
            get(compare),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/test-runs/{id}/links",
            axum::routing::patch(links),
            &[(
                axum::http::Method::PATCH,
                super::link_registry::Access::Link,
            )],
        )
}

// ---- upload ----

/// The JSON form of an upload.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    data_base64: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    cards: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

/// A JSON body is the envelope when its top level has `content` or
/// `data_base64`. A JSON report (CTRF, Vitest, Jest) has neither.
fn is_envelope(body: &[u8]) -> bool {
    let head: Vec<u8> = body
        .iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace)
        .take(1)
        .collect();
    if head != b"{" {
        return false;
    }
    serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body)
        .is_ok_and(|m| m.contains_key("content") || m.contains_key("data_base64"))
}

/// The file, its format (`None`: auto), and the other fields.
type Upload = (Vec<u8>, Option<Format>, NewRun);

fn format_of(v: Option<&str>) -> ApiResult<Option<Format>> {
    Format::parse_name(v.unwrap_or("auto")).map_err(test_runs::parse_error)
}

fn too_large(size: usize) -> ApiError {
    test_runs::parse_error(test_results::ParseError::TooLarge {
        size,
        max: test_results::MAX_BYTES,
    })
}

fn parse_upload(headers: &HeaderMap, query: &[(String, String)], body: Bytes) -> ApiResult<Upload> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct.starts_with("multipart/form-data") {
        let parts = parse_multipart(ct, &body)?;
        let texts = |name: &str| -> Vec<String> {
            parts
                .iter()
                .filter(|p| p.name == name)
                .map(|p| String::from_utf8_lossy(&p.data).trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let text = |name: &str| texts(name).into_iter().next();
        let file = parts
            .iter()
            .find(|p| p.name == "file")
            .ok_or_else(|| ApiError::bad_request("the multipart upload needs a `file` part"))?;
        let mut cards = texts("card");
        cards.extend(texts("cards"));
        let mut labels = texts("label");
        labels.extend(texts("labels"));
        let meta = NewRun {
            name: text("name"),
            file_name: text("file_name").or_else(|| file.filename.clone()),
            cards,
            labels,
            commit: text("commit"),
            branch: text("branch"),
        };
        let format = format_of(text("format").as_deref())?;
        return Ok((file.data.clone(), format, meta));
    }
    if (ct.is_empty() || ct.starts_with("application/json")) && is_envelope(&body) {
        let e: Envelope = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad upload JSON: {e}")))?;
        let data = match (e.content, e.data_base64) {
            (Some(c), None) => c.into_bytes(),
            (None, Some(b)) => {
                let b64: String = b.chars().filter(|c| !c.is_ascii_whitespace()).collect();
                if b64.len() / 4 * 3 > test_results::MAX_BYTES + 3 {
                    return Err(too_large(b64.len() / 4 * 3));
                }
                base64::engine::general_purpose::STANDARD
                    .decode(b64.as_bytes())
                    .map_err(|e| ApiError::bad_request(format!("bad data_base64: {e}")))?
            }
            _ => {
                return Err(ApiError::bad_request(
                    "give one of `content` and `data_base64`",
                ));
            }
        };
        let format = format_of(e.format.as_deref())?;
        let meta = NewRun {
            name: e.name,
            file_name: e.file_name,
            cards: e.cards,
            labels: e.labels,
            commit: e.commit,
            branch: e.branch,
        };
        return Ok((data, format, meta));
    }
    let one = |k: &str| {
        query
            .iter()
            .find(|(q, _)| q == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty())
    };
    let many = |k: &str| -> Vec<String> {
        query
            .iter()
            .filter(|(q, _)| q == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty())
            .collect()
    };
    for (k, _) in query {
        if ![
            "name",
            "format",
            "card",
            "label",
            "commit",
            "branch",
            "file_name",
        ]
        .contains(&k.as_str())
        {
            return Err(ApiError::bad_request(format!(
                "unknown query field `{k}`: use name, format, card, label, commit, branch, file_name"
            )));
        }
    }
    let format = format_of(one("format").as_deref())?;
    let meta = NewRun {
        name: one("name"),
        file_name: one("file_name"),
        cards: many("card"),
        labels: many("label"),
        commit: one("commit"),
        branch: one("branch"),
    };
    Ok((body.to_vec(), format, meta))
}

async fn upload(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(query): ApiQuery<Vec<(String, String)>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<TestRunSummary>)> {
    crate::policy::write_test_runs(&actor, &name)?;
    let p = name.clone();
    state
        .db
        .read(move |c| crate::store::projects::get(c, &p).map(|_| ()))
        .await?;
    let (data, format, meta) = parse_upload(&headers, &query, body)?;
    if data.len() > test_results::MAX_BYTES {
        return Err(too_large(data.len()));
    }
    let redactor = state.db.redactor().get();
    let view = crate::secrets::redact::view_of(&actor);
    let prep =
        tokio::task::spawn_blocking(move || test_runs::prepare(&data, format, &redactor, &view))
            .await
            .map_err(|e| ApiError::internal(format!("parse task failed: {e}")))??;
    let s = state
        .db
        .write(actor.label(), move |t| {
            test_runs::create(t, &actor, &name, &meta, prep)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(s)))
}

// ---- reads ----

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Response> {
    let rows = state
        .db
        .read(move |c| test_runs::list(c, &actor, &name, &q))
        .await?;
    Ok(paged(rows))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<CaseQuery>,
) -> ApiResult<Json<TestRunDetail>> {
    Ok(Json(
        state
            .db
            .read(move |c| test_runs::get(c, &actor, &id, &q))
            .await?,
    ))
}

/// `filename="..."` with ASCII only, and `filename*` for the full name.
fn disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut enc = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}")
}

async fn raw(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (file_name, ctype, data) = state
        .db
        .read(move |c| test_runs::raw(c, &actor, &id))
        .await?;
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&ctype)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(v) = HeaderValue::from_str(&disposition(&file_name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(resp)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompareQuery {
    with: Option<String>,
}

async fn compare(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<CompareQuery>,
) -> ApiResult<Json<Compare>> {
    Ok(Json(
        state
            .db
            .read(move |c| test_runs::compare(c, &actor, &id, q.with.as_deref()))
            .await?,
    ))
}

// ---- writes ----

async fn links(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(p): ApiJson<LinksPatch>,
) -> ApiResult<Json<TestRunSummary>> {
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| {
                test_runs::update_links(t, &actor, &id, &p)
            })
            .await?,
    ))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    state
        .db
        .write(actor.label(), move |t| test_runs::delete(t, &actor, &id))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn envelope_detection() {
        assert!(is_envelope(br#" {"content": "1..1\nok 1\n"}"#));
        assert!(is_envelope(br#"{"data_base64": "", "name": "x"}"#));
        assert!(!is_envelope(br#"{"results": {"tests": []}}"#));
        assert!(!is_envelope(br#"{"testResults": [], "content": 1"#));
        assert!(!is_envelope(b"<testsuites/>"));
    }

    #[test]
    fn query_fields() {
        let q = vec![
            ("name".to_owned(), "unit".to_owned()),
            ("card".to_owned(), "TER-1".to_owned()),
            ("card".to_owned(), "TER-2".to_owned()),
            ("format".to_owned(), "tap".to_owned()),
        ];
        let (data, f, m) =
            parse_upload(&HeaderMap::new(), &q, Bytes::from_static(b"ok 1\n")).unwrap();
        assert_eq!(data, b"ok 1\n");
        assert_eq!(f, Some(Format::Tap));
        assert_eq!(m.cards, ["TER-1", "TER-2"]);
        assert_eq!(m.name.as_deref(), Some("unit"));
        let bad = vec![("nope".to_owned(), "1".to_owned())];
        assert!(parse_upload(&HeaderMap::new(), &bad, Bytes::new()).is_err());
        let fmt = vec![("format".to_owned(), "xml".to_owned())];
        let e = parse_upload(&HeaderMap::new(), &fmt, Bytes::new()).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
    }

    #[test]
    fn content_disposition() {
        assert_eq!(
            disposition("junit.xml"),
            "attachment; filename=\"junit.xml\"; filename*=UTF-8''junit.xml"
        );
        assert_eq!(
            disposition("a\"b é.xml"),
            "attachment; filename=\"a_b _.xml\"; filename*=UTF-8''a%22b%20%C3%A9.xml"
        );
    }
}
