//! Event sources (docs/api.md §20): the webhook senders that post to the
//! ingest listener (`crate::ingest`), and the delivery IDs that it saw.
//!
//! A source belongs to a project, or is global (`scope` is `''`). Its
//! secret is the HMAC key (GitHub, Forgejo) or the bearer token (generic).
//! [`EventSource`] never holds the secret: only [`create`] and [`rotate`]
//! return it, and only the ingest listener reads it ([`for_ingest`]).

use std::io::Read;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::{check_name, projects};

/// The days that the daily prune keeps a delivery ID.
pub const KEEP_DELIVERY_DAYS: i64 = 7;

/// The sender of the webhooks of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Github,
    Forgejo,
    Generic,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Forgejo => "forgejo",
            Self::Generic => "generic",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "github" => Self::Github,
            "forgejo" => Self::Forgejo,
            _ => Self::Generic,
        }
    }
}

/// One event source, as the API shows it. No secret.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventSource {
    pub id: String,
    pub name: String,
    /// The project. `None` is a global source.
    pub project: Option<String>,
    pub kind: SourceKind,
    pub enabled: bool,
    pub created_by: String,
    /// The last request that passed the signature check.
    pub last_seen: Option<String>,
    /// Why the last request failed, or `None` after a good one.
    pub last_error: Option<String>,
    /// The events that this source added to the log.
    pub received: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl EventSource {
    /// `project/name`, or `_/name` for a global source: the path of the
    /// hook URL, and the actor `source:<key>` of its events.
    pub fn key(&self) -> String {
        format!("{}/{}", self.project.as_deref().unwrap_or("_"), self.name)
    }

    /// The actor of the events of this source.
    pub fn actor(&self) -> String {
        format!("source:{}", self.key())
    }
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<EventSource> {
    let kind: String = r.get("kind")?;
    Ok(EventSource {
        id: r.get("id")?,
        name: r.get("name")?,
        project: r.get("project")?,
        kind: SourceKind::parse(&kind),
        enabled: r.get::<_, i64>("enabled")? != 0,
        created_by: r.get("created_by")?,
        last_seen: r.get("last_seen")?,
        last_error: r.get("last_error")?,
        received: r.get("received")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

/// A new secret: 32 random bytes in hex.
pub fn new_secret() -> ApiResult<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| ApiError::internal(format!("cannot read random bytes: {e}")))?;
    Ok(terrarium_host::protocol::hex(&bytes))
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<EventSource> {
    conn.query_row("SELECT * FROM event_sources WHERE id = ?1", [id], from_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no event source `{id}`")))
}

/// The source `name` in `project` (`None`: global).
fn find_in(conn: &Connection, project: Option<&str>, name: &str) -> ApiResult<Option<EventSource>> {
    Ok(conn
        .query_row(
            "SELECT * FROM event_sources WHERE scope = ?1 AND name = ?2",
            params![project.unwrap_or_default(), name],
            from_row,
        )
        .optional()?)
}

/// Find a source by ULID, `project/name` (`_/name` for a global source),
/// or a name: first in `context` (the caller's project), then global.
pub fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<EventSource> {
    let key = key.trim();
    if key.parse::<ulid::Ulid>().is_ok()
        && let Ok(s) = get(conn, key)
    {
        return Ok(s);
    }
    let found = match key.split_once('/') {
        Some(("_", n)) => find_in(conn, None, n)?,
        Some((p, n)) => find_in(conn, Some(p), n)?,
        None => match context {
            Some(p) => match find_in(conn, Some(p), key)? {
                Some(s) => Some(s),
                None => find_in(conn, None, key)?,
            },
            None => find_in(conn, None, key)?,
        },
    };
    found.ok_or_else(|| ApiError::not_found(format!("no event source `{key}`")))
}

/// The source and its secret for the hook path `/hooks/{scope}/{name}`
/// (`_` is the global scope). The ingest listener only.
pub fn for_ingest(
    conn: &Connection,
    scope: &str,
    name: &str,
) -> ApiResult<Option<(EventSource, String)>> {
    let scope = if scope == "_" { "" } else { scope };
    Ok(conn
        .query_row(
            "SELECT * FROM event_sources WHERE scope = ?1 AND name = ?2",
            params![scope, name],
            |r| Ok((from_row(r)?, r.get::<_, String>("secret")?)),
        )
        .optional()?)
}

/// The filters of a list.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only this project's sources.
    pub project: Option<String>,
    /// The global sources too (with `project`), or only them (without).
    pub global: bool,
    /// Without `project` and `global`: all sources.
    pub all: bool,
}

/// Sources, global first, then by project and name.
pub fn list(conn: &Connection, f: &ListFilter) -> ApiResult<Vec<EventSource>> {
    let mut stmt = conn.prepare("SELECT * FROM event_sources ORDER BY scope, name")?;
    let rows = stmt.query_map([], from_row)?;
    let all: Vec<EventSource> = rows.collect::<Result<_, _>>()?;
    Ok(all
        .into_iter()
        .filter(|s| {
            f.all
                || match (&s.project, &f.project) {
                    (None, _) => f.global,
                    (Some(p), Some(want)) => p == want,
                    (Some(_), None) => false,
                }
        })
        .collect())
}

/// A new source.
#[derive(Debug, Clone)]
pub struct NewSource {
    pub name: String,
    pub project: Option<String>,
    pub kind: SourceKind,
    pub enabled: bool,
    pub created_by: String,
}

fn emit(t: &mut Tx<'_>, s: &EventSource, change: &str) -> ApiResult<()> {
    let mut d = EventDraft::new(
        "event_source",
        format!("event source {} {change}", s.key()),
        json!({"change": change, "source": s.id, "name": s.name, "key": s.key(),
               "kind": s.kind, "enabled": s.enabled}),
    );
    if let Some(p) = &s.project {
        d = d.project(p.clone());
    }
    t.emit(d)?;
    Ok(())
}

/// Make a source. Returns it and its secret.
pub fn create(t: &mut Tx<'_>, new: &NewSource) -> ApiResult<(EventSource, String)> {
    check_name("event source", &new.name)?;
    if let Some(p) = &new.project
        && projects::find(t, p)?.is_none()
    {
        return Err(ApiError::not_found(format!("no project `{p}`")));
    }
    if find_in(t, new.project.as_deref(), &new.name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "event source `{}` exists in this scope",
            new.name
        )));
    }
    let id = ulid::Ulid::new().to_string();
    let secret = new_secret()?;
    let at = now();
    t.execute(
        "INSERT INTO event_sources (id, name, project, scope, kind, secret, enabled, created_by,
                                    created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![
            id,
            new.name,
            new.project,
            new.project.as_deref().unwrap_or_default(),
            new.kind.as_str(),
            secret,
            i64::from(new.enabled),
            new.created_by,
            at
        ],
    )?;
    let s = get(t, &id)?;
    t.audit(
        "event_source.create",
        &s.id,
        s.project.as_deref(),
        json!({"name": s.name, "kind": s.kind, "enabled": s.enabled}),
    )?;
    emit(t, &s, "created")?;
    Ok((s, secret))
}

/// The fields of a change. `None` keeps the value.
#[derive(Debug, Clone, Default)]
pub struct SourcePatch {
    pub name: Option<String>,
    pub enabled: Option<bool>,
}

pub fn update(t: &mut Tx<'_>, id: &str, p: &SourcePatch) -> ApiResult<EventSource> {
    let s = get(t, id)?;
    if let Some(name) = &p.name
        && *name != s.name
    {
        check_name("event source", name)?;
        if find_in(t, s.project.as_deref(), name)?.is_some() {
            return Err(ApiError::conflict(format!(
                "event source `{name}` exists in this scope"
            )));
        }
        t.execute(
            "UPDATE event_sources SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, name, now()],
        )?;
    }
    if let Some(on) = p.enabled {
        t.execute(
            "UPDATE event_sources SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, i64::from(on), now()],
        )?;
    }
    let after = get(t, id)?;
    t.audit(
        "event_source.update",
        id,
        after.project.as_deref(),
        json!({"name": p.name, "enabled": p.enabled}),
    )?;
    let change = match p.enabled {
        Some(true) if !s.enabled => "enabled",
        Some(false) if s.enabled => "disabled",
        _ => "updated",
    };
    emit(t, &after, change)?;
    Ok(after)
}

/// A new secret for a source. The old one stops working at once.
pub fn rotate(t: &mut Tx<'_>, id: &str) -> ApiResult<(EventSource, String)> {
    let secret = new_secret()?;
    let n = t.execute(
        "UPDATE event_sources SET secret = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, secret, now()],
    )?;
    if n == 0 {
        return Err(ApiError::not_found(format!("no event source `{id}`")));
    }
    let s = get(t, id)?;
    t.audit("event_source.rotate", id, s.project.as_deref(), json!({}))?;
    emit(t, &s, "rotated")?;
    Ok((s, secret))
}

pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let s = get(t, id)?;
    t.execute("DELETE FROM event_sources WHERE id = ?1", [id])?;
    t.audit(
        "event_source.delete",
        id,
        s.project.as_deref(),
        json!({"name": s.name, "kind": s.kind}),
    )?;
    emit(t, &s, "deleted")?;
    Ok(())
}

/// Note a delivery ID. `false` when the source saw it before.
pub fn record_delivery(t: &Tx<'_>, source: &str, delivery: &str) -> ApiResult<bool> {
    let n = t.execute(
        "INSERT OR IGNORE INTO event_deliveries (source, delivery, at) VALUES (?1, ?2, ?3)",
        params![source, delivery, now()],
    )?;
    Ok(n == 1)
}

/// A request that passed the signature check: `last_seen` is now, and
/// `last_error` is cleared. `counted`: it added an event.
pub fn mark_seen(t: &Tx<'_>, id: &str, counted: bool) -> ApiResult<()> {
    t.execute(
        "UPDATE event_sources SET last_seen = ?2, last_error = NULL,
                                  received = received + ?3 WHERE id = ?1",
        params![id, now(), i64::from(counted)],
    )?;
    Ok(())
}

/// A request that failed. The webhook sender gets no detail; the source
/// keeps the reason for its admin.
pub fn mark_error(t: &Tx<'_>, id: &str, error: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE event_sources SET last_error = ?2 WHERE id = ?1",
        params![id, format!("{}: {error}", now())],
    )?;
    Ok(())
}

/// Delete the delivery IDs older than [`KEEP_DELIVERY_DAYS`].
pub fn prune_deliveries(t: &Tx<'_>, now: chrono::DateTime<chrono::Utc>) -> ApiResult<usize> {
    let cutoff = (now - chrono::Duration::days(KEEP_DELIVERY_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(t.execute("DELETE FROM event_deliveries WHERE at < ?1", [cutoff])?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        crate::db::migrate(&mut c).unwrap();
        c.execute(
            "INSERT INTO projects (name, description, created_at, updated_at)
             VALUES ('terra', '', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        c
    }

    #[test]
    fn create_find_rotate_dedupe_prune() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let mut t = Tx::for_test(tx, "operator");
        let new = NewSource {
            name: "gh".into(),
            project: Some("terra".into()),
            kind: SourceKind::Github,
            enabled: true,
            created_by: "operator".into(),
        };
        let (s, secret) = create(&mut t, &new).unwrap();
        assert_eq!(secret.len(), 64);
        assert_eq!(s.key(), "terra/gh");
        assert_eq!(s.actor(), "source:terra/gh");
        assert!(create(&mut t, &new).is_err(), "the name is unique");
        let global = NewSource {
            project: None,
            ..new.clone()
        };
        let (g, _) = create(&mut t, &global).unwrap();
        assert_eq!(g.key(), "_/gh");
        assert_eq!(find(&t, "gh", Some("terra")).unwrap().id, s.id);
        assert_eq!(find(&t, "_/gh", Some("terra")).unwrap().id, g.id);
        assert_eq!(find(&t, "gh", None).unwrap().id, g.id);
        let (_, got) = for_ingest(&t, "terra", "gh").unwrap().unwrap();
        assert_eq!(got, secret);
        let (_, rotated) = rotate(&mut t, &s.id).unwrap();
        assert_ne!(rotated, secret);
        assert_eq!(for_ingest(&t, "terra", "gh").unwrap().unwrap().1, rotated);
        // The serialised source has no secret.
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("secret").is_none());
        assert!(!v.to_string().contains(&secret));

        assert!(record_delivery(&t, &s.id, "d1").unwrap());
        assert!(!record_delivery(&t, &s.id, "d1").unwrap());
        assert!(record_delivery(&t, &g.id, "d1").unwrap());
        let later = chrono::Utc::now() + chrono::Duration::days(8);
        assert_eq!(prune_deliveries(&t, later).unwrap(), 2);

        mark_error(&t, &s.id, "bad signature").unwrap();
        assert!(get(&t, &s.id).unwrap().last_error.is_some());
        mark_seen(&t, &s.id, true).unwrap();
        let s2 = get(&t, &s.id).unwrap();
        assert_eq!(s2.received, 1);
        assert!(s2.last_error.is_none() && s2.last_seen.is_some());
    }
}
