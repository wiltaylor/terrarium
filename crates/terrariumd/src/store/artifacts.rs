//! Artifacts (PLAN §15.2): files and static bundles stored by content hash.
//!
//! - Blobs live in `<state>/artifacts/<sha256>`; a bundle (a tar) is
//!   unpacked into `<state>/artifacts/<sha256>.bundle/` and served from its
//!   `index.html`. The same bytes are stored once.
//! - The content type is sniffed and fixed on upload. Executables download
//!   and never render.
//! - A per-file limit and a per-project quota come from
//!   `<config>/daemon.toml` (`[artifacts]`), with defaults.
//! - The serving routes (`/a/...`) check a short-lived HMAC signature.
//! - Retention: an artifact attached to something (a live page, a card, a
//!   label, or a version from before TER-58) or embedded in a live page is
//!   kept.
//!   An unattached artifact older than `retention_days` is pruned, after an
//!   audit warning at least a day before.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;
use crate::store::memory::{name_of, next_number, resolve_short};
use crate::store::{Page, Paged, board, link_label, projects, wiki};

pub const PREFIX: &str = "ART";
/// The hard cap of one upload, whatever the config says.
pub const HARD_MAX_BYTES: u64 = 32 * 1024 * 1024;

// ---- config, secret ----

/// `[artifacts]` in `<config>/daemon.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// The largest upload, in bytes. At most 32 MiB.
    pub max_file_bytes: u64,
    /// The total size of a project's artifacts, in bytes.
    pub project_quota_bytes: u64,
    /// Unattached artifacts older than this are pruned.
    pub retention_days: i64,
    /// The default lifetime of a signed URL, in seconds.
    pub url_ttl_secs: i64,
    /// How often the daemon runs the retention prune, in hours.
    pub prune_interval_hours: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: HARD_MAX_BYTES,
            project_quota_bytes: 1024 * 1024 * 1024,
            retention_days: 30,
            url_ttl_secs: 300,
            prune_interval_hours: 24,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct DaemonToml {
    #[serde(default)]
    artifacts: Option<Limits>,
}

/// The blob dir, the limits, and the URL signing key of one daemon.
pub struct ArtifactCtx {
    pub dir: PathBuf,
    pub limits: Limits,
    secret: Vec<u8>,
    /// Blob locks, one stripe per first digest byte ([`ArtifactCtx::digest_lock`]).
    locks: Vec<Arc<tokio::sync::Mutex<()>>>,
}

/// The number of blob lock stripes: one per value of the first digest byte.
const LOCK_STRIPES: usize = 256;

fn new_locks() -> Vec<Arc<tokio::sync::Mutex<()>>> {
    (0..LOCK_STRIPES)
        .map(|_| Arc::new(tokio::sync::Mutex::new(())))
        .collect()
}

impl std::fmt::Debug for ArtifactCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactCtx")
            .field("dir", &self.dir)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

fn load_limits(config_dir: &Path) -> Limits {
    let path = config_dir.join("daemon.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Limits::default();
    };
    match toml::from_str::<DaemonToml>(&text) {
        Ok(d) => {
            let mut l = d.artifacts.unwrap_or_default();
            l.max_file_bytes = l.max_file_bytes.min(HARD_MAX_BYTES);
            l.url_ttl_secs = l.url_ttl_secs.clamp(10, 24 * 3600);
            l
        }
        Err(e) => {
            tracing::error!(path = %path.display(), "bad daemon.toml, using artifact defaults: {e}");
            Limits::default()
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Read the signing key, or make one: 32 random bytes in
/// `<state>/artifact-url.key`, mode 0600.
fn load_secret(state_dir: &Path) -> anyhow::Result<Vec<u8>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = state_dir.join("artifact-url.key");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Some(key) = unhex(text.trim())
        && key.len() >= 32
    {
        return Ok(key);
    }
    let mut key = vec![0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    std::fs::create_dir_all(state_dir)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(hex(&key).as_bytes())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(key)
}

/// The context of the daemon with this state dir. It is made once and kept.
pub fn context(state_dir: &Path, config_dir: &Path) -> ApiResult<Arc<ArtifactCtx>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<ArtifactCtx>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = match cache.lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    if let Some(c) = map.get(state_dir) {
        return Ok(c.clone());
    }
    let ctx = Arc::new(ArtifactCtx {
        dir: state_dir.join("artifacts"),
        limits: load_limits(config_dir),
        secret: load_secret(state_dir)
            .map_err(|e| ApiError::internal(format!("cannot read the artifact URL key: {e}")))?,
        locks: new_locks(),
    });
    map.insert(state_dir.to_owned(), ctx.clone());
    Ok(ctx)
}

type HmacSha256 = Hmac<Sha256>;

impl ArtifactCtx {
    fn mac(&self, id: &str, exp: i64) -> Option<HmacSha256> {
        let mut mac = HmacSha256::new_from_slice(&self.secret).ok()?;
        mac.update(format!("{id}:{exp}").as_bytes());
        Some(mac)
    }

    /// The signature of `id` until the Unix time `exp`.
    pub fn sign(&self, id: &str, exp: i64) -> String {
        self.mac(id, exp)
            .map(|m| hex(&m.finalize().into_bytes()))
            .unwrap_or_default()
    }

    /// Check a signature and its expiry, in constant time.
    pub fn verify(&self, id: &str, exp: i64, sig: &str, now: i64) -> bool {
        if exp < now {
            return false;
        }
        let (Some(mac), Some(sig)) = (self.mac(id, exp), unhex(sig)) else {
            return false;
        };
        mac.verify_slice(&sig).is_ok()
    }

    pub fn blob_path(&self, sha: &str) -> PathBuf {
        self.dir.join(sha)
    }

    pub fn bundle_dir(&self, sha: &str) -> PathBuf {
        self.dir.join(format!("{sha}.bundle"))
    }

    /// The lock of the blobs of this digest. Hold it from the blob write
    /// until the artifact row is committed, and from the "is it used" check
    /// until the blob is removed, so an upload and a delete of the same
    /// bytes cannot leave a row without a blob. Take it before a database
    /// call, never inside one, and hold one at a time. Digests share a
    /// stripe by their first byte; that only serialises them.
    pub fn digest_lock(&self, sha: &str) -> Arc<tokio::sync::Mutex<()>> {
        let i = sha
            .get(..2)
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .unwrap_or_else(|| {
                sha.bytes()
                    .fold(0usize, |a, b| a.wrapping_mul(31) ^ b as usize)
            });
        self.locks[i % self.locks.len()].clone()
    }
}

// ---- sniffing ----

/// Types the viewer may show inline. Everything else downloads.
const RENDERABLE: [&str; 14] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/svg+xml",
    "application/pdf",
    "text/plain",
    "text/markdown",
    "text/html",
    "text/css",
    "text/csv",
    "application/json",
    "video/mp4",
    "audio/mpeg",
];

pub fn renders(content_type: &str) -> bool {
    let base = content_type.split(';').next().unwrap_or("").trim();
    RENDERABLE.contains(&base)
}

fn is_executable(data: &[u8]) -> bool {
    data.starts_with(b"\x7fELF")
        || data.starts_with(b"MZ")
        || data.starts_with(&[0xfe, 0xed, 0xfa, 0xce])
        || data.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
        || data.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
        || data.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
        || data.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
        || data.starts_with(b"\0asm")
        || data.starts_with(b"#!")
}

/// The content type of an upload, from its bytes first and its name second.
/// The type the client claims is not trusted.
pub fn sniff(name: &str, data: &[u8]) -> String {
    if is_executable(data) {
        return "application/octet-stream".into();
    }
    let magic: [(&[u8], &str); 8] = [
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"),
        (b"ID3", "audio/mpeg"),
    ];
    for (m, t) in magic {
        if data.starts_with(m) {
            return t.into();
        }
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return "video/mp4".into();
    }
    let guess = mime_guess::from_path(name).first();
    let text = !data.contains(&0) && std::str::from_utf8(data).is_ok();
    if text {
        let by_name = guess.as_ref().map(|m| m.essence_str().to_owned());
        let t = match by_name.as_deref() {
            Some(
                t @ ("text/html" | "text/css" | "text/csv" | "text/markdown" | "application/json"
                | "image/svg+xml"),
            ) => t.to_owned(),
            Some(t) if t.starts_with("text/") => "text/plain".to_owned(),
            Some("application/javascript" | "text/javascript") => "text/plain".to_owned(),
            _ if name.ends_with(".md") => "text/markdown".to_owned(),
            _ => "text/plain".to_owned(),
        };
        return if t.starts_with("text/") {
            format!("{t}; charset=utf-8")
        } else {
            t
        };
    }
    // Binary that is not a known type: never trust the name with a type
    // that renders.
    match guess {
        Some(m) if !renders(m.essence_str()) => m.essence_str().to_owned(),
        _ => "application/octet-stream".into(),
    }
}

/// The content type of a file inside a bundle, by its name.
pub fn bundle_file_type(path: &str, data: &[u8]) -> String {
    if is_executable(data) {
        return "application/octet-stream".into();
    }
    let guess = mime_guess::from_path(path).first_or_octet_stream();
    let t = guess.essence_str();
    if t.starts_with("text/") || t == "application/javascript" {
        format!("{t}; charset=utf-8")
    } else {
        t.to_owned()
    }
}

// ---- blobs ----

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Check a bundle tar: regular files and dirs only, no path out of the
/// root, an `index.html` at the root.
fn check_bundle(data: &[u8]) -> ApiResult<()> {
    let mut ar = tar::Archive::new(std::io::Cursor::new(data));
    let entries = ar
        .entries()
        .map_err(|e| ApiError::invalid(format!("the bundle is not a tar: {e}")))?;
    let mut index = false;
    let mut count = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| ApiError::invalid(format!("bad tar entry: {e}")))?;
        count += 1;
        if count > 10_000 {
            return Err(ApiError::invalid("the bundle has more than 10000 entries"));
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(ApiError::invalid(
                "the bundle may hold regular files and directories only (no links)",
            ));
        }
        let path = entry
            .path()
            .map_err(|e| ApiError::invalid(format!("bad tar path: {e}")))?;
        let mut clean = PathBuf::new();
        for c in path.components() {
            match c {
                std::path::Component::Normal(p) => clean.push(p),
                std::path::Component::CurDir => {}
                _ => {
                    return Err(ApiError::invalid(format!(
                        "bad path `{}` in the bundle",
                        path.display()
                    )));
                }
            }
        }
        if kind.is_file() && clean == Path::new("index.html") {
            index = true;
        }
    }
    if !index {
        return Err(ApiError::invalid(
            "a bundle needs an index.html at its root",
        ));
    }
    Ok(())
}

fn unpack_bundle(ctx: &ArtifactCtx, sha: &str, data: &[u8]) -> ApiResult<()> {
    let dest = ctx.bundle_dir(sha);
    if dest.is_dir() {
        return Ok(());
    }
    let tmp = ctx.dir.join(format!("{sha}.tmp-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&tmp).map_err(|e| ApiError::internal(format!("{e}")))?;
    let mut ar = tar::Archive::new(std::io::Cursor::new(data));
    ar.set_preserve_permissions(false);
    ar.set_overwrite(true);
    let result = (|| -> std::io::Result<()> {
        for entry in ar.entries()? {
            let mut entry = entry?;
            entry.unpack_in(&tmp)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(ApiError::invalid(format!("cannot unpack the bundle: {e}")));
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        if !dest.is_dir() {
            return Err(ApiError::internal(format!("cannot store the bundle: {e}")));
        }
    }
    Ok(())
}

/// One upload, before it is stored.
#[derive(Debug, Clone)]
pub struct Upload {
    pub name: String,
    /// The client's claim. Kept only for the record; the sniffed type wins.
    pub claimed_type: Option<String>,
    pub data: Vec<u8>,
    /// `file` or `bundle`.
    pub kind: String,
    pub attach: Option<NewArtifactLink>,
}

/// The blob part of an upload: checked and on disk.
#[derive(Debug, Clone)]
pub struct StoredBlob {
    pub sha256: String,
    pub size: u64,
    pub content_type: String,
}

/// Is an artifact of this type text that redaction applies to? `text/*`
/// and JSON. A binary type stays as it is.
fn redactable(content_type: &str) -> bool {
    let base = content_type.split(';').next().unwrap_or("").trim();
    base.starts_with("text/") || base == "application/json" || base.ends_with("+json")
}

/// A checked upload: its digest, size and type, and the redacted bytes
/// when redaction applies. Nothing is on disk yet ([`write_blob`]).
#[derive(Debug, Clone)]
pub struct PreparedBlob {
    pub blob: StoredBlob,
    redacted: Option<Vec<u8>>,
}

/// Check the size and the bundle, sniff the type, and hash the bytes to
/// store. A `file` of a text type (`text/*`, JSON) is redacted first (PLAN
/// §16b.6); a binary file and a bundle are not. Writes nothing. Blocking;
/// run it off the async runtime.
pub fn prepare_blob(
    ctx: &ArtifactCtx,
    u: &Upload,
    redactor: &terrarium_core::redact::Redactor,
    view: &terrarium_core::redact::View,
) -> ApiResult<PreparedBlob> {
    let size = u.data.len() as u64;
    if size > ctx.limits.max_file_bytes {
        return Err(ApiError::new(
            Code::TooLarge,
            format!(
                "the upload is {size} bytes; the limit is {} bytes",
                ctx.limits.max_file_bytes
            ),
        )
        .with_details(json!({"size": size, "max_file_bytes": ctx.limits.max_file_bytes})));
    }
    if size == 0 {
        return Err(ApiError::invalid("the upload is empty"));
    }
    match u.kind.as_str() {
        "file" => {
            let content_type = sniff(&u.name, &u.data);
            let redacted = if redactable(&content_type) {
                match redactor.redact_bytes_as(&u.data, view) {
                    std::borrow::Cow::Borrowed(_) => None,
                    std::borrow::Cow::Owned(v) => Some(v),
                }
            } else {
                None
            };
            let data = redacted.as_deref().unwrap_or(&u.data);
            Ok(PreparedBlob {
                blob: StoredBlob {
                    sha256: sha256_hex(data),
                    size: data.len() as u64,
                    content_type,
                },
                redacted,
            })
        }
        "bundle" => {
            check_bundle(&u.data)?;
            Ok(PreparedBlob {
                blob: StoredBlob {
                    sha256: sha256_hex(&u.data),
                    size,
                    content_type: "text/html; charset=utf-8".into(),
                },
                redacted: None,
            })
        }
        other => Err(ApiError::invalid(format!(
            "bad artifact kind `{other}`: use file or bundle"
        ))),
    }
}

/// Write a prepared blob (once per hash). Hold its
/// [`ArtifactCtx::digest_lock`]. Blocking; run it off the async runtime.
pub fn write_blob(ctx: &ArtifactCtx, u: &Upload, p: &PreparedBlob) -> ApiResult<StoredBlob> {
    std::fs::create_dir_all(&ctx.dir)
        .map_err(|e| ApiError::internal(format!("cannot create the artifact dir: {e}")))?;
    let sha = &p.blob.sha256;
    match u.kind.as_str() {
        "file" => {
            let path = ctx.blob_path(sha);
            if !path.is_file() {
                let data = p.redacted.as_deref().unwrap_or(&u.data);
                write_atomic(&path, data)
                    .map_err(|e| ApiError::internal(format!("cannot store the blob: {e}")))?;
            }
        }
        "bundle" => unpack_bundle(ctx, sha, &u.data)?,
        other => {
            return Err(ApiError::invalid(format!(
                "bad artifact kind `{other}`: use file or bundle"
            )));
        }
    }
    Ok(p.blob.clone())
}

/// Is a blob of this digest and kind used by an artifact row?
pub fn blob_used(conn: &Connection, sha: &str, kind: &str) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM artifacts WHERE sha256 = ?1 AND kind = ?2",
            params![sha, kind],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// One entry of the blob dir that no row may own.
#[derive(Debug, Clone, PartialEq)]
pub enum SweepEntry {
    /// A blob (`file`) or an unpacked bundle (`bundle`): removed when no
    /// row uses it.
    Blob { sha: String, kind: String },
    /// A temp file or dir of a write that did not finish. `sha` is the
    /// digest it was for.
    Temp { sha: String, path: PathBuf },
}

fn is_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The entries of the blob dir the orphan sweep checks: blobs, bundles,
/// and temp entries older than `min_temp_age`. Other names stay.
pub fn sweep_candidates(
    ctx: &ArtifactCtx,
    now: std::time::SystemTime,
    min_temp_age: std::time::Duration,
) -> std::io::Result<Vec<SweepEntry>> {
    let rd = match std::fs::read_dir(&ctx.dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let ft = entry.file_type()?;
        if is_digest(&name) && ft.is_file() {
            out.push(SweepEntry::Blob {
                sha: name,
                kind: "file".into(),
            });
        } else if let Some(sha) = name.strip_suffix(".bundle")
            && is_digest(sha)
            && ft.is_dir()
        {
            out.push(SweepEntry::Blob {
                sha: sha.to_owned(),
                kind: "bundle".into(),
            });
        } else if let Some((sha, _)) = name.split_once(".tmp-")
            && is_digest(sha)
        {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= min_temp_age);
            if old {
                out.push(SweepEntry::Temp {
                    sha: sha.to_owned(),
                    path: entry.path(),
                });
            }
        }
    }
    Ok(out)
}

/// Remove one sweep entry when no row owns it. Returns whether it was
/// removed. Hold the entry's [`ArtifactCtx::digest_lock`].
pub fn sweep_entry(ctx: &ArtifactCtx, conn: &Connection, e: &SweepEntry) -> ApiResult<bool> {
    let result = match e {
        SweepEntry::Blob { sha, kind } => {
            if blob_used(conn, sha, kind)? {
                return Ok(false);
            }
            if kind == "bundle" {
                std::fs::remove_dir_all(ctx.bundle_dir(sha))
            } else {
                std::fs::remove_file(ctx.blob_path(sha))
            }
        }
        SweepEntry::Temp { path, .. } => {
            if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            }
        }
    };
    match result {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ApiError::internal(format!(
            "cannot remove an orphan artifact blob: {err}"
        ))),
    }
}

/// Remove the blobs no artifact uses any more. Hold the
/// [`ArtifactCtx::digest_lock`] of each blob.
pub fn remove_blobs(ctx: &ArtifactCtx, conn: &Connection, blobs: &[(String, String)]) {
    for (sha, kind) in blobs {
        // On a read error keep the blob: the orphan sweep finds it later.
        let used = blob_used(conn, sha, kind).unwrap_or(true);
        if used {
            continue;
        }
        let result = if kind == "bundle" {
            std::fs::remove_dir_all(ctx.bundle_dir(sha))
        } else {
            std::fs::remove_file(ctx.blob_path(sha))
        };
        if let Err(e) = result
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(sha, "cannot remove an artifact blob: {e}");
        }
    }
}

// ---- shapes ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactLink {
    /// `page`, `card`, or `label`.
    pub kind: String,
    /// The page ID, the card ID, or the label (`version:1.4.0`).
    #[serde(rename = "ref")]
    pub reference: String,
    /// The page path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// `ART-9`.
    pub id: String,
    /// `project/ART-9`.
    pub key: String,
    /// The ULID in serving URLs.
    pub uid: String,
    pub project: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub sha256: String,
    /// `file` or `bundle`.
    pub kind: String,
    /// The viewer shows it inline. False means download.
    pub renders: bool,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
    pub links: Vec<ArtifactLink>,
    /// Live pages that embed it: `project/path`.
    pub embedded_in: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ArtRow {
    pub pk: i64,
    pub uid: String,
    pub project: String,
    pub num: i64,
    pub name: String,
    pub content_type: String,
    pub size: i64,
    pub sha256: String,
    pub kind: String,
    pub created_by: String,
    pub created_at: String,
    pub warned_at: Option<String>,
}

fn art_row(r: &Row<'_>) -> rusqlite::Result<ArtRow> {
    Ok(ArtRow {
        pk: r.get("pk")?,
        uid: r.get("id")?,
        project: r.get("project")?,
        num: r.get("num")?,
        name: r.get("name")?,
        content_type: r.get("content_type")?,
        size: r.get("size")?,
        sha256: r.get("sha256")?,
        kind: r.get("kind")?,
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
        warned_at: r.get("warned_at")?,
    })
}

fn short(num: i64) -> String {
    format!("{PREFIX}-{num}")
}

pub fn by_uid(conn: &Connection, uid: &str) -> ApiResult<Option<ArtRow>> {
    Ok(conn
        .query_row("SELECT * FROM artifacts WHERE id = ?1", [uid], art_row)
        .optional()?)
}

fn by_pk(conn: &Connection, pk: i64) -> ApiResult<ArtRow> {
    conn.query_row("SELECT * FROM artifacts WHERE pk = ?1", [pk], art_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found("no such artifact"))
}

/// Find by `ART-9`, `project/ART-9`, or the ULID.
pub fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<ArtRow> {
    let key = key.trim();
    if key.len() == 26
        && key.parse::<ulid::Ulid>().is_ok()
        && let Some(r) = by_uid(conn, key)?
    {
        return Ok(r);
    }
    let (project, num) = resolve_short(conn, key, PREFIX, "artifacts", context)?;
    conn.query_row(
        "SELECT * FROM artifacts WHERE project = ?1 AND num = ?2",
        params![project, num],
        art_row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("no artifact {}", short(num))))
}

pub fn find_for(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<ArtRow> {
    let row = find(conn, key, actor.project())?;
    policy::read_project(actor, &row.project)?;
    Ok(row)
}

pub fn render(conn: &Connection, row: ArtRow) -> ApiResult<Artifact> {
    let mut links = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT l.kind, l.ref, l.created_by, l.created_at, p.path FROM artifact_links l
           LEFT JOIN wiki_pages p ON l.kind = 'page' AND p.id = l.ref
         WHERE l.artifact = ?1 ORDER BY l.created_at",
    )?;
    let rows = stmt
        .query_map([row.pk], |r| {
            Ok(ArtifactLink {
                kind: r.get(0)?,
                reference: r.get(1)?,
                created_by: r.get(2)?,
                created_at: r.get(3)?,
                label: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    links.extend(rows);
    let mut stmt = conn.prepare(
        "SELECT label, created_by, created_at FROM artifact_labels
         WHERE artifact = ?1 ORDER BY created_at, label",
    )?;
    let rows = stmt
        .query_map([row.pk], |r| {
            Ok(ArtifactLink {
                kind: "label".into(),
                reference: r.get(0)?,
                created_by: r.get(1)?,
                created_at: r.get(2)?,
                label: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    links.extend(rows);
    let target = format!("{}/{}", row.project, short(row.num));
    let mut stmt = conn.prepare(
        "SELECT p.project || '/' || p.path FROM wiki_links l JOIN wiki_pages p ON p.id = l.page
         WHERE l.kind = 'artifact' AND l.target = ?1 AND p.deleted_at IS NULL ORDER BY 1",
    )?;
    let embedded_in = stmt
        .query_map([&target], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Artifact {
        id: short(row.num),
        key: target,
        // A bundle is a site: the viewer shows it in a sandboxed frame.
        renders: row.kind == "bundle" || renders(&row.content_type),
        created_by_name: name_of(conn, &row.created_by)?,
        uid: row.uid,
        project: row.project,
        name: row.name,
        content_type: row.content_type,
        size: row.size.max(0) as u64,
        sha256: row.sha256,
        kind: row.kind,
        created_by: row.created_by,
        created_at: row.created_at,
        links,
        embedded_in,
    })
}

pub fn get(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<Artifact> {
    let row = find_for(conn, actor, key)?;
    render(conn, row)
}

/// `GET /api/projects/{name}/artifacts` query: the page, and `label` (the
/// exact label, such as `version:1.4.0`; TER-129).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    pub label: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// Artifacts of a project, newest first. Cursor: number. With `label`, only
/// the artifacts that have the label.
pub fn list(conn: &Connection, project: &str, q: &ListQuery) -> ApiResult<Paged<Artifact>> {
    projects::get(conn, project)?;
    let page = Page {
        limit: q.limit,
        cursor: q.cursor.clone(),
    };
    let limit = page.limit();
    let before: i64 = page
        .cursor()
        .and_then(|c| c.parse().ok())
        .unwrap_or(i64::MAX);
    let label = q.label.as_deref().map(str::trim).filter(|l| !l.is_empty());
    let mut stmt = conn.prepare(
        "SELECT * FROM artifacts WHERE project = ?1 AND num < ?2
           AND (?4 IS NULL OR pk IN (SELECT artifact FROM artifact_labels WHERE label = ?4))
         ORDER BY num DESC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![project, before, (limit + 1) as i64, label], art_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut items = Vec::new();
    for r in rows {
        items.push(render(conn, r)?);
    }
    Ok(Paged::from_rows(items, limit, |a| {
        a.id.trim_start_matches("ART-").to_owned()
    }))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub project: String,
    pub count: i64,
    pub used_bytes: u64,
    pub quota_bytes: u64,
    pub max_file_bytes: u64,
}

fn used_bytes(conn: &Connection, project: &str) -> ApiResult<(i64, u64)> {
    let (n, used): (i64, i64) = conn.query_row(
        "SELECT count(*), coalesce(sum(size), 0) FROM artifacts WHERE project = ?1",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((n, used.max(0) as u64))
}

pub fn usage(conn: &Connection, project: &str, limits: &Limits) -> ApiResult<Usage> {
    projects::get(conn, project)?;
    let (count, used) = used_bytes(conn, project)?;
    Ok(Usage {
        project: project.to_owned(),
        count,
        used_bytes: used,
        quota_bytes: limits.project_quota_bytes,
        max_file_bytes: limits.max_file_bytes,
    })
}

// ---- writes ----

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewArtifactLink {
    /// `page`, `card`, or `label`.
    pub kind: String,
    /// A page key (path or ID), a card ID, or a label (`version:1.4.0`).
    #[serde(rename = "ref")]
    pub reference: String,
}

fn emit(t: &mut Tx<'_>, row: &ArtRow, change: &str, extra: serde_json::Value) -> ApiResult<()> {
    let id = short(row.num);
    let mut data = json!({
        "artifact_id": id,
        "key": format!("{}/{id}", row.project),
        "uid": row.uid,
        "name": row.name,
        "kind": row.kind,
    });
    if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            d.insert(k.clone(), v.clone());
        }
    }
    t.emit(
        EventDraft::new(
            format!("artifact.{change}"),
            format!("{id} {change}: {}", row.name),
            data,
        )
        .project(row.project.clone()),
    )?;
    Ok(())
}

/// Check the name and the project quota of an upload of `size` bytes.
/// Returns the trimmed name. The upload route calls it before it writes
/// the blob, and [`create`] again in the transaction.
pub fn check_upload(
    conn: &Connection,
    project: &str,
    name: &str,
    size: u64,
    limits: &Limits,
) -> ApiResult<String> {
    let name = check_name(name)?;
    let (_, used) = used_bytes(conn, project)?;
    if used + size > limits.project_quota_bytes {
        return Err(ApiError::new(
            Code::TooLarge,
            format!(
                "project `{project}` uses {used} of {} bytes; this upload of {size} bytes does not fit",
                limits.project_quota_bytes
            ),
        )
        .with_details(json!({
            "used_bytes": used,
            "quota_bytes": limits.project_quota_bytes,
            "size": size,
        })));
    }
    Ok(name)
}

fn check_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 255 || n.contains(['/', '\\', '\0']) || n == "." || n == ".." {
        return Err(ApiError::invalid(format!(
            "bad artifact name `{name}`: 1-255 characters, no `/`"
        )));
    }
    Ok(n.to_owned())
}

/// Write the artifact row for a stored blob.
pub fn create(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    u: &Upload,
    blob: &StoredBlob,
    limits: &Limits,
) -> ApiResult<Artifact> {
    projects::get(t, project)?;
    policy::write_project(actor, project)?;
    let name = check_upload(t, project, &u.name, blob.size, limits)?;
    let num = next_number(t, project, "artifact")?;
    let uid = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO artifacts (id, project, num, name, content_type, size, sha256, kind,
                                created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            uid,
            project,
            num,
            name,
            blob.content_type,
            blob.size as i64,
            blob.sha256,
            u.kind,
            actor.address(),
            now()
        ],
    )?;
    let row = by_pk(t, t.last_insert_rowid())?;
    emit(
        t,
        &row,
        "created",
        json!({"size": blob.size, "content_type": blob.content_type, "sha256": blob.sha256,
               "claimed_type": u.claimed_type}),
    )?;
    if let Some(l) = &u.attach {
        link_row(t, actor, &row, l)?;
    }
    render(t, by_pk(t, row.pk)?)
}

fn link_row(t: &mut Tx<'_>, actor: &Actor, row: &ArtRow, l: &NewArtifactLink) -> ApiResult<()> {
    let reference = l.reference.trim();
    if reference.is_empty() {
        return Err(ApiError::invalid("the link needs a ref"));
    }
    let stored = match l.kind.as_str() {
        "page" => {
            let page = wiki::find(t, actor, &qualify(&row.project, reference))
                .or_else(|_| wiki::find(t, actor, reference))?;
            if page.project != row.project {
                return Err(ApiError::invalid(
                    "an artifact attaches to a page of its project",
                ));
            }
            page.id
        }
        "card" => {
            let card = board::get_card(t, reference)?;
            if card.project != row.project {
                return Err(ApiError::invalid(
                    "an artifact attaches to a card of its project",
                ));
            }
            card.id
        }
        "label" => link_label(reference)?,
        "version" => {
            return Err(ApiError::invalid(
                "version objects are removed (TER-58): link a version label, \
                 kind `label` with ref `version:<X.Y.Z>`",
            ));
        }
        other => {
            return Err(ApiError::invalid(format!(
                "bad link kind `{other}`: use page, card, or label"
            )));
        }
    };
    if l.kind == "label" {
        t.execute(
            "INSERT OR IGNORE INTO artifact_labels (artifact, label, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![row.pk, stored, actor.address(), now()],
        )?;
    } else {
        t.execute(
            "INSERT OR IGNORE INTO artifact_links (artifact, kind, ref, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.pk, l.kind, stored, actor.address(), now()],
        )?;
    }
    t.execute(
        "UPDATE artifacts SET warned_at = NULL WHERE pk = ?1",
        [row.pk],
    )?;
    emit(
        t,
        row,
        "linked",
        json!({"link_kind": l.kind, "ref": stored}),
    )?;
    Ok(())
}

/// `project/ref` unless `ref` names a project already.
fn qualify(project: &str, reference: &str) -> String {
    if reference.len() == 26 && reference.parse::<ulid::Ulid>().is_ok() {
        reference.to_owned()
    } else {
        format!("{project}/{reference}")
    }
}

pub fn add_link(
    t: &mut Tx<'_>,
    actor: &Actor,
    key: &str,
    l: &NewArtifactLink,
) -> ApiResult<Artifact> {
    let row = find_for(t, actor, key)?;
    // A write: the own project only, also for an architect (TER-47).
    crate::policy::write_project(actor, &row.project)?;
    link_row(t, actor, &row, l)?;
    render(t, by_pk(t, row.pk)?)
}

/// Delete an artifact. Returns the blob to remove when no one else uses it.
pub fn delete(t: &mut Tx<'_>, actor: &Actor, key: &str) -> ApiResult<(String, String)> {
    let row = find_for(t, actor, key)?;
    // A write: the own project only, also for an architect (TER-47).
    crate::policy::write_project(actor, &row.project)?;
    policy::delete_artifact(actor, &row.project, &row.created_by)?;
    t.execute("DELETE FROM artifacts WHERE pk = ?1", [row.pk])?;
    if actor.address() != row.created_by {
        t.audit(
            "artifact.delete",
            &format!("{}/{}", row.project, short(row.num)),
            Some(&row.project),
            json!({"name": row.name, "sha256": row.sha256}),
        )?;
    }
    emit(t, &row, "deleted", json!({}))?;
    Ok((row.sha256, row.kind))
}

// ---- retention ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PruneItem {
    pub key: String,
    pub name: String,
    pub size: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PruneReport {
    pub dry_run: bool,
    pub as_of: String,
    pub retention_days: i64,
    /// Removed now (or, on a dry run, would be).
    pub pruned: Vec<PruneItem>,
    /// Warned now (or would be): they go on the next run a day or more later.
    pub warned: Vec<PruneItem>,
    /// Artifacts the rule keeps.
    pub kept: usize,
    /// `(sha256, kind)` of the pruned rows; remove the unused blobs after
    /// the commit with [`remove_blobs`].
    #[serde(skip)]
    pub blobs: Vec<(String, String)>,
}

/// Kept: attached to anything that still exists (a live page, a card, a
/// label) or embedded in a live page.
fn attached(conn: &Connection, row: &ArtRow) -> ApiResult<bool> {
    let target = format!("{}/{}", row.project, short(row.num));
    let n: i64 = conn.query_row(
        "SELECT
           (SELECT count(*) FROM artifact_labels WHERE artifact = ?1)
         + (SELECT count(*) FROM artifact_links l JOIN wiki_pages p ON p.id = l.ref
             WHERE l.artifact = ?1 AND l.kind = 'page' AND p.deleted_at IS NULL)
         + (SELECT count(*) FROM artifact_links l JOIN cards c ON c.id = l.ref
             WHERE l.artifact = ?1 AND l.kind = 'card')
         + (SELECT count(*) FROM wiki_links w JOIN wiki_pages p ON p.id = w.page
             WHERE w.kind = 'artifact' AND w.target = ?2 AND p.deleted_at IS NULL)",
        params![row.pk, target],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn item(row: &ArtRow) -> PruneItem {
    PruneItem {
        key: format!("{}/{}", row.project, short(row.num)),
        name: row.name.clone(),
        size: row.size.max(0) as u64,
        created_at: row.created_at.clone(),
    }
}

/// Apply the retention rule as of `now`. An unattached artifact older than
/// `retention_days - 1` gets an audit warning; one older than
/// `retention_days` whose warning is a day old or more is pruned. With
/// `dry_run`, nothing changes and the report says what would happen.
pub fn prune(
    t: &mut Tx<'_>,
    now: DateTime<Utc>,
    retention_days: i64,
    dry_run: bool,
) -> ApiResult<PruneReport> {
    let mut report = PruneReport {
        dry_run,
        as_of: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        retention_days,
        ..PruneReport::default()
    };
    let mut stmt = t.prepare("SELECT * FROM artifacts ORDER BY pk")?;
    let rows = stmt
        .query_map([], art_row)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let keep_for = Duration::days(retention_days.max(1));
    let warn_at = keep_for - Duration::days(1);
    let stamp = report.as_of.clone();
    for row in rows {
        if attached(t, &row)? {
            report.kept += 1;
            if row.warned_at.is_some() && !dry_run {
                t.execute(
                    "UPDATE artifacts SET warned_at = NULL WHERE pk = ?1",
                    [row.pk],
                )?;
            }
            continue;
        }
        let Some(created) = parse_time(&row.created_at) else {
            report.kept += 1;
            continue;
        };
        let age = now - created;
        let warned = row.warned_at.as_deref().and_then(parse_time);
        let warned_long_ago = warned.is_some_and(|w| now - w >= Duration::days(1));
        if age >= keep_for && warned_long_ago {
            report.pruned.push(item(&row));
            report.blobs.push((row.sha256.clone(), row.kind.clone()));
            if !dry_run {
                t.execute("DELETE FROM artifacts WHERE pk = ?1", [row.pk])?;
                t.audit(
                    "artifact.prune",
                    &format!("{}/{}", row.project, short(row.num)),
                    Some(&row.project),
                    json!({"name": row.name, "size": row.size, "created_at": row.created_at,
                           "warned_at": row.warned_at}),
                )?;
                emit(t, &row, "pruned", json!({}))?;
            }
        } else if age >= warn_at && warned.is_none() {
            report.warned.push(item(&row));
            if !dry_run {
                t.execute(
                    "UPDATE artifacts SET warned_at = ?2 WHERE pk = ?1",
                    params![row.pk, stamp],
                )?;
                t.audit(
                    "artifact.prune_warning",
                    &format!("{}/{}", row.project, short(row.num)),
                    Some(&row.project),
                    json!({"name": row.name, "size": row.size, "created_at": row.created_at,
                           "prune_after": (now + Duration::days(1))
                               .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)}),
                )?;
            }
        } else {
            report.kept += 1;
        }
    }
    if dry_run {
        report.blobs.clear();
    }
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        assert_eq!(sniff("a.png", b"\x89PNG\r\n\x1a\n...."), "image/png");
        // The name does not make a binary render.
        assert_eq!(
            sniff("evil.html", b"\x7fELF\x02\x01"),
            "application/octet-stream"
        );
        assert_eq!(
            sniff("run.sh", b"#!/bin/sh\necho hi\n"),
            "application/octet-stream"
        );
        assert_eq!(
            sniff("r.html", b"<html></html>"),
            "text/html; charset=utf-8"
        );
        assert_eq!(sniff("x.bin", &[0, 1, 2, 3]), "application/octet-stream");
        assert_eq!(sniff("x.png", &[0, 1, 2, 3]), "application/octet-stream");
        assert_eq!(sniff("notes.txt", b"hello"), "text/plain; charset=utf-8");
        assert!(renders("text/html; charset=utf-8"));
        assert!(!renders("application/octet-stream"));
    }

    #[test]
    fn signatures() {
        let ctx = ArtifactCtx {
            dir: PathBuf::from("/nonexistent"),
            limits: Limits::default(),
            secret: vec![7; 32],
            locks: new_locks(),
        };
        let sig = ctx.sign("A", 100);
        assert!(ctx.verify("A", 100, &sig, 50));
        assert!(!ctx.verify("A", 100, &sig, 101), "expired");
        assert!(!ctx.verify("B", 100, &sig, 50), "other artifact");
        assert!(!ctx.verify("A", 101, &sig, 50), "other expiry");
        assert!(!ctx.verify("A", 100, "zz", 50));
    }
}
