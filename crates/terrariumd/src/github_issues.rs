//! Daemon-owned GitHub issue import. Network data never supplies instructions.

use rusqlite::{OptionalExtension, params};
use serde::Deserialize;

use crate::actor::Actor;
use crate::api::Shared;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::secrets::{self, Secret};
use crate::store::{board, repos};

#[derive(Deserialize)]
pub(crate) struct Issue {
    number: u64,
    title: String,
    body: Option<String>,
    user: User,
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

/// Only canonical GitHub repository URLs can select an API resource.
fn repository(url: &str) -> ApiResult<String> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .ok_or_else(|| ApiError::invalid("issue import needs a github.com repository"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err(ApiError::invalid("invalid GitHub owner/repository"));
    }
    Ok(path.to_ascii_lowercase())
}

/// Reusable importer. Credentials and the repository come from daemon config,
/// never from an arbitrary URL in the request. Failed fetches create no card.
pub async fn import(
    state: &Shared,
    actor: Actor,
    project: String,
    repo_name: String,
    number: u64,
) -> ApiResult<board::Card> {
    policy::write_project(&actor, &project)?;
    if number == 0 {
        return Err(ApiError::invalid("issue number must be positive"));
    }
    let p = project.clone();
    let repo = state
        .db
        .read(move |c| repos::resolve(c, &repo_name, Some(&p)))
        .await?;
    if repo.project != project {
        return Err(ApiError::invalid("repository belongs to another project"));
    }
    let repository = repository(&repo.url)?;
    let url = format!("https://api.github.com/repos/{repository}/issues/{number}");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| ApiError::internal("cannot build GitHub client"))?;
    let mut request = client
        .get(url)
        .header("User-Agent", "terrarium")
        .header("Accept", "application/vnd.github+json");
    if let Some(name) = &repo.credential {
        let Secret::Token { token, .. } = secrets::load(state, repo.owner(), name).await? else {
            return Err(ApiError::invalid(
                "GitHub issue import needs a token credential",
            ));
        };
        request = request.bearer_auth(token.as_str());
    }
    let issue = fetch_issue(request, number).await?;
    state
        .db
        .write(actor.label(), move |t| {
            import_verified(t, &actor, &project, &repo.name, &repository, issue)
        })
        .await
}

async fn fetch_issue(request: reqwest::RequestBuilder, number: u64) -> ApiResult<Issue> {
    let mut response = request
        .send()
        .await
        .map_err(|_| ApiError::new(Code::Unavailable, "GitHub issue fetch failed"))?;
    if !response.status().is_success() {
        return Err(ApiError::new(
            Code::Unavailable,
            format!("GitHub issue fetch returned {}", response.status()),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ApiError::new(Code::Unavailable, "GitHub issue read failed"))?
    {
        if body.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(ApiError::new(
                Code::Unavailable,
                "GitHub issue response too large",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let issue: Issue = serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(Code::Unavailable, "invalid GitHub issue response"))?;
    if issue.number != number || issue.pull_request.is_some() || issue.user.login.is_empty() {
        return Err(ApiError::invalid(
            "response is not the requested GitHub issue",
        ));
    }
    Ok(issue)
}

/// A verified webhook can reuse this transaction without trusting a caller's
/// author assertion. The import identity is not writable through the card API.
pub(crate) fn import_verified(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    repo: &str,
    repository: &str,
    issue: Issue,
) -> ApiResult<board::Card> {
    policy::write_project(actor, project)?;
    let url = format!("https://github.com/{repository}/issues/{}", issue.number);
    let existing: Option<String> = t
        .query_row(
            "SELECT card FROM github_issue_imports WHERE project = ?1 AND url = ?2",
            params![project, url],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return board::get_card(t, &id);
    }
    let card = board::create_card_with_author(
        t,
        actor,
        project,
        board::NewCard {
            title: Some(format!("GitHub #{}: {}", issue.number, issue.title)),
            body: Some(format!(
                "{url}\nAuthor: {}\n\n{}",
                issue.user.login,
                issue.body.unwrap_or_default()
            )),
            labels: vec!["github".into()],
            repo: Some(repo.into()),
            ..Default::default()
        },
        Some(&issue.user.login),
    )?;
    t.execute(
        "INSERT INTO card_links (card, kind, ref, created_by, created_at)
         VALUES (?1, 'url', ?2, ?3, ?4)",
        params![
            card.id,
            url,
            actor.address(),
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    t.execute(
        "INSERT INTO github_issue_imports (project, url, card) VALUES (?1, ?2, ?3)",
        params![project, url, card.id],
    )?;
    board::get_card(t, &card.id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_checks_api_data_and_refuses_errors() {
        use axum::{Router, routing::get};
        let valid = r#"{"number":7,"title":"x","body":null,"user":{"login":"eve"}}"#;
        let app = Router::new()
            .route("/ok", get(move || async move { valid }))
            .route("/bad", get(|| async { "not json" }))
            .route(
                "/pr",
                get(|| async {
                    r#"{"number":7,"title":"x","user":{"login":"wiltaylor"},"pull_request":{}}"#
                }),
            )
            .route(
                "/redirect",
                get(|| async { axum::response::Redirect::temporary("/ok") }),
            )
            .route("/large", get(|| async { "x".repeat(2 * 1024 * 1024 + 1) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let request = |path| client.get(format!("http://{addr}/{path}"));
        assert_eq!(
            fetch_issue(request("ok"), 7).await.unwrap().user.login,
            "eve"
        );
        assert!(fetch_issue(request("ok"), 8).await.is_err());
        for path in ["bad", "pr", "redirect", "missing", "large"] {
            assert!(fetch_issue(request(path), 7).await.is_err(), "{path}");
        }
        server.abort();
    }

    #[test]
    fn only_fixed_github_repositories() {
        assert_eq!(
            repository("git@github.com:Owner/Repo.git").ok().as_deref(),
            Some("owner/repo")
        );
        for url in [
            "https://evil.test/a/b",
            "https://github.com/a/b?x=y",
            "https://github.com/a/..",
            "https://github.com/a/b/c",
        ] {
            assert!(repository(url).is_err(), "{url}");
        }
    }

    #[test]
    fn verified_authors_gate_and_import_is_idempotent() {
        use crate::store::{agents, projects};
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
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
        projects::set_github_authors(&mut t, "p", &["wiltaylor".into(), "wiltaylor-daf".into()])
            .unwrap();
        let def = toml::from_str("name = 'a'\nproject = 'p'\nharness = 'claude'\n").unwrap();
        agents::create(&mut t, &def, std::path::Path::new("/state"), "api").unwrap();
        let actor = Actor::Agent(agents::actor_for(&t, "p/a").unwrap().unwrap());
        let issue = |number, login| {
            serde_json::from_value(serde_json::json!({
                "number": number, "title": "issue", "body": "untrusted data",
                "user": {"login": login}
            }))
            .unwrap()
        };
        let a = import_verified(
            &mut t,
            &actor,
            "p",
            "repo",
            "o/r",
            issue(1, "WilTaylor-DAF"),
        )
        .unwrap();
        assert_eq!(a.column, "backlog");
        let again = import_verified(&mut t, &actor, "p", "repo", "o/r", issue(1, "eve")).unwrap();
        assert_eq!(again.id, a.id);
        let held = import_verified(&mut t, &actor, "p", "repo", "o/r", issue(2, "eve")).unwrap();
        assert_eq!(held.column, "blocked");
        assert!(held.labels.contains(&"hold".into()));
        let again =
            import_verified(&mut t, &actor, "p", "repo", "o/r", issue(2, "wiltaylor")).unwrap();
        assert_eq!(again.id, held.id);
        assert_eq!(again.column, "blocked");
        assert!(
            board::add_link(
                &mut t,
                &actor,
                &a.id,
                &board::NewLink {
                    kind: "github_issue".into(),
                    reference: "forged".into()
                }
            )
            .is_err()
        );
    }
}
