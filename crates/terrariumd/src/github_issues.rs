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

mod close;
pub use close::close_done;

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

/// Called only after the ingest listener verifies the source's signature.
/// A source can import only repositories registered in its own project.
pub(crate) fn import_webhook(
    t: &mut Tx<'_>,
    project: &str,
    payload: serde_json::Value,
) -> ApiResult<board::Card> {
    let url = payload["repository"]["html_url"]
        .as_str()
        .ok_or_else(|| ApiError::invalid("missing GitHub repository URL"))?;
    let target = repository(url)?;
    let issue: Issue = serde_json::from_value(payload["issue"].clone())
        .map_err(|_| ApiError::invalid("invalid GitHub issue"))?;
    if issue.number == 0 || issue.pull_request.is_some() || issue.user.login.is_empty() {
        return Err(ApiError::invalid("not a GitHub issue"));
    }
    let mut page = crate::store::Page::default();
    loop {
        let repos = repos::list_repos(t, project, &page)?;
        for repo in repos.items {
            if repository(&repo.url).ok().as_deref() == Some(target.as_str()) {
                return import_verified(t, &Actor::Operator, project, &repo.name, &target, issue);
            }
        }
        match repos.next {
            Some(cursor) => page.cursor = Some(cursor),
            None => {
                return Err(ApiError::invalid(
                    "GitHub repository is not registered in this project",
                ));
            }
        }
    }
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
    let request = client
        .get(url)
        .header("User-Agent", "terrarium")
        .header("Accept", "application/vnd.github+json");
    let credential = match &repo.credential {
        Some(name) => secrets::load(state, repo.owner(), name).await.ok(),
        None => None,
    };
    let issue = fetch_issue(token_auth(request, credential), number).await?;
    state
        .db
        .write(actor.label(), move |t| {
            import_verified(t, &actor, &project, &repo.name, &repository, issue)
        })
        .await
}

fn token_auth(
    request: reqwest::RequestBuilder,
    credential: Option<Secret>,
) -> reqwest::RequestBuilder {
    match credential {
        Some(Secret::Token { token, .. }) => request.bearer_auth(token.as_str()),
        _ => request,
    }
}

async fn fetch_issue(request: reqwest::RequestBuilder, number: u64) -> ApiResult<Issue> {
    let authenticated = request
        .try_clone()
        .and_then(|r| r.build().ok())
        .is_some_and(|r| r.headers().contains_key(reqwest::header::AUTHORIZATION));
    let mut response = request
        .send()
        .await
        .map_err(|_| ApiError::new(Code::Unavailable, "GitHub issue fetch failed"))?;
    if !response.status().is_success() {
        if !authenticated && matches!(response.status().as_u16(), 401 | 403 | 404) {
            return Err(ApiError::new(
                Code::Unavailable,
                "GitHub issue unavailable without a token. For a private repository, configure a granted token credential; public issues may be missing or rate-limited.",
            ));
        }
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
            title: Some(format!(
                "GitHub #{}: {}",
                issue.number,
                issue
                    .title
                    .chars()
                    .filter(|c| !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(200)
                    .collect::<String>()
            )),
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
            .route(
                "/ok",
                get(move |headers: axum::http::HeaderMap| async move {
                    assert!(!headers.contains_key("authorization"));
                    valid
                }),
            )
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
        let ssh = Secret::SshKey(zeroize::Zeroizing::new("test deploy key".into()));
        assert_eq!(
            fetch_issue(token_auth(request("ok"), Some(ssh)), 7)
                .await
                .unwrap()
                .number,
            7
        );
        assert!(
            fetch_issue(token_auth(request("missing"), None), 7)
                .await
                .err()
                .unwrap()
                .message
                .contains("private repository")
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
        let operator_held = import_verified(
            &mut t,
            &Actor::Operator,
            "p",
            "repo",
            "o/r",
            issue(20, "eve"),
        )
        .unwrap();
        assert_eq!(operator_held.column, "blocked");
        assert!(operator_held.labels.contains(&"hold".into()));
        let operator_allowed = import_verified(
            &mut t,
            &Actor::Operator,
            "p",
            "repo",
            "o/r",
            issue(21, "wiltaylor"),
        )
        .unwrap();
        assert_eq!(operator_allowed.column, "backlog");
        let again =
            import_verified(&mut t, &actor, "p", "repo", "o/r", issue(2, "wiltaylor")).unwrap();
        assert_eq!(again.id, held.id);
        assert_eq!(again.column, "blocked");
        projects::set_github_authors(&mut t, "p", &[]).unwrap();
        let unconfigured =
            import_verified(&mut t, &actor, "p", "repo", "o/r", issue(3, "wiltaylor")).unwrap();
        assert_eq!(unconfigured.column, "blocked");
        assert!(unconfigured.labels.contains(&"hold".into()));
        let forged = board::create_card(
            &mut t,
            &actor,
            "p",
            board::NewCard {
                title: Some("ordinary".into()),
                ..Default::default()
            },
        )
        .unwrap();
        board::add_link(
            &mut t,
            &actor,
            &forged.id,
            &board::NewLink {
                kind: "url".into(),
                reference: "https://github.com/o/r/issues/4".into(),
            },
        )
        .unwrap();
        let mut malicious = issue(4, "wiltaylor");
        malicious.title = format!(
            "bad\n\r\u{1b}\u{061c}\u{200e}\u{200f}\u{202e}\u{2066}{}",
            "x".repeat(1000)
        );
        let imported = import_verified(&mut t, &actor, "p", "repo", "o/r", malicious).unwrap();
        assert_ne!(imported.id, forged.id);
        assert!(!imported.title.chars().any(char::is_control));
        assert!(imported.title.starts_with("GitHub #4: bad   xxx"));
        assert!(imported.title.chars().count() <= 230);
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
