//! Operator sidebar bookmarks. Mutations affect one URL, never a stale whole list.
use axum::{Json, Router, extract::State, routing::get};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ApiJson, Shared};
use crate::{
    actor::Actor,
    error::{ApiError, ApiResult},
    policy,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Bookmark {
    title: String,
    href: String,
}

fn normalize(value: &str) -> ApiResult<String> {
    let value = value.trim();
    let relative = value.starts_with('/') && !value.starts_with("//");
    let url = if relative {
        url::Url::parse("https://dashboard.invalid/").and_then(|base| base.join(value))
    } else {
        url::Url::parse(value)
    }
    .map_err(|_| ApiError::invalid("Invalid bookmark URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(ApiError::invalid(
            "Bookmarks require an HTTP(S) URL without credentials",
        ));
    }
    if relative {
        let parts: Vec<_> = url.path().split('/').collect();
        if url.host_str() != Some("dashboard.invalid")
            || parts.len() < 5
            || parts[1] != "p"
            || parts[2].is_empty()
            || parts[3] != "knowledge"
            || parts[4] != "wiki"
        {
            return Err(ApiError::invalid(
                "Relative bookmarks must link to a wiki page",
            ));
        }
        return Ok(url[url::Position::BeforePath..].to_owned());
    }
    if !value.to_ascii_lowercase().starts_with("http://")
        && !value.to_ascii_lowercase().starts_with("https://")
    {
        return Err(ApiError::invalid("Use an absolute HTTP(S) URL"));
    }
    Ok(url.to_string())
}

fn list_rows(c: &Connection, owner: &str) -> ApiResult<Vec<Bookmark>> {
    let mut stmt =
        c.prepare("SELECT title, href FROM bookmarks WHERE owner = ?1 ORDER BY rowid")?;
    Ok(stmt
        .query_map([owner], |r| {
            Ok(Bookmark {
                title: r.get(0)?,
                href: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

pub fn routes() -> Router<Shared> {
    Router::new().route("/bookmarks", get(list).put(add).delete(remove))
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<Bookmark>>> {
    policy::operator_only(&actor, "bookmarks")?;
    let owner = actor.address();
    Ok(Json(state.db.read(move |c| list_rows(c, &owner)).await?))
}

async fn add(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<Bookmark>,
) -> ApiResult<Json<Vec<Bookmark>>> {
    policy::operator_only(&actor, "bookmarks")?;
    let href = normalize(&body.href)?;
    let title = body.title.trim().to_owned();
    if title.is_empty() || title.len() > 500 || href.len() > 8192 {
        return Err(ApiError::invalid(
            "A bookmark needs a title (up to 500 bytes) and a URL (up to 8192 bytes)",
        ));
    }
    let owner = actor.address();
    Ok(Json(state.db.write(actor.label(), move |t| {
        t.execute("INSERT INTO bookmarks(owner, href, title) VALUES (?1, ?2, ?3) ON CONFLICT(owner, href) DO UPDATE SET title = excluded.title", params![owner, href, title])?;
        t.audit("bookmark.add", &href, None, json!({"owner": owner}))?;
        list_rows(t, &owner)
    }).await?))
}

#[derive(Deserialize)]
struct Remove {
    href: String,
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<Remove>,
) -> ApiResult<Json<Vec<Bookmark>>> {
    policy::operator_only(&actor, "bookmarks")?;
    let href = normalize(&body.href)?;
    let owner = actor.address();
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| {
                t.execute(
                    "DELETE FROM bookmarks WHERE owner = ?1 AND href = ?2",
                    params![owner, href],
                )?;
                t.audit("bookmark.remove", &href, None, json!({"owner": owner}))?;
                list_rows(t, &owner)
            })
            .await?,
    ))
}
