//! Closing is deliberately limited to the operator-approved repository.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
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
    explicit: Option<(u64, String)>,
) -> ApiResult<Vec<Closure>> {
    if explicit.is_some() {
        crate::policy::operator_only(&actor, "overriding a legacy issue version")?;
    }
    match &actor {
        Actor::Operator => (),
        Actor::Agent(a)
            if a.project == project && a.roles.contains(&terrarium_core::Role::ProjectAdmin) => {}
        _ => {
            return Err(ApiError::forbidden(
                "closing GitHub issues requires the operator or this project's project_admin",
            ));
        }
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
    // Fresh daemon-owned mirror tags also cover manually cut releases.
    let view = crate::api::repo_trees::ensure_mirror(state, &repo, true).await?;
    if view.error.is_some() {
        return Err(ApiError::new(Code::Unavailable, "cannot refresh published repository tags"));
    }
    let published = mirror_tags(crate::api::repo_trees::mirror_dir(state, &repo)).await?;
    if let Some((number, version)) = &explicit {
        if *number == 0 || !published.contains(&format!("v{version}")) {
            return Err(ApiError::invalid("explicit issue version must name an existing published tag vV"));
        }
    }
    let (p, name, override_version) = (project.clone(), repo.name.clone(), explicit.clone());
    let (candidates, after, closed) = state.db.read(move |c| {
        let candidates = candidates_with(c, &p, &name, published, override_version)?;
        let after = c.query_row("SELECT after_issue FROM github_closure_cursor WHERE project=?1 AND repo=?2", params![p,name], |r| r.get::<_,u64>(0)).optional()?.unwrap_or(0);
        let mut q = c.prepare("SELECT issue FROM github_closed_issues WHERE project=?1 AND repo=?2 ORDER BY issue LIMIT 20001")?;
        let closed = q.query_map(params![p,name], |r| r.get::<_,u64>(0))?.collect::<Result<BTreeSet<_>,_>>()?;
        if closed.len() > 20000 { return Err(ApiError::invalid("closed issue cache exceeds 20000 issues")); }
        Ok((candidates, after, closed))
    }).await?;
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
    let selected = select_batch(candidates, after, &closed, explicit.is_some());
    let mut budget = Budget::default();
    for (number, comment) in selected {
        // Each issue needs at most 13 requests (state, ten comment pages,
        // comment, close). Never start one that could exceed the run budget.
        if budget.remaining < 13 { break; }
        let outcome = close_one(
            &client,
            "https://api.github.com/repos/wiltaylor/terrarium",
            number,
            &comment,
            dry_run,
            &mut budget,
        )
        .await;
        if !dry_run && explicit.is_none() {
            let (p,n) = (project.clone(),repo.name.clone());
            let completed = outcome.is_ok();
            state.db.write(actor.label(), move |t| {
                t.execute("INSERT INTO github_closure_cursor(project,repo,after_issue) VALUES (?1,?2,?3) ON CONFLICT(project,repo) DO UPDATE SET after_issue=excluded.after_issue",params![p,n,number])?;
                if completed { t.execute("INSERT OR IGNORE INTO github_closed_issues(project,repo,issue) VALUES (?1,?2,?3)",params![p,n,number])?; }
                Ok(())
            }).await?;
        }
        if outcome? {
            result.push(Closure {
                number,
                comment,
                dry_run,
            });
        }
    }
    Ok(result)
}

async fn mirror_tags(path: std::path::PathBuf) -> ApiResult<BTreeSet<String>> {
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), tokio::process::Command::new("git")
        .arg("--git-dir").arg(path).args(["for-each-ref", "--count=10001", "--format=%(refname:strip=2)", "refs/tags/"])
        .kill_on_drop(true).output()).await
        .map_err(|_| ApiError::new(Code::Unavailable,"published tag read timed out"))?
        .map_err(|_| ApiError::new(Code::Unavailable,"cannot read published tags"))?;
    if !out.status.success() { return Err(ApiError::new(Code::Unavailable,"cannot read published tags")); }
    let tags: BTreeSet<_> = String::from_utf8(out.stdout).map_err(|_| ApiError::invalid("invalid published tags"))?.lines().map(str::to_owned).collect();
    if tags.len() > 10000 { return Err(ApiError::invalid("too many published tags")); }
    Ok(tags)
}

fn select_batch(candidates: BTreeMap<u64,String>, after: u64, closed: &BTreeSet<u64>, explicit: bool) -> Vec<(u64,String)> {
    let mut rows: Vec<_> = candidates.into_iter().filter(|(n,_)| explicit || !closed.contains(n)).collect();
    rows.sort_by_key(|(n,_)| (*n <= after, *n));
    rows.truncate(20);
    rows
}

struct Budget { remaining: usize }
impl Default for Budget { fn default() -> Self { Self { remaining: 60 } } }

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
#[cfg(test)]
fn candidates(c: &Connection, project: &str, repo: &str) -> ApiResult<BTreeMap<u64, String>> {
    candidates_with(c, project, repo, BTreeSet::new(), None)
}

fn candidates_with(c: &Connection, project: &str, repo: &str, mut published: BTreeSet<String>, explicit: Option<(u64,String)>) -> ApiResult<BTreeMap<u64, String>> {
    published.extend(version_groups::built_versions(c, project)?.into_iter().map(|v| format!("v{v}")));
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
            let mut links = c.prepare("SELECT ref FROM card_links WHERE card=?1 AND kind='url' ORDER BY ref LIMIT 101")?;
            let mut link_count = 0;
            for link in links.query_map([&id], |r| r.get::<_, String>(0))? {
                link_count += 1;
                if link_count > 100 { return Err(ApiError::invalid("issue closure exceeds 100 URL links per card")); }
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
                    .find_map(|v| published_tag(v, &published))
            } else {
                None
            };
            for n in numbers {
                if explicit.as_ref().is_some_and(|(only,_)| *only != n) { continue; }
                let version = if column == done && !labels.iter().any(|l| l == "hold") {
                    explicit.as_ref().map(|(_,v)| format!("v{v}")).or_else(|| version.clone())
                } else { None };
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

fn published_tag(label: &str, published: &BTreeSet<String>) -> Option<String> {
    let version = label.strip_prefix('v').unwrap_or(label);
    [format!("v{version}"), format!("v{version}-alpha")].into_iter().find(|v| published.contains(v))
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

async fn response(request: reqwest::RequestBuilder, budget: &mut Budget) -> ApiResult<Value> {
    if budget.remaining == 0 { return Err(ApiError::new(Code::Unavailable, "GitHub request budget exhausted")); }
    budget.remaining -= 1;
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
    budget: &mut Budget,
) -> ApiResult<bool> {
    let url = format!("{base}/issues/{number}");
    let issue = response(client.get(&url), budget).await?;
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
    for page in 1..=10 {
        let rows = response(client.get(format!("{url}/comments?per_page=100&page={page}")), budget).await?;
        let rows = rows
            .as_array()
            .ok_or_else(|| ApiError::invalid("invalid GitHub comments"))?;
        found |= rows
            .iter()
            .any(|r| r["body"].as_str().is_some_and(|s| s.contains(MARKER)));
        if found || rows.len() < 100 {
            break;
        }
        if page == 10 {
            return Err(ApiError::invalid(
                "GitHub comment scan exceeds 1000 comments",
            ));
        }
    }
    if !found {
        response(
            client
                .post(format!("{url}/comments"))
                .header("Content-Type", "application/json")
                .body(json!({"body":comment}).to_string()),
            budget,
        )
        .await?;
    }
    response(
        client
            .patch(url)
            .header("Content-Type", "application/json")
            .body(json!({"state":"closed", "state_reason":"completed"}).to_string()),
        budget,
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
        let mut budget = Budget::default();
        assert!(close_one(&client, &base, 7, &comment, true, &mut budget).await.unwrap());
        assert_eq!(posted.load(Ordering::SeqCst), 0);
        assert!(close_one(&client, &base, 7, &comment, false, &mut budget).await.is_err());
        assert!(close_one(&client, &base, 7, &comment, false, &mut budget).await.unwrap());
        assert!(!close_one(&client, &base, 7, &comment, false, &mut budget).await.unwrap());
        let mut exhausted = Budget { remaining: 0 };
        assert!(close_one(&client, &base, 7, &comment, false, &mut exhausted).await.unwrap_err().message.contains("budget"));
        assert_eq!(budget.remaining, 51);
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
            vec!["version:1.0.0".into()],
        );
        assert!(candidates(&t, "p", "repo").unwrap().is_empty());
        // Hand cuts have a repository tag but no group acknowledgement.
        let published = BTreeSet::from(["v1.0.0-alpha".into()]);
        assert!(candidates_with(&t,"p","repo",published.clone(),None).unwrap()[&7].contains("v1.0.0-alpha"));
        let legacy = make(&mut t,"GitHub #20: legacy", "done",vec![]);
        assert!(!candidates_with(&t,"p","repo",published.clone(),None).unwrap().contains_key(&20));
        assert!(candidates_with(&t,"p","repo",published.clone(),Some((20,"1.0.0-alpha".into()))).unwrap()[&20].contains(&legacy.id));
        t.execute("UPDATE cards SET labels='[\"hold\"]' WHERE id=?1",[&legacy.id]).unwrap();
        assert!(candidates_with(&t,"p","repo",published,Some((20,"1.0.0-alpha".into()))).unwrap().is_empty());
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

    #[test]
    fn issue_budget_continues_after_cursor_and_skips_cached_closures() {
        let candidates: BTreeMap<_,_> = (1..=50).map(|n|(n,n.to_string())).collect();
        let first = select_batch(candidates.clone(),0,&BTreeSet::new(),false);
        assert_eq!(first.len(),20);
        assert_eq!(first.last().unwrap().0,20);
        let next = select_batch(candidates.clone(),20,&BTreeSet::from([21,22]),false);
        assert_eq!(next.first().unwrap().0,23);
        assert_eq!(next.len(),20);
        let wrap = select_batch(candidates,49,&BTreeSet::from([1,2]),false);
        assert_eq!(wrap[0].0,50);
        assert_eq!(wrap[1].0,3);
    }
}
