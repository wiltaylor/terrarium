//! `POST /api/agents/{id}/attachments` (`docs/api.md` §6.5a): put an image
//! into the agent's workspace, so that the next turn can name its path.
//!
//! The body is the raw image, with the `Content-Type` of the image (the
//! dashboard), or JSON `{"data_base64"}` (the CLI: its transports, the
//! runner tunnel too, carry JSON only). The daemon checks the magic bytes,
//! then writes the file to
//! `<workspace>/.terrarium/attachments/<YYYYMMDD-HHMMSS>-<id>.<ext>` (a temp
//! file, then a rename). The answer names the path that the harness sees
//! (`Supervisor::workspace_inside`), never the host path.
//!
//! When the workspace root is a git repo, `/.terrarium/` goes into its
//! `.git/info/exclude` once, so an agent does not commit the attachments.
//! The prune timer removes attachments older than [`MAX_AGE`] ([`prune`]);
//! the rest go with the workspace when the agent is removed.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use base64::Engine;
use serde::Serialize;
use serde_json::json;

use super::Shared;
use super::agents::load;
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;

/// The largest image: 20 MiB.
pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// The attachments dir, relative to the workspace root.
pub const ATTACHMENTS_DIR: &str = ".terrarium/attachments";

/// The line in `.git/info/exclude` that keeps the attachments out of git.
pub const EXCLUDE_LINE: &str = "/.terrarium/";

/// The prune timer removes an attachment older than this.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 86_400);

/// The request body limit: the largest image as base64 in JSON, with room
/// for the rest. A body over it is a `413 too_large` in the API error
/// shape, from the handler.
const BODY_LIMIT: usize = MAX_ATTACHMENT_BYTES / 3 * 4 + 64 * 1024;

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new().route(
        "/agents/{id}/attachments",
        post(upload).layer(DefaultBodyLimit::max(BODY_LIMIT)),
        &[(axum::http::Method::POST, super::link_registry::Access::Link)],
    )
}

/// The JSON form of an upload.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonUpload {
    data_base64: String,
}

/// The image types an attachment can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    pub const ALL: [ImageKind; 4] = [Self::Png, Self::Jpeg, Self::Gif, Self::Webp];

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// The kind of a `Content-Type` (without parameters). `image/jpg` is
    /// the same as `image/jpeg`.
    pub fn from_content_type(ct: &str) -> Option<Self> {
        let ct = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match ct.as_str() {
            "image/jpg" => Some(Self::Jpeg),
            _ => Self::ALL.into_iter().find(|k| k.content_type() == ct),
        }
    }

    /// The kind from the magic bytes of the file.
    pub fn sniff(b: &[u8]) -> Option<Self> {
        if b.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

#[derive(Debug, Serialize)]
struct Attachment {
    /// The path of the file as the agent's harness sees it.
    path: String,
    /// The file name.
    name: String,
    bytes: usize,
    content_type: &'static str,
}

fn unsupported(m: impl Into<String>) -> ApiError {
    ApiError::new(Code::UnsupportedMediaType, m)
}

fn too_large() -> ApiError {
    ApiError::new(
        Code::TooLarge,
        format!(
            "an image has at most {} MiB",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ),
    )
}

async fn upload(
    State(state): State<Shared>,
    actor: Actor,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<Attachment>)> {
    let a = load(&state, &actor, id).await?;
    // The same right as `POST /api/agents/{id}/send`.
    policy::admin_project(&actor, &a.agent.project, "attaching an image to an agent")?;
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let json = ct
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"));
    if !json && ImageKind::from_content_type(ct).is_none() {
        return Err(unsupported(format!(
            "the Content-Type `{ct}` is not an image type; send image/png, image/jpeg, image/gif, or image/webp"
        )));
    }
    let mut body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_large()
        } else {
            ApiError::bad_request(format!("cannot read the body: {e}"))
        }
    })?;
    if json {
        let j: JsonUpload = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad JSON body: {e}")))?;
        body = base64::engine::general_purpose::STANDARD
            .decode(j.data_base64.trim())
            .map_err(|e| ApiError::bad_request(format!("`data_base64` is not base64: {e}")))?
            .into();
    }
    if body.len() > MAX_ATTACHMENT_BYTES {
        return Err(too_large());
    }
    if body.is_empty() {
        return Err(ApiError::invalid("the image is empty"));
    }
    // The bytes decide the type, not the header.
    let kind = ImageKind::sniff(&body).ok_or_else(|| {
        unsupported(
            "the body is not a PNG, JPEG, GIF, or WebP image (the magic bytes do not match)",
        )
    })?;

    let workspace = crate::workspace::ensure_generic(&state, &a.agent, false)
        .await?
        .ok_or_else(|| {
            ApiError::internal(format!("cannot create {}", a.agent.workspace.display()))
        })?;
    let name = file_name(chrono::Utc::now(), kind);
    let bytes = body.len();
    {
        let name = name.clone();
        tokio::task::spawn_blocking(move || store(&workspace, &name, &body, SystemTime::now()))
            .await
            .map_err(|e| ApiError::internal(format!("the attachment task failed: {e}")))??;
    }
    let path = state
        .supervisor
        .workspace_inside(&a.agent)
        .join(ATTACHMENTS_DIR)
        .join(&name);

    let (aid, project, logged) = (
        a.agent.id.to_string(),
        a.agent.project.clone(),
        name.clone(),
    );
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "agent.attachment",
                &aid,
                Some(&project),
                json!({"name": logged, "bytes": bytes, "content_type": kind.content_type()}),
            )
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(Attachment {
            path: path.display().to_string(),
            name,
            bytes,
            content_type: kind.content_type(),
        }),
    ))
}

/// `<YYYYMMDD-HHMMSS>-<the last 8 characters of a ULID, lower case>.<ext>`.
pub fn file_name(now: chrono::DateTime<chrono::Utc>, kind: ImageKind) -> String {
    let id = ulid::Ulid::new().to_string().to_ascii_lowercase();
    let short = &id[id.len() - 8..];
    format!("{}-{short}.{}", now.format("%Y%m%d-%H%M%S"), kind.ext())
}

fn io(what: &str, p: &Path, e: std::io::Error) -> ApiError {
    ApiError::internal(format!("cannot {what} {}: {e}", p.display()))
}

/// Make `dir` as a real dir. A symlink is refused: the agent owns the
/// workspace, and the daemon must not write through its links.
fn real_dir(dir: &Path) -> ApiResult<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(ApiError::conflict(format!(
            "{} in the workspace is not a dir; remove it and try again",
            dir.file_name().unwrap_or_default().to_string_lossy()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(dir).map_err(|e| io("create", dir, e))?;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
            Ok(())
        }
        Err(e) => Err(io("read", dir, e)),
    }
}

/// Write `data` to `<workspace>/.terrarium/attachments/<name>` (a temp
/// file, then a rename; mode 0644), add the git exclude line, and remove
/// the attachments of this workspace older than [`MAX_AGE`].
pub fn store(workspace: &Path, name: &str, data: &[u8], now: SystemTime) -> ApiResult<PathBuf> {
    let top = workspace.join(".terrarium");
    real_dir(&top)?;
    let dir = workspace.join(ATTACHMENTS_DIR);
    real_dir(&dir)?;
    let tmp = dir.join(format!(".{name}.part"));
    let fin = dir.join(name);
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(data)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        f.sync_all()?;
        std::fs::rename(&tmp, &fin)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(io("write", &fin, e));
    }
    if let Err(e) = exclude(workspace) {
        tracing::warn!(workspace = %workspace.display(), "cannot add the git exclude of the attachments: {e}");
    }
    prune_dir(&dir, now, MAX_AGE);
    Ok(fin)
}

/// Add [`EXCLUDE_LINE`] to `<workspace>/.git/info/exclude` once, when the
/// workspace root is a git repo (a `.git` dir). True: the line went in.
/// A symlink on the way is not followed.
pub fn exclude(workspace: &Path) -> std::io::Result<bool> {
    let git = workspace.join(".git");
    if !std::fs::symlink_metadata(&git).is_ok_and(|m| m.is_dir()) {
        return Ok(false);
    }
    let info = git.join("info");
    match std::fs::symlink_metadata(&info) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&info)?,
        Err(e) => return Err(e),
    }
    let file = info.join("exclude");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o644)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&file)?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut f, &mut text)?;
    if text.lines().any(|l| {
        let l = l.trim();
        l == EXCLUDE_LINE || l == ".terrarium/" || l == "/.terrarium" || l == ".terrarium"
    }) {
        return Ok(false);
    }
    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    write!(
        f,
        "{sep}# Terrarium: images the operator attached to a turn\n{EXCLUDE_LINE}\n"
    )?;
    Ok(true)
}

/// Remove the files in `dir` older than `max_age` (by mtime). A symlink
/// `dir` is not followed. Returns the count.
fn prune_dir(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let Ok(m) = e.path().symlink_metadata() else {
            continue;
        };
        if m.is_dir() {
            continue;
        }
        let old = m
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if old && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// Remove the attachments older than [`MAX_AGE`] in every workspace under
/// the work root (`<work>/<project>/<agent>/.terrarium/attachments`). The
/// prune timer calls it. Returns the count.
pub fn prune(work_dir: &Path, now: SystemTime) -> usize {
    let mut n = 0;
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    for project in dirs(work_dir) {
        for ws in dirs(&project) {
            let top = ws.join(".terrarium");
            if std::fs::symlink_metadata(&top).is_ok_and(|m| m.is_dir()) {
                n += prune_dir(&ws.join(ATTACHMENTS_DIR), now, MAX_AGE);
            }
        }
    }
    n
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sniff_and_content_types() {
        assert_eq!(
            ImageKind::sniff(b"\x89PNG\r\n\x1a\nrest"),
            Some(ImageKind::Png)
        );
        assert_eq!(
            ImageKind::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(ImageKind::sniff(b"GIF89a.."), Some(ImageKind::Gif));
        assert_eq!(
            ImageKind::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageKind::Webp)
        );
        assert_eq!(ImageKind::sniff(b"RIFF\0\0\0\0WAVE"), None);
        assert_eq!(ImageKind::sniff(b"<svg"), None);
        assert_eq!(
            ImageKind::from_content_type("image/JPG; q=1"),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(ImageKind::from_content_type("image/svg+xml"), None);
        let n = file_name(chrono::Utc::now(), ImageKind::Webp);
        assert_eq!(n.len(), "20261028-120000-abcdefgh.webp".len(), "{n}");
    }

    #[test]
    fn store_refuses_a_symlinked_dir_and_prunes_old_files() {
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().join("ws");
        let elsewhere = t.path().join("elsewhere");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, ws.join(".terrarium")).unwrap();
        assert!(store(&ws, "a.png", b"x", SystemTime::now()).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        std::fs::remove_file(ws.join(".terrarium")).unwrap();

        let p = store(&ws, "a.png", b"x", SystemTime::now()).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        // A file eight days old: the next store removes it, a new one stays.
        let eight_days = Duration::from_secs(8 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(SystemTime::now() - eight_days)
            .unwrap();
        store(&ws, "b.png", b"y", SystemTime::now()).unwrap();
        assert!(!p.exists());
        assert!(ws.join(ATTACHMENTS_DIR).join("b.png").exists());
        // The timer walks `<work>/<project>/<agent>`.
        let work = t.path();
        let later = SystemTime::now() + eight_days;
        assert_eq!(prune(work, later), 0, "`ws` is not two levels down");
        std::fs::create_dir_all(t.path().join("p")).unwrap();
        std::fs::rename(&ws, t.path().join("p/ws")).unwrap();
        assert_eq!(prune(work, SystemTime::now()), 0, "b.png is new");
        assert_eq!(prune(work, later), 1);
    }

    #[test]
    fn exclude_once_and_not_without_git() {
        let t = tempfile::tempdir().unwrap();
        assert!(!exclude(t.path()).unwrap());
        std::fs::create_dir_all(t.path().join(".git")).unwrap();
        assert!(exclude(t.path()).unwrap());
        assert!(!exclude(t.path()).unwrap());
        let text = std::fs::read_to_string(t.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(EXCLUDE_LINE).count(), 1, "{text}");
    }
}
