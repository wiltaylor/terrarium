//! Home-side links. Keys never participate in the normal secret registry.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};

#[derive(Clone, Serialize, Deserialize)]
pub struct Remote {
    pub id: String,
    pub name: String,
    pub url: String,
    pub instance_id: String,
    pub pin_sha256: Option<String>,
    pub ca_pem: Option<String>,
    pub label: String,
    pub read_only: bool,
    pub created_at: String,
    pub last_ok_at: Option<String>,
    pub last_error: Option<String>,
    pub status: String,
}

const COLUMNS: &str = "id,name,url,instance_id,pin_sha256,ca_pem,label,read_only,created_at,last_ok_at,last_error,status";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Remote> {
    Ok(Remote {
        id: r.get(0)?,
        name: r.get(1)?,
        url: r.get(2)?,
        instance_id: r.get(3)?,
        pin_sha256: r.get(4)?,
        ca_pem: r.get(5)?,
        label: r.get(6)?,
        read_only: r.get(7)?,
        created_at: r.get(8)?,
        last_ok_at: r.get(9)?,
        last_error: r.get(10)?,
        status: r.get(11)?,
    })
}

pub fn list(c: &Connection) -> ApiResult<Vec<Remote>> {
    Ok(
        c.prepare(&format!("SELECT {COLUMNS} FROM remotes ORDER BY name"))?
            .query_map([], row)?
            .collect::<rusqlite::Result<_>>()?,
    )
}

pub fn get(c: &Connection, name: &str) -> ApiResult<Remote> {
    c.query_row(
        &format!("SELECT {COLUMNS} FROM remotes WHERE name=?1"),
        [name],
        row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found("remote not found"))
}

pub fn sealed(c: &Connection, id: &str) -> ApiResult<Vec<u8>> {
    Ok(c.query_row("SELECT sealed FROM remotes WHERE id=?1", [id], |r| r.get(0))?)
}

pub fn insert(t: &Tx<'_>, r: &Remote, sealed: &[u8]) -> ApiResult<()> {
    if list(t)?.iter().any(|old| old.name == r.name) {
        return Err(ApiError::conflict("remote name is already used"));
    }
    t.execute("INSERT INTO remotes (id,name,url,instance_id,pin_sha256,ca_pem,label,sealed,read_only,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![r.id,r.name,r.url,r.instance_id,r.pin_sha256,r.ca_pem,r.label,sealed,r.read_only,r.created_at])?;
    Ok(())
}

pub fn mark(t: &Tx<'_>, id: &str, status: &str) -> ApiResult<()> {
    let ok = status == "online" || status == "version_skew";
    t.execute("UPDATE remotes SET status=?2,last_error=?3,last_ok_at=CASE WHEN ?4 THEN ?5 ELSE last_ok_at END WHERE id=?1",
        params![id,status,if ok {None} else {Some(status)},ok,now()])?;
    Ok(())
}

pub fn aad(id: &str) -> Vec<u8> {
    format!("remote:{id}").into_bytes()
}
