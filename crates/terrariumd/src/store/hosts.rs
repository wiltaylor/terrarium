//! The host registry rows (PLAN §19, docs/host-protocol.md).
//!
//! One row per host agent. The token is shown once, when the row is made;
//! the table keeps only its SHA-256. The live connection state is in
//! [`crate::hosts::HostRegistry`]; the row keeps the last `hello` and the
//! last time the host was seen.

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
}

const COLS: &str =
    "name, id, created_by, capabilities, os, arch, version, status, last_seen, created_at";

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
    })
}

/// The SHA-256 hex of a token, as stored.
pub fn token_hash(token: &str) -> String {
    terrarium_host::protocol::hex(&Sha256::digest(token.as_bytes()))
}

/// A new random token: the prefix and 32 random bytes in hex.
pub fn new_token() -> ApiResult<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| ApiError::internal(format!("cannot read random bytes: {e}")))?;
    Ok(format!(
        "{TOKEN_PREFIX}{}",
        terrarium_host::protocol::hex(&bytes)
    ))
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

/// Make a host row. Returns the row and the token, which is never stored.
pub fn create(t: &mut Tx<'_>, name: &str) -> ApiResult<(Host, String)> {
    check_name("host", name)?;
    if find(t, name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "host agent `{name}` exists; delete it first to mint a new token"
        )));
    }
    let token = new_token()?;
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO hosts (name, id, token_hash, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![name, id, token_hash(&token), t.actor, now()],
    )?;
    Ok((get(t, name)?, token))
}

/// Delete a host row, so its token stops working. False when it was absent.
pub fn delete(t: &mut Tx<'_>, name: &str) -> ApiResult<bool> {
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
