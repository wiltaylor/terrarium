//! Closing is deliberately limited to the operator-approved repository.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::repository;
use crate::{
    actor::Actor,
    api::Shared,
    error::{ApiError, ApiResult, Code},
    secrets::{self, Secret},
    store::{board, repos, version_groups},
};

// Serialize callers through the whole remote read/comment/close sequence.
static CLOSE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MARKER: &str = "<!-- terrarium:cards-done:v1 -->";

#[derive(Debug, Serialize)]
pub struct Closure {
    number: u64,
    comment: String,
    dry_run: bool,
}

/// No caller-supplied API URL or credential reference can redirect the token.
pub async fn close_done(
    state: &Shared,
    actor: Actor,
    project: String,
    repo_name: String,
    dry_run: bool,
) -> ApiResult<Vec<Closure>> {
    match &actor {
        Actor::Operator => (),
        Actor::Agent(a) if a.project == project && a.roles.contains(&terrarium_core::Role::ProjectAdmin) => (),
        _ => return Err(ApiError::forbidden("closing GitHub issues requires the operator or this project's project_admin")),
    }
    if project != "terrarium" {
        return Err(ApiError::invalid(
            "issue closure is approved only for project terrarium",
        ));
    }
    let _guard = CLOSE_LOCK.lock().await;
    let p = project.clone();
    let repo = state
        .db
        .read(move |c| repos::resolve(c, &repo_name, Some(&p)))
        .await?;
    if repo.project != project || repository(&repo.url)? != "wiltaylor/terrarium" {
        return Err(ApiError::invalid(
            "issue closure is approved only for wiltaylor/terrarium",
        ));
    }
    let credential = secrets::load(state, repo.owner(), "github-issues-token")
        .await
        .map_err(|_| {
            ApiError::forbidden("missing or unavailable github-issues-token secret for terrarium")
        })?;
    let Secret::Token { token, .. } = credential else {
        return Err(ApiError::invalid(
            "github-issues-token must be a token secret",
        ));
    };
    let name = repo.name;
    let candidates = state
        .db
        .read(move |c| candidates(c, &project, &name))
        .await?;
    let mut headers = reqwest::header::HeaderMap::new();
    let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.as_str()))
        .map_err(|_| ApiError::invalid("invalid github-issues-token"))?;
    auth.set_sensitive(true);
    headers.insert(reqwest::header::AUTHORIZATION, auth);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .user_agent("terrarium")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| ApiError::internal("cannot build GitHub client"))?;
    let mut result = Vec::new();
    for (number, comment) in candidates {
        if close_one(
            &client,
            "https://api.github.com/repos/wiltaylor/terrarium",
            number,
            &comment,
            dry_run,
        )
        .await?
        {
            result.push(Closure {
                number,
                comment,
                dry_run,
            });
        }
    }
    Ok(result)
}

fn issue_number(title: &str) -> Option<u64> {
    title
        .strip_prefix("GitHub #")?
        .split_once(':')?
        .0
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
}

// Project/number keyset pages include archived cards: archiving unfinished work
// must not make an issue eligible. Link lookups use the card_links primary key.
fn candidates(c: &Connection, project: &str, repo: &str) -> ApiResult<BTreeMap<u64, String>> {
    let published = version_groups::built_versions(c, project)?;
    let done = board::role_name(c, project, "done", "done")?;
    let mut issues: BTreeMap<u64, Vec<(String, Option<String>)>> = BTreeMap::new();
    let mut after = 0i64;
    for _ in 0..100 {
        let mut q = c.prepare("SELECT c.number,c.id,c.title,c.labels,b.name,c.repo FROM cards c JOIN board_columns b ON b.id=c.column_id WHERE c.project=?1 AND c.number>?2 ORDER BY c.number LIMIT 200")?;
        let rows = q
            .query_map(params![project, after], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Ok(comments(issues));
        }
        for (number, id, title, labels, column, card_repo) in rows {
            after = number;
            let mut numbers = BTreeSet::new();
            if card_repo.as_deref().is_none_or(|r| r == repo) {
                numbers.extend(issue_number(&title));
            }
            let mut links = c.prepare("SELECT ref FROM card_links WHERE card=?1 AND kind='url'")?;
            for link in links.query_map([&id], |r| r.get::<_, String>(0))? {
                if let Some(n) = link?
                    .strip_prefix("https://github.com/wiltaylor/terrarium/issues/")
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|n| *n > 0)
                {
                    numbers.insert(n);
                }
            }
            let labels: Vec<String> = serde_json::from_str(&labels)
                .map_err(|_| ApiError::internal("invalid card labels"))?;
            let version = if column == done && !labels.iter().any(|l| l == "hold") {
                labels
                    .iter()
                    .filter_map(|l| l.strip_prefix("version:"))
                    .find(|v| published.contains(*v))
                    .map(str::to_owned)
            } else {
                None
            };
            for n in numbers {
                issues
                    .entry(n)
                    .or_default()
                    .push((id.clone(), version.clone()));
            }
        }
    }
    Err(ApiError::invalid(
        "issue closure card scan exceeds 20000 cards",
    ))
}

fn comments(issues: BTreeMap<u64, Vec<(String, Option<String>)>>) -> BTreeMap<u64, String> {
    issues
        .into_iter()
        .filter_map(|(n, cards)| {
            if cards.iter().any(|(_, v)| v.is_none()) {
                return None;
            }
            let versions: BTreeSet<_> = cards.iter().filter_map(|(_, v)| v.as_deref()).collect();
            let versions = versions.into_iter().collect::<Vec<_>>().join(", ");
            let ids = cards
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Some((
                n,
                format!("Done in Terrarium {versions} (cards {ids}). Closing.\n\n{MARKER}"),
            ))
        })
        .collect()
}

async fn response(request: reqwest::RequestBuilder) -> ApiResult<Value> {
    let mut r = request
        .send()
        .await
        .map_err(|_| ApiError::new(Code::Unavailable, "GitHub issue request failed"))?;
    if !r.status().is_success() {
        return Err(ApiError::new(
            Code::Unavailable,
            format!("GitHub issue request returned {}", r.status()),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = r
        .chunk()
        .await
        .map_err(|_| ApiError::new(Code::Unavailable, "GitHub issue read failed"))?
    {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(ApiError::new(
                Code::Unavailable,
                "GitHub issue response too large",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::new(Code::Unavailable, "invalid GitHub issue response"))
}

async fn close_one(
    client: &reqwest::Client,
    base: &str,
    number: u64,
    comment: &str,
    dry_run: bool,
) -> ApiResult<bool> {
    let url = format!("{base}/issues/{number}");
    let issue = response(client.get(&url)).await?;
    if issue["number"].as_u64() != Some(number) || issue.get("pull_request").is_some() {
        return Err(ApiError::invalid(
            "response is not the requested GitHub issue",
        ));
    }
    if issue["state"] == "closed" {
        return Ok(false);
    }
    if issue["state"] != "open" {
        return Err(ApiError::invalid("invalid GitHub issue state"));
    }
    if dry_run {
        return Ok(true);
    }
    // Recover after a successful comment but a failed close (including restart).
    let mut found = false;
    for page in 1..=100 {
        let rows = response(client.get(format!("{url}/comments?per_page=100&page={page}"))).await?;
        let rows = rows
            .as_array()
            .ok_or_else(|| ApiError::invalid("invalid GitHub comments"))?;
        found |= rows
            .iter()
            .any(|r| r["body"].as_str().is_some_and(|s| s.contains(MARKER)));
        if found || rows.len() < 100 {
            break;
        }
        if page == 100 {
            return Err(ApiError::invalid(
                "GitHub comment scan exceeds 10000 comments",
            ));
        }
    }
    if !found {
        response(
            client
                .post(format!("{url}/comments"))
                .header("Content-Type", "application/json")
                .body(json!({"body":comment}).to_string()),
        )
        .await?;
    }
    response(
        client
            .patch(url)
            .header("Content-Type", "application/json")
            .body(json!({"state":"closed", "state_reason":"completed"}).to_string()),
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn dry_run_retry_and_repeated_close_never_comment_twice() {
        let posted = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let fail_close = Arc::new(AtomicBool::new(true));
        let app = Router::new().route("/issues/7", get({
            let closed = closed.clone();
            move || { let closed = closed.clone(); async move {
                axum::Json(json!({"number":7,"state":if closed.load(Ordering::SeqCst) {"closed"} else {"open"}}))
            }}
        }).patch({
            let closed = closed.clone(); let fail = fail_close.clone();
            move |axum::Json(body): axum::Json<Value>| { let closed = closed.clone(); let fail = fail.clone(); async move {
                assert_eq!(body["state"], "closed");
                if fail.swap(false, Ordering::SeqCst) { return (axum::http::StatusCode::BAD_GATEWAY, axum::Json(json!({}))); }
                closed.store(true, Ordering::SeqCst);
                (axum::http::StatusCode::OK, axum::Json(json!({})))
            }}
        })).route("/issues/7/comments", get({
            let posted = posted.clone();
            move || { let posted = posted.clone(); async move { axum::Json(if posted.load(Ordering::SeqCst) > 0 { json!([{"body":MARKER}]) } else { json!([]) }) }}
        }).post({
            let posted = posted.clone();
            move |axum::Json(body): axum::Json<Value>| { let posted = posted.clone(); async move {
                assert!(body["body"].as_str().unwrap().contains("TER-1"));
                posted.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({}))
            }}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let comment = format!("Done in Terrarium 1.0.0-alpha (cards TER-1). Closing.\n\n{MARKER}");
        assert!(close_one(&client, &base, 7, &comment, true).await.unwrap());
        assert_eq!(posted.load(Ordering::SeqCst), 0);
        assert!(close_one(&client, &base, 7, &comment, false).await.is_err());
        assert!(close_one(&client, &base, 7, &comment, false).await.unwrap());
        assert!(!close_one(&client, &base, 7, &comment, false).await.unwrap());
        assert_eq!(posted.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn all_cards_must_be_done_in_an_acknowledged_version() {
        use crate::{db::Tx, store::projects};
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        let mut t = Tx::for_test(c.transaction().unwrap(), "operator");
        projects::create(
            &mut t,
            &projects::NewProject {
                name: "p".into(),
                ..Default::default()
            },
            "api",
        )
        .unwrap();
        let make = |t: &mut Tx<'_>, title: &str, column: &str, labels: Vec<String>| {
            board::create_card(
                t,
                &Actor::Operator,
                "p",
                board::NewCard {
                    title: Some(title.into()),
                    column: Some(column.into()),
                    labels,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let a = make(
            &mut t,
            "GitHub #7: shipped",
            "done",
            vec!["version:1.0.0-alpha".into()],
        );
        assert!(candidates(&t, "p", "repo").unwrap().is_empty());
        // This is the same publication acknowledgement used by group built.
        t.execute("INSERT INTO card_groups (project,key,card,status,rule,done_when,closed_at) VALUES ('p','version:test',?1,'Built 1.0.0-alpha','test','done','now')", [&a.id]).unwrap();
        let eligible = candidates(&t, "p", "repo").unwrap();
        assert!(eligible[&7].contains(&a.id));
        assert!(eligible[&7].contains("1.0.0-alpha"));
        let b = make(&mut t, "more work", "ready", vec![]);
        board::add_link(
            &mut t,
            &Actor::Operator,
            &b.id,
            &board::NewLink {
                kind: "url".into(),
                reference: "https://github.com/wiltaylor/terrarium/issues/7".into(),
            },
        )
        .unwrap();
        assert!(candidates(&t, "p", "repo").unwrap().is_empty());
        t.execute("UPDATE cards SET archived=1 WHERE id=?1", [&b.id])
            .unwrap();
        assert!(candidates(&t, "p", "repo").unwrap().is_empty());
        make(
            &mut t,
            "GitHub #8: uncut",
            "done",
            vec!["version:vnext".into()],
        );
        assert!(candidates(&t, "p", "repo").unwrap().is_empty());
    }
}
