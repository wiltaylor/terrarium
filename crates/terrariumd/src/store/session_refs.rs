//! The sessions behind board work (PLAN §13, §18). A card comment of an
//! agent links to the agent's current session, and a card event of an
//! agent carries it as `data.actor_session`. A session keeps its row
//! after `agent rm` (migration 37): the row then names the harness, and
//! `archive` holds the copy of its session file (`crate::session_archive`).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::db::{Tx, now};
use crate::error::ApiResult;

/// A session, with enough of its agent to render a link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRef {
    pub id: String,
    pub agent_id: String,
    /// The agent name, without the project.
    pub agent: String,
    pub project: String,
    pub harness: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// The agent is deleted. The transcript comes from the archive.
    pub removed: bool,
}

/// The internal row: a [`SessionRef`] and its archive path.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session: SessionRef,
    pub harness_session_id: Option<String>,
    pub current: bool,
    /// The archived session file, relative to the state dir.
    pub archive: Option<String>,
}

const SELECT: &str = "SELECT s.id, s.agent_id, s.harness_session_id, s.current, s.started_at,
            s.ended_at, s.archive,
            coalesce(a.name, r.name, s.agent_id) AS agent_name,
            coalesce(a.project, r.project, '') AS project,
            coalesce(a.harness, s.harness, '') AS harness,
            a.id IS NULL AS removed
     FROM sessions s
     LEFT JOIN agents a ON a.id = s.agent_id
     LEFT JOIN agents_removed r ON r.id = s.agent_id";

/// Sessions attributed by usage, including removed ephemeral agents, or by
/// the live agent when no usage has been collected yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubscriptionSession {
    #[serde(flatten)]
    pub session: SessionRef,
    pub model: Option<String>,
    pub card: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

pub fn on_subscription(
    conn: &Connection,
    subscription: &str,
    since: &str,
    until: &str,
) -> ApiResult<Vec<SubscriptionSession>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT}
         WHERE julianday(s.started_at) >= julianday(?2)
           AND julianday(s.started_at) < julianday(?3)
           AND (EXISTS (SELECT 1 FROM usage_samples u
                WHERE u.agent = s.agent_id
                  AND u.session IN (s.id, s.harness_session_id) AND u.subscription = ?1)
             OR (a.subscription = ?1 AND NOT EXISTS (SELECT 1 FROM usage_samples u
                WHERE u.agent = s.agent_id AND u.session IN (s.id, s.harness_session_id))))
         ORDER BY julianday(s.started_at) DESC, s.id DESC"
    ))?;
    let sessions = stmt.query_map(params![subscription, since, until], row)?
        .collect::<Result<Vec<_>, _>>()?;
    sessions.into_iter().map(|s| {
        let (model, card, input_tokens, output_tokens) = conn.query_row(
            "SELECT max(model), max(card_id), sum(input_tokens), sum(output_tokens)
             FROM usage_samples WHERE agent = ?1 AND session IN (?2, ?3)
               AND subscription = ?4 AND julianday(at) >= julianday(?5)
               AND julianday(at) < julianday(?6)",
            params![s.session.agent_id, s.session.id, s.harness_session_id, subscription, since, until],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        Ok(SubscriptionSession { session: s.session, model, card, input_tokens, output_tokens })
    }).collect()
}

fn row(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session: SessionRef {
            id: r.get("id")?,
            agent_id: r.get("agent_id")?,
            agent: r.get("agent_name")?,
            project: r.get("project")?,
            harness: r.get("harness")?,
            started_at: r.get("started_at")?,
            ended_at: r.get("ended_at")?,
            removed: r.get::<_, i64>("removed")? != 0,
        },
        harness_session_id: r.get("harness_session_id")?,
        current: r.get::<_, i64>("current")? != 0,
        archive: r.get("archive")?,
    })
}

/// One session by its ID, of a live or a deleted agent.
pub fn get(conn: &Connection, id: &str) -> ApiResult<Option<SessionRow>> {
    Ok(conn
        .query_row(&format!("{SELECT} WHERE s.id = ?1"), [id], row)
        .optional()?)
}

/// The current session of `author` when it is an agent; `None` for the
/// operator, a project, or an agent with no session.
pub fn current_of(conn: &Connection, author: &str) -> ApiResult<Option<String>> {
    let id = author.strip_prefix("agent:").unwrap_or(author);
    if id == "operator" || id == "system" || id.starts_with("project:") {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT id FROM sessions WHERE agent_id = ?1 AND current = 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// The distinct sessions that touched `card`: the sessions of its
/// comments and of its card events (`data.actor_session`), oldest first.
pub fn of_card(conn: &Connection, card: &str) -> ApiResult<Vec<SessionRef>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE s.id IN (
             SELECT session_id FROM card_comments WHERE card = ?1 AND session_id IS NOT NULL
             UNION
             SELECT json_extract(payload, '$.actor_session') FROM events
             WHERE card = ?1 AND kind = 'card'
               AND json_extract(payload, '$.actor_session') IS NOT NULL)
         ORDER BY s.started_at, s.id"
    ))?;
    let rows = stmt
        .query_map([card], row)?
        .map(|r| r.map(|x| x.session))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The end of an agent delete (`store::agents::delete`): its sessions
/// stay, end, and keep the harness of the agent for the transcript
/// parser.
pub fn on_agent_deleted(t: &mut Tx<'_>, agent: &str, harness: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE sessions SET harness = ?2,
                current = 0,
                ended_at = coalesce(ended_at, ?3),
                end_reason = coalesce(end_reason, 'removed')
         WHERE agent_id = ?1",
        params![agent, harness, now()],
    )?;
    Ok(())
}

/// The sessions of a deleted agent whose file is not archived yet.
pub fn unarchived(conn: &Connection, agent: &str) -> ApiResult<Vec<SessionRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE s.agent_id = ?1 AND s.archive IS NULL ORDER BY s.id"
    ))?;
    let rows = stmt
        .query_map([agent], row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Record the archived file of a session.
pub fn set_archive(t: &mut Tx<'_>, session: &str, rel: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE sessions SET archive = ?2 WHERE id = ?1",
        params![session, rel],
    )?;
    Ok(())
}

/// The session of a log row (an event, a message, an audit row), before
/// the rights check. The API shows it as `session_id` and `session_label`
/// only when the reader may read the session ([`SessionField::show`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionLink {
    pub id: String,
    /// `<agent> · <start time>`, for example `dev · 2026-10-26 09:12 UTC`.
    pub label: String,
    /// The project of the session's agent: the read right of the link.
    pub project: String,
    /// The agent is deleted. Then the link needs the archived file.
    pub removed: bool,
    /// The archived session file, relative to the state dir.
    pub archive: Option<String>,
}

/// The session fields of a log row. `session_id` and `session_label` are
/// empty until [`SessionField::show`] passes the rights check, so a row
/// that skips the check shows no link.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionField {
    /// The session that was active when the row was written: a link to
    /// `/api/sessions/{id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `<agent> · <start time>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
    /// The found session, before the rights check.
    #[serde(skip)]
    pub link: Option<SessionLink>,
}

impl SessionField {
    pub fn of(link: Option<SessionLink>) -> Self {
        Self {
            link,
            ..Self::default()
        }
    }

    /// Show the link when `may` lets the reader read the session; else
    /// clear the shown fields.
    pub fn show(&mut self, may: impl FnOnce(&SessionLink) -> bool) {
        match &self.link {
            Some(l) if may(l) => {
                self.session_id = Some(l.id.clone());
                self.session_label = Some(l.label.clone());
            }
            _ => {
                self.session_id = None;
                self.session_label = None;
            }
        }
    }
}

/// What to find the session of a log row by: the exact session when the
/// row recorded one, else the agent and the time of the row.
#[derive(Debug, Clone, Copy)]
pub struct LinkKey<'a> {
    pub session: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub at: &'a str,
}

/// The agent ULID of an actor or a mail address: `agent:<ulid>` or a bare
/// ULID. `None` for the operator, `system`, and a project.
pub fn agent_of(who: &str) -> Option<&str> {
    let id = who.strip_prefix("agent:").unwrap_or(who);
    (id.len() == 26 && id.parse::<ulid::Ulid>().is_ok()).then_some(id)
}

/// The label of a session link: the agent and the start, to the minute.
fn link_label(agent: &str, started_at: &str) -> String {
    let start = chrono::DateTime::parse_from_rfc3339(started_at)
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M UTC")
                .to_string()
        })
        .unwrap_or_else(|_| started_at.to_owned());
    format!("{agent} · {start}")
}

/// The sessions of a page of log rows, in the order of `keys`: two
/// queries for the whole page. A key with a session takes that session.
/// A key with an agent only takes the agent's session whose time window
/// holds `at`: `started_at <= at`, and `ended_at` is empty or `>= at`.
/// When windows overlap (a resumed session keeps its first start), the
/// latest start wins. The `sessions(agent_id, started_at)` index serves
/// the lookup.
pub fn links(conn: &Connection, keys: &[LinkKey<'_>]) -> ApiResult<Vec<Option<SessionLink>>> {
    use rusqlite::types::Value as Sql;
    let mut ids: Vec<Option<String>> = keys.iter().map(|k| k.session.map(str::to_owned)).collect();
    // The window lookup, one row per key that needs it.
    let want: Vec<usize> = (0..keys.len())
        .filter(|&i| keys[i].session.is_none() && keys[i].agent.is_some())
        .collect();
    for chunk in want.chunks(300) {
        let mut values = Vec::with_capacity(chunk.len() * 3);
        let rows: Vec<&str> = chunk.iter().map(|_| "(?, ?, ?)").collect();
        for &i in chunk {
            values.push(Sql::Integer(i as i64));
            values.push(Sql::Text(keys[i].agent.unwrap_or_default().to_owned()));
            values.push(Sql::Text(keys[i].at.to_owned()));
        }
        let sql = format!(
            "WITH w(i, agent, at) AS (VALUES {})
             SELECT w.i, (SELECT s.id FROM sessions s
                          WHERE s.agent_id = w.agent AND s.started_at <= w.at
                            AND (s.ended_at IS NULL OR s.ended_at >= w.at)
                          ORDER BY s.started_at DESC LIMIT 1)
             FROM w",
            rows.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let found = stmt.query_map(params_from_iter(values), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for f in found {
            let (i, id) = f?;
            if let Some(slot) = ids.get_mut(i as usize) {
                *slot = id;
            }
        }
    }
    // The sessions themselves, once each.
    let mut distinct: Vec<&str> = ids.iter().flatten().map(String::as_str).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let mut by_id: HashMap<String, SessionLink> = HashMap::new();
    for chunk in distinct.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let mut stmt = conn.prepare(&format!("{SELECT} WHERE s.id IN ({marks})"))?;
        let found = stmt.query_map(params_from_iter(chunk.iter()), row)?;
        for f in found {
            let r = f?;
            by_id.insert(
                r.session.id.clone(),
                SessionLink {
                    label: link_label(&r.session.agent, &r.session.started_at),
                    id: r.session.id,
                    project: r.session.project,
                    removed: r.session.removed,
                    archive: r.archive,
                },
            );
        }
    }
    Ok(ids
        .into_iter()
        .map(|id| id.and_then(|id| by_id.get(&id).cloned()))
        .collect())
}

/// The session link of one row: [`links`] for a single key.
pub fn link(conn: &Connection, key: LinkKey<'_>) -> ApiResult<Option<SessionLink>> {
    Ok(links(conn, &[key])?.pop().flatten())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        c
    }

    fn agent(c: &Connection, project: &str, name: &str) -> String {
        let id = ulid::Ulid::new().to_string();
        c.execute(
            "INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'claude', '/w', 't', 't')",
            params![id, name, project],
        )
        .unwrap();
        id
    }

    fn session(c: &Connection, agent: &str, start: &str, end: Option<&str>) -> String {
        let id = ulid::Ulid::new().to_string();
        c.execute(
            "INSERT INTO sessions (id, agent_id, current, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, agent, end.is_none(), start, end],
        )
        .unwrap();
        id
    }

    fn at<'a>(agent: &'a str, at: &'a str) -> LinkKey<'a> {
        LinkKey {
            session: None,
            agent: Some(agent),
            at,
        }
    }

    const T1: &str = "2026-10-26T09:00:00.000Z";
    const T2: &str = "2026-10-26T10:00:00.000Z";
    const T3: &str = "2026-10-26T11:00:00.000Z";

    #[test]
    fn a_recorded_session_wins_over_the_time() {
        let c = db();
        let a = agent(&c, "p", "dev");
        let old = session(&c, &a, T1, Some(T2));
        let new = session(&c, &a, T2, None);
        let l = link(
            &c,
            LinkKey {
                session: Some(&old),
                agent: Some(&a),
                at: T3,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(l.id, old);
        assert_eq!(l.project, "p");
        assert_eq!(l.label, "dev · 2026-10-26 09:00 UTC");
        assert!(!l.removed);
        // The same key with no session: the open one at T3.
        assert_eq!(link(&c, at(&a, T3)).unwrap().unwrap().id, new);
    }

    #[test]
    fn the_window_holds_its_bounds_and_nothing_outside() {
        let c = db();
        let a = agent(&c, "p", "dev");
        let s = session(&c, &a, T1, Some(T2));
        let keys = [
            at(&a, T1),
            at(&a, "2026-10-26T09:30:00.000Z"),
            at(&a, T2),
            at(&a, "2026-10-26T08:59:59.999Z"),
            at(&a, "2026-10-26T10:00:00.001Z"),
        ];
        let got: Vec<Option<String>> = links(&c, &keys)
            .unwrap()
            .into_iter()
            .map(|l| l.map(|l| l.id))
            .collect();
        assert_eq!(got, [Some(s.clone()), Some(s.clone()), Some(s), None, None]);
    }

    #[test]
    fn no_agent_no_session_and_an_unknown_session_give_no_link() {
        let c = db();
        let a = agent(&c, "p", "dev");
        let other = agent(&c, "p", "ops");
        session(&c, &other, T1, None);
        let keys = [
            LinkKey {
                session: None,
                agent: None,
                at: T2,
            },
            at(&a, T2),
            LinkKey {
                session: Some("01NOSUCHSESSION00000000000"),
                agent: Some(&a),
                at: T2,
            },
        ];
        assert_eq!(links(&c, &keys).unwrap(), [None, None, None]);
        assert!(links(&c, &[]).unwrap().is_empty());
    }

    #[test]
    fn overlapping_windows_take_the_latest_start() {
        let c = db();
        let a = agent(&c, "p", "dev");
        // A resumed session keeps its first start and is open again.
        let resumed = session(&c, &a, T1, None);
        let between = session(&c, &a, "2026-10-26T09:30:00.000Z", Some(T2));
        assert_eq!(
            link(&c, at(&a, "2026-10-26T09:45:00.000Z"))
                .unwrap()
                .unwrap()
                .id,
            between
        );
        assert_eq!(link(&c, at(&a, T3)).unwrap().unwrap().id, resumed);
    }

    #[test]
    fn subscription_sessions_include_removed_workers_only_in_period() {
        let c = db();
        let persistent = agent(&c, "p", "lead");
        let worker = agent(&c, "p", "card-worker");
        c.execute("UPDATE agents SET subscription = 'sub'", []).unwrap();
        let old = session(&c, &persistent, "2026-09-30T23:59:59Z", None);
        let live = session(&c, &persistent, "2026-10-01T00:00:00Z", None);
        let ephemeral = session(&c, &worker, "2026-10-02T00:00:00Z", None);
        c.execute("UPDATE sessions SET harness_session_id = 'harness-id', harness = 'claude' WHERE id = ?1", [&ephemeral]).unwrap();
        c.execute("INSERT INTO usage_samples (agent, project, subscription, model, input_tokens, output_tokens, cache_read, cache_write, message_id, session, at, card_id)
            VALUES (?1, 'p', 'sub', 'model', 12, 3, 0, 0, 'msg', 'harness-id', '2026-10-02T00:01:00Z', 'TER-244')", [&worker]).unwrap();
        c.execute("INSERT INTO agents_removed (id, project, name, removed_at) VALUES (?1, 'p', 'card-worker', '2026-10-03T00:00:00Z')", [&worker]).unwrap();
        c.execute("DELETE FROM agents WHERE id = ?1", [&worker]).unwrap();
        let sessions = on_subscription(&c, "sub", "2026-10-01T00:00:00Z", "2026-11-01T00:00:00Z").unwrap();
        assert_eq!(sessions.iter().map(|s| &s.session.id).collect::<Vec<_>>(), vec![&ephemeral, &live]);
        assert!(!sessions.iter().any(|s| s.session.id == old));
        assert!(sessions[0].session.removed);
        assert_eq!(sessions[0].input_tokens, Some(12));
        assert_eq!(sessions[0].card.as_deref(), Some("TER-244"));
        assert!(on_subscription(&c, "other", "2026-10-01T00:00:00Z", "2026-11-01T00:00:00Z").unwrap().is_empty());
    }

    #[test]
    fn a_deleted_agent_links_through_its_tombstone() {
        let c = db();
        let a = agent(&c, "p", "dev");
        let s = session(&c, &a, T1, Some(T2));
        c.execute(
            "UPDATE sessions SET harness = 'claude', archive = 'archive/agents/x' WHERE id = ?1",
            [&s],
        )
        .unwrap();
        c.execute(
            "INSERT INTO agents_removed (id, project, name, removed_at) VALUES (?1, 'p', 'dev', ?2)",
            params![a, T3],
        )
        .unwrap();
        c.execute("DELETE FROM agents WHERE id = ?1", [&a]).unwrap();
        let l = link(&c, at(&a, T1)).unwrap().unwrap();
        assert_eq!(l.id, s);
        assert!(l.removed);
        assert_eq!(l.project, "p");
        assert_eq!(l.archive.as_deref(), Some("archive/agents/x"));
    }

    #[test]
    fn a_page_larger_than_one_chunk_resolves_every_row() {
        let c = db();
        let a = agent(&c, "p", "dev");
        let s = session(&c, &a, T1, None);
        // More keys than one chunk of the window query.
        let times: Vec<String> = (0..700)
            .map(|i| format!("2026-10-26T10:{:02}:{:02}.000Z", i / 60 % 60, i % 60))
            .collect();
        let keys: Vec<LinkKey<'_>> = times.iter().map(|t| at(&a, t)).collect();
        let got = links(&c, &keys).unwrap();
        assert_eq!(got.len(), 700);
        assert!(got.iter().all(|l| l.as_ref().is_some_and(|l| l.id == s)));
    }

    /// The window lookup reads the `sessions(agent_id, started_at)` index,
    /// not the whole table.
    #[test]
    fn the_window_lookup_uses_the_index() {
        let c = db();
        let plan: Vec<String> = c
            .prepare(
                "EXPLAIN QUERY PLAN SELECT s.id FROM sessions s
                 WHERE s.agent_id = 'a' AND s.started_at <= 't'
                   AND (s.ended_at IS NULL OR s.ended_at >= 't')
                 ORDER BY s.started_at DESC LIMIT 1",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|p| p.contains("sessions_agent")),
            "{plan:?}"
        );
    }

    #[test]
    fn show_needs_the_check() {
        let mut f = SessionField::of(Some(SessionLink {
            id: "S".into(),
            label: "dev · x".into(),
            project: "p".into(),
            removed: false,
            archive: None,
        }));
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json, serde_json::json!({}), "no link before the check");
        f.show(|_| true);
        assert_eq!(f.session_id.as_deref(), Some("S"));
        assert_eq!(f.session_label.as_deref(), Some("dev · x"));
        f.show(|_| false);
        assert!(f.session_id.is_none() && f.session_label.is_none());
    }

    #[test]
    fn agent_of_reads_actors_and_addresses() {
        let id = ulid::Ulid::new().to_string();
        assert_eq!(agent_of(&format!("agent:{id}")), Some(id.as_str()));
        assert_eq!(agent_of(&id), Some(id.as_str()));
        assert_eq!(agent_of("operator"), None);
        assert_eq!(agent_of("project:p"), None);
        assert_eq!(agent_of("system"), None);
    }
}
