//! `/api/links` (TER-61, `docs/api.md` §35, wiki design/linked-instances
//! D2): the link keys with which another Terrarium daemon (the home) calls
//! this one, and `GET /api/system/instance`.
//!
//! Only the operator manages links. A link key itself may call none of
//! these routes but `DELETE /api/links/self` (the deny list,
//! `crate::links::denied`). `POST /api/links/pair` needs no bearer: the
//! one-time pairing code is the proof.

use axum::extract::{ConnectInfo, Path, State};
use std::net::SocketAddr;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::links::{Instance, LinkCaller};
use crate::policy;
use crate::store::links::{self, Home, Link};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/system/instance", get(instance))
        .route("/links", get(list).post(create))
        .route("/links/pair", post(pair))
        .route("/links/self", delete(end_self))
        .route("/links/{id}", get(show).delete(revoke))
        .route("/links/{id}/rotate", post(rotate))
}

/// This daemon's instance: the ID, the name, and the build.
pub fn this_instance(state: &Shared) -> ApiResult<Instance> {
    Ok(Instance {
        id: state.links.instance_id(&state.state_dir)?,
        name: crate::links::instance_name(state.web.public_url.as_deref()),
        version: terrarium_version::VERSION.into(),
    })
}

/// `GET /api/system/instance`: `{id, name, version}`. Every caller may
/// read it; the home checks the ID at pairing and on each new stream.
async fn instance(State(state): State<Shared>) -> ApiResult<Json<Instance>> {
    Ok(Json(this_instance(&state)?))
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    /// Also the revoked links.
    #[serde(default)]
    revoked: bool,
}

/// `GET /api/links`: the links, newest first; `?revoked=true` adds the
/// revoked ones.
async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Link>>> {
    policy::operator_only(&actor, "listing the links")?;
    Ok(Json(
        state.db.read(move |c| links::list(c, q.revoked)).await?,
    ))
}

/// `GET /api/links/{id}`.
async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Link>> {
    policy::operator_only(&actor, "reading a link")?;
    Ok(Json(state.db.read(move |c| links::get(c, &id)).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewLink {
    label: String,
    #[serde(default)]
    read_only: bool,
}

/// The answer that carries a pairing code, once: the link, the code, and
/// what the operator types or compares on the home.
fn with_code(state: &Shared, link: Link, code: links::Code) -> ApiResult<Value> {
    let instance = this_instance(state)?;
    Ok(json!({
        "link": link,
        "code": code.code,
        "code_expires_at": code.expires_at,
        "instance": instance,
        "url": state.web.public_url,
    }))
}

/// `POST /api/links` `{label, read_only?}`: a new link with a one-time
/// pairing code (10 minutes). The code is in this answer only; the daemon
/// keeps its hash.
async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<NewLink>,
) -> ApiResult<Response> {
    policy::operator_only(&actor, "making a link")?;
    let (link, code) = state
        .db
        .write(actor.label(), move |t| {
            let (l, code) = links::create(t, &body.label, body.read_only)?;
            t.audit(
                "link.new",
                &l.id,
                None,
                json!({"label": l.label, "read_only": l.read_only,
                       "code_expires_at": code.expires_at}),
            )?;
            Ok((l, code))
        })
        .await?;
    let v = with_code(&state, link, code)?;
    Ok(no_store((StatusCode::CREATED, Json(v)).into_response()))
}

/// `POST /api/links/{id}/rotate`: a new pairing code. The old key stops
/// working now, and its open streams close; the home takes the code with
/// its Re-pair.
async fn rotate(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    policy::operator_only(&actor, "rotating a link")?;
    let (link, code, old) = state
        .db
        .write(actor.label(), move |t| {
            let (l, code, old) = links::rotate(t, &id)?;
            t.audit(
                "link.rotate",
                &l.id,
                None,
                json!({"label": l.label, "code_expires_at": code.expires_at}),
            )?;
            Ok((l, code, old))
        })
        .await?;
    if let Some(h) = old {
        state.links.revoke(&h);
    }
    let v = with_code(&state, link, code)?;
    Ok(no_store(Json(v).into_response()))
}

/// Revoke a link and close its streams.
async fn revoke_link(state: &Shared, actor: String, id: String, by_self: bool) -> ApiResult<()> {
    let (_, old) = state
        .db
        .write(actor, move |t| {
            let (l, old) = links::revoke(t, &id)?;
            t.audit(
                "link.revoke",
                &l.id,
                None,
                json!({"label": l.label, "by_self": by_self}),
            )?;
            Ok((l, old))
        })
        .await?;
    if let Some(h) = old {
        state.links.revoke(&h);
    }
    Ok(())
}

/// `DELETE /api/links/{id}`: revoke a link. The key stops working, and
/// its open streams close at once. The row stays as `revoked`.
async fn revoke(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    policy::operator_only(&actor, "revoking a link")?;
    revoke_link(&state, actor.label(), id, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/links/self`: a link key ends its own link (the home's
/// Remove). Only a link key may call it.
async fn end_self(
    State(state): State<Shared>,
    actor: Actor,
    caller: Option<Extension<LinkCaller>>,
) -> ApiResult<StatusCode> {
    let Some(Extension(caller)) = caller else {
        return Err(ApiError::bad_request(
            "DELETE /api/links/self ends the link of the calling link key; \
             revoke a link by its ID (DELETE /api/links/{id})",
        ));
    };
    revoke_link(&state, actor.label(), caller.id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairBody {
    code: String,
    /// The instance ID of the home (a ULID).
    home_instance_id: String,
    #[serde(default)]
    home_name: Option<String>,
}

/// The one answer of every failed pairing, so it tells nothing about the
/// code.
const PAIR_FAILED: &str = "the pairing code is wrong, was used, or has expired; make a new one \
                           on this daemon (terrarium admin link new, or Settings, Links)";

/// `POST /api/links/pair` `{code, home_instance_id, home_name?}`: swap a
/// pairing code for the long-lived link key. One call per code wins. The
/// answer carries the key once, with `Cache-Control: no-store`; the daemon
/// keeps its hash.
async fn pair(
    State(state): State<Shared>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    ApiJson(body): ApiJson<PairBody>,
) -> ApiResult<Response> {
    if !body.code.starts_with(links::CODE_PREFIX) {
        return Err(ApiError::new(Code::Unauthorized, PAIR_FAILED));
    }
    let home_instance_id = body
        .home_instance_id
        .trim()
        .parse::<ulid::Ulid>()
        .map_err(|_| ApiError::invalid("`home_instance_id` is a ULID (GET /api/system/instance)"))?
        .to_string();
    let home_name = match body.home_name.as_deref() {
        Some(n) => Some(links::check_label(n)?),
        None => None,
    };
    let instance = this_instance(&state)?;
    if home_instance_id == instance.id {
        return Err(ApiError::invalid("a daemon cannot link to itself"));
    }
    let home = Home {
        instance_id: home_instance_id,
        name: home_name,
        from: peer.map_or_else(|| "unknown".to_owned(), |Extension(c)| c.0.ip().to_string()),
    };
    let code = body.code;
    let paired = state
        .db
        .write("system", move |t| {
            let Some((l, key)) = links::pair(t, &code, &home)? else {
                return Ok(None);
            };
            t.actor = format!("link:{}", l.id);
            t.audit(
                "link.pair",
                &l.id,
                None,
                json!({"label": l.label, "home": home.instance_id,
                       "home_name": home.name, "from": home.from}),
            )?;
            Ok(Some((l, key)))
        })
        .await?;
    let Some((link, key)) = paired else {
        return Err(ApiError::new(Code::Unauthorized, PAIR_FAILED));
    };
    Ok(no_store(
        Json(json!({
            "link_id": link.id,
            "label": link.label,
            "read_only": link.read_only,
            "key": key,
            "instance": instance,
        }))
        .into_response(),
    ))
}
