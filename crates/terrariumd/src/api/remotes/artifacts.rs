//! Home capabilities are not remote credentials. The artifact connection has
//! its own address validation and TLS pin and receives no credential headers.
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{key, load, net, operator, own_ports};
use crate::actor::Actor;
use crate::api::{Shared, artifacts, session::CurrentSession};
use crate::error::{ApiError, ApiResult};

const MAX: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Capability {
    remote: String,
    remote_id: String,
    instance: String,
    target: String,
    artifact_origin: Option<String>,
    sid: String,
    jti: String,
    pv: u64,
}

fn denied() -> ApiError {
    ApiError::forbidden("remote artifact capability is invalid or expired")
}

pub(super) async fn mint(
    State(s): State<Shared>,
    actor: Actor,
    session: Option<Extension<CurrentSession>>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<artifacts::SignedUrl>> {
    operator(&actor)?;
    let session = session
        .ok_or_else(|| ApiError::forbidden("remote artifacts require an operator session"))?
        .0
        .0;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ApiError::bad_request("invalid artifact id"));
    }
    let r = load(&s, name).await?;
    let limits = s.remotes.get(&r.id);
    if *limits.closed.borrow() {
        return Err(net::unreachable());
    }
    let _permit = limits.acquire(false)?;
    let c = net::client(&r, &own_ports(&s)).await?;
    let h = net::request_headers(
        &HeaderMap::new(),
        &key(&s, &r).await?,
        &s.links.instance_id(&s.state_dir)?,
    )?;
    let instance = net::instance(&c, &r, &h).await?;
    let response = c
        .post(format!("{}api/artifacts/{id}/url", r.url))
        .headers(h)
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"ttl":120}"#)
        .timeout(net::PLAIN)
        .send()
        .await
        .map_err(net::connection_error)?;
    if response.status() != StatusCode::OK {
        return Err(net::unreachable());
    }
    #[derive(Deserialize)]
    struct Minted {
        url: String,
        exp: i64,
    }
    let minted: Minted = serde_json::from_slice(&net::limited(response, 16384, net::PLAIN).await?)
        .map_err(|_| net::unreachable())?;
    let base = url::Url::parse(&r.url).map_err(|_| net::unreachable())?;
    let target = base.join(&minted.url).map_err(|_| net::unreachable())?;
    check_target(&target)?;
    destination_policy(&r.url, instance.artifact_origin.as_deref(), &target)?;
    let exp =
        (Utc::now().timestamp() + 120)
            .min(minted.exp)
            .min(session.claims.exp)
            .min(session.claims.auth_time.saturating_add(
                i64::try_from(s.web.session_max_age.as_secs()).unwrap_or(i64::MAX),
            ));
    let cap = Capability {
        remote: r.name,
        remote_id: r.id,
        instance: r.instance_id,
        target: target.to_string(),
        artifact_origin: instance.artifact_origin,
        sid: session.claims.sid,
        jti: session.claims.jti,
        pv: session.claims.pv,
    };
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cap).map_err(|_| denied())?);
    let ctx = crate::store::artifacts::context(&s.state_dir, &s.config_dir)?;
    let sig = ctx.sign(&format!("remote-artifact.v1.{encoded}"), exp);
    // A trailing slash lets a bundle resolve its own relative assets.
    let origin = artifacts::artifact_origin(&s, &headers);
    Ok(Json(artifacts::SignedUrl {
        url: format!("{origin}/ra/{encoded}/{exp}/{sig}/"),
        exp,
        expires_at: chrono::DateTime::from_timestamp(exp, 0)
            .ok_or_else(denied)?
            .to_rfc3339(),
    }))
}

pub(crate) fn serving_routes() -> Router<Shared> {
    Router::new()
        .route("/ra/{cap}/{exp}/{sig}/", get(root))
        .route("/ra/{cap}/{exp}/{sig}/{*path}", get(asset))
}

async fn root(
    State(s): State<Shared>,
    Path((cap, exp, sig)): Path<(String, i64, String)>,
) -> ApiResult<Response> {
    serve(s, cap, exp, sig, String::new()).await
}
async fn asset(
    State(s): State<Shared>,
    Path((cap, exp, sig, path)): Path<(String, i64, String, String)>,
) -> ApiResult<Response> {
    serve(s, cap, exp, sig, path).await
}

fn check_target(u: &url::Url) -> ApiResult<()> {
    // The origin may differ from the API, but never its trust policy. net::client
    // checks it again, independently, before any connection is made.
    if !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
        || !u.path().starts_with("/a/")
    {
        return Err(denied());
    }
    Ok(())
}

fn target(base: &str, path: &str) -> ApiResult<url::Url> {
    let mut u = url::Url::parse(base).map_err(|_| denied())?;
    check_target(&u)?;
    if path.is_empty() {
        return Ok(u);
    }
    // Only bundle capabilities allow suffixes. Reject encoded separators and
    // dot segments before URL normalization can move a request outside it.
    if !u.path().contains("/_s/")
        || !u.path().ends_with('/')
        || path
            .split('/')
            .any(|p| p == "." || p == ".." || p.is_empty())
        || path
            .bytes()
            .any(|b| b == b'%' || b == b'\\' || b == b'?' || b == b'#' || b.is_ascii_control())
    {
        return Err(denied());
    }
    let prefix = u.path().to_owned();
    u.set_path(&format!("{prefix}{path}"));
    Ok(u)
}

async fn serve(
    s: Shared,
    encoded: String,
    exp: i64,
    sig: String,
    path: String,
) -> ApiResult<Response> {
    let now = Utc::now().timestamp();
    let ctx = crate::store::artifacts::context(&s.state_dir, &s.config_dir)?;
    if encoded.len() > 16384
        || exp <= now
        || exp > now + 120
        || !ctx.verify(&format!("remote-artifact.v1.{encoded}"), exp, &sig, now)
    {
        return Err(denied());
    }
    let cap: Capability =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&encoded).map_err(|_| denied())?)
            .map_err(|_| denied())?;
    if s.operator_auth
        .store
        .current()
        .map_err(|_| denied())?
        .version
        != cap.pv
    {
        return Err(denied());
    }
    let (sid, jti) = (cap.sid, cap.jti);
    let revoked =
        s.db.read(move |c| {
            Ok(c.query_row(
                "SELECT 1 FROM revoked_sessions WHERE nonce IN (?1, ?2)",
                [&sid, &jti],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })
        .await?;
    if revoked {
        return Err(denied());
    }
    let r = load(&s, cap.remote).await?;
    if r.id != cap.remote_id || r.instance_id != cap.instance {
        return Err(denied());
    }
    let limits = s.remotes.get(&r.id);
    if *limits.closed.borrow() {
        return Err(net::unreachable());
    }
    let _permit = limits.acquire(false)?;
    let u = target(&cap.target, &path)?;
    let mut origin = r.clone();
    origin.url = format!("{}/", u.origin().ascii_serialization());
    // A separate TLS origin must prove possession of the linked remote's leaf.
    if u.scheme() == "https" && origin.pin_sha256.is_none() {
        return Err(denied());
    }
    let mode = destination_policy(&r.url, cap.artifact_origin.as_deref(), &u)?;
    let c = net::client_with_policy(&origin, &own_ports(&s), mode).await?;
    // Deliberately no cookies, Authorization, link key or incoming headers.
    let response = c
        .get(u)
        .timeout(net::PLAIN)
        .send()
        .await
        .map_err(net::connection_error)?;
    if response.status() != StatusCode::OK {
        return Err(net::unreachable());
    }
    let mime = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !allowed_type(&mime) {
        return Err(denied());
    }
    let bytes = net::limited(response, MAX, net::PLAIN).await?;
    let mut out = Body::from(bytes).into_response();
    let h = out.headers_mut();
    h.insert(header::CONTENT_TYPE, mime.parse().map_err(|_| denied())?);
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        artifacts::ARTIFACT_CSP.parse().map_err(|_| denied())?,
    );
    h.insert(
        header::CACHE_CONTROL,
        "no-store".parse().map_err(|_| denied())?,
    );
    h.insert(
        header::REFERRER_POLICY,
        "no-referrer".parse().map_err(|_| denied())?,
    );
    h.insert(
        "x-content-type-options",
        "nosniff".parse().map_err(|_| denied())?,
    );
    Ok(out)
}

fn allowed_type(raw: &str) -> bool {
    let mime = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "text/plain"
            | "text/html"
            | "text/markdown"
            | "text/css"
            | "text/javascript"
            | "application/javascript"
            | "application/json"
            | "application/pdf"
            | "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/svg+xml"
            | "application/octet-stream"
    )
}

/// The stored API URL is the operator's boundary. Only a literal private API
/// address opts this remote into private destinations. A DNS name must never
/// gain that right merely because its answer changed after linking.
fn destination_policy(
    remote: &str,
    declared: Option<&str>,
    target: &url::Url,
) -> ApiResult<crate::webhooks::net::PrivateMode> {
    use crate::webhooks::net::{PrivateMode, private_reason};
    let api = net::check_url(remote)?;
    if api.scheme() != target.scheme() {
        return Err(denied());
    }
    let declared = declared.filter(|s| !s.is_empty())
        .map(net::check_url).transpose()?;
    if api.origin() != target.origin()
        && declared.as_ref().is_none_or(|d| d.origin() != target.origin()) {
        return Err(denied());
    }
    let private = match api.host() {
        Some(url::Host::Ipv4(ip)) => private_reason(ip.into()).is_some(),
        Some(url::Host::Ipv6(ip)) => private_reason(ip.into()).is_some(),
        // localhost explicitly names loopback, unlike arbitrary DNS names.
        Some(url::Host::Domain("localhost")) => true,
        _ => false,
    };
    Ok(if private { PrivateMode::Allow } else { PrivateMode::Deny })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn destination_boundary_and_rebinding_policy_are_explicit() {
        use crate::webhooks::net::{self, PrivateMode};
        let api = "https://public.example/";
        let downgrade = url::Url::parse("http://public.example/a/U1").unwrap();
        assert!(destination_policy(api, None, &downgrade).is_err());
        let other = url::Url::parse("https://other.example/a/U1").unwrap();
        assert!(destination_policy(api, None, &other).is_err());
        let mode = destination_policy(api, Some("https://other.example/"), &other).unwrap();
        assert_eq!(mode, PrivateMode::Deny);
        let resolver: net::Resolver = std::sync::Arc::new(|_, _| vec!["127.0.0.1".parse().unwrap()]);
        assert!(net::resolve(&other, mode, &[], Some(resolver)).await.is_err());
        let private = url::Url::parse("https://10.0.0.2/a/U1").unwrap();
        assert_eq!(destination_policy("https://10.0.0.2/", None, &private).unwrap(), PrivateMode::Allow);
    }
    #[test]
    fn suffix_cannot_escape_one_artifact() {
        let base = "https://example.test/a/U1/_s/123/sig/";
        for path in [
            "../other",
            "%2e%2e/other",
            "x/../../other",
            "//evil",
            "x\\y",
            "x?sig=bad",
        ] {
            assert!(target(base, path).is_err(), "{path}");
        }
        assert_eq!(
            target(base, "assets/a.css").unwrap().path(),
            "/a/U1/_s/123/sig/assets/a.css"
        );
        assert!(target("https://example.test/a/U1/file?sig=x", "other").is_err());
    }
    #[test]
    fn unsafe_targets_and_types_are_rejected() {
        for raw in [
            "https://user@example.test/a/U1",
            "https://example.test/api/system",
            "https://example.test/a/U1#x",
        ] {
            assert!(check_target(&url::Url::parse(raw).unwrap()).is_err());
        }
        assert!(allowed_type("text/html; charset=utf-8"));
        assert!(!allowed_type("application/x-sh"));
        assert!(!allowed_type(""));
    }
}
