//! Dashboard sessions: `POST /api/session` trades the bearer token for an
//! `HttpOnly` cookie, so that the dashboard keeps no token that a script can
//! read (`docs/security-review.md`, F3). `docs/api.md` §1.1 is the contract.
//!
//! The cookie is stateless and signed:
//!
//! ```text
//! terrarium_session=v1.<exp>.<nonce>.<sig>
//! sig  = base64url(HMAC-SHA256(key, "terrarium-session.v1.<exp>.<nonce>"))
//! csrf = base64url(HMAC-SHA256(key, "terrarium-csrf.v1.<nonce>"))
//! ```
//!
//! `exp` is a Unix time, `nonce` 16 random bytes in base64url. The key is
//! 32 random bytes in `<state>/session.key` (hex, mode 0600). A new key
//! makes every session invalid. The server keeps no session rows.

use std::path::Path;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;

use crate::error::{ApiError, ApiResult, Code};

/// The name of the session cookie.
pub const COOKIE: &str = "terrarium_session";
/// The header that a cookie-authenticated `POST`, `PUT`, `PATCH`, or
/// `DELETE` must carry, with the `csrf` value of `POST /api/session`.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// The file of the signing key, in the state dir.
pub const KEY_FILE: &str = "session.key";

const VERSION: &str = "v1";

type HmacSha256 = Hmac<Sha256>;

/// The signing key of the session cookies.
#[derive(Clone)]
pub struct SessionKey {
    secret: Vec<u8>,
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(..)")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn random(n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

impl SessionKey {
    /// A key from its bytes (tests).
    pub fn from_bytes(secret: Vec<u8>) -> Self {
        Self { secret }
    }

    /// Read `<state>/session.key`, or make it: 32 random bytes as hex,
    /// mode 0600. A file that is not a valid key is replaced, which ends
    /// every session.
    pub fn load_or_create(state_dir: &Path) -> anyhow::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let path = state_dir.join(KEY_FILE);
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some(key) = unhex(text.trim())
            && key.len() >= 32
        {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            return Ok(Self { secret: key });
        }
        let key = random(32)?;
        std::fs::create_dir_all(state_dir)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(hex(&key).as_bytes())?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { secret: key })
    }

    fn mac(&self, msg: &str) -> String {
        match HmacSha256::new_from_slice(&self.secret) {
            Ok(mut m) => {
                m.update(msg.as_bytes());
                URL_SAFE_NO_PAD.encode(m.finalize().into_bytes())
            }
            Err(_) => String::new(),
        }
    }

    fn sig(&self, exp: i64, nonce: &str) -> String {
        self.mac(&format!("terrarium-session.{VERSION}.{exp}.{nonce}"))
    }

    fn csrf(&self, nonce: &str) -> String {
        self.mac(&format!("terrarium-csrf.{VERSION}.{nonce}"))
    }

    /// A new session that ends at the Unix time `exp`.
    pub fn mint(&self, exp: i64) -> std::io::Result<Session> {
        let nonce = URL_SAFE_NO_PAD.encode(random(16)?);
        Ok(Session {
            cookie: format!("{VERSION}.{exp}.{nonce}.{}", self.sig(exp, &nonce)),
            csrf: self.csrf(&nonce),
            exp,
        })
    }

    /// The session of a cookie value, if the signature is right and the
    /// session has not ended at the Unix time `now`.
    pub fn verify(&self, cookie: &str, now: i64) -> Option<Session> {
        let mut it = cookie.split('.');
        let (Some(v), Some(exp), Some(nonce), Some(sig), None) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            return None;
        };
        if v != VERSION || nonce.is_empty() {
            return None;
        }
        let exp: i64 = exp.parse().ok()?;
        if !super::token_matches(sig.as_bytes(), self.sig(exp, nonce).as_bytes()) || exp <= now {
            return None;
        }
        Some(Session {
            cookie: cookie.to_owned(),
            csrf: self.csrf(nonce),
            exp,
        })
    }
}

/// One session: the cookie value, its CSRF token, and its end.
#[derive(Debug, Clone)]
pub struct Session {
    pub cookie: String,
    pub csrf: String,
    pub exp: i64,
}

impl Session {
    /// Does `given` equal the CSRF token? Constant time.
    pub fn csrf_matches(&self, given: &str) -> bool {
        super::token_matches(given.as_bytes(), self.csrf.as_bytes())
    }
}

/// The sessions of the TCP listener: the key and the cookie life.
#[derive(Debug, Clone)]
pub struct Sessions {
    pub key: SessionKey,
    pub ttl: std::time::Duration,
}

/// The value of the cookie `name` in the `Cookie` headers.
pub fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k.trim() == name).then(|| v.trim().trim_matches('"'))
        })
}

/// Is the request on HTTPS: the URI scheme, or `X-Forwarded-Proto: https`
/// from a proxy?
pub fn is_https(req_scheme: Option<&str>, headers: &HeaderMap) -> bool {
    req_scheme.is_some_and(|s| s.eq_ignore_ascii_case("https"))
        || headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
}

fn set_cookie(value: &str, max_age: u64, secure: bool) -> String {
    let mut c = format!("{COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict");
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// How the middleware let a request in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVia {
    /// `Authorization: Bearer`, or the WebSocket subprotocol or query.
    Bearer,
    /// The session cookie.
    Cookie,
}

#[derive(Debug, Serialize)]
pub struct SessionAnswer {
    /// Send it as `X-CSRF-Token` on every `POST`, `PUT`, `PATCH`, and
    /// `DELETE` that the cookie authenticates.
    pub csrf: String,
    pub exp: i64,
    pub expires_at: String,
}

fn no_sessions() -> ApiError {
    ApiError::not_found("sessions are on the TCP listener only")
}

fn with_cookie(mut resp: Response, cookie: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// `POST /api/session`: the bearer token in `Authorization` buys a cookie.
pub async fn create(
    sessions: Option<Extension<Arc<Sessions>>>,
    via: Option<Extension<AuthVia>>,
    req: Request,
) -> ApiResult<Response> {
    let Some(Extension(sessions)) = sessions else {
        return Err(no_sessions());
    };
    if via.map(|Extension(v)| v) != Some(AuthVia::Bearer) {
        return Err(ApiError::new(
            Code::Unauthorized,
            "POST /api/session needs the bearer token in `Authorization`",
        ));
    }
    let ttl = sessions.ttl.as_secs().max(1);
    let exp = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX));
    let s = sessions
        .key
        .mint(exp)
        .map_err(|e| ApiError::internal(format!("cannot make a session: {e}")))?;
    let secure = is_https(req.uri().scheme_str(), req.headers());
    let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default();
    let body = Json(SessionAnswer {
        csrf: s.csrf,
        exp,
        expires_at,
    });
    Ok(with_cookie(
        (StatusCode::CREATED, body).into_response(),
        &set_cookie(&s.cookie, ttl, secure),
    ))
}

/// `DELETE /api/session`: clear the cookie. The auth middleware lets it
/// through, so that a client that lost its cookie can still log out.
///
/// - No cookie (a bearer client, or a browser that has no session): `204`
///   and the clearing `Set-Cookie`.
/// - A valid session cookie: it must send `X-CSRF-Token` of that session,
///   or the answer is `403` and the cookie stays. This stops a logout CSRF.
/// - A cookie that is wrong or past `exp`: `204`. It is no session, so to
///   clear it ends nothing.
pub async fn delete(
    sessions: Option<Extension<Arc<Sessions>>>,
    req: Request,
) -> ApiResult<Response> {
    let Some(Extension(sessions)) = sessions else {
        return Err(no_sessions());
    };
    let headers = req.headers();
    if let Some(cookie) = cookie_value(headers, COOKIE)
        && let Some(s) = sessions.key.verify(cookie, chrono::Utc::now().timestamp())
    {
        let ok = headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| s.csrf_matches(t));
        if !ok {
            return Err(ApiError::new(
                Code::Forbidden,
                "DELETE /api/session with the session cookie must send X-CSRF-Token with the \
                 csrf value of POST /api/session",
            ));
        }
    }
    let secure = is_https(req.uri().scheme_str(), headers);
    Ok(with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        &set_cookie("", 0, secure),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn mint_and_verify() {
        let k = SessionKey::from_bytes(vec![7; 32]);
        let s = k.mint(1000).unwrap();
        let back = k.verify(&s.cookie, 999).unwrap();
        assert_eq!(back.csrf, s.csrf);
        assert!(back.csrf_matches(&s.csrf));
        assert!(!back.csrf_matches("nope"));
        // Ended.
        assert!(k.verify(&s.cookie, 1000).is_none());
        // A changed exp breaks the signature.
        let forged = s.cookie.replacen("1000", "9999", 1);
        assert!(k.verify(&forged, 999).is_none());
        // Another key.
        assert!(
            SessionKey::from_bytes(vec![8; 32])
                .verify(&s.cookie, 999)
                .is_none()
        );
        assert!(k.verify("v1.1000", 0).is_none());
        assert!(k.verify("", 0).is_none());
    }

    #[test]
    fn key_file_is_0600_and_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let a = SessionKey::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let b = SessionKey::load_or_create(dir.path()).unwrap();
        assert_eq!(a.secret, b.secret);
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            HeaderValue::from_static("a=1; terrarium_session=v1.x"),
        );
        h.append(header::COOKIE, HeaderValue::from_static("b=2"));
        assert_eq!(cookie_value(&h, COOKIE), Some("v1.x"));
        assert_eq!(cookie_value(&h, "b"), Some("2"));
        assert_eq!(cookie_value(&h, "c"), None);
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(is_https(None, &h));
        assert!(!is_https(None, &HeaderMap::new()));
    }
}
