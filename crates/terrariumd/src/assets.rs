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

/// Serve a file from the dashboard. Unknown paths without an extension get
/// `index.html`, so client-side routes work.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty()
        && let Some(file) = Dist::get(path)
    {
        return file_response(path, file);
    }
    let looks_like_route = !path.rsplit('/').next().unwrap_or(path).contains('.');
    if looks_like_route {
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
