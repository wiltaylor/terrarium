//! The host registry rows (PLAN §19, docs/host-protocol.md).
//!
//! One row per host agent. The token is shown once, when the row is made
//! or rotated; the table keeps only its SHA-256. The live connection state
//! is in [`crate::hosts::HostRegistry`]; the row keeps the last `hello`,
//! the last time the host was seen, and where its token was last used.
//!
//! An enrol token (`the_` prefix, 10 minutes) is a one-time token. The
//! host agent dials with it once; the daemon then makes the long-lived
//! token and sends it in `welcome.token`, so the long-lived secret never
//! passes through the operator's hands.

use std::io::Read;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::check_name;

/// The prefix of a host token, so a leaked one is easy to spot.
pub const TOKEN_PREFIX: &str = "thk_";
/// The prefix of a one-time enrol token.
pub const ENROL_PREFIX: &str = "the_";
/// How long an enrol token stays good.
pub const ENROL_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// A host row, without the token hash.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Host {
    pub name: String,
    pub id: String,
    pub created_by: String,
    /// `HostCapabilities` from the last `hello`, `{}` before the first one.
    pub capabilities: Value,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
    /// `online` or `offline`, as stored. The API shows the live state.
    pub status: String,
    pub last_seen: Option<String>,
    pub created_at: String,
    /// The first connect with the current token.
    pub first_used_at: Option<String>,
    pub last_used_at: Option<String>,
    /// The peer address of the last connect, as the daemon saw it (a
    /// reverse proxy shows as itself).
    pub last_used_from: Option<String>,
    /// The last `POST /api/hosts/{name}/rotate`.
    pub rotated_at: Option<String>,
    /// Set while an enrol token is open.
    pub enrol_expires_at: Option<String>,
}

const COLS: &str = "name, id, created_by, capabilities, os, arch, version, status, last_seen, \
                    created_at, first_used_at, last_used_at, last_used_from, rotated_at, \
                    enrol_expires_at";

fn row(r: &Row<'_>) -> rusqlite::Result<Host> {
    let caps: String = r.get(3)?;
    Ok(Host {
        name: r.get(0)?,
        id: r.get(1)?,
        created_by: r.get(2)?,
        capabilities: serde_json::from_str(&caps)
            .unwrap_or_else(|_| Value::Object(Default::default())),
        os: r.get(4)?,
        arch: r.get(5)?,
        version: r.get(6)?,
        status: r.get(7)?,
        last_seen: r.get(8)?,
        created_at: r.get(9)?,
        first_used_at: r.get(10)?,
        last_used_at: r.get(11)?,
        last_used_from: r.get(12)?,
        rotated_at: r.get(13)?,
        enrol_expires_at: r.get(14)?,
    })
}

/// The SHA-256 hex of a token, as stored.
pub fn token_hash(token: &str) -> String {
    terrarium_host::protocol::hex(&Sha256::digest(token.as_bytes()))
}

/// A new random token: the prefix and 32 random bytes in hex.
pub fn new_token() -> ApiResult<String> {
    random_token(TOKEN_PREFIX)
}

/// A random token: `prefix` and 32 random bytes in hex. Link keys use it
/// too (`store::links`).
pub fn random_token(prefix: &str) -> ApiResult<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| ApiError::internal(format!("cannot read random bytes: {e}")))?;
    Ok(format!("{prefix}{}", terrarium_host::protocol::hex(&bytes)))
}

/// A token as the API hands it out, once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub token: String,
    /// `host` (long-lived) or `enrol` (one time, short-lived).
    pub kind: &'static str,
    /// When an enrol token stops working.
    pub expires_at: Option<String>,
}

/// What a bearer token on `/hosts/connect` is.
#[derive(Debug, Clone, PartialEq)]
pub enum Auth {
    /// The long-lived token of this host.
    Host(Host),
    /// An open enrol token of this host.
    Enrol(Host),
    /// An enrol token that is past its time.
    Expired,
    Unknown,
}

fn enrol_expiry() -> String {
    let ttl = chrono::Duration::from_std(ENROL_TTL).unwrap_or(chrono::Duration::minutes(10));
    (chrono::Utc::now() + ttl).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Make a fresh pair of hashes for a host: the long-lived token (or, with
/// `enrol`, a hidden one that nobody knows) and the enrol token.
fn issue(enrol: bool) -> ApiResult<(Issued, String, Option<String>)> {
    if enrol {
        let hidden = new_token()?;
        let token = random_token(ENROL_PREFIX)?;
        let hash = token_hash(&token);
        Ok((
            Issued {
                token,
                kind: "enrol",
                expires_at: Some(enrol_expiry()),
            },
            token_hash(&hidden),
            Some(hash),
        ))
    } else {
        let token = new_token()?;
        let hash = token_hash(&token);
        Ok((
            Issued {
                token,
                kind: "host",
                expires_at: None,
            },
            hash,
            None,
        ))
    }
}

pub fn list(conn: &Connection) -> ApiResult<Vec<Host>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM hosts ORDER BY name"))?;
    let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn find(conn: &Connection, name: &str) -> ApiResult<Option<Host>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM hosts WHERE name = ?1"),
            [name],
            row,
        )
        .optional()?)
}

pub fn get(conn: &Connection, name: &str) -> ApiResult<Host> {
    find(conn, name)?.ok_or_else(|| ApiError::not_found(format!("no host agent `{name}`")))
}

/// The host whose token has this hash.
pub fn by_token_hash(conn: &Connection, hash: &str) -> ApiResult<Option<Host>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM hosts WHERE token_hash = ?1"),
            [hash],
            row,
        )
        .optional()?)
}

/// Look up a bearer token by its hash: a long-lived token, or an enrol
/// token while it is open.
pub fn authenticate(conn: &Connection, hash: &str) -> ApiResult<Auth> {
    if let Some(h) = by_token_hash(conn, hash)? {
        return Ok(Auth::Host(h));
    }
    let enrol = conn
        .query_row(
            &format!("SELECT {COLS} FROM hosts WHERE enrol_token_hash = ?1"),
            [hash],
            row,
        )
        .optional()?;
    Ok(match enrol {
        Some(h)
            if h.enrol_expires_at
                .as_deref()
                .is_some_and(|e| e > now().as_str()) =>
        {
            Auth::Enrol(h)
        }
        Some(_) => Auth::Expired,
        None => Auth::Unknown,
    })
}

/// Make a host row. Returns the row and the token, which is never stored.
/// With `enrol`, the token is a one-time enrol token, and the long-lived
/// token comes into being at the first connect.
pub fn create(t: &mut Tx<'_>, name: &str, enrol: bool) -> ApiResult<(Host, Issued)> {
    check_name("host", name)?;
    let alias_exists: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM job_host_aliases WHERE alias = ?1 COLLATE NOCASE)",
        [name],
        |r| r.get(0),
    )?;
    if alias_exists {
        return Err(ApiError::conflict(format!(
            "host agent name `{name}` is already a job-host alias"
        )));
    }
    if find(t, name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "host agent `{name}` exists; rotate its token (POST /api/hosts/{name}/rotate) \
             or delete it first"
        )));
    }
    let (issued, hash, enrol_hash) = issue(enrol)?;
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO hosts (name, id, token_hash, created_by, created_at,
                            enrol_token_hash, enrol_expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            name,
            id,
            hash,
            t.actor,
            now(),
            enrol_hash,
            issued.expires_at
        ],
    )?;
    Ok((get(t, name)?, issued))
}

/// Give a host a new token. The old token stops working for new
/// connections at once; a link that is up now stays up until it ends.
/// With `enrol`, the new token is a one-time enrol token.
pub fn rotate(t: &mut Tx<'_>, name: &str, enrol: bool) -> ApiResult<(Host, Issued)> {
    get(t, name)?;
    let (issued, hash, enrol_hash) = issue(enrol)?;
    t.execute(
        "UPDATE hosts SET token_hash = ?2, enrol_token_hash = ?3, enrol_expires_at = ?4,
                          rotated_at = ?5, first_used_at = NULL
         WHERE name = ?1",
        params![name, hash, enrol_hash, issued.expires_at, now()],
    )?;
    Ok((get(t, name)?, issued))
}

/// Swap an open enrol token for a new long-lived token. Returns the token,
/// or `None` when the enrol token was used or ran out in the meantime (two
/// dials with one enrol token: only one wins).
pub fn complete_enrol(t: &mut Tx<'_>, name: &str, enrol_hash: &str) -> ApiResult<Option<String>> {
    let token = new_token()?;
    let n = t.execute(
        "UPDATE hosts SET token_hash = ?3, enrol_token_hash = NULL, enrol_expires_at = NULL
         WHERE name = ?1 AND enrol_token_hash = ?2 AND enrol_expires_at > ?4",
        params![name, enrol_hash, token_hash(&token), now()],
    )?;
    Ok((n == 1).then_some(token))
}

/// Record a connect: the first use of the token, the last use, and the
/// peer address.
pub fn mark_used(t: &mut Tx<'_>, name: &str, from: Option<&str>) -> ApiResult<()> {
    let at = now();
    t.execute(
        "UPDATE hosts SET first_used_at = COALESCE(first_used_at, ?2), last_used_at = ?2,
                          last_used_from = ?3
         WHERE name = ?1",
        params![name, at, from],
    )?;
    Ok(())
}

/// Delete a host row, so its token stops working. False when it was absent.
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<bool> {
    // Its job-host aliases go with it (TER-105).
    t.execute(
        "DELETE FROM job_host_aliases WHERE host_id IN (SELECT id FROM hosts WHERE name = ?1)",
        [name],
    )?;
    Ok(t.execute("DELETE FROM hosts WHERE name = ?1", [name])? > 0)
}

/// Store a `hello`: the capabilities and versions, and `online`.
pub fn mark_online(
    t: &mut Tx<'_>,
    name: &str,
    capabilities: &Value,
    os: &str,
    arch: &str,
    version: &str,
) -> ApiResult<()> {
    t.execute(
        "UPDATE hosts SET capabilities = ?2, os = ?3, arch = ?4, version = ?5,
                          status = 'online', last_seen = ?6
         WHERE name = ?1",
        params![name, capabilities.to_string(), os, arch, version, now()],
    )?;
    Ok(())
}

pub fn mark_offline(t: &mut Tx<'_>, name: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE hosts SET status = 'offline', last_seen = ?2 WHERE name = ?1",
        params![name, now()],
    )?;
    Ok(())
}
