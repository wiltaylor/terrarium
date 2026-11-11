//! The files of a card (docs/api.md §8.4, PLAN §13): markdown documents,
//! text, data files, images, and PDFs that the operator or an agent
//! attaches to a card.
//!
//! - The bytes live in a blob store of the daemon,
//!   `<state>/card-files/<sha256>`: the same bytes are stored once, and the
//!   state root is in each backup (§32). The rows of `card_files`
//!   (migration 61) name them: one row for each version of a file.
//! - A name is one path segment of safe characters ([`check_name`]),
//!   unique per card with no regard to case. The extension picks the type
//!   from an allowlist ([`FileKind`]), and the bytes must agree (magic
//!   bytes, UTF-8 text with no NUL, a JSON parse, an `<svg` root).
//! - Text types are redacted on the way in (PLAN §16b.6), as artifacts are.
//! - A replace adds a version and keeps the old ones. A remove is a soft
//!   delete: every version of the name gets `deleted_at`. The prune timer
//!   ([`gc`]) deletes the removed rows after `TERRARIUM_TRASH_DAYS`, then
//!   the blobs that no row uses.
//! - Caps: [`MAX_FILE_ENV`] per file (default 10 MiB) and [`MAX_CARD_ENV`]
//!   for all the stored versions of a card (default 50 MiB).
//! - The rights: a change has the rights of a card edit
//!   (`board::check_file_edit`); a read has the read right of the project.
//! - Each change writes an audit row (`card.file_add`, `card.file_replace`,
//!   `card.file_remove`) and emits `card.file_added`, `card.file_replaced`,
//!   or `card.file_removed`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::json;

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::store::artifacts::sha256_hex;
use crate::store::board::{self, CardRef};
use crate::store::memory::name_of;

/// The blob dir in the state dir.
pub const BLOB_DIR: &str = "card-files";
/// The most bytes in one file.
pub const MAX_FILE_ENV: &str = "TERRARIUM_CARD_FILE_MAX_BYTES";
/// The most bytes in all the stored versions of one card's files.
pub const MAX_CARD_ENV: &str = "TERRARIUM_CARD_FILES_MAX_BYTES";
pub const DEFAULT_MAX_FILE: u64 = 10 * 1024 * 1024;
pub const DEFAULT_MAX_CARD: u64 = 50 * 1024 * 1024;
/// The longest name, in bytes.
pub const MAX_NAME: usize = 128;
/// The name of the index that the delivery writes in each card dir
/// (`crate::card_delivery`). A file cannot have it.
pub const INDEX_NAME: &str = "index.md";
/// A blob younger than this is never pruned: its row may not be written
/// yet.
const BLOB_GRACE: Duration = Duration::from_secs(3600);

/// The caps of the card files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_file: u64,
    pub max_card: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file: DEFAULT_MAX_FILE,
            max_card: DEFAULT_MAX_CARD,
        }
    }
}

impl Limits {
    /// The caps from [`MAX_FILE_ENV`] and [`MAX_CARD_ENV`] (`10M`,
    /// `10485760`, `512K`). A value that does not parse, or `0`, is the
    /// default.
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(MAX_FILE_ENV).ok().as_deref(),
            std::env::var(MAX_CARD_ENV).ok().as_deref(),
        )
    }

    /// The caps from the two values, as [`Limits::from_env`] reads them.
    pub fn from_values(file: Option<&str>, card: Option<&str>) -> Self {
        let get = |v: Option<&str>, d: u64| {
            v.and_then(terrarium_core::skills::parse_size)
                .map(|n| n as u64)
                .filter(|n| *n > 0)
                .unwrap_or(d)
        };
        Self {
            max_file: get(file, DEFAULT_MAX_FILE),
            max_card: get(card, DEFAULT_MAX_CARD),
        }
    }
}

/// The file types that a card takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Markdown,
    Text,
    Json,
    Yaml,
    Csv,
    Png,
    Jpeg,
    Gif,
    Webp,
    Svg,
    Pdf,
}

/// The extensions that a name may have, for the messages.
pub const EXTENSIONS: &str = "md, txt, json, yaml, yml, csv, png, jpg, jpeg, gif, webp, svg, pdf";

impl FileKind {
    /// The kind of a name, by its extension (any case).
    pub fn from_name(name: &str) -> Option<Self> {
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match ext.as_str() {
            "md" | "markdown" => Self::Markdown,
            "txt" => Self::Text,
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "csv" => Self::Csv,
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "gif" => Self::Gif,
            "webp" => Self::Webp,
            "svg" => Self::Svg,
            "pdf" => Self::Pdf,
            _ => return None,
        })
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Self::Markdown => "text/markdown; charset=utf-8",
            Self::Text => "text/plain; charset=utf-8",
            Self::Json => "application/json",
            Self::Yaml => "application/yaml",
            Self::Csv => "text/csv; charset=utf-8",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Svg => "image/svg+xml",
            Self::Pdf => "application/pdf",
        }
    }

    /// UTF-8 text: checked, and redacted on the way in.
    pub fn is_text(self) -> bool {
        matches!(
            self,
            Self::Markdown | Self::Text | Self::Json | Self::Yaml | Self::Csv | Self::Svg
        )
    }

    fn label(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Csv => "CSV",
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Svg => "SVG",
            Self::Pdf => "PDF",
        }
    }
}

/// Check a file name: one path segment of ASCII letters, digits, `.`,
/// `_`, `-`, `+`, and inner spaces, at most [`MAX_NAME`] bytes; not hidden
/// (a leading `.`), not [`INDEX_NAME`], with an allowed extension.
pub fn check_name(name: &str) -> ApiResult<FileKind> {
    let bad = |why: &str| ApiError::invalid(format!("bad file name `{name}`: {why}"));
    if name.is_empty() {
        return Err(ApiError::invalid("the file name is empty"));
    }
    if name.len() > MAX_NAME {
        return Err(bad(&format!("at most {MAX_NAME} bytes")));
    }
    if name.starts_with('.') {
        return Err(bad("a hidden name (a leading `.`) is not allowed"));
    }
    if name.starts_with(' ') || name.ends_with(' ') || name.ends_with('.') {
        return Err(bad("no leading or trailing space, and no trailing `.`"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | ' ')))
    {
        return Err(bad(&format!(
            "the character {c:?} is not allowed; use letters, digits, `.`, `_`, `-`, `+`, and spaces"
        )));
    }
    if name.eq_ignore_ascii_case(INDEX_NAME) {
        return Err(bad(
            "`index.md` is the name of the index that the daemon writes in the card dir of an agent",
        ));
    }
    FileKind::from_name(name).ok_or_else(|| {
        ApiError::new(
            Code::UnsupportedMediaType,
            format!("`{name}`: a card takes only these file types: {EXTENSIONS}"),
        )
    })
}

fn unsupported(m: String) -> ApiError {
    ApiError::new(Code::UnsupportedMediaType, m)
}

/// Check that the bytes are what the extension says: the magic bytes of an
/// image or a PDF; UTF-8 with no NUL for text; a JSON value for JSON; an
/// `<svg` root element for SVG.
pub fn sniff(kind: FileKind, name: &str, data: &[u8]) -> ApiResult<()> {
    let wrong = || {
        unsupported(format!(
            "`{name}` is not a {} file (the content does not match the extension)",
            kind.label()
        ))
    };
    let image = crate::api::attachments::ImageKind::sniff(data);
    use crate::api::attachments::ImageKind as I;
    match kind {
        FileKind::Png if image == Some(I::Png) => Ok(()),
        FileKind::Jpeg if image == Some(I::Jpeg) => Ok(()),
        FileKind::Gif if image == Some(I::Gif) => Ok(()),
        FileKind::Webp if image == Some(I::Webp) => Ok(()),
        FileKind::Png | FileKind::Jpeg | FileKind::Gif | FileKind::Webp => Err(wrong()),
        FileKind::Pdf if data.starts_with(b"%PDF-") => Ok(()),
        FileKind::Pdf => Err(wrong()),
        _ => {
            let text = std::str::from_utf8(data).map_err(|_| {
                unsupported(format!(
                    "`{name}` is not UTF-8 text; a {} file must be UTF-8",
                    kind.label()
                ))
            })?;
            if text.contains('\0') {
                return Err(unsupported(format!(
                    "`{name}` has a NUL byte; a {} file is text",
                    kind.label()
                )));
            }
            match kind {
                FileKind::Json => serde_json::from_str::<serde_json::Value>(text)
                    .map(|_| ())
                    .map_err(|e| ApiError::invalid(format!("`{name}` is not valid JSON: {e}"))),
                FileKind::Svg if svg_root(text) => Ok(()),
                FileKind::Svg => Err(wrong()),
                _ => Ok(()),
            }
        }
    }
}

/// Does the text start with an `<svg` element, after an optional BOM, XML
/// declaration, doctype, comments, and white space?
fn svg_root(text: &str) -> bool {
    let mut rest = text.trim_start_matches('\u{feff}').trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("<?") {
            let Some(end) = r.find("?>") else {
                return false;
            };
            rest = r[end + 2..].trim_start();
        } else if let Some(r) = rest.strip_prefix("<!--") {
            let Some(end) = r.find("-->") else {
                return false;
            };
            rest = r[end + 3..].trim_start();
        } else if rest
            .as_bytes()
            .get(..9)
            .is_some_and(|b| b.eq_ignore_ascii_case(b"<!doctype"))
        {
            let Some(end) = rest.find('>') else {
                return false;
            };
            rest = rest[end + 1..].trim_start();
        } else {
            break;
        }
    }
    let lower = rest.get(..5).map(str::to_ascii_lowercase);
    lower.as_deref() == Some("<svg ")
        || lower.as_deref() == Some("<svg>")
        || lower.as_deref() == Some("<svg\n")
        || lower.as_deref() == Some("<svg\t")
        || lower.as_deref() == Some("<svg\r")
}

/// An upload after its checks, ready for the blob store and the row.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub name: String,
    pub kind: FileKind,
    /// The bytes to store: redacted when the kind is text.
    pub data: Vec<u8>,
    pub sha256: String,
}

/// Check an upload: the name, the size, the type, and the content. A text
/// file is redacted for `view` (PLAN §16b.6), and checked again after.
pub fn prepare(
    name: &str,
    data: Vec<u8>,
    limits: Limits,
    redactor: &terrarium_core::redact::Redactor,
    view: &terrarium_core::redact::View,
) -> ApiResult<Prepared> {
    let kind = check_name(name)?;
    if data.len() as u64 > limits.max_file {
        return Err(too_large_file(data.len() as u64, limits.max_file));
    }
    if data.is_empty() {
        return Err(ApiError::invalid(format!("`{name}` is empty")));
    }
    sniff(kind, name, &data)?;
    let data = if kind.is_text() {
        redactor.redact_bytes_as(&data, view).into_owned()
    } else {
        data
    };
    if data.len() as u64 > limits.max_file {
        return Err(too_large_file(data.len() as u64, limits.max_file));
    }
    let sha256 = sha256_hex(&data);
    Ok(Prepared {
        name: name.to_owned(),
        kind,
        data,
        sha256,
    })
}

pub fn too_large_file(size: u64, max: u64) -> ApiError {
    ApiError::new(
        Code::TooLarge,
        format!("the file is {size} bytes; a card file has at most {max} bytes ({MAX_FILE_ENV})"),
    )
    .with_details(json!({"size": size, "max_file_bytes": max}))
}

// ---- the blob store ----

pub fn blob_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(BLOB_DIR)
}

/// `<state>/card-files/<sha256>`. The SHA-256 must be 64 hex digits.
pub fn blob_path(state_dir: &Path, sha: &str) -> ApiResult<PathBuf> {
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::internal(format!("bad blob name `{sha}`")));
    }
    Ok(blob_dir(state_dir).join(sha))
}

/// Write the blob of `p` when the store does not have it: a temp file,
/// then a rename. Mode 0640.
pub fn store_blob(state_dir: &Path, p: &Prepared) -> ApiResult<()> {
    let dir = blob_dir(state_dir);
    let path = blob_path(state_dir, &p.sha256)?;
    let fail = |e: std::io::Error| ApiError::internal(format!("cannot store the card file: {e}"));
    if path.is_file() {
        // Mark it as used now, so the prune does not take it before the
        // row is written.
        let _ = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(fail)?;
    let tmp = dir.join(format!(".{}.{}.part", p.sha256, ulid::Ulid::new()));
    let write = || -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o640)
            .open(&tmp)?;
        f.write_all(&p.data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(e));
    }
    Ok(())
}

/// The bytes of a blob.
pub fn read_blob(state_dir: &Path, sha: &str) -> ApiResult<Vec<u8>> {
    let path = blob_path(state_dir, sha)?;
    std::fs::read(&path).map_err(|e| {
        ApiError::internal(format!(
            "cannot read the card file blob {sha}: {e}; restore the state dir from a backup"
        ))
    })
}

// ---- the rows ----

/// The latest version of a file of a card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CardFile {
    pub card: String,
    pub name: String,
    pub version: i64,
    pub size: i64,
    pub sha256: String,
    pub media_type: String,
    /// `operator` or an agent ULID: who stored this version.
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
    /// The stored versions of the name.
    pub versions: i64,
}

/// One version of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileVersion {
    pub name: String,
    pub version: i64,
    pub size: i64,
    pub sha256: String,
    pub media_type: String,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
}

const VERSION_SELECT: &str =
    "SELECT name, version, size, sha256, media_type, created_by, created_at FROM card_files";

fn version_row(r: &Row<'_>) -> rusqlite::Result<FileVersion> {
    Ok(FileVersion {
        name: r.get(0)?,
        version: r.get(1)?,
        size: r.get(2)?,
        sha256: r.get(3)?,
        media_type: r.get(4)?,
        created_by: r.get(5)?,
        created_by_name: String::new(),
        created_at: r.get(6)?,
    })
}

fn named(conn: &Connection, mut v: FileVersion) -> ApiResult<FileVersion> {
    v.created_by_name = name_of(conn, &v.created_by)?;
    Ok(v)
}

/// The key of a name: lower case.
pub fn name_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// The live files of card `card` (an ID in canonical case), by name: the
/// latest version of each.
pub fn list(conn: &Connection, card: &str) -> ApiResult<Vec<CardFile>> {
    let mut stmt = conn.prepare(&format!(
        "{VERSION_SELECT} f WHERE card = ?1 AND deleted_at IS NULL
           AND version = (SELECT max(version) FROM card_files g
                          WHERE g.card = f.card AND g.name_key = f.name_key
                            AND g.deleted_at IS NULL)
         ORDER BY name_key"
    ))?;
    let rows = stmt
        .query_map([card], version_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for v in rows {
        let versions: i64 = conn.query_row(
            "SELECT count(*) FROM card_files WHERE card = ?1 AND name_key = ?2
               AND deleted_at IS NULL",
            params![card, name_key(&v.name)],
            |r| r.get(0),
        )?;
        let v = named(conn, v)?;
        out.push(CardFile {
            card: card.to_owned(),
            name: v.name,
            version: v.version,
            size: v.size,
            sha256: v.sha256,
            media_type: v.media_type,
            created_by: v.created_by,
            created_by_name: v.created_by_name,
            created_at: v.created_at,
            versions,
        });
    }
    Ok(out)
}

/// The live versions of a file, newest first. `404` when the card has no
/// such file.
pub fn versions(conn: &Connection, card: &str, name: &str) -> ApiResult<Vec<FileVersion>> {
    let mut stmt = conn.prepare(&format!(
        "{VERSION_SELECT} WHERE card = ?1 AND name_key = ?2 AND deleted_at IS NULL
         ORDER BY version DESC"
    ))?;
    let rows = stmt
        .query_map(params![card, name_key(name)], version_row)?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Err(no_file(card, name));
    }
    rows.into_iter().map(|v| named(conn, v)).collect()
}

fn no_file(card: &str, name: &str) -> ApiError {
    ApiError::not_found(format!("{card} has no file `{name}`"))
}

/// One version of a file: `version`, else the latest.
pub fn get(
    conn: &Connection,
    card: &str,
    name: &str,
    version: Option<i64>,
) -> ApiResult<FileVersion> {
    let row = match version {
        Some(v) => conn
            .query_row(
                &format!(
                    "{VERSION_SELECT} WHERE card = ?1 AND name_key = ?2 AND version = ?3
                       AND deleted_at IS NULL"
                ),
                params![card, name_key(name), v],
                version_row,
            )
            .optional()?
            .ok_or_else(|| {
                ApiError::not_found(format!("{card} has no version {v} of the file `{name}`"))
            })?,
        None => conn
            .query_row(
                &format!(
                    "{VERSION_SELECT} WHERE card = ?1 AND name_key = ?2 AND deleted_at IS NULL
                     ORDER BY version DESC LIMIT 1"
                ),
                params![card, name_key(name)],
                version_row,
            )
            .optional()?
            .ok_or_else(|| no_file(card, name))?,
    };
    named(conn, row)
}

/// The bytes that the live versions of a card's files take.
fn card_bytes(conn: &Connection, card: &str) -> ApiResult<u64> {
    let n: i64 = conn.query_row(
        "SELECT coalesce(sum(size), 0) FROM card_files WHERE card = ?1 AND deleted_at IS NULL",
        [card],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// What [`add`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddOutcome {
    /// A new name.
    Added,
    /// A new version of a live file.
    Replaced,
    /// The latest version has the same bytes: nothing changed.
    Unchanged,
}

/// Store the row of a new file, or a new version with `replace`. The blob
/// must be in the store already ([`store_blob`]).
///
/// - A live file of the same name (any case) without `replace` is `409
///   conflict`.
/// - `replace` with no live file adds the file.
/// - The new version is one more than the highest version of the name,
///   removed versions too.
/// - All the live versions of the card, with the new one, must fit in
///   `limits.max_card` (`413 too_large`).
pub fn add(
    t: &mut Tx<'_>,
    actor: &Actor,
    card: &str,
    p: &Prepared,
    replace: bool,
    limits: Limits,
) -> ApiResult<(CardFile, AddOutcome)> {
    let c = board::check_file_edit(t, actor, card)?;
    let key = name_key(&p.name);
    let live = match get(t, &c.id, &p.name, None) {
        Ok(v) => Some(v),
        Err(e) if e.code == Code::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(cur) = &live {
        if !replace {
            return Err(ApiError::conflict(format!(
                "{} has a file `{}` already; replace it (`--replace`, `?replace=true`) to add a version",
                c.id, cur.name
            ))
            .with_details(json!({"name": cur.name, "version": cur.version})));
        }
        if cur.sha256 == p.sha256 && cur.name == p.name {
            let file = find(t, &c.id, &p.name)?;
            return Ok((file, AddOutcome::Unchanged));
        }
    }
    let used = card_bytes(t, &c.id)?;
    let size = p.data.len() as u64;
    if used + size > limits.max_card {
        return Err(ApiError::new(
            Code::TooLarge,
            format!(
                "{} has {used} bytes of files (all the versions); {size} more is over the limit of {} bytes ({MAX_CARD_ENV}). Remove a file first.",
                c.id, limits.max_card
            ),
        )
        .with_details(json!({"used": used, "size": size, "max_card_bytes": limits.max_card})));
    }
    let version: i64 = t.query_row(
        "SELECT coalesce(max(version), 0) + 1 FROM card_files WHERE card = ?1 AND name_key = ?2",
        params![c.id, key],
        |r| r.get(0),
    )?;
    let me = actor.address();
    t.execute(
        "INSERT INTO card_files (id, card, name, name_key, version, size, sha256, media_type,
                                 created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            ulid::Ulid::new().to_string(),
            c.id,
            p.name,
            key,
            version,
            size as i64,
            p.sha256,
            p.kind.media_type(),
            me,
            now()
        ],
    )?;
    let outcome = if live.is_some() {
        AddOutcome::Replaced
    } else {
        AddOutcome::Added
    };
    let (action, change, verb) = match outcome {
        AddOutcome::Replaced => ("card.file_replace", "file_replaced", "replaced"),
        _ => ("card.file_add", "file_added", "added"),
    };
    let detail = json!({
        "name": p.name,
        "version": version,
        "size": size,
        "sha256": p.sha256,
        "media_type": p.kind.media_type(),
    });
    t.audit(action, &c.id, Some(&c.project), detail.clone())?;
    board::file_event(
        t,
        &c.id,
        change,
        format!("{} file {} v{version} {verb}", c.id, p.name),
        detail,
    )?;
    Ok((find(t, &c.id, &p.name)?, outcome))
}

fn find(conn: &Connection, card: &str, name: &str) -> ApiResult<CardFile> {
    let key = name_key(name);
    list(conn, card)?
        .into_iter()
        .find(|f| name_key(&f.name) == key)
        .ok_or_else(|| no_file(card, name))
}

/// Remove a file: a soft delete of all its versions. The rows and the
/// blobs stay for `TERRARIUM_TRASH_DAYS` ([`gc_rows`]).
pub fn remove(t: &mut Tx<'_>, actor: &Actor, card: &str, name: &str) -> ApiResult<FileVersion> {
    let c = board::check_file_edit(t, actor, card)?;
    let cur = get(t, &c.id, name, None)?;
    let n = t.execute(
        "UPDATE card_files SET deleted_at = ?3, deleted_by = ?4
         WHERE card = ?1 AND name_key = ?2 AND deleted_at IS NULL",
        params![c.id, name_key(name), now(), actor.address()],
    )?;
    let detail = json!({"name": cur.name, "version": cur.version, "versions": n});
    t.audit("card.file_remove", &c.id, Some(&c.project), detail.clone())?;
    board::file_event(
        t,
        &c.id,
        "file_removed",
        format!("{} file {} removed", c.id, cur.name),
        detail,
    )?;
    Ok(cur)
}

/// A card whose files go into the workspace of its assignee
/// (`crate::card_delivery`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredCard {
    pub card: CardRef,
    pub files: Vec<CardFile>,
}

/// The cards assigned to `agent` (a ULID) that are not archived and not
/// done, each with its live files (the latest versions), by card ID.
pub fn assigned(conn: &Connection, agent: &str) -> ApiResult<Vec<DeliveredCard>> {
    let mut stmt = conn.prepare(
        "SELECT c.id FROM cards c JOIN board_columns col ON col.id = c.column_id
         WHERE c.assignee = ?1 AND c.archived = 0 AND col.name != ?2
         ORDER BY c.id",
    )?;
    let ids = stmt
        .query_map(params![agent, board::DONE], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(DeliveredCard {
            card: board::card_ref(conn, &id)?,
            files: list(conn, &id)?,
        });
    }
    Ok(out)
}

/// Delete the rows of the files removed before `cutoff` (RFC 3339).
/// [`gc_blobs`] then removes the blobs that no row names.
pub fn gc_rows(t: &mut Tx<'_>, cutoff: &str) -> ApiResult<usize> {
    Ok(t.execute(
        "DELETE FROM card_files WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
        [cutoff],
    )?)
}

/// The SHA-256 of every blob that a row names.
pub fn used_blobs(conn: &Connection) -> ApiResult<std::collections::BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT sha256 FROM card_files")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Remove the blobs (and the stale temp files) in the blob dir that are
/// not in `used` and older than an hour at `now`. Returns the count.
pub fn gc_blobs(
    state_dir: &Path,
    used: &std::collections::BTreeSet<String>,
    now: SystemTime,
) -> usize {
    let Ok(entries) = std::fs::read_dir(blob_dir(state_dir)) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if used.contains(&name) {
            continue;
        }
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
            .is_some_and(|age| age > BLOB_GRACE);
        if old && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(check_name("spec.md").unwrap(), FileKind::Markdown);
        assert_eq!(check_name("Design v2.PNG").unwrap(), FileKind::Png);
        assert_eq!(check_name("a+b_c-d.yml").unwrap(), FileKind::Yaml);
        for bad in [
            "",
            ".hidden.md",
            "a/b.md",
            "..",
            "a\\b.md",
            " lead.md",
            "trail.md ",
            "x.md.",
            "naïve.md",
            "tab\t.md",
            "index.md",
            "INDEX.MD",
            "semi;colon.md",
        ] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        let long = format!("{}.md", "a".repeat(MAX_NAME));
        assert!(check_name(&long).is_err());
        let e = check_name("run.sh").unwrap_err();
        assert_eq!(e.code, Code::UnsupportedMediaType);
        assert!(check_name("noext").is_err());
    }

    #[test]
    fn sniffing() {
        let ok = |name: &str, data: &[u8]| sniff(check_name(name).unwrap(), name, data);
        assert!(ok("a.png", b"\x89PNG\r\n\x1a\nrest").is_ok());
        assert!(ok("a.png", b"GIF89a....").is_err());
        assert!(ok("a.jpg", &[0xFF, 0xD8, 0xFF, 0xE0]).is_ok());
        assert!(ok("a.gif", b"GIF87a").is_ok());
        assert!(ok("a.webp", b"RIFF\0\0\0\0WEBPVP8 ").is_ok());
        assert!(ok("a.pdf", b"%PDF-1.7\n").is_ok());
        assert!(ok("a.pdf", b"<html>").is_err());
        assert!(ok("a.md", "# Title\n\nnaïve".as_bytes()).is_ok());
        assert!(ok("a.md", b"\xff\xfe").is_err());
        assert!(ok("a.txt", b"a\0b").is_err());
        assert!(
            ok("a.md", b"\x89PNG\r\n\x1a\n").is_err(),
            "a PNG is not UTF-8"
        );
        assert!(ok("a.json", br#"{"a":1}"#).is_ok());
        assert!(ok("a.json", b"{nope").is_err());
        assert!(ok("a.yaml", b"a: 1\n").is_ok());
        assert!(ok("a.csv", b"a,b\n1,2\n").is_ok());
        assert!(ok("a.svg", br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#).is_ok());
        assert!(
            ok(
                "a.svg",
                b"\xef\xbb\xbf<?xml version=\"1.0\"?>\n<!-- c -->\n<!DOCTYPE svg>\n<svg>\n</svg>"
            )
            .is_ok()
        );
        assert!(ok("a.svg", b"<html><svg></svg></html>").is_err());
        assert!(ok("a.svg", b"<svgx>").is_err());
        // A multi-byte character across byte 9 is not a doctype, and no panic.
        assert!(ok("a.svg", "<abcdefg\u{e9}/>".as_bytes()).is_err());
        assert!(ok("a.svg", "<!-- x --><abcdefg\u{20ac}>".as_bytes()).is_err());
        assert!(!svg_root("<!doctyp\u{e9}>"));
    }

    #[test]
    fn prepare_redacts_text_and_checks_the_caps() {
        let r = terrarium_core::redact::Redactor::default();
        let v = terrarium_core::redact::View::All;
        let small = Limits {
            max_file: 8,
            max_card: 100,
        };
        let e = prepare("a.txt", b"123456789".to_vec(), small, &r, &v).unwrap_err();
        assert_eq!(e.code, Code::TooLarge);
        assert!(prepare("a.txt", Vec::new(), small, &r, &v).is_err());
        let p = prepare("a.txt", b"hello".to_vec(), small, &r, &v).unwrap();
        assert_eq!(p.sha256, sha256_hex(b"hello"));
        assert_eq!(p.kind.media_type(), "text/plain; charset=utf-8");
    }

    #[test]
    fn limits_from_env_values() {
        let d = Limits::default();
        assert_eq!(d.max_file, 10 << 20);
        assert_eq!(d.max_card, 50 << 20);
        assert_eq!(Limits::from_values(None, None), d);
        let l = Limits::from_values(Some("1M"), Some("3072K"));
        assert_eq!((l.max_file, l.max_card), (1 << 20, 3 << 20));
        assert_eq!(Limits::from_values(Some("0"), Some("lots")), d);
    }

    async fn db_with_card() -> (crate::db::Db, String) {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        let db = crate::db::Db::new(c, crate::events::EventHub::default());
        let id = db
            .write("operator", |t| {
                crate::store::projects::create(
                    t,
                    &crate::store::projects::NewProject {
                        name: "p".into(),
                        ..Default::default()
                    },
                    crate::store::projects::SOURCE_API,
                )?;
                let card = board::create_card(
                    t,
                    &Actor::Operator,
                    "p",
                    serde_json::from_value(json!({"title": "T"})).unwrap(),
                )?;
                Ok(card.id)
            })
            .await
            .unwrap();
        (db, id)
    }

    fn prep(name: &str, data: &[u8]) -> Prepared {
        let r = terrarium_core::redact::Redactor::default();
        prepare(
            name,
            data.to_vec(),
            Limits::default(),
            &r,
            &terrarium_core::redact::View::All,
        )
        .unwrap()
    }

    /// The card cap counts every stored version; a remove frees the room.
    #[tokio::test]
    async fn the_card_cap_counts_every_version() {
        let (db, card) = db_with_card().await;
        let limits = Limits {
            max_file: 100,
            max_card: 10,
        };
        let add = |name: &str, data: &[u8], replace: bool| {
            let (p, card) = (prep(name, data), card.clone());
            let db = db.clone();
            async move {
                db.write("operator", move |t| {
                    add(t, &Actor::Operator, &card, &p, replace, limits)
                })
                .await
            }
        };
        assert_eq!(
            add("a.txt", b"12345", false).await.unwrap().1,
            AddOutcome::Added
        );
        assert_eq!(
            add("a.txt", b"1234", true).await.unwrap().1,
            AddOutcome::Replaced
        );
        let e = add("b.txt", b"12", false).await.unwrap_err();
        assert_eq!(e.code, Code::TooLarge);
        assert!(e.message.contains(MAX_CARD_ENV), "{}", e.message);
        assert_eq!(
            add("b.txt", b"1", false).await.unwrap().1,
            AddOutcome::Added
        );
        let c2 = card.clone();
        db.write("operator", move |t| {
            remove(t, &Actor::Operator, &c2, "A.TXT")
        })
        .await
        .unwrap();
        assert_eq!(
            add("c.txt", b"123456789", false).await.unwrap().1,
            AddOutcome::Added
        );
        let c3 = card.clone();
        let files = db.read(move |c| list(c, &c3)).await.unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["b.txt", "c.txt"]);
    }

    #[test]
    fn blob_gc_keeps_used_and_young_blobs() {
        let t = tempfile::tempdir().unwrap();
        let p = |data: &[u8]| Prepared {
            name: "a.txt".into(),
            kind: FileKind::Text,
            data: data.to_vec(),
            sha256: sha256_hex(data),
        };
        let (a, b) = (p(b"a"), p(b"b"));
        store_blob(t.path(), &a).unwrap();
        store_blob(t.path(), &b).unwrap();
        store_blob(t.path(), &a).unwrap();
        assert_eq!(read_blob(t.path(), &a.sha256).unwrap(), b"a");
        let mut used = std::collections::BTreeSet::new();
        used.insert(a.sha256.clone());
        assert_eq!(gc_blobs(t.path(), &used, SystemTime::now()), 0, "young");
        let later = SystemTime::now() + Duration::from_secs(7200);
        assert_eq!(gc_blobs(t.path(), &used, later), 1);
        assert!(blob_path(t.path(), &a.sha256).unwrap().is_file());
        assert!(!blob_path(t.path(), &b.sha256).unwrap().exists());
        assert!(blob_path(t.path(), "../x").is_err());
    }
}
