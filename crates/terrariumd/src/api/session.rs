//! Operator sessions (`docs/api.md` §1.1, `docs/security-review.md` F3 and
//! F15). `POST /api/session` trades the operator password, or the bearer
//! token, for a session token: a JWT in an `HttpOnly` cookie for the
//! dashboard, or in the body for the CLI (`terrarium login`). The dashboard
//! keeps no credential that a script can read.
//!
//! The token is a compact JWT, HS256 only, with a fixed header:
//!
//! ```text
//! header  = {"alg":"HS256","typ":"JWT"}
//! claims  = {"sub":"operator","iat","exp","jti","sid","pv","kind"}
//! sig     = HMAC-SHA256(jwt_key, b64(header) "." b64(claims))
//! jwt_key = HMAC-SHA256(key, "terrarium-jwt.v2")
//! csrf    = base64url(HMAC-SHA256(key, "terrarium-csrf.v2.<sid>"))
//! ```
//!
//! The key is 32 random bytes in `<state>/session.key` (hex, mode 0600). A
//! new key makes every token invalid. `jti` is new for each token. `sid`
//! names the session: the `jti` of its first token. A renewed token keeps
//! the `sid`, so its CSRF value stays the same. `pv` is the password
//! version (`crate::password`): a new password or a "log out everywhere"
//! ends every token with an older `pv`. `kind` is `browser` (the cookie,
//! with CSRF) or `cli` (`Authorization: Bearer`, no CSRF); each kind works
//! on its own path only.
//!
//! The server keeps no session rows, only the sessions that a logout ended
//! (`revoked_sessions`, keyed by `sid`), until their last token ends.

use std::path::Path;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;

use rusqlite::OptionalExtension;

use super::Shared;
use crate::actor::Actor;
use crate::db::Db;
use crate::error::{ApiError, ApiResult, Code};
use crate::password::{FailReport, OperatorAuth, PasswordError, Refusal, Verdict};

/// The name of the session cookie.
pub const COOKIE: &str = "terrarium_session";
/// The header that a cookie-authenticated `POST`, `PUT`, `PATCH`, or
/// `DELETE` must carry, with the `csrf` value of the session.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// The file of the signing key, in the state dir.
pub const KEY_FILE: &str = "session.key";

/// The one header the daemon makes and takes.
const JWT_HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;
/// The only subject: Terrarium has one operator.
const SUBJECT: &str = "operator";
/// The message for every failed password login: the same text for each
/// cause, so that the answer tells an attacker nothing.
pub const LOGIN_FAILED: &str = "login failed";

type HmacSha256 = Hmac<Sha256>;

/// The signing key of the session tokens.
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

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    match HmacSha256::new_from_slice(key) {
        Ok(mut m) => {
            m.update(msg);
            m.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// The two kinds of session token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// The dashboard: the cookie, with CSRF.
    Browser,
    /// The CLI: `Authorization: Bearer`, no CSRF.
    Cli,
}

/// The claims of a session token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
    /// New for each token.
    pub jti: String,
    /// The session: the `jti` of its first token. The CSRF value is an
    /// HMAC of it, and a logout revokes it.
    pub sid: String,
    /// The password version at the mint.
    pub pv: u64,
    pub kind: TokenKind,
    /// When the operator logged in (a Unix time). A renewal copies it, so
    /// a session cannot outlive `TERRARIUM_SESSION_MAX_AGE`.
    pub auth_time: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    typ: Option<String>,
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

    fn jwt_key(&self) -> Vec<u8> {
        hmac(&self.secret, b"terrarium-jwt.v2")
    }

    fn sign(&self, signing_input: &str) -> String {
        URL_SAFE_NO_PAD.encode(hmac(&self.jwt_key(), signing_input.as_bytes()))
    }

    /// The CSRF value of the session `sid`.
    pub fn csrf(&self, sid: &str) -> String {
        URL_SAFE_NO_PAD.encode(hmac(
            &self.secret,
            format!("terrarium-csrf.v2.{sid}").as_bytes(),
        ))
    }

    /// Sign `claims` as a JWT.
    pub fn encode(&self, claims: &Claims) -> String {
        let header = URL_SAFE_NO_PAD.encode(JWT_HEADER);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
        let input = format!("{header}.{payload}");
        let sig = self.sign(&input);
        format!("{input}.{sig}")
    }

    /// A new token of `kind` from `now` for `ttl` seconds, with the
    /// password version `pv`. `renew`: the `sid` and `auth_time` of the
    /// session it renews; `None` starts a new session at `now`.
    pub fn mint(
        &self,
        kind: TokenKind,
        renew: Option<(&str, i64)>,
        now: i64,
        ttl: i64,
        pv: u64,
    ) -> std::io::Result<Session> {
        let jti = URL_SAFE_NO_PAD.encode(random(16)?);
        let claims = Claims {
            sub: SUBJECT.to_owned(),
            iat: now,
            exp: now.saturating_add(ttl.max(1)),
            sid: renew.map_or_else(|| jti.clone(), |(sid, _)| sid.to_owned()),
            jti,
            pv,
            kind,
            auth_time: renew.map_or(now, |(_, at)| at),
        };
        Ok(self.session(self.encode(&claims), claims))
    }

    fn session(&self, token: String, claims: Claims) -> Session {
        let csrf = (claims.kind == TokenKind::Browser).then(|| self.csrf(&claims.sid));
        Session {
            token,
            claims,
            csrf,
        }
    }

    /// The claims of a token, if the token is a JWT that this key signed
    /// with HS256, and it has not ended at the Unix time `now`. The checks:
    /// three parts; a header of `alg` `HS256` (and `typ` `JWT`, if any) and
    /// nothing else, so `none` and every other algorithm fail; the
    /// signature, in constant time; then the claims: the subject, `iat`
    /// before `exp`, and `exp` after `now`.
    pub fn verify(&self, token: &str, now: i64) -> Option<Session> {
        let mut it = token.split('.');
        let (Some(h), Some(p), Some(sig), None) = (it.next(), it.next(), it.next(), it.next())
        else {
            return None;
        };
        let header: JwtHeader = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h).ok()?).ok()?;
        if header.alg != "HS256" || header.typ.as_deref().is_some_and(|t| t != "JWT") {
            return None;
        }
        let expected = self.sign(&format!("{h}.{p}"));
        if !super::token_matches(sig.as_bytes(), expected.as_bytes()) {
            return None;
        }
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).ok()?).ok()?;
        if claims.sub != SUBJECT
            || claims.jti.is_empty()
            || claims.sid.is_empty()
            || claims.iat > claims.exp
            || claims.auth_time > claims.iat
            || claims.exp <= now
        {
            return None;
        }
        Some(self.session(token.to_owned(), claims))
    }
}

/// One session token: the JWT, its claims, and (browser) its CSRF value.
#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub claims: Claims,
    /// `Some` for a browser token.
    pub csrf: Option<String>,
}

impl Session {
    /// Does `given` equal the CSRF value? Constant time. A CLI token has
    /// none, so nothing matches.
    pub fn csrf_matches(&self, given: &str) -> bool {
        self.csrf
            .as_deref()
            .is_some_and(|c| super::token_matches(given.as_bytes(), c.as_bytes()))
    }

    /// Is the browser token past half its life at `now`? Then the daemon
    /// sets a renewed cookie on the answer.
    pub fn needs_renewal(&self, now: i64) -> bool {
        let c = &self.claims;
        c.kind == TokenKind::Browser && now >= c.iat + (c.exp - c.iat) / 2
    }
}

/// The sessions of the TCP listener: the key, the token lives, the store
/// of the revoked sessions, and the operator auth (the password version).
#[derive(Clone)]
pub struct Sessions {
    pub key: SessionKey,
    /// The life of a browser token (`TERRARIUM_SESSION_TTL`).
    pub ttl: std::time::Duration,
    /// The life of a CLI token (`TERRARIUM_CLI_SESSION_TTL`).
    pub cli_ttl: std::time::Duration,
    /// The longest life of a browser session through its renewals
    /// (`TERRARIUM_SESSION_MAX_AGE`), counted from `auth_time`.
    pub max_age: std::time::Duration,
    pub db: Db,
    pub operator: Arc<OperatorAuth>,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("ttl", &self.ttl)
            .field("cli_ttl", &self.cli_ttl)
            .field("max_age", &self.max_age)
            .finish_non_exhaustive()
    }
}

fn secs(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_secs().max(1)).unwrap_or(i64::MAX)
}

fn password_error(e: &PasswordError) -> ApiError {
    tracing::error!("operator password: {e}");
    ApiError::internal("cannot read the operator password file; see the daemon log")
}

impl Sessions {
    fn ttl_of(&self, kind: TokenKind) -> i64 {
        match kind {
            TokenKind::Browser => secs(self.ttl),
            TokenKind::Cli => secs(self.cli_ttl),
        }
    }

    /// The current password version.
    pub fn version(&self) -> ApiResult<u64> {
        self.operator
            .store
            .current()
            .map(|s| s.version)
            .map_err(|e| password_error(&e))
    }

    /// A new token of `kind` at the Unix time `now`, for the password
    /// version `pv`: the version that the login checked, or the `pv` of the
    /// token it renews. When the version changed since (a new password, a
    /// "log out everywhere"), no token: `409`. `renew`: the claims of the
    /// session it renews; a browser session never lives past `auth_time` +
    /// [`Self::max_age`].
    pub fn mint(
        &self,
        kind: TokenKind,
        renew: Option<&Claims>,
        now: i64,
        pv: u64,
    ) -> ApiResult<Session> {
        if self.version()? != pv {
            return Err(ApiError::conflict(
                "the password changed while this login ran; log in again",
            ));
        }
        let auth_time = renew.map_or(now, |c| c.auth_time);
        let mut ttl = self.ttl_of(kind);
        if kind == TokenKind::Browser {
            ttl = ttl.min(auth_time.saturating_add(secs(self.max_age)) - now);
            if ttl <= 0 {
                return Err(ApiError::new(
                    Code::Unauthorized,
                    "the session is past TERRARIUM_SESSION_MAX_AGE; log in again",
                ));
            }
        }
        self.key
            .mint(
                kind,
                renew.map(|c| (c.sid.as_str(), c.auth_time)),
                now,
                ttl,
                pv,
            )
            .map_err(|e| ApiError::internal(format!("cannot make a session: {e}")))
    }

    /// The session of a token of `kind`: the signature is right, the token
    /// has not ended at the Unix time `now`, its `pv` is the current
    /// password version, a browser session is not older than
    /// [`Self::max_age`], and no logout revoked its `jti` or `sid`.
    pub async fn check(
        &self,
        token: &str,
        kind: TokenKind,
        now: i64,
    ) -> ApiResult<Option<Session>> {
        let Some(s) = self.key.verify(token, now) else {
            return Ok(None);
        };
        if s.claims.kind != kind || s.claims.pv != self.version()? {
            return Ok(None);
        }
        if kind == TokenKind::Browser && now.saturating_sub(s.claims.auth_time) > secs(self.max_age)
        {
            return Ok(None);
        }
        let (jti, sid) = (s.claims.jti.clone(), s.claims.sid.clone());
        let revoked = self
            .db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT 1 FROM revoked_sessions WHERE nonce IN (?1, ?2)",
                    [&jti, &sid],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
            })
            .await?;
        Ok((!revoked).then_some(s))
    }

    /// End the session of `s` (its `sid`, so every renewed token of it)
    /// before its end, and prune the revocations past their end (at the
    /// Unix time `now`). The row lives as long as the longest token of the
    /// session can.
    pub async fn revoke(&self, s: &Session, now: i64) -> ApiResult<()> {
        let sid = s.claims.sid.clone();
        let exp = s.claims.exp.max(
            now.saturating_add(
                self.ttl_of(TokenKind::Browser)
                    .max(self.ttl_of(TokenKind::Cli)),
            ),
        );
        self.db
            .write("operator", move |t| {
                t.execute("DELETE FROM revoked_sessions WHERE exp <= ?1", [now])?;
                t.execute(
                    "INSERT OR IGNORE INTO revoked_sessions (nonce, exp) VALUES (?1, ?2)",
                    rusqlite::params![sid, exp],
                )?;
                Ok(())
            })
            .await
    }
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

/// The `Authorization: Bearer` value, if any.
pub fn bearer_value(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
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

/// The `Set-Cookie` value of a session token (or of `""` with `max_age` 0,
/// which clears the cookie). Persistent: `Max-Age` is the token life.
pub fn set_cookie(value: &str, max_age: i64, secure: bool) -> String {
    let mut c = format!(
        "{COOKIE}={value}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict",
        max_age.max(0)
    );
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// The `Set-Cookie` of the session `s`, with the time left as `Max-Age`.
pub fn session_cookie(s: &Session, now: i64, secure: bool) -> String {
    set_cookie(&s.token, s.claims.exp - now, secure)
}

/// How the middleware let a request in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVia {
    /// The static bearer token: `Authorization: Bearer`, or the WebSocket
    /// subprotocol or query.
    Bearer,
    /// A CLI session token in `Authorization: Bearer`.
    CliToken,
    /// The session cookie.
    Cookie,
    /// A link key (`tlk_`, `docs/api.md` §36).
    Link,
}

/// The session of a request that a session token let in, as a request
/// extension.
#[derive(Debug, Clone)]
pub struct CurrentSession(pub Session);

/// The answer of `POST /api/session` and `GET /api/session`.
#[derive(Debug, Serialize)]
pub struct SessionAnswer {
    /// `browser`, `cli`, `token` (the static bearer token), `link` (a link
    /// key, `docs/api.md` §36), or `local` (the Unix socket).
    pub kind: &'static str,
    /// Browser: send it as `X-CSRF-Token` on every `POST`, `PUT`, `PATCH`,
    /// and `DELETE` that the cookie authenticates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csrf: Option<String>,
    /// CLI: the token, for `Authorization: Bearer`. Only `POST` gives it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Is an operator password set? `GET` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_set: Option<bool>,
}

fn rfc3339(t: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

impl SessionAnswer {
    fn of(s: &Session, with_token: bool) -> Self {
        Self {
            kind: match s.claims.kind {
                TokenKind::Browser => "browser",
                TokenKind::Cli => "cli",
            },
            csrf: s.csrf.clone(),
            token: with_token.then(|| s.token.clone()),
            exp: Some(s.claims.exp),
            expires_at: Some(rfc3339(s.claims.exp)),
            password_set: None,
        }
    }
}

fn no_sessions() -> ApiError {
    ApiError::not_found("sessions are on the TCP listener only")
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Add a `Set-Cookie` to an answer, and `Cache-Control: no-store`.
pub fn with_cookie(mut resp: Response, cookie: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    no_store(resp)
}

fn clear_cookie(req_scheme: Option<&str>, headers: &HeaderMap) -> String {
    set_cookie("", 0, is_https(req_scheme, headers))
}

/// The body of `POST /api/session`. Empty with the bearer token.
#[derive(Debug, Default, Deserialize)]
pub struct LoginBody {
    #[serde(default)]
    pub password: Option<String>,
    /// `cli`: answer the token in the body, with no cookie.
    #[serde(default)]
    pub client: Option<String>,
}

/// A `429 rate_limited` with `Retry-After` (whole seconds, at least 1).
fn rate_limited(r: Refusal) -> Response {
    let wait = r.wait();
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let secs = secs.max(1);
    let why = match r {
        Refusal::Locked { global: false, .. } => "too many failed logins from this address",
        Refusal::Locked { global: true, .. } => {
            "too many failed logins from all addresses; the password login is paused (the \
             static token still works)"
        }
        Refusal::Busy { .. } => "too many logins in progress",
    };
    let mut resp = ApiError::new(Code::RateLimited, format!("{why}; try again in {secs} s"))
        .with_details(json!({ "retry_after": secs }))
        .into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    resp
}

/// Write the `auth.login` audit row of a good login. Never the password.
/// Failures go to [`audit_failure`].
async fn audit_login(state: &Shared, ip: &str, method: &str, kind: TokenKind) {
    let detail = json!({ "ok": true, "ip": ip, "method": method, "kind": kind });
    if let Err(e) = state
        .db
        .write("operator", move |t| {
            t.audit("auth.login", "operator", None, detail)
        })
        .await
    {
        tracing::warn!("cannot write the auth.login audit row: {e}");
    }
}

/// Write a failed password check: one `auth.login_failed` row per address
/// per window, with a count (a new row, or the count of the row goes up),
/// and an `auth.lockout` row when a lockout starts. Never the password.
async fn audit_failure(state: &Shared, r: FailReport, ip: &str) {
    let detail = json!({
        "ip": r.key, "last_ip": ip, "count": r.count,
        "window_secs": crate::password::AUDIT_WINDOW.as_secs(),
    });
    let ip = ip.to_owned();
    let res = state
        .db
        .write("anonymous", move |t| {
            if r.new_row {
                t.audit_with_id(&r.audit_id, "auth.login_failed", "operator", None, detail)?;
            } else {
                t.audit_update(&r.audit_id, detail)?;
            }
            if let Some(d) = r.lockout {
                t.audit(
                    "auth.lockout",
                    "operator",
                    None,
                    json!({ "scope": "address", "ip": r.key, "last_ip": ip, "seconds": d.as_secs() }),
                )?;
            }
            if let Some(d) = r.global_lockout {
                t.audit(
                    "auth.lockout",
                    "operator",
                    None,
                    json!({ "scope": "global", "seconds": d.as_secs() }),
                )?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = res {
        tracing::warn!("cannot write the auth.login_failed audit row: {e}");
    }
}

/// Check a password from the client at `ip` with the login guard and the
/// hash. `Ok(version)`: it matches the hash of that password version; mint
/// with it. `Err`: the answer to send (`429`, `409`, or `failed`), with the
/// failure counted and audited.
pub(crate) async fn check_password(
    state: &Shared,
    ip: &str,
    password: String,
    failed: impl FnOnce() -> Response,
) -> Result<u64, Response> {
    match state.operator_auth.check(ip, password).await {
        Err(e) => Err(password_error(&e).into_response()),
        Ok(Verdict::NoPassword) => {
            Err(ApiError::conflict(crate::password::NO_PASSWORD).into_response())
        }
        Ok(Verdict::Good { version }) => Ok(version),
        Ok(Verdict::Wrong(report)) => {
            audit_failure(state, report, ip).await;
            Err(failed())
        }
        Ok(Verdict::Refused(r)) => Err(rate_limited(r)),
    }
}

/// The largest body that `POST /api/session` and the password routes read.
const MAX_AUTH_BODY: usize = 16 * 1024;

/// Read a JSON body; an empty body is `T::default()`. A body needs
/// `Content-Type: application/json`, so a cross-site form (which cannot
/// set it without a CORS preflight) cannot post one.
pub(crate) async fn read_json<T: serde::de::DeserializeOwned + Default>(
    req: Request,
) -> ApiResult<T> {
    let json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    let bytes = axum::body::to_bytes(req.into_body(), MAX_AUTH_BODY)
        .await
        .map_err(|_| ApiError::bad_request("the body is too large or cannot be read"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    if !json {
        return Err(ApiError::bad_request(
            "the body needs `Content-Type: application/json`",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|e| ApiError::bad_request(format!("bad JSON: {e}")))
}

/// `POST /api/session`: log in.
///
/// - `Authorization: Bearer <TERRARIUM_TOKEN>`: a browser session (the
///   cookie), or a CLI token with `{"client": "cli"}`.
/// - No `Authorization`, and `{"password": "..."}`: the same, with the
///   operator password. The login limiter counts each failure per client
///   address. Every wrong password gets the same `401`.
/// - A CLI token or a cookie cannot buy another session: `401`.
pub async fn create(
    State(state): State<Shared>,
    sessions: Option<Extension<Arc<Sessions>>>,
    via: Option<Extension<AuthVia>>,
    req: Request,
) -> ApiResult<Response> {
    let Some(Extension(sessions)) = sessions else {
        return Err(no_sessions());
    };
    let via = via.map(|Extension(v)| v);
    let secure = is_https(req.uri().scheme_str(), req.headers());
    let ip = super::client_addr(&req);
    let body: LoginBody = read_json(req).await?;
    let kind = match body.client.as_deref() {
        Some("cli") => TokenKind::Cli,
        None | Some("browser") => TokenKind::Browser,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "`client` is `cli` or `browser`, not `{other}`"
            )));
        }
    };
    let (method, pv) = match (via, body.password) {
        (Some(AuthVia::Bearer), _) => ("token", sessions.version()?),
        (None, Some(password)) => {
            match check_password(&state, &ip, password, || {
                ApiError::new(Code::Unauthorized, LOGIN_FAILED).into_response()
            })
            .await
            {
                Ok(version) => ("password", version),
                Err(resp) => return Ok(resp),
            }
        }
        _ => {
            return Err(ApiError::new(
                Code::Unauthorized,
                "POST /api/session needs the bearer token in `Authorization`, or \
                 {\"password\": ...} with no `Authorization`",
            ));
        }
    };
    let now = chrono::Utc::now().timestamp();
    let s = sessions.mint(kind, None, now, pv)?;
    audit_login(&state, &ip, method, kind).await;
    let created = |a: SessionAnswer| (StatusCode::CREATED, Json(a)).into_response();
    Ok(match kind {
        TokenKind::Browser => with_cookie(
            created(SessionAnswer::of(&s, false)),
            &session_cookie(&s, now, secure),
        ),
        TokenKind::Cli => no_store(created(SessionAnswer::of(&s, true))),
    })
}

/// `GET /api/session`: the session of this request. The dashboard calls it
/// at load: a `200` means the cookie is good, and gives the CSRF value, so
/// the page keeps no copy of it. A renewal by the middleware shows here
/// already (the new `exp`).
pub async fn current(
    State(state): State<Shared>,
    via: Option<Extension<AuthVia>>,
    session: Option<Extension<CurrentSession>>,
) -> ApiResult<Response> {
    let password_set = state
        .operator_auth
        .store
        .current()
        .map_err(|e| password_error(&e))?
        .is_set();
    let mut answer = match (session, via) {
        (Some(Extension(CurrentSession(s))), _) => SessionAnswer::of(&s, false),
        (None, Some(Extension(AuthVia::Link))) => SessionAnswer {
            kind: "link",
            csrf: None,
            token: None,
            exp: None,
            expires_at: None,
            password_set: None,
        },
        (None, Some(Extension(_))) => SessionAnswer {
            kind: "token",
            csrf: None,
            token: None,
            exp: None,
            expires_at: None,
            password_set: None,
        },
        (None, None) => SessionAnswer {
            kind: "local",
            csrf: None,
            token: None,
            exp: None,
            expires_at: None,
            password_set: None,
        },
    };
    answer.password_set = Some(password_set);
    Ok(no_store(Json(answer).into_response()))
}

/// `DELETE /api/session`: end this session and clear the cookie. The auth
/// middleware lets it through, so that a client that lost its cookie can
/// still log out.
///
/// - A CLI token in `Authorization: Bearer`: revoked. `204`.
/// - No cookie (a bearer client, or a browser that has no session): `204`
///   and the clearing `Set-Cookie`.
/// - A valid session cookie: it must send `X-CSRF-Token` of that session,
///   or the answer is `403` and the cookie stays. This stops a logout CSRF.
///   With the token, the session is revoked: a copy of the cookie (and of
///   each renewed cookie) gets `401`.
/// - A cookie that is wrong, revoked, or past `exp`: `204`. It is no
///   session, so to clear it ends nothing.
pub async fn delete(
    sessions: Option<Extension<Arc<Sessions>>>,
    req: Request,
) -> ApiResult<Response> {
    let Some(Extension(sessions)) = sessions else {
        return Err(no_sessions());
    };
    let headers = req.headers();
    let now = chrono::Utc::now().timestamp();
    if let Some(token) = bearer_value(headers)
        && let Some(s) = sessions.check(token, TokenKind::Cli, now).await?
    {
        sessions.revoke(&s, now).await?;
    }
    let session = match cookie_value(headers, COOKIE) {
        Some(cookie) => sessions.check(cookie, TokenKind::Browser, now).await?,
        None => None,
    };
    if let Some(s) = session {
        let ok = headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| s.csrf_matches(t));
        if !ok {
            return Err(ApiError::new(
                Code::Forbidden,
                "DELETE /api/session with the session cookie must send X-CSRF-Token with the \
                 csrf value of the session",
            ));
        }
        sessions.revoke(&s, now).await?;
    }
    Ok(with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        &clear_cookie(req.uri().scheme_str(), headers),
    ))
}

fn operator_only(actor: &Actor) -> ApiResult<()> {
    if actor.is_operator() {
        Ok(())
    } else {
        Err(ApiError::forbidden("only the operator manages the login"))
    }
}

/// `DELETE /api/sessions`: log out everywhere. The password version goes
/// up by one, so every session token (browser and CLI) ends. The static
/// bearer token stays good. With the cookie, the middleware wants
/// `X-CSRF-Token`, as on every `DELETE`.
pub async fn delete_all(
    State(state): State<Shared>,
    actor: Actor,
    req: Request,
) -> ApiResult<Response> {
    operator_only(&actor)?;
    let op = state.operator_auth.clone();
    let version = tokio::task::spawn_blocking(move || op.store.bump())
        .await
        .map_err(|e| ApiError::internal(format!("the password task failed: {e}")))?
        .map_err(|e| password_error(&e))?;
    let ip = super::client_addr(&req);
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "auth.logout_all",
                "operator",
                None,
                json!({ "ip": ip, "version": version }),
            )
        })
        .await?;
    Ok(with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        &clear_cookie(req.uri().scheme_str(), req.headers()),
    ))
}

/// The body of `PUT /api/auth/password`.
#[derive(Debug, Default, Deserialize)]
pub struct PasswordBody {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub new: String,
}

/// `PUT /api/auth/password`: set the operator password.
///
/// - The static bearer token, or the Unix socket: `current` is not needed.
///   If it is sent and a password is set, it must match.
/// - A session (the cookie, or a CLI token): `current` must match, when a
///   password is set. A wrong one is `403` (not `401`: the session is
///   still good), and the login limiter counts it.
///
/// `new` needs 12 characters or more (`422 invalid`). The password version
/// goes up by one, so every session ends, also the caller's.
pub async fn set_password(
    State(state): State<Shared>,
    actor: Actor,
    via: Option<Extension<AuthVia>>,
    req: Request,
) -> ApiResult<Response> {
    operator_only(&actor)?;
    let via = via.map(|Extension(v)| v);
    let ip = super::client_addr(&req);
    let scheme = req.uri().scheme_str().map(str::to_owned);
    let headers = req.headers().clone();
    let body: PasswordBody = read_json(req).await?;
    crate::password::check_new(&body.new).map_err(|e| ApiError::invalid(e.to_string()))?;
    let op = state.operator_auth.clone();
    let is_set = op.store.current().map_err(|e| password_error(&e))?.is_set();
    let session = matches!(via, Some(AuthVia::Cookie | AuthVia::CliToken));
    if is_set {
        match body.current {
            Some(current) => {
                if let Err(resp) = check_password(&state, &ip, current, || {
                    ApiError::forbidden("the current password is wrong").into_response()
                })
                .await
                {
                    return Ok(resp);
                }
            }
            None if session => {
                return Err(ApiError::forbidden(
                    "a session must send the current password as `current`",
                ));
            }
            None => {}
        }
    }
    let new = body.new;
    let store_op = op.clone();
    let version = tokio::task::spawn_blocking(move || store_op.store.set(&new))
        .await
        .map_err(|e| ApiError::internal(format!("the password task failed: {e}")))?
        .map_err(|e| match e {
            PasswordError::TooShort | PasswordError::TooLong => ApiError::invalid(e.to_string()),
            e => password_error(&e),
        })?;
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "auth.password_set",
                "operator",
                None,
                json!({ "ip": ip, "version": version }),
            )
        })
        .await?;
    let resp = StatusCode::NO_CONTENT.into_response();
    Ok(if via == Some(AuthVia::Cookie) {
        with_cookie(resp, &clear_cookie(scheme.as_deref(), &headers))
    } else {
        no_store(resp)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn key() -> SessionKey {
        SessionKey::from_bytes(vec![7; 32])
    }

    fn b64(v: &str) -> String {
        URL_SAFE_NO_PAD.encode(v)
    }

    #[test]
    fn mint_and_verify() {
        let k = key();
        let s = k.mint(TokenKind::Browser, None, NOW, 100, 3).unwrap();
        assert_eq!(s.token.split('.').count(), 3);
        let back = k.verify(&s.token, NOW + 99).unwrap();
        assert_eq!(back.claims, s.claims);
        assert_eq!(back.claims.sid, back.claims.jti);
        assert_eq!(back.claims.pv, 3);
        assert_eq!(back.csrf, s.csrf);
        assert!(back.csrf_matches(s.csrf.as_deref().unwrap()));
        assert!(!back.csrf_matches("nope"));
        // Ended.
        assert!(k.verify(&s.token, NOW + 100).is_none());
        // Another key.
        assert!(
            SessionKey::from_bytes(vec![8; 32])
                .verify(&s.token, NOW)
                .is_none()
        );
        assert!(k.verify("a.b", NOW).is_none());
        assert!(k.verify("", NOW).is_none());
        // The v1 cookie of older daemons is no token.
        assert!(k.verify("v1.9999999999.bm9uY2U.c2ln", NOW).is_none());
        // A CLI token has no CSRF value.
        let c = k.mint(TokenKind::Cli, None, NOW, 100, 0).unwrap();
        assert!(c.csrf.is_none());
        assert!(!c.csrf_matches(""));
    }

    /// A renewal keeps the `sid`, so the CSRF value stays.
    #[test]
    fn renewal_keeps_the_csrf_binding() {
        let k = key();
        let a = k.mint(TokenKind::Browser, None, NOW, 100, 0).unwrap();
        let b = k
            .mint(
                TokenKind::Browser,
                Some((&a.claims.sid, a.claims.auth_time)),
                NOW + 60,
                100,
                0,
            )
            .unwrap();
        assert_ne!(a.claims.jti, b.claims.jti);
        assert_eq!(a.claims.sid, b.claims.sid);
        assert_eq!(a.csrf, b.csrf);
        assert!(!a.needs_renewal(NOW + 49));
        assert!(a.needs_renewal(NOW + 50));
        let c = k.mint(TokenKind::Cli, None, NOW, 100, 0).unwrap();
        assert!(!c.needs_renewal(NOW + 99));
    }

    fn forge(k: &SessionKey, header: &str, claims: &str) -> String {
        let input = format!("{}.{}", b64(header), b64(claims));
        format!("{input}.{}", k.sign(&input))
    }

    fn claims_json(exp: i64) -> String {
        format!(
            r#"{{"sub":"operator","iat":{NOW},"exp":{exp},"jti":"j","sid":"j","pv":0,"kind":"browser","auth_time":{NOW}}}"#
        )
    }

    /// Strict checks: alg confusion, a tampered payload, a bad signature.
    #[test]
    fn verify_is_strict() {
        let k = key();
        let good = forge(&k, JWT_HEADER, &claims_json(NOW + 10));
        assert!(k.verify(&good, NOW).is_some());
        // alg none, with no signature or with the right one.
        let none_hdr = r#"{"alg":"none","typ":"JWT"}"#;
        let unsigned = format!("{}.{}.", b64(none_hdr), b64(&claims_json(NOW + 10)));
        assert!(k.verify(&unsigned, NOW).is_none());
        assert!(
            k.verify(&forge(&k, none_hdr, &claims_json(NOW + 10)), NOW)
                .is_none()
        );
        // HS512, and a header with more fields.
        for h in [
            r#"{"alg":"HS512","typ":"JWT"}"#,
            r#"{"alg":"hs256"}"#,
            r#"{"alg":"HS256","typ":"JWT","kid":"x"}"#,
            r#"{"alg":"HS256","typ":"JOSE+JSON"}"#,
        ] {
            assert!(
                k.verify(&forge(&k, h, &claims_json(NOW + 10)), NOW)
                    .is_none(),
                "{h}"
            );
        }
        // No `typ` is fine.
        assert!(
            k.verify(
                &forge(&k, r#"{"alg":"HS256"}"#, &claims_json(NOW + 10)),
                NOW
            )
            .is_some()
        );
        // A tampered payload with the old signature.
        let parts: Vec<&str> = good.split('.').collect();
        let tampered = format!(
            "{}.{}.{}",
            parts[0],
            b64(&claims_json(NOW + 99_999)),
            parts[2]
        );
        assert!(k.verify(&tampered, NOW).is_none());
        // A bad signature.
        let bad = format!("{}.{}.{}", parts[0], parts[1], b64("x"));
        assert!(k.verify(&bad, NOW).is_none());
        // Another subject, and iat after exp.
        let other = claims_json(NOW + 10).replace("\"operator\"", "\"agent\"");
        assert!(k.verify(&forge(&k, JWT_HEADER, &other), NOW).is_none());
        let back = format!(
            r#"{{"sub":"operator","iat":{},"exp":{},"jti":"j","sid":"j","pv":0,"kind":"cli","auth_time":0}}"#,
            NOW + 20,
            NOW + 10
        );
        assert!(k.verify(&forge(&k, JWT_HEADER, &back), NOW).is_none());
        // Expired.
        assert!(
            k.verify(&forge(&k, JWT_HEADER, &claims_json(NOW)), NOW)
                .is_none()
        );
        // Four parts.
        assert!(k.verify(&format!("{good}.x"), NOW).is_none());
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
            HeaderValue::from_static("a=1; terrarium_session=x.y.z"),
        );
        h.append(header::COOKIE, HeaderValue::from_static("b=2"));
        assert_eq!(cookie_value(&h, COOKIE), Some("x.y.z"));
        assert_eq!(cookie_value(&h, "b"), Some("2"));
        assert_eq!(cookie_value(&h, "c"), None);
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(is_https(None, &h));
        assert!(!is_https(None, &HeaderMap::new()));
        assert_eq!(
            set_cookie("t", -5, true),
            "terrarium_session=t; Path=/; Max-Age=0; HttpOnly; SameSite=Strict; Secure"
        );
    }
}
