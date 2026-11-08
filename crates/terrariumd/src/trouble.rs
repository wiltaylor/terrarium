//! Card trouble signals. Notices are durable and sent once per card/signal,
//! even after a resolution, recurrence, or daemon restart. No operator mail.
//! Working time is replayed from card/status/wait events, not wall time.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::Shared;
use crate::db::Tx;
use crate::error::ApiResult;
use crate::events::EventDraft;
use crate::store::{card_history::ReviewEstimates, mail, usage};
use mail::{Address, Outgoing};

/// Project overrides. Missing token/turn limits are off. Type names are
/// the suffixes of type: labels. Time limits are seconds, zero disables.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub time_seconds: BTreeMap<String, u64>,
    pub tokens: BTreeMap<String, u64>,
    pub turns: BTreeMap<String, u64>,
    pub red_streak: usize,
    pub red_same_commit: usize,
    pub review_rounds: u64,
    pub eta_delays: u64,
    pub timer_polls: u64,
    pub compactions: u64,
    pub review_wait_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            time_seconds: BTreeMap::new(),
            tokens: BTreeMap::new(),
            turns: BTreeMap::new(),
            red_streak: 3,
            red_same_commit: 2,
            review_rounds: 2,
            eta_delays: 3,
            timer_polls: 3,
            compactions: 3,
            review_wait_seconds: 3600,
        }
    }
}

pub fn settings(c: &Connection, project: &str) -> ApiResult<Settings> {
    let raw: Option<String> = c
        .query_row(
            "SELECT settings FROM project_trouble WHERE project = ?1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default())
}

pub fn set_settings(t: &mut Tx<'_>, project: &str, s: &Settings) -> ApiResult<()> {
    crate::store::projects::get(t, project)?;
    t.execute(
        "INSERT INTO project_trouble(project, settings) VALUES (?1, ?2)
        ON CONFLICT(project) DO UPDATE SET settings = excluded.settings",
        params![project, serde_json::to_string(s)?],
    )?;
    t.audit("project.trouble", project, Some(project), json!(s))?;
    Ok(())
}

/// Overrides win over 4x the project/type p70 from TER-316. That history
/// requires five distinct completed cards. Otherwise use fixed budgets.
fn time_limit(kind: &str, s: &Settings, p70_ms: Option<f64>) -> u64 {
    s.time_seconds.get(kind).copied().unwrap_or_else(|| {
        p70_ms
            .map(|ms| (4.0 * ms / 1000.0).ceil().max(1.0) as u64)
            .unwrap_or(match kind {
                "small" => 45 * 60,
                "impl-hard" | "hard" => 4 * 60 * 60,
                _ => 2 * 60 * 60,
            })
    })
}

#[derive(Debug, Clone)]
struct Event {
    at: DateTime<Utc>,
    kind: String,
    agent: Option<String>,
    card: Option<String>,
    data: Value,
}

fn parse(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn history(c: &Connection, card: &str) -> ApiResult<Vec<Event>> {
    // Start at the first claim after installation, not at card creation.
    // Only explicit assignees contribute status/wait/reset events. A lead
    // commenting on the card cannot pull in the lead's lifetime history.
    let start: Option<i64> = c.query_row(
        "SELECT min(seq) FROM events
        WHERE card = ?1 AND kind = 'card' AND json_extract(payload, '$.change') = 'claimed'
        AND seq > (SELECT seq FROM card_trouble_baseline WHERE singleton = 1)",
        [card],
        |r| r.get(0),
    )?;
    let Some(start) = start else {
        return Ok(Vec::new());
    };
    let mut q = c.prepare(
        "WITH assignees AS (
            SELECT DISTINCT json_extract(payload, '$.assignee') AS agent FROM events
            WHERE card = ?1 AND seq >= ?2 AND kind = 'card'
            AND json_extract(payload, '$.change') IN ('claimed', 'assigned')
        ) SELECT e.at, e.kind, e.agent_id, e.card,
          CASE WHEN e.kind = 'wait' THEN json_set(e.payload, '$.due_at', w.due_at) ELSE e.payload END
        FROM events e LEFT JOIN waits w ON e.kind = 'wait' AND w.id = json_extract(e.payload, '$.wait_id')
        WHERE (e.card = ?1 AND e.seq >= ?2)
          OR (e.seq >= ?2 AND e.kind IN ('status', 'wait', 'agent.reset')
              AND e.agent_id IN (SELECT agent FROM assignees))
          OR e.seq IN (SELECT max(s.seq) FROM events s JOIN assignees a ON a.agent = s.agent_id
              WHERE s.kind = 'status' AND s.seq < ?2 GROUP BY s.agent_id)
        ORDER BY e.seq",
    )?;
    let rows = q
        .query_map(params![card, start], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(at, kind, agent, card, data)| {
            Some(Event {
                at: parse(&at)?,
                kind,
                agent,
                card,
                data: serde_json::from_str(&data).ok()?,
            })
        })
        .collect())
}

#[derive(Default, Debug)]
struct Facts {
    seconds: u64,
    reviews: u64,
    eta_delays: u64,
    polls: u64,
    compactions: u64,
    review_since: Option<DateTime<Utc>>,
}

/// Replay only the intervals when this card has an assignee in a working
/// column and that assignee is working without an open wait. Events from
/// other agents never change the selected assignee's clock.
fn facts(card: &str, events: &[Event], now: DateTime<Utc>) -> Facts {
    let mut f = Facts::default();
    let mut agent: Option<String> = None;
    let mut working = false;
    let mut status = HashMap::<String, bool>::new();
    let mut waits = HashMap::<String, BTreeSet<String>>::new();
    let mut previous: Option<DateTime<Utc>> = None;
    let mut eta = None;
    let mut millis = 0;
    for e in events {
        if e.at > now {
            continue;
        }
        if let Some(at) = previous
            && working
            && let Some(a) = &agent
            && status.get(a) == Some(&true)
            && waits.get(a).is_none_or(BTreeSet::is_empty)
        {
            millis += (e.at - at).num_milliseconds().max(0) as u64;
        }
        previous = Some(e.at);
        let d = &e.data;
        if e.kind == "status" {
            if let Some(a) = &e.agent {
                status.insert(a.clone(), d["status"] == "working");
            }
        } else if e.kind == "wait" {
            if let Some(a) = &e.agent {
                let open = waits.entry(a.clone()).or_default();
                if let Some(id) = d["wait_id"].as_str() {
                    if d["change"] == "opened" {
                        open.insert(id.to_owned());
                    } else {
                        open.remove(id);
                    }
                }
                if working
                    && agent.as_ref() == Some(a)
                    && d["change"] == "opened"
                    && d["kind"] == "time"
                    && d["due_at"].as_str().and_then(parse).is_some_and(|due| {
                        due >= e.at && due <= e.at + chrono::Duration::minutes(1)
                    })
                    && e.at >= now - chrono::Duration::hours(1)
                {
                    f.polls += 1;
                }
            }
        } else if e.kind == "agent.reset"
            && working
            && e.agent == agent
            && d["kind"] == "compact"
            && e.at >= now - chrono::Duration::hours(1)
        {
            f.compactions += 1;
        }
        if e.card.as_deref() == Some(card) && e.kind == "card" {
            match d["change"].as_str().unwrap_or_default() {
                "created" | "claimed" | "assigned" | "moved" => {
                    working = d["column_role"] == "working" || d["column"] == "research";
                    agent = d["assignee"].as_str().map(str::to_owned).or_else(|| {
                        (d["change"] == "created")
                            .then(|| e.agent.clone())
                            .flatten()
                    });
                    if d["change"] == "moved" && (d["to_role"] == "review" || d["to"] == "review") {
                        f.reviews += 1;
                        f.review_since = Some(e.at);
                    } else {
                        f.review_since = None;
                    }
                    if d["change"] == "moved" {
                        eta = None;
                    }
                }
                "archived" => {
                    working = false;
                    agent = None;
                }
                "eta" => {
                    let next = d["eta_at"].as_str().and_then(parse);
                    let previous_eta = eta.or_else(|| d["previous"].as_str().and_then(parse));
                    if let (Some(old), Some(new)) = (previous_eta, next)
                        && new > old
                        && d["job"] != true
                    {
                        f.eta_delays += 1;
                    }
                    eta = next;
                }
                _ => {}
            }
        }
    }
    if working
        && let Some(a) = agent
        && status.get(&a) == Some(&true)
        && waits.get(&a).is_none_or(BTreeSet::is_empty)
        && let Some(at) = previous
    {
        millis += (now - at).num_milliseconds().max(0) as u64;
    }
    f.seconds = millis / 1000;
    f
}

/// Only linked runs uploaded by this card's worker count. Different
/// suite names are independent. A green/empty run breaks its red streak.
fn validation_red(c: &Connection, card: &str, agent: &str, s: &Settings) -> ApiResult<bool> {
    let mut q = c.prepare(
        "SELECT r.name, r.commit_sha, r.failed + r.errored FROM test_runs r
        JOIN test_run_cards l ON l.run = r.pk WHERE l.card = ?1 AND r.created_by = ?2
        ORDER BY r.pk DESC LIMIT 128",
    )?;
    let runs = q
        .query_map(params![card, agent], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stopped = BTreeSet::new();
    let mut counts = HashMap::<String, usize>::new();
    let mut commits = HashMap::<(String, String), usize>::new();
    for (name, commit, red) in runs {
        if stopped.contains(&name) {
            continue;
        }
        if red == 0 {
            stopped.insert(name);
            continue;
        }
        let count = counts.entry(name.clone()).or_default();
        *count += 1;
        if s.red_streak > 0 && *count >= s.red_streak {
            return Ok(true);
        }
        if let Some(sha) = commit.filter(|s| !s.is_empty()) {
            let count = commits.entry((name, sha)).or_default();
            *count += 1;
            if s.red_same_commit > 0 && *count >= s.red_same_commit {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Serialize)]
pub struct View {
    pub working_seconds: u64,
    pub signals: Vec<String>,
}

pub fn view(c: &Connection, card: &str, now: DateTime<Utc>) -> ApiResult<View> {
    let seconds = facts(card, &history(c, card)?, now).seconds;
    Ok(View {
        working_seconds: seconds,
        signals: active_signals(c, card)?,
    })
}

pub fn active_signals(c: &Connection, card: &str) -> ApiResult<Vec<String>> {
    let mut q = c.prepare(
        "SELECT signal FROM card_trouble WHERE card = ?1 AND active = 1 ORDER BY signal",
    )?;
    let signals = q
        .query_map([card], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(signals)
}

/// The tick does not hydrate card bodies, comments, links, dependencies,
/// or test cases. Those belong to the interactive card API, not this scan.
struct Watched {
    id: String,
    project: String,
    title: String,
    column: String,
    column_role: Option<String>,
    agent: Option<String>,
    labels: Vec<String>,
    eta_at: Option<String>,
    archived: bool,
}

struct Change {
    card: Watched,
    signals: BTreeSet<String>,
    facts: Facts,
    totals: usage::Totals,
}

fn desired(
    c: &Connection,
    card: &Watched,
    now: DateTime<Utc>,
    estimates: &mut ReviewEstimates,
) -> ApiResult<(BTreeSet<String>, Facts, usage::Totals)> {
    let mut out = BTreeSet::new();
    if card.archived {
        return Ok((out, Facts::default(), usage::Totals::default()));
    }
    if !matches!(card.column_role.as_deref(), Some("working" | "review"))
        && card.column != "research"
    {
        return Ok((out, Facts::default(), usage::Totals::default()));
    }
    let s = settings(c, &card.project)?;
    let events = history(c, &card.id)?;
    if events.is_empty() {
        return Ok((out, Facts::default(), usage::Totals::default()));
    }
    let f = facts(&card.id, &events, now);
    let kind = card
        .labels
        .iter()
        .find_map(|l| l.strip_prefix("type:"))
        .unwrap_or("impl");
    let p70 = if s.time_seconds.contains_key(kind) {
        None
    } else {
        let labels = if card.labels.iter().any(|l| l.starts_with("type:")) {
            card.labels.clone()
        } else {
            vec!["type:impl".into()]
        };
        estimates.worker_ms(c, &card.project, &labels, None)?
    };
    let limit = time_limit(kind, &s, p70);
    if limit > 0 && f.seconds >= limit {
        out.insert("time_budget".into());
    }
    if card
        .eta_at
        .as_deref()
        .and_then(parse)
        .is_some_and(|at| now >= at)
    {
        out.insert("eta_overdue".into());
    }
    if s.eta_delays > 0 && f.eta_delays >= s.eta_delays {
        out.insert("eta_delayed".into());
    }
    if s.review_rounds > 0 && f.reviews >= s.review_rounds {
        out.insert("review_rounds".into());
    }
    if s.review_wait_seconds > 0
        && card.column_role.as_deref() == Some("review")
        && f.review_since
            .is_some_and(|at| (now - at).num_seconds().max(0) as u64 >= s.review_wait_seconds)
    {
        out.insert("review_wait".into());
    }
    if s.timer_polls > 0 && f.polls >= s.timer_polls {
        out.insert("timer_polls".into());
    }
    if s.compactions > 0 && f.compactions >= s.compactions {
        out.insert("compactions".into());
    }
    let totals = usage::totals(
        c,
        &usage::UsageFilter {
            card: Some(card.id.clone()),
            since: events
                .iter()
                .find(|e| {
                    e.card.as_deref() == Some(card.id.as_str()) && e.data["change"] == "claimed"
                })
                .map(|e| crate::time_param::stored(e.at)),
            ..Default::default()
        },
    )?;
    if s.tokens
        .get(kind)
        .is_some_and(|n| *n > 0 && totals.tokens() >= *n)
    {
        out.insert("token_budget".into());
    }
    if s.turns
        .get(kind)
        .is_some_and(|n| *n > 0 && totals.turns >= *n)
    {
        out.insert("turn_budget".into());
    }
    if let Some(a) = card.agent.as_ref()
        && validation_red(c, &card.id, a, &s)?
    {
        out.insert("validation_red".into());
    }
    Ok((out, f, totals))
}

fn reconcile(t: &mut Tx<'_>, change: &Change) -> ApiResult<()> {
    let Change {
        card,
        signals,
        facts: f,
        totals,
    } = change;
    let mut q = t.prepare("SELECT signal, active FROM card_trouble WHERE card = ?1")?;
    let old = q
        .query_map([&card.id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    drop(q);
    if old.iter().any(|(s, active)| *active != signals.contains(s))
        || signals.iter().any(|s| !old.contains_key(s))
    {
        t.execute(
            "UPDATE cards SET updated_at = ?2 WHERE id = ?1",
            params![card.id, crate::db::now()],
        )?;
    }
    for (signal, active) in &old {
        if *active && !signals.contains(signal) {
            t.execute(
                "UPDATE card_trouble SET active = 0 WHERE card = ?1 AND signal = ?2",
                params![card.id, signal],
            )?;
            t.emit(
                EventDraft::new(
                    "card",
                    format!("{}: {signal} cleared", card.id),
                    json!({"change": "trouble_cleared", "signal": signal}),
                )
                .project(&card.project)
                .card(&card.id),
            )?;
        }
    }
    for signal in signals {
        if old.get(signal) == Some(&true) {
            continue;
        }
        t.execute(
            "INSERT INTO card_trouble(card, signal, active, first_at) VALUES (?1, ?2, 1, ?3)
            ON CONFLICT(card, signal) DO UPDATE SET active = 1",
            params![card.id, signal, crate::db::now()],
        )?;
        if !old.contains_key(signal) {
            let subject = format!("{}: trouble signal {signal}", card.id);
            let body = format!(
                "Card {} ({}) in {}: {signal}.\nWorking time: {} seconds. Usage: {} turns, {} tokens. Review rounds: {}. ETA extensions: {}. Short timer polls in the last hour: {}. Compaction requests in the last hour: {}.\nPlease check its state. This notice is sent once per card and signal.",
                card.id,
                card.title,
                card.column,
                f.seconds,
                totals.turns,
                totals.tokens(),
                f.reviews,
                f.eta_delays,
                f.polls,
                f.compactions
            );
            let agent = card.agent.as_deref().unwrap_or("");
            for to in crate::question_guard::leads(t, &card.project, agent)? {
                mail::send(
                    t,
                    Outgoing {
                        from: &Address::System,
                        to: &to,
                        subject: &subject,
                        body: &body,
                        thread: None,
                        in_reply_to: None,
                    },
                )?;
            }
            if let Ok(to @ Address::Agent { .. }) = mail::resolve(t, agent, Some(&card.project)) {
                mail::send(
                    t,
                    Outgoing {
                        from: &Address::System,
                        to: &to,
                        subject: &subject,
                        body: &format!(
                            "{body}\nReport your current state and next action on the card."
                        ),
                        thread: None,
                        in_reply_to: None,
                    },
                )?;
            }
        }
        t.emit(
            EventDraft::new(
                "card",
                format!("{}: {signal}", card.id),
                json!({"change": "trouble", "signal": signal}),
            )
            .project(&card.project)
            .card(&card.id),
        )?;
    }
    Ok(())
}

fn plan(c: &Connection, now: DateTime<Utc>) -> ApiResult<Vec<Change>> {
    let mut q = c.prepare("SELECT c.id, c.project, c.title, k.name, k.role,
        coalesce(c.assignee, (SELECT s.id FROM spawns s WHERE s.bound_kind = 'card'
            AND s.bound_id = c.id AND s.finished_at IS NULL LIMIT 1)), c.labels, c.eta_at, c.archived
        FROM cards c JOIN board_columns k ON k.id = c.column_id
        WHERE (c.archived = 0 AND (k.role IN ('working', 'review') OR k.name = 'research'))
        OR EXISTS (SELECT 1 FROM card_trouble m WHERE m.card = c.id AND m.active = 1)")?;
    let rows = q
        .query_map([], |r| {
            Ok(Watched {
                id: r.get(0)?,
                project: r.get(1)?,
                title: r.get(2)?,
                column: r.get(3)?,
                column_role: r.get(4)?,
                agent: r.get(5)?,
                labels: crate::store::json_col(&r.get::<_, String>(6)?),
                eta_at: r.get(7)?,
                archived: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut changes = Vec::new();
    let mut estimates = ReviewEstimates::default();
    for card in rows {
        let (signals, facts, totals) = desired(c, &card, now, &mut estimates)?;
        let old: BTreeSet<String> = active_signals(c, &card.id)?.into_iter().collect();
        if old != signals {
            changes.push(Change {
                card,
                signals,
                facts,
                totals,
            });
        }
    }
    Ok(changes)
}

fn revision(c: &Connection) -> ApiResult<(i64, i64)> {
    Ok(c.query_row(
        "SELECT (SELECT coalesce(max(seq), 0) FROM events),
        (SELECT coalesce(max(seq), 0) FROM audit)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

pub async fn pass(state: &Shared) {
    let planned = state
        .db
        .read(|c| Ok((revision(c)?, plan(c, Utc::now())?)))
        .await;
    let (expected, changes) = match planned {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("card trouble read failed: {e}");
            return;
        }
    };
    if changes.is_empty() {
        return;
    }
    if let Err(e) = state
        .db
        .write("system", move |t| {
            // A move, status, wait, reassignment or settings change won the
            // race with the read. Retry next tick, without replaying in a write.
            if revision(t)? != expected {
                return Ok(());
            }
            for change in changes {
                reconcile(t, &change)?;
            }
            Ok(())
        })
        .await
    {
        tracing::warn!("card trouble write failed: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }

    fn event(seconds: i64, kind: &str, agent: &str, data: Value) -> Event {
        Event {
            at: at(seconds),
            kind: kind.into(),
            agent: Some(agent.into()),
            card: (kind == "card").then(|| "P-1".into()),
            data,
        }
    }

    fn claim(seconds: i64, agent: &str) -> Event {
        event(
            seconds,
            "card",
            agent,
            json!({"change": "claimed", "assignee": agent, "column_role": "working"}),
        )
    }

    #[test]
    fn working_clock_excludes_waits_idle_and_other_assignees() {
        let e = vec![
            event(0, "status", "a", json!({"status": "working"})),
            claim(0, "a"),
            event(10, "wait", "a", json!({"change": "opened", "wait_id": "w"})),
            event(
                20,
                "wait",
                "b",
                json!({"change": "resolved", "wait_id": "w"}),
            ),
            event(
                30,
                "wait",
                "a",
                json!({"change": "resolved", "wait_id": "w"}),
            ),
            event(40, "status", "a", json!({"status": "idle"})),
            event(50, "status", "b", json!({"status": "working"})),
            claim(60, "b"),
            event(
                70,
                "card",
                "b",
                json!({"change": "moved", "to": "review", "to_role": "review", "column_role": "review"}),
            ),
        ];
        assert_eq!(facts("P-1", &e, at(100)).seconds, 30);
        assert_eq!(facts("P-1", &e, at(100)).reviews, 1);
    }

    #[test]
    fn short_timer_polls_and_compactions_expire_after_one_hour() {
        let mut e = vec![claim(0, "a")];
        for n in 0..3 {
            e.push(event(n * 20, "wait", "a", json!({"change": "opened", "kind": "time", "wait_id": n.to_string(), "due_at": at(n * 20 + 30).to_rfc3339()})));
            e.push(event(
                n * 20,
                "agent.reset",
                "a",
                json!({"kind": "compact"}),
            ));
        }
        e.push(event(
            70,
            "wait",
            "a",
            json!({"change": "opened", "kind": "time", "due_at": at(700).to_rfc3339()}),
        ));
        let f = facts("P-1", &e, at(100));
        assert_eq!((f.polls, f.compactions), (3, 3));
        let f = facts("P-1", &e, at(4000));
        assert_eq!((f.polls, f.compactions), (0, 0));
    }

    #[test]
    fn eta_delays_count_only_extensions_and_review_entries_count() {
        let mut e = vec![claim(0, "a")];
        for n in 1..5 {
            e.push(event(
                n,
                "card",
                "a",
                json!({"change": "eta", "eta_at": at(100 + n).to_rfc3339()}),
            ));
        }
        e.push(event(
            5,
            "card",
            "a",
            json!({"change": "eta", "eta_at": at(50).to_rfc3339()}),
        ));
        e.push(event(
            6,
            "card",
            "a",
            json!({"change": "moved", "to_role": "review"}),
        ));
        e.push(claim(7, "a"));
        e.push(event(
            8,
            "card",
            "a",
            json!({"change": "moved", "to_role": "review"}),
        ));
        let f = facts("P-1", &e, at(10));
        assert_eq!((f.eta_delays, f.reviews), (3, 2));
    }

    #[test]
    fn project_budgets_default_and_can_be_disabled() {
        let mut s: Settings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(time_limit("small", &s, None), 2700);
        assert_eq!(time_limit("impl", &s, None), 7200);
        assert_eq!(time_limit("impl-hard", &s, None), 14400);
        assert_eq!(time_limit("small", &s, Some(60000.0)), 240);
        assert!(s.tokens.is_empty() && s.turns.is_empty());
        s.time_seconds.insert("small".into(), 0);
        assert_eq!(time_limit("small", &s, Some(60000.0)), 0);
        assert!(serde_json::from_value::<Settings>(json!({"red_streak": -1})).is_err());
        assert!(serde_json::from_value::<Settings>(json!({"unknown": 1})).is_err());
    }

    #[test]
    fn job_set_etas_are_not_agent_extensions() {
        let e = vec![
            claim(0, "a"),
            event(
                1,
                "card",
                "a",
                json!({"change": "eta", "eta_at": at(100).to_rfc3339()}),
            ),
            event(
                2,
                "card",
                "a",
                json!({"change": "eta", "eta_at": at(200).to_rfc3339(), "job": true}),
            ),
        ];
        assert_eq!(facts("P-1", &e, at(10)).eta_delays, 0);
    }

    #[test]
    fn p70_requires_five_distinct_cards_and_uses_the_matching_type() {
        use crate::store::card_history::{WorkSample, calibrated_ms};
        let samples: Vec<_> = (1..=5)
            .map(|n| WorkSample {
                card: format!("P-{n}"),
                start_seq: n * 2,
                end_seq: n * 2 + 1,
                actual_ms: n as f64 * 1000.0,
                guess_ms: 10000.0,
                labels: vec!["type:small".into()],
            })
            .collect();
        let labels = vec!["type:small".into()];
        let s = Settings::default();
        assert_eq!(
            time_limit("small", &s, calibrated_ms(&samples[..4], &labels, None)),
            2700
        );
        assert_eq!(
            time_limit("small", &s, calibrated_ms(&samples, &labels, None)),
            16
        );
        assert_eq!(
            time_limit(
                "impl",
                &s,
                calibrated_ms(&samples, &["type:impl".into()], None)
            ),
            7200
        );
    }

    #[test]
    fn validation_uses_same_suite_worker_and_red_streak() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE test_runs(pk INTEGER PRIMARY KEY, name TEXT, commit_sha TEXT, failed INTEGER, errored INTEGER, created_by TEXT);
            CREATE TABLE test_run_cards(run INTEGER, card TEXT);").unwrap();
        let add = |id, suite, sha, red, who| {
            c.execute(
                "INSERT INTO test_runs VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                params![id, suite, sha, red, who],
            )
            .unwrap();
            c.execute("INSERT INTO test_run_cards VALUES (?1, 'P-1')", [id])
                .unwrap();
        };
        let s = Settings::default();
        add(1, "check", "a", 1, "worker");
        assert!(!validation_red(&c, "P-1", "worker", &s).unwrap());
        add(2, "check", "a", 1, "other");
        add(3, "explore", "a", 1, "worker");
        assert!(!validation_red(&c, "P-1", "worker", &s).unwrap());
        add(4, "check", "a", 1, "worker");
        assert!(validation_red(&c, "P-1", "worker", &s).unwrap());
        add(5, "check", "b", 0, "worker");
        assert!(!validation_red(&c, "P-1", "worker", &s).unwrap());
        add(6, "check", "c", 1, "worker");
        add(7, "check", "d", 1, "worker");
        assert!(!validation_red(&c, "P-1", "worker", &s).unwrap());
        add(8, "check", "e", 1, "worker");
        assert!(validation_red(&c, "P-1", "worker", &s).unwrap());
        assert!(!validation_red(&c, "P-2", "worker", &s).unwrap());
    }
}
