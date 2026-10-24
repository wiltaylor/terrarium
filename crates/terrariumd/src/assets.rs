//! The dashboard, embedded from `dashboard/dist` at build time.

use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../dashboard/dist"]
struct Dist;

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Terrarium</title>
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;line-height:1.5}</style>
</head>
<body>
<h1>Terrarium</h1>
<p>The daemon runs, but this build has no dashboard.</p>
<p>Build the dashboard with <code>just dashboard::build</code>, then build the daemon again.</p>
<p>The API is at <code>/api/health</code>.</p>
</body>
</html>
"#;

/// The path prefixes that are never a dashboard route: the host link
/// (`/hosts/connect`) and the artifact routes (`/a/...`). A request there
/// on a port that does not mount them gets 404, not `index.html`. So a
/// WebSocket upgrade to `/hosts/connect` on the dashboard port fails.
/// `/hosts` alone is the Hosts page of the dashboard.
const NOT_ROUTES: &[&str] = &["hosts/", "a/", "api/"];

/// True: `path` (without the leading `/`) can be a client-side route of the
/// dashboard. A path with an extension in its last segment is a file, and a
/// path under [`NOT_ROUTES`] (or `a` itself) belongs to a server route.
fn is_route(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or(path);
    !last.contains('.') && path != "a" && !NOT_ROUTES.iter().any(|p| path.starts_with(p))
}

/// Serve a file from the dashboard. Unknown paths without an extension get
/// `index.html`, so client-side routes work. A path that [`is_route`]
/// refuses gets 404.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty()
        && let Some(file) = Dist::get(path)
    {
        return file_response(path, file);
    }
    if is_route(path) {
        return match Dist::get("index.html") {
            Some(file) => file_response("index.html", file),
            None => Html(PLACEHOLDER).into_response(),
        };
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn file_response(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_owned()),
            (header::CACHE_CONTROL, cache.to_owned()),
        ],
        file.data,
    )
        .into_response()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    async fn status(path: &str) -> StatusCode {
        serve(path.parse().expect("uri")).await.status()
    }

    #[tokio::test]
    async fn the_fallback_serves_routes_and_refuses_server_paths() {
        for p in [
            "/",
            "/agents",
            "/agents/01ABC",
            "/hosts",
            "/p/demo/board",
            "/no/such/page",
        ] {
            assert_eq!(status(p).await, StatusCode::OK, "{p}");
        }
        for p in [
            "/hosts/connect",
            "/hosts/x/y",
            "/a",
            "/a/01ABC/index.html",
            "/a/01ABC",
            "/api/nothing",
            "/missing.js",
            "/assets/nothing.css",
            "/agents/x.json",
        ] {
            assert_eq!(status(p).await, StatusCode::NOT_FOUND, "{p}");
        }
    }
}
