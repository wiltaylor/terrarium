//! The link rows (TER-61, wiki design/linked-instances D2, `docs/api.md`
//! §35): the keys with which another Terrarium daemon (the home) calls
//! this one.
//!
//! A link is made with a one-time pairing code (`tlp_`, 10 minutes). The
//! home sends the code once to `POST /api/links/pair` and gets the
//! long-lived link key (`tlk_`) in the answer, so the key never passes
//! through the operator's hands. The table keeps only the SHA-256 of the
//! code and of the key, as for host tokens (`store::hosts`).
//!
//! A revoked link keeps its row without hashes, so the audit rows of its
//! actor `link:<id>` keep their label.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::hosts::{random_token, token_hash};

/// The prefix of a link key, so a leaked one is easy to spot.
pub const KEY_PREFIX: &str = "tlk_";
/// The prefix of a one-time pairing code.
pub const CODE_PREFIX: &str = "tlp_";
/// How long a pairing code stays good.
pub const PAIR_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// The longest label.
pub const MAX_LABEL: usize = 64;

/// A link row, without the hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub id: String,
    pub label: String,
    /// `GET`, SSE, and the event WebSocket only (design Q2).
    pub read_only: bool,
    /// `pending` (a pairing code is open or ran out, no key), `active`
    /// (paired), or `revoked`.
    pub status: &'static str,
    /// The instance ID that the home sent at pairing.
    pub home_instance_id: Option<String>,
    pub home_name: Option<String>,
    pub paired_at: Option<String>,
    /// The peer address of the pairing call.
    pub paired_from: Option<String>,
    pub created_by: String,
    pub created_at: String,
    /// The last `POST /api/links/{id}/rotate`.
    pub rotated_at: Option<String>,
    /// The first call with the current key.
    pub first_used_at: Option<String>,
    pub last_used_at: Option<String>,
    /// The peer address of the last call, as the daemon saw it (a reverse
    /// proxy shows as itself).
    pub last_used_from: Option<String>,
    /// Set while a pairing code is open (or until it is used or replaced).
    pub pair_expires_at: Option<String>,
    pub revoked_at: Option<String>,
}

const COLS: &str = "id, label, read_only, key_hash IS NOT NULL, home_instance_id, home_name, \
                    paired_at, paired_from, created_by, created_at, rotated_at, first_used_at, \
                    last_used_at, last_used_from, pair_expires_at, revoked_at";

fn row(r: &Row<'_>) -> rusqlite::Result<Link> {
    let keyed: bool = r.get(3)?;
    let revoked_at: Option<String> = r.get(15)?;
    let status = if revoked_at.is_some() {
        "revoked"
    } else if keyed {
        "active"
    } else {
        "pending"
    };
    Ok(Link {
        id: r.get(0)?,
        label: r.get(1)?,
        read_only: r.get(2)?,
        status,
        home_instance_id: r.get(4)?,
        home_name: r.get(5)?,
        paired_at: r.get(6)?,
        paired_from: r.get(7)?,
        created_by: r.get(8)?,
        created_at: r.get(9)?,
        rotated_at: r.get(10)?,
        first_used_at: r.get(11)?,
        last_used_at: r.get(12)?,
        last_used_from: r.get(13)?,
        pair_expires_at: r.get(14)?,
        revoked_at,
    })
}

/// A pairing code as the API hands it out, once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub code: String,
    pub expires_at: String,
}

fn pair_expiry() -> String {
    let ttl = chrono::Duration::from_std(PAIR_TTL).unwrap_or(chrono::Duration::minutes(10));
    (chrono::Utc::now() + ttl).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn new_code() -> ApiResult<(Code, String)> {
    let code = random_token(CODE_PREFIX)?;
    let hash = token_hash(&code);
    Ok((
        Code {
            code,
            expires_at: pair_expiry(),
        },
        hash,
    ))
}

/// Check a label: trimmed, 1 to 64 characters, no control characters.
pub fn check_label(raw: &str) -> ApiResult<String> {
    let label = raw.trim();
    if label.is_empty() {
        return Err(ApiError::invalid("a link label is empty"));
    }
    if label.chars().count() > MAX_LABEL || label.chars().any(char::is_control) {
        return Err(ApiError::invalid(format!(
            "a link label has at most {MAX_LABEL} characters and no control characters"
        )));
    }
    Ok(label.to_owned())
}

/// The links, newest first. `revoked`: also the revoked ones.
pub fn list(conn: &Connection, revoked: bool) -> ApiResult<Vec<Link>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM links WHERE ?1 OR revoked_at IS NULL ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt
        .query_map([revoked], row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn find(conn: &Connection, id: &str) -> ApiResult<Option<Link>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM links WHERE id = ?1"),
            [id],
            row,
        )
        .optional()?)
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Link> {
    find(conn, id)?.ok_or_else(|| ApiError::not_found(format!("no link `{id}`")))
}

/// The labels of these link IDs, for the audit list.
pub fn labels(conn: &Connection, ids: &[&str]) -> ApiResult<Vec<Option<String>>> {
    let mut stmt = conn.prepare_cached("SELECT label FROM links WHERE id = ?1")?;
    ids.iter()
        .map(|id| Ok(stmt.query_row([id], |r| r.get(0)).optional()?))
        .collect()
}

/// The active link whose key has this hash.
pub fn by_key_hash(conn: &Connection, hash: &str) -> ApiResult<Option<Link>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM links WHERE key_hash = ?1 AND revoked_at IS NULL"),
            [hash],
            row,
        )
        .optional()?)
}

/// Make a link with an open pairing code. Returns the row and the code,
/// which is never stored.
pub fn create(t: &mut Tx<'_>, label: &str, read_only: bool) -> ApiResult<(Link, Code)> {
    let label = check_label(label)?;
    let (code, hash) = new_code()?;
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO links (id, label, read_only, pair_code_hash, pair_expires_at,
                            created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            label,
            read_only,
            hash,
            code.expires_at,
            t.actor,
            now()
        ],
    )?;
    Ok((get(t, &id)?, code))
}

/// A new pairing code for a link. The old key stops working at once (a
/// rotate after a leak must cut the key now); the home takes the code with
/// its Re-pair. Returns the row, the code, and the hash of the old key, so
/// the caller can close the streams of the old key.
pub fn rotate(t: &mut Tx<'_>, id: &str) -> ApiResult<(Link, Code, Option<String>)> {
    let old = live_key_hash(t, id)?;
    let (code, hash) = new_code()?;
    t.execute(
        "UPDATE links SET key_hash = NULL, pair_code_hash = ?2, pair_expires_at = ?3,
                          rotated_at = ?4, first_used_at = NULL
         WHERE id = ?1",
        params![id, hash, code.expires_at, now()],
    )?;
    Ok((get(t, id)?, code, old))
}

/// The key hash of a link that is not revoked. `404` for an unknown or a
/// revoked link.
fn live_key_hash(t: &Tx<'_>, id: &str) -> ApiResult<Option<String>> {
    let found: Option<(Option<String>, Option<String>)> = t
        .query_row(
            "SELECT key_hash, revoked_at FROM links WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match found {
        Some((hash, None)) => Ok(hash),
        Some((_, Some(_))) => Err(ApiError::not_found(format!("link `{id}` is revoked"))),
        None => Err(ApiError::not_found(format!("no link `{id}`"))),
    }
}

/// What the home sends with the pairing code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    pub instance_id: String,
    pub name: Option<String>,
    pub from: String,
}

/// Swap an open pairing code for a new long-lived key: one update, so of
/// two calls with one code only one wins. Returns the link and the key, or
/// `None` when the code is wrong, used, or past its time.
pub fn pair(t: &mut Tx<'_>, code: &str, home: &Home) -> ApiResult<Option<(Link, String)>> {
    let key = random_token(KEY_PREFIX)?;
    let at = now();
    let id: Option<String> = t
        .query_row(
            "UPDATE links SET key_hash = ?2, pair_code_hash = NULL, pair_expires_at = NULL,
                              paired_at = ?3, paired_from = ?4, home_instance_id = ?5,
                              home_name = ?6, first_used_at = NULL
             WHERE pair_code_hash = ?1 AND pair_expires_at > ?3 AND revoked_at IS NULL
             RETURNING id",
            params![
                token_hash(code),
                token_hash(&key),
                at,
                home.from,
                home.instance_id,
                home.name
            ],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => Ok(Some((get(t, &id)?, key))),
        None => Ok(None),
    }
}

/// Revoke a link: the key and an open code stop working. Returns the row
/// as it was and the hash of its key (to close its streams). `404` for an
/// unknown or a revoked link.
pub fn revoke(t: &mut Tx<'_>, id: &str) -> ApiResult<(Link, Option<String>)> {
    let old = live_key_hash(t, id)?;
    let before = get(t, id)?;
    t.execute(
        "UPDATE links SET key_hash = NULL, pair_code_hash = NULL, pair_expires_at = NULL,
                          revoked_at = ?2
         WHERE id = ?1",
        params![id, now()],
    )?;
    Ok((before, old))
}

/// Record a call: the first use of the key, the last use, and the peer
/// address.
pub fn mark_used(t: &mut Tx<'_>, id: &str, from: &str) -> ApiResult<()> {
    let at = now();
    t.execute(
        "UPDATE links SET first_used_at = COALESCE(first_used_at, ?2), last_used_at = ?2,
                          last_used_from = ?3
         WHERE id = ?1 AND revoked_at IS NULL",
        params![id, at, from],
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn open() -> Db {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        Db::new(conn, crate::events::EventHub::default())
    }

    fn home() -> Home {
        Home {
            instance_id: "01HOME".into(),
            name: Some("home".into()),
            from: "10.0.0.1".into(),
        }
    }

    #[tokio::test]
    async fn pair_once_then_the_key_authenticates() {
        let db = open();
        let (link, code) = db
            .write("operator", |t| create(t, "lab", false))
            .await
            .unwrap();
        assert_eq!(link.status, "pending");
        assert!(code.code.starts_with(CODE_PREFIX));
        assert_eq!(code.code.len(), 4 + 64);
        let c = code.code.clone();
        let first = db
            .write("operator", move |t| pair(t, &c, &home()))
            .await
            .unwrap();
        let (paired, key) = first.unwrap();
        assert_eq!(paired.status, "active");
        assert_eq!(paired.home_instance_id.as_deref(), Some("01HOME"));
        assert!(key.starts_with(KEY_PREFIX));
        // The same code a second time: nothing.
        let c = code.code.clone();
        let again = db
            .write("operator", move |t| pair(t, &c, &home()))
            .await
            .unwrap();
        assert!(again.is_none());
        let h = token_hash(&key);
        let found = db.read(move |c| by_key_hash(c, &h)).await.unwrap();
        assert_eq!(found.unwrap().id, link.id);
        // Neither the key nor the code is stored in clear.
        let k = key.clone();
        let clear: i64 = db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM links WHERE key_hash = ?1 OR pair_code_hash = ?2",
                    params![k, code.code],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(clear, 0);
    }

    #[tokio::test]
    async fn an_expired_code_does_not_pair() {
        let db = open();
        let (link, code) = db
            .write("operator", |t| create(t, "lab", true))
            .await
            .unwrap();
        let id = link.id.clone();
        db.write("operator", move |t| {
            t.execute(
                "UPDATE links SET pair_expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let got = db
            .write("operator", move |t| pair(t, &code.code, &home()))
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn revoke_and_rotate_stop_the_key() {
        let db = open();
        let (link, code) = db
            .write("operator", |t| create(t, "lab", false))
            .await
            .unwrap();
        let (_, key) = db
            .write("operator", move |t| pair(t, &code.code, &home()))
            .await
            .unwrap()
            .unwrap();
        let id = link.id.clone();
        let (_, new_code, old) = db
            .write("operator", move |t| rotate(t, &id))
            .await
            .unwrap();
        assert_eq!(old, Some(token_hash(&key)));
        let h = token_hash(&key);
        assert!(db.read(move |c| by_key_hash(c, &h)).await.unwrap().is_none());
        let (_, key2) = db
            .write("operator", move |t| pair(t, &new_code.code, &home()))
            .await
            .unwrap()
            .unwrap();
        let id = link.id.clone();
        let (_, old) = db.write("operator", move |t| revoke(t, &id)).await.unwrap();
        assert_eq!(old, Some(token_hash(&key2)));
        let h = token_hash(&key2);
        assert!(db.read(move |c| by_key_hash(c, &h)).await.unwrap().is_none());
        let id = link.id.clone();
        let shown = db.read(move |c| get(c, &id)).await.unwrap();
        assert_eq!(shown.status, "revoked");
        // A revoked link cannot be revoked or rotated again.
        let id = link.id.clone();
        assert!(db.write("operator", move |t| revoke(t, &id)).await.is_err());
        let id = link.id.clone();
        assert!(db.write("operator", move |t| rotate(t, &id)).await.is_err());
        // It stays in the list with `revoked`, and out of it without.
        assert!(db.read(|c| list(c, false)).await.unwrap().is_empty());
        assert_eq!(db.read(|c| list(c, true)).await.unwrap().len(), 1);
    }

    #[test]
    fn labels_are_checked() {
        assert!(check_label("  ").is_err());
        assert!(check_label("a\nb").is_err());
        assert!(check_label(&"x".repeat(65)).is_err());
        assert_eq!(check_label(" lab ").unwrap(), "lab");
    }
}
