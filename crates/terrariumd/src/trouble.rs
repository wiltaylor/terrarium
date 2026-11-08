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
use crate::store::{board, mail, usage};
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

/// Hook for the calibrated p70 history (TER-316). Until history is
/// available, these are the approved fixed *budget* fallbacks, not ETAs.
fn time_limit(kind: &str, s: &Settings) -> u64 {
    s.time_seconds.get(kind).copied().unwrap_or(match kind {
        "small" => 45 * 60,
        "impl-hard" | "hard" => 4 * 60 * 60,
        _ => 2 * 60 * 60,
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
    let mut q = c.prepare(
        "SELECT e.at, e.kind, e.agent_id, e.card,
          CASE WHEN e.kind = 'wait' THEN json_set(e.payload, '$.due_at', w.due_at) ELSE e.payload END
        FROM events e LEFT JOIN waits w ON e.kind = 'wait' AND w.id = json_extract(e.payload, '$.wait_id')
        WHERE e.card = ?1 OR (e.kind IN ('status', 'wait', 'agent.reset') AND e.agent_id IN
          (SELECT agent_id FROM events WHERE card = ?1 AND agent_id IS NOT NULL))
        ORDER BY e.seq",
    )?;
    let rows = q
        .query_map([card], |r| {
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
        ORDER BY r.pk DESC",
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

fn desired(c: &Connection, card: &board::Card, now: DateTime<Utc>) -> ApiResult<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    if card.archived {
        return Ok(out);
    }
    if !matches!(card.column_role.as_deref(), Some("working" | "review"))
        && card.column != "research"
    {
        return Ok(out);
    }
    let s = settings(c, &card.project)?;
    let f = facts(&card.id, &history(c, &card.id)?, now);
    let kind = card
        .labels
        .iter()
        .find_map(|l| l.strip_prefix("type:"))
        .unwrap_or("impl");
    let limit = time_limit(kind, &s);
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
    if let Some(a) = card.assignee.as_ref().or(card.agent.as_ref())
        && validation_red(c, &card.id, a, &s)?
    {
        out.insert("validation_red".into());
    }
    Ok(out)
}

fn reconcile(t: &mut Tx<'_>, card: &board::Card, signals: &BTreeSet<String>) -> ApiResult<()> {
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
            let f = facts(&card.id, &history(t, &card.id)?, Utc::now());
            let totals = usage::totals(
                t,
                &usage::UsageFilter {
                    card: Some(card.id.clone()),
                    ..Default::default()
                },
            )?;
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
            let agent = card
                .assignee
                .as_deref()
                .or(card.agent.as_deref())
                .unwrap_or("");
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

pub async fn pass(state: &Shared) {
    if let Err(e) = state
        .db
        .write("system", |t| {
            let mut q = t.prepare(
                "SELECT c.id FROM cards c JOIN board_columns k ON k.id = c.column_id
            WHERE (c.archived = 0 AND (k.role IN ('working', 'review') OR k.name = 'research'))
            OR EXISTS (SELECT 1 FROM card_trouble m WHERE m.card = c.id AND m.active = 1)",
            )?;
            let ids = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(q);
            let now = Utc::now();
            for id in ids {
                let card = board::get_card(t, &id)?;
                let signals = desired(t, &card, now)?;
                reconcile(t, &card, &signals)?;
            }
            Ok(())
        })
        .await
    {
        tracing::warn!("card trouble pass failed: {e}");
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
        assert_eq!(time_limit("small", &s), 2700);
        assert_eq!(time_limit("impl", &s), 7200);
        assert_eq!(time_limit("impl-hard", &s), 14400);
        assert!(s.tokens.is_empty() && s.turns.is_empty());
        s.time_seconds.insert("small".into(), 0);
        assert_eq!(time_limit("small", &s), 0);
        assert!(serde_json::from_value::<Settings>(json!({"red_streak": -1})).is_err());
        assert!(serde_json::from_value::<Settings>(json!({"unknown": 1})).is_err());
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
