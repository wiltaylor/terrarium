//! "Fetch models" of a self-hosted server
//! (`POST /api/subscriptions/fetch-models`, `terrarium_core::selfhosted`).
//!
//! This is the one place where the daemon sends a request to a URL that
//! an operator typed (a server-side request to any host; an SSRF risk).
//! The limits:
//!
//! - Only the operator and sysadmin can ask for it (the API checks
//!   `policy::manage_subscriptions`). An agent never can.
//! - `http` and `https` only (`check_http_url`); no user or password in
//!   the URL.
//! - One `GET` of `{base}/models` (OpenAI style) or `{base}/v1/models`
//!   (Anthropic style). No redirect is followed: a `3xx` answer is an
//!   error.
//! - [`TIMEOUT`] for the whole request, and at most [`MAX_BODY`] bytes of
//!   body.
//! - The answer is a list of model ids, nothing else. The key goes in
//!   `Authorization: Bearer` (and `x-api-key` for the Anthropic style);
//!   it is never in an error text.
//!
//! A list is cached for [`FETCH_TTL`] per base URL and style.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use terrarium_core::selfhosted::{Api, SelfHosted};

use super::{ModelInfo, parse_openai_models};

/// The time limit of one fetch.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// The most bytes of a model list body.
pub const MAX_BODY: usize = 1024 * 1024;
/// How long a fetched list stays in the cache.
pub const FETCH_TTL: Duration = Duration::from_secs(5 * 60);
/// The Anthropic API version header of the Anthropic-style list.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The fetched lists, by `<api> <base url>`.
#[derive(Default)]
pub struct FetchCache(Mutex<HashMap<String, (Instant, Vec<ModelInfo>)>>);

impl FetchCache {
    fn key(sh: &SelfHosted) -> String {
        format!("{} {}", sh.api.as_str(), sh.base_url)
    }

    pub fn get(&self, sh: &SelfHosted) -> Option<Vec<ModelInfo>> {
        let m = self.0.lock().ok()?;
        let (at, list) = m.get(&Self::key(sh))?;
        (at.elapsed() < FETCH_TTL).then(|| list.clone())
    }

    pub fn put(&self, sh: &SelfHosted, list: &[ModelInfo]) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(Self::key(sh), (Instant::now(), list.to_vec()));
        }
    }
}

/// Fetch the model list of the server of `sh` with `key` (none: no
/// auth header). `sh` must be checked (`SelfHosted::checked`).
pub async fn fetch(sh: &SelfHosted, key: Option<&str>) -> Result<Vec<ModelInfo>, String> {
    let url = sh.models_url();
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("terrarium/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("cannot make the HTTP client: {e}"))?;
    let mut req = client.get(&url).header("accept", "application/json");
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        req = req.header("authorization", format!("Bearer {k}"));
        if sh.api == Api::Anthropic {
            req = req.header("x-api-key", k);
        }
    }
    if sh.api == Api::Anthropic {
        req = req.header("anthropic-version", ANTHROPIC_VERSION);
    }
    let scrub = |e: String| match key {
        Some(k) if !k.is_empty() => e.replace(k, "<redacted>"),
        _ => e,
    };
    // reqwest's error text names the URL, never a header.
    let mut resp = req
        .send()
        .await
        .map_err(|e| scrub(format!("GET {url}: {e}")))?;
    let status = resp.status();
    if status.is_redirection() {
        return Err(format!(
            "GET {url}: status {} (a redirect); Terrarium does not follow redirects, so give \
             the final base URL",
            status.as_u16()
        ));
    }
    if !status.is_success() {
        return Err(format!("GET {url}: status {}", status.as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_BODY as u64) {
        return Err(format!(
            "GET {url}: the answer is larger than {MAX_BODY} bytes"
        ));
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| scrub(format!("GET {url}: {e}")))?
    {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(format!(
                "GET {url}: the answer is larger than {MAX_BODY} bytes"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&body);
    // Both styles answer `{"data": [{"id": ...}, ...]}`.
    parse_openai_models(&text).map_err(|e| format!("GET {url}: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;

    async fn serve(app: Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn sh(api: Api, base: &str) -> SelfHosted {
        SelfHosted {
            api,
            base_url: base.into(),
            ..SelfHosted::default()
        }
        .checked()
        .unwrap()
    }

    #[tokio::test]
    async fn openai_and_anthropic_lists() {
        let app = Router::new()
            .route(
                "/v1/models",
                get(|h: HeaderMap| async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    let ver = h.contains_key("anthropic-version");
                    let body = format!(
                        r#"{{"object":"list","data":[{{"id":"qwen3-coder"}},{{"id":"{}"}},{{"id":"{}"}}]}}"#,
                        if auth == "Bearer sk-1" { "auth-ok" } else { "no-auth" },
                        if ver { "anthropic" } else { "openai" }
                    );
                    ([("content-type", "application/json")], body)
                }),
            );
        let base = serve(app).await;
        let o = fetch(&sh(Api::Openai, &format!("{base}/v1")), Some("sk-1"))
            .await
            .unwrap();
        let ids: Vec<&str> = o.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["qwen3-coder", "auth-ok", "openai"]);
        let a = fetch(&sh(Api::Anthropic, &base), None).await.unwrap();
        let ids: Vec<&str> = a.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["qwen3-coder", "no-auth", "anthropic"]);
    }

    #[tokio::test]
    async fn redirects_errors_and_big_bodies_are_refused() {
        let app = Router::new()
            .route(
                "/redir/models",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "http://169.254.169.254/latest/meta-data")],
                    )
                        .into_response()
                }),
            )
            .route(
                "/denied/models",
                get(|| async { StatusCode::UNAUTHORIZED.into_response() }),
            )
            .route(
                "/big/models",
                get(|| async { "x".repeat(MAX_BODY + 10).into_response() }),
            )
            .route("/junk/models", get(|| async { "<html>" }));
        let base = serve(app).await;
        let e = fetch(&sh(Api::Openai, &format!("{base}/redir")), None)
            .await
            .unwrap_err();
        assert!(e.contains("redirect"), "{e}");
        let e = fetch(
            &sh(Api::Openai, &format!("{base}/denied")),
            Some("sk-secret"),
        )
        .await
        .unwrap_err();
        assert!(e.contains("401") && !e.contains("sk-secret"), "{e}");
        let e = fetch(&sh(Api::Openai, &format!("{base}/big")), None)
            .await
            .unwrap_err();
        assert!(e.contains("larger than"), "{e}");
        let e = fetch(&sh(Api::Openai, &format!("{base}/junk")), None)
            .await
            .unwrap_err();
        assert!(e.contains("not an OpenAI model list"), "{e}");
    }

    #[test]
    fn the_cache_keys_by_style_and_url() {
        let c = FetchCache::default();
        let a = sh(Api::Openai, "http://gpu/v1");
        let b = sh(Api::Anthropic, "http://gpu/v1");
        c.put(&a, &[ModelInfo::id("m")]);
        assert_eq!(c.get(&a).unwrap()[0].id, "m");
        assert!(c.get(&b).is_none());
    }
}
