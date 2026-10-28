//! `/api/secrets`: the secret manager (PLAN §16b.5, docs/api.md §5.2), and
//! `/api/credentials`, its alias for one release (global secrets in the
//! old shapes).
//!
//! No answer holds a secret field, except `POST
//! /api/secrets/{scope}/{name}/reveal`: the operator only, with the
//! operator password in the body. Each change and each reveal writes an
//! audit row (`secret.put`, `secret.delete`, `secret.reveal`) with no
//! value.

use std::collections::BTreeMap;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{ApiJson, ApiQuery, Shared, session};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::secrets::{self, GLOBAL, Kind, NewSecret, PutMode, SecretInfo, SecretInput};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/secrets", get(list).post(create))
        .route(
            "/secrets/{scope}/{name}",
            get(show).put(replace).delete(remove),
        )
        .route("/secrets/{scope}/{name}/reveal", post(reveal))
        .route("/credentials", get(list_credentials).post(put_credential))
        .route("/credentials/{name}", delete(delete_credential))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// `global` or a project. Without it: each scope that the caller
    /// manages.
    #[serde(default)]
    pub scope: Option<String>,
}

/// The body of `POST /api/secrets`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    /// `global` (the default) or a project.
    #[serde(default)]
    pub scope: Option<String>,
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub fields: Map<String, Value>,
    #[serde(default)]
    pub secret_fields: Vec<String>,
}

/// The body of `PUT /api/secrets/{scope}/{name}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutBody {
    pub kind: Kind,
    #[serde(default)]
    pub fields: Map<String, Value>,
    #[serde(default)]
    pub secret_fields: Vec<String>,
}

/// The body of a reveal.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevealBody {
    #[serde(default)]
    pub password: String,
}

/// The scopes that a list without `?scope=` shows: all for the operator
/// and a sysadmin, the own project for its project_admin.
fn visible_scope(actor: &Actor) -> ApiResult<Option<String>> {
    match actor {
        Actor::Operator => Ok(None),
        Actor::Agent(a) if a.is_sysadmin() => Ok(None),
        // A project named `global` from before the reserved name (S7):
        // its scope is the global scope, which needs sysadmin.
        Actor::Agent(a) if a.project == GLOBAL => Err(ApiError::forbidden(
            "project `global` has a reserved name: its secrets are the global secrets, and \
             listing them needs sysadmin",
        )),
        Actor::Agent(a) if a.is_admin_of(&a.project) => Ok(Some(a.project.clone())),
        Actor::Agent(_) => Err(ApiError::forbidden(
            "listing secrets needs project_admin, sysadmin, or the operator",
        )),
    }
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<SecretInfo>>> {
    let scope = match q.scope {
        Some(s) => {
            policy::manage_secrets(&actor, &s)?;
            Some(s)
        }
        None => visible_scope(&actor)?,
    };
    let rows = state
        .db
        .read(move |c| secrets::list(c, scope.as_deref()))
        .await?;
    Ok(Json(rows))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
) -> ApiResult<Json<SecretInfo>> {
    policy::manage_secrets(&actor, &scope)?;
    let (s, n) = (scope.clone(), name.clone());
    state
        .db
        .read(move |c| secrets::get(c, &s, &n))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no secret `{scope}/{name}`")))
}

/// Store a secret and write its `secret.put` audit row, in one
/// transaction. The transaction builds the redaction matcher again before
/// its commit (PLAN §16b.6, S11).
async fn store(
    state: &Shared,
    actor: &Actor,
    mode: PutMode,
    scope: String,
    name: String,
    kind: Kind,
    input: SecretInput,
) -> ApiResult<(SecretInfo, bool)> {
    policy::manage_secrets(actor, &scope)?;
    let key = secrets::key(&state.state_dir)?;
    let by = actor.label();
    let dir = state.state_dir.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.rebuild_redactor(&dir);
            let (info, created) = secrets::put(
                t,
                &key,
                mode,
                &NewSecret {
                    scope: &scope,
                    name: &name,
                    kind,
                    input: &input,
                    by: &by,
                    strict: true,
                },
            )?;
            t.audit(
                "secret.put",
                &format!("{scope}/{name}"),
                secrets::scope_project(&scope),
                json!({
                    "scope": scope, "name": name, "kind": kind, "created": created,
                    "plain_fields": info.fields.keys().collect::<Vec<_>>(),
                    "secret_fields": info.secret_fields,
                    "warnings": info.warnings,
                }),
            )?;
            Ok((info, created))
        })
        .await
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<SecretInfo>)> {
    let scope = b.scope.unwrap_or_else(|| GLOBAL.to_owned());
    let input = SecretInput {
        fields: b.fields,
        secret_fields: b.secret_fields,
    };
    let (info, _) = store(
        &state,
        &actor,
        PutMode::Create,
        scope,
        b.name,
        b.kind,
        input,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn replace(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
    ApiJson(b): ApiJson<PutBody>,
) -> ApiResult<(StatusCode, Json<SecretInfo>)> {
    let input = SecretInput {
        fields: b.fields,
        secret_fields: b.secret_fields,
    };
    let (info, created) =
        store(&state, &actor, PutMode::Upsert, scope, name, b.kind, input).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(info)))
}

/// `?force=true` on a delete: delete a secret that a target uses.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteQuery {
    #[serde(default)]
    pub force: bool,
}

/// Delete a secret and write its `secret.delete` audit row.
async fn delete_one(
    state: &Shared,
    actor: &Actor,
    scope: String,
    name: String,
    force: bool,
) -> ApiResult<()> {
    policy::manage_secrets(actor, &scope)?;
    let gone = state
        .db
        .write(actor.label(), move |t| {
            let Some(info) = secrets::get(t, &scope, &name)? else {
                return Ok(false);
            };
            // A target login (PLAN §16b.6a): 409, unless `force`.
            let users = crate::store::targets::guard_secret_delete(t, &scope, &name, force)?;
            secrets::delete(t, &scope, &name)?;
            t.audit(
                "secret.delete",
                &format!("{scope}/{name}"),
                secrets::scope_project(&scope),
                json!({"scope": scope, "name": name, "kind": info.kind, "targets": users}),
            )?;
            Ok(true)
        })
        .await?;
    // A delete only shrinks the matcher: until this rebuild, the old one
    // redacts more, never less (PLAN §16b.6).
    if gone {
        crate::secrets::redact::rebuild(state).await;
        Ok(())
    } else {
        Err(ApiError::not_found("no such secret"))
    }
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let label = format!("{scope}/{name}");
    delete_one(&state, &actor, scope, name, q.force)
        .await
        .map_err(|e| match e.code {
            crate::error::Code::NotFound => ApiError::not_found(format!("no secret `{label}`")),
            _ => e,
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// The answer of a reveal: the secret and its secret fields.
#[derive(Serialize)]
struct Revealed<'a> {
    #[serde(flatten)]
    info: &'a SecretInfo,
    values: BTreeMap<&'a str, &'a str>,
}

/// `POST /api/secrets/{scope}/{name}/reveal`: the operator only, with the
/// operator password (`{"password"}`), checked as a login checks it (the
/// login limiter counts a wrong one). The answer has `Cache-Control:
/// no-store`.
async fn reveal(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
    req: Request,
) -> ApiResult<Response> {
    policy::reveal_secret(&actor)?;
    let ip = super::client_addr(&req);
    let body: RevealBody = session::read_json(req).await?;
    if body.password.is_empty() {
        return Err(ApiError::invalid(
            "a reveal needs the operator password as `password`",
        ));
    }
    if let Err(resp) = session::check_password(&state, &ip, body.password, || {
        ApiError::forbidden("the operator password is wrong").into_response()
    })
    .await
    {
        return Ok(resp);
    }
    let key = secrets::key(&state.state_dir)?;
    let (s, n) = (scope.clone(), name.clone());
    let opened = state
        .db
        .write(actor.label(), move |t| {
            let Some(o) = secrets::open(t, &key, &s, &n)? else {
                return Ok(None);
            };
            t.audit(
                "secret.reveal",
                &format!("{s}/{n}"),
                secrets::scope_project(&s),
                json!({"scope": s, "name": n, "kind": o.info.kind,
                       "secret_fields": o.info.secret_fields, "ip": ip}),
            )?;
            Ok(Some(o))
        })
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no secret `{scope}/{name}`")))?;
    let answer = Revealed {
        info: &opened.info,
        values: opened
            .values
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect(),
    };
    let mut resp = Json(answer).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

// ---- `/api/credentials`: the alias of one release ----

/// The kinds of the old API. `password` is a `login` now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    Token,
    SshKey,
    Password,
}

/// The body of `POST /api/credentials`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCredential {
    pub name: String,
    pub kind: CredentialKind,
    pub value: String,
    #[serde(default)]
    pub username: Option<String>,
}

/// What `/api/credentials` shows of a global secret.
#[derive(Debug, Serialize)]
pub struct CredentialInfo {
    pub name: String,
    /// `token`, `ssh_key`, or `password` (a `login`); another kind shows
    /// its own name.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub created_at: String,
}

impl From<SecretInfo> for CredentialInfo {
    fn from(s: SecretInfo) -> Self {
        let kind = match s.kind {
            Kind::Login => "password".to_owned(),
            k => k.as_str().to_owned(),
        };
        Self {
            name: s.name,
            kind,
            username: s
                .fields
                .get("username")
                .and_then(Value::as_str)
                .map(str::to_owned),
            created_at: s.created_at,
        }
    }
}

async fn list_credentials(
    State(state): State<Shared>,
    actor: Actor,
) -> ApiResult<Json<Vec<CredentialInfo>>> {
    policy::manage_secrets(&actor, GLOBAL)?;
    let rows = state.db.read(|c| secrets::list(c, Some(GLOBAL))).await?;
    Ok(Json(rows.into_iter().map(CredentialInfo::from).collect()))
}

async fn put_credential(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<NewCredential>,
) -> ApiResult<(StatusCode, Json<CredentialInfo>)> {
    let mut fields = Map::new();
    let kind = match b.kind {
        CredentialKind::Token => {
            fields.insert("value".into(), b.value.into());
            Kind::Token
        }
        CredentialKind::SshKey => {
            if b.username.is_some() {
                return Err(ApiError::invalid(
                    "an ssh_key has no username; put the user in the repo URL",
                ));
            }
            fields.insert("private_key".into(), b.value.into());
            Kind::SshKey
        }
        CredentialKind::Password => {
            fields.insert("password".into(), b.value.into());
            Kind::Login
        }
    };
    if let Some(u) = b.username {
        fields.insert("username".into(), u.into());
    }
    let input = SecretInput {
        fields,
        secret_fields: Vec::new(),
    };
    let (info, _) = store(
        &state,
        &actor,
        PutMode::Upsert,
        GLOBAL.to_owned(),
        b.name,
        kind,
        input,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(info.into())))
}

async fn delete_credential(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    delete_one(&state, &actor, GLOBAL.to_owned(), name.clone(), false)
        .await
        .map_err(|e| match e.code {
            crate::error::Code::NotFound => ApiError::not_found(format!("no credential `{name}`")),
            _ => e,
        })?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::actor::AgentActor;
    use terrarium_core::Role;

    fn admin_of(project: &str) -> Actor {
        Actor::Agent(AgentActor {
            id: "01K8AGENT0000000000000000A".into(),
            name: "lead".into(),
            project: project.into(),
            roles: vec![Role::ProjectAdmin],
            policy: Default::default(),
        })
    }

    /// S7: the project_admin of a project named `global` (made before the
    /// name was reserved) does not list the global secrets.
    #[test]
    fn a_project_named_global_does_not_list_the_global_secrets() {
        assert_eq!(visible_scope(&admin_of("web")).unwrap(), Some("web".into()));
        let e = visible_scope(&admin_of(GLOBAL)).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Forbidden);
        assert!(policy::manage_secrets(&admin_of(GLOBAL), GLOBAL).is_err());
    }
}
