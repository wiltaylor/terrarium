//! Grants of secrets (PLAN §16b.3, docs/api.md §5.2), and the agent side:
//! `GET /api/agent/secrets` and `POST /api/agent/secrets/{name}/get`.
//!
//! - `GET|POST|DELETE /api/secrets/{scope}/{name}/grants`: the rule of
//!   [`policy::grant_secret`]. Each change writes `secret.grant` or
//!   `secret.revoke`.
//! - `GET /api/agent/secrets`: the caller's effective grants (its own and
//!   its project's), with no value.
//! - `POST /api/agent/secrets/{name}/get`: one value, for a `read` grant
//!   only. Each answer writes `secret.get`. At most [`GET_LIMIT`] a
//!   minute per agent.
//! - `POST /api/agent/secrets/env`: the env vars of the caller's `read`
//!   grants and the credential env of its harness (the subscription's API
//!   key, the Copilot account token), for `terrarium exec-env` at the
//!   start of the harness (PLAN §16b.4). Each answer writes `secret.env`
//!   with the names only. It has its own limit of [`GET_LIMIT`] a minute.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::secrets::grants::{self, Effective, Grant, Level, NewGrant};
use crate::secrets::{self, Kind};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/secrets/{scope}/{name}/grants",
            get(list).post(create).delete(revoke),
        )
        .route("/agent/secrets", get(mine))
        .route("/agent/secrets/{name}/get", post(read_value))
        .route("/agent/secrets/env", post(launch_env))
}

/// `terrarium secret get` answers at most this many times per agent in
/// [`GET_WINDOW`].
pub const GET_LIMIT: usize = 30;
const GET_WINDOW: Duration = Duration::from_secs(60);

/// The body of `POST /api/secrets/{scope}/{name}/grants`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBody {
    /// `agent:<project>/<name>`, `agent:<id>`, or `project:<name>`.
    pub grantee: String,
    /// `use` or `read`. Default: `use` for `token`, `github_token`, and
    /// `ssh_key`; `read` for the other kinds.
    #[serde(default)]
    pub level: Option<Level>,
    /// An env var name. Needs `read`.
    #[serde(default)]
    pub env: Option<String>,
}

/// The query of `DELETE /api/secrets/{scope}/{name}/grants`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeQuery {
    pub grantee: String,
}

fn no_secret(scope: &str, name: &str) -> ApiError {
    ApiError::not_found(format!("no secret `{scope}/{name}`"))
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Grant>>> {
    policy::grant_secret(&actor, &scope)?;
    let (s, n) = (scope.clone(), name.clone());
    let rows = state
        .db
        .read(move |c| match secrets::get(c, &s, &n)? {
            Some(info) => grants::list(c, &info).map(Some),
            None => Ok(None),
        })
        .await?
        .ok_or_else(|| no_secret(&scope, &name))?;
    Ok(Json(rows))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
    ApiJson(b): ApiJson<GrantBody>,
) -> ApiResult<(StatusCode, Json<Grant>)> {
    policy::grant_secret(&actor, &scope)?;
    let by = actor.label();
    let (s, n) = (scope.clone(), name.clone());
    let (grant, created) = state
        .db
        .write(actor.label(), move |t| {
            let Some(info) = secrets::get(t, &s, &n)? else {
                return Err(no_secret(&s, &n));
            };
            let grantee = grants::parse_grantee(t, &b.grantee)?;
            let (grant, created) = grants::put(
                t,
                &info,
                &NewGrant {
                    grantee: &grantee,
                    level: b.level,
                    env: b.env.as_deref(),
                    by: &by,
                },
            )?;
            t.audit(
                "secret.grant",
                &format!("{s}/{n}"),
                secrets::scope_project(&s),
                json!({
                    "scope": s, "name": n, "kind": info.kind,
                    "grantee": grant.grantee, "grantee_label": grant.grantee_label,
                    "level": grant.level, "env": grant.env, "created": created,
                }),
            )?;
            Ok((grant, created))
        })
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(grant)))
}

async fn revoke(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<RevokeQuery>,
) -> ApiResult<StatusCode> {
    policy::grant_secret(&actor, &scope)?;
    let (s, n) = (scope.clone(), name.clone());
    state
        .db
        .write(actor.label(), move |t| {
            let Some(info) = secrets::get(t, &s, &n)? else {
                return Err(no_secret(&s, &n));
            };
            // A grantee that is gone (a removed agent) still matches its
            // stored key.
            let key = match grants::parse_grantee(t, &q.grantee) {
                Ok(g) => g.key(),
                Err(_) => q.grantee.clone(),
            };
            if !grants::delete(t, &info, &key)? {
                return Err(ApiError::not_found(format!(
                    "secret `{s}/{n}` has no grant to `{}`",
                    q.grantee
                )));
            }
            t.audit(
                "secret.revoke",
                &format!("{s}/{n}"),
                secrets::scope_project(&s),
                json!({"scope": s, "name": n, "kind": info.kind, "grantee": key}),
            )?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/agent/secrets`: the caller's effective grants.
async fn mine(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<Effective>>> {
    let me = policy::agent_secrets(&actor)?.clone();
    let rows = state
        .db
        .read(move |c| grants::effective(c, &me.id, &me.project))
        .await?;
    Ok(Json(rows))
}

/// The body of `POST /api/agent/secrets/{name}/get`. An empty body is
/// `{}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetBody {
    /// A field of the secret. Default: the main secret field of the kind
    /// (`value`, `password`, `private_key`, or the only secret field of a
    /// `generic`).
    #[serde(default)]
    pub field: Option<String>,
}

/// The answer of a get.
#[derive(Serialize)]
struct GetAnswer {
    scope: String,
    name: String,
    kind: Kind,
    field: String,
    /// Zeroed on drop (PLAN §16b.2); the serialized body is not.
    #[serde(serialize_with = "crate::secrets::serialize_zeroizing")]
    value: zeroize::Zeroizing<String>,
}

/// A sliding window per agent.
fn rate_ok(agent: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<String, VecDeque<Instant>>>> = OnceLock::new();
    let map = SEEN.get_or_init(Default::default);
    let mut map = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    let q = map.entry(agent.to_owned()).or_default();
    while q
        .front()
        .is_some_and(|t| now.duration_since(*t) > GET_WINDOW)
    {
        q.pop_front();
    }
    if q.len() >= GET_LIMIT {
        return false;
    }
    q.push_back(now);
    true
}

/// `POST /api/agent/secrets/{name}/get`: one value of a secret with a
/// `read` grant. The answer has `Cache-Control: no-store`.
async fn read_value(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let me = policy::agent_secrets(&actor)?.clone();
    let b: GetBody = if body.iter().all(u8::is_ascii_whitespace) {
        GetBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    if !rate_ok(&me.id) {
        return Err(ApiError::new(
            Code::RateLimited,
            format!("`secret get` answers at most {GET_LIMIT} times a minute per agent"),
        ));
    }
    let key = secrets::key(&state.state_dir)?;
    let n = name.clone();
    let answer = state
        .db
        .write(actor.label(), move |t| {
            let Some(e) = grants::find(t, &me.id, &me.project, &n)? else {
                return Err(ApiError::not_found(format!(
                    "no secret `{n}` is granted to you; `terrarium secret list` shows your grants"
                )));
            };
            if e.level != Level::Read {
                return Err(ApiError::forbidden(format!(
                    "your grant of secret `{}/{}` is `use`: the brokers use it, and you cannot \
                     read it; reading needs a `read` grant",
                    e.scope, e.name
                )));
            }
            let field = match b.field.clone() {
                Some(f) => f,
                None => grants::main_field(e.kind, &e.secret_fields).ok_or_else(|| {
                    ApiError::invalid(format!(
                        "secret `{}/{}` has several secret fields; name one with `field`: {}",
                        e.scope,
                        e.name,
                        e.secret_fields.join(", ")
                    ))
                })?,
            };
            let value = zeroize::Zeroizing::new(if let Some(v) = e.fields.get(&field) {
                v.as_str().map_or_else(|| v.to_string(), str::to_owned)
            } else {
                let opened = secrets::open(t, &key, &e.scope, &e.name)?
                    .ok_or_else(|| ApiError::not_found("the secret is gone"))?;
                let Some(v) = opened.values.get(&field) else {
                    let mut all: Vec<&str> = e.fields.keys().map(String::as_str).collect();
                    all.extend(e.secret_fields.iter().map(String::as_str));
                    return Err(ApiError::invalid(format!(
                        "secret `{}/{}` has no field `{field}`; its fields are {}",
                        e.scope,
                        e.name,
                        all.join(", ")
                    )));
                };
                v.to_string()
            });
            t.audit(
                "secret.get",
                &format!("{}/{}", e.scope, e.name),
                secrets::scope_project(&e.scope),
                json!({"agent": me.id, "scope": e.scope, "name": e.name,
                       "kind": e.kind, "field": field}),
            )?;
            Ok(GetAnswer {
                scope: e.scope,
                name: e.name,
                kind: e.kind,
                field,
                value,
            })
        })
        .await?;
    let mut resp = Json(answer).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

/// The answer of `POST /api/agent/secrets/env`. `vars` and `credentials`
/// hold values: never log them.
#[derive(Serialize)]
struct EnvAnswer {
    /// The env vars of the `read` grants. `terrarium exec-env` sets each
    /// one that the env does not hold yet.
    vars: BTreeMap<String, EnvValue>,
    /// The credential env of the harness: the stored API key of the
    /// agent's subscription (with the vars that go with it) and the token
    /// of its plan account (Copilot). `terrarium exec-env` sets each one,
    /// over a var of the same name.
    credentials: BTreeMap<String, EnvValue>,
}

/// One value of [`EnvAnswer`]. Zeroed on drop (PLAN §16b.2); the
/// serialized body is not.
#[derive(Serialize)]
struct EnvValue(
    #[serde(serialize_with = "crate::secrets::serialize_zeroizing")] zeroize::Zeroizing<String>,
);

/// `POST /api/agent/secrets/env`: the env of the caller's harness, for
/// `terrarium exec-env` in the sandbox just before it starts the harness
/// (PLAN §16b.4). So no value is in the launch spec, the runner spec file,
/// or the env of `bwrap`.
///
/// - `vars`: the caller's `read` grants with an `env` name
///   ([`grants::launch`]: each final name passed the name rule). A grant
///   whose name is also a credential var is left out, as at the launch.
/// - `credentials`: the credential env of the caller's harness
///   ([`credential_env`]), from the agent's subscription at the time of
///   the request. So a key change or an account swap applies at the next
///   start.
///
/// The answer has `Cache-Control: no-store`. Each answer writes
/// `secret.env` with the names only. It has its own rate limit (not the
/// one of `secret get`), so many `secret get` calls cannot stop a start.
async fn launch_env(State(state): State<Shared>, actor: Actor) -> ApiResult<Response> {
    let me = policy::agent_secrets(&actor)?.clone();
    if !rate_ok(&format!("env:{}", me.id)) {
        return Err(ApiError::new(
            Code::RateLimited,
            format!("`exec-env` answers at most {GET_LIMIT} times a minute per agent"),
        ));
    }
    let key = secrets::key(&state.state_dir)?;
    let state_dir = state.state_dir.clone();
    let answer = state
        .db
        .write(actor.label(), move |t| {
            let credentials = credential_env(t, &state_dir, &me.id)?;
            let l = grants::launch(t, &key, &me.id, &me.project)?;
            let vars: BTreeMap<String, EnvValue> = l
                .env
                .into_iter()
                .filter(|(k, _)| !credentials.contains_key(k))
                .map(|(k, v)| (k, EnvValue(v)))
                .collect();
            let names: Vec<&String> = vars.keys().collect();
            let credential_names: Vec<&String> = credentials.keys().collect();
            t.audit(
                "secret.env",
                &format!("agent:{}", me.id),
                Some(&me.project),
                json!({"agent": me.id, "names": names, "credentials": credential_names}),
            )?;
            Ok(EnvAnswer { vars, credentials })
        })
        .await?;
    let mut resp = Json(answer).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

/// The credential env of the harness of agent `agent_id` (PLAN §16b.4).
/// The account's vars first ([`crate::accounts::credential_env`], for the
/// account that the launch picked), then the subscription's stored API
/// key ([`crate::login_session::credential_env_for`]) over them, as the
/// launch put them before. A subscription that the harness cannot spend
/// gives nothing (the start refuses it).
fn credential_env(
    conn: &rusqlite::Connection,
    state_dir: &std::path::Path,
    agent_id: &str,
) -> ApiResult<BTreeMap<String, EnvValue>> {
    let agent = crate::store::agents::get(conn, agent_id)?.agent;
    let mut out = BTreeMap::new();
    let account = crate::runner_supervisor::launched_account(state_dir, agent.id);
    for (k, v) in crate::accounts::credential_env(state_dir, agent.harness, account.as_deref()) {
        out.insert(k, EnvValue(v));
    }
    let sub = match &agent.subscription {
        Some(n) => crate::store::subscriptions::find(conn, n)?,
        None => None,
    };
    if let Some(sub) = sub.filter(|s| s.harnesses.contains(&agent.harness)) {
        for (k, v) in crate::login_session::credential_env_for(state_dir, &sub) {
            out.insert(k, EnvValue(zeroize::Zeroizing::new(v)));
        }
    }
    Ok(out)
}

/// The granted `token` or `github_token` secret for a git host, for the
/// credential helper (`POST /api/repos/credential`). It writes
/// `secret.use`. `None`: no grant for the host.
pub async fn helper_answer(
    state: &Shared,
    actor: &Actor,
    agent_id: &str,
    project: &str,
    host: &str,
) -> ApiResult<Option<(String, zeroize::Zeroizing<String>)>> {
    let key = secrets::key(&state.state_dir)?;
    let (id, p, h) = (agent_id.to_owned(), project.to_owned(), host.to_owned());
    state
        .db
        .write(actor.label(), move |t| {
            let Some((e, opened)) = grants::token_for_host(t, &key, &id, &p, &h)? else {
                return Ok(None);
            };
            let secrets::Secret::Token { username, token } = opened.to_secret()? else {
                return Ok(None);
            };
            t.audit(
                "secret.use",
                &format!("{}/{}", e.scope, e.name),
                secrets::scope_project(&e.scope),
                json!({"agent": id, "scope": e.scope, "name": e.name,
                       "broker": "git", "host": h}),
            )?;
            Ok(Some((username, token)))
        })
        .await
}
