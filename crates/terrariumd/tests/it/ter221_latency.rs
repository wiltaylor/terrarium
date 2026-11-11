//! TER-221 probes retained for TER-233: the latency of the board rule
//! engine from an event to the end of its rule run, idle and under load,
//! and the per-card order of the outbox under concurrent writers.
//!
//! Run: `cargo test -p terrariumd --test it ter221_latency:: -- --nocapture --test-threads=1`
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::events::EventDraft;

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn emit(st: &Shared, kind: &str, project: &str, card: &str, n: i64) -> i64 {
    let (kind, project, card) = (kind.to_owned(), project.to_owned(), card.to_owned());
    st.db
        .write("source:test/ping", move |t| {
            Ok(t.emit(
                EventDraft::new(kind, format!("ping {n}"), json!({"n": n}))
                    .project(project)
                    .card(card),
            )?
            .seq)
        })
        .await
        .unwrap()
}

async fn put_rules(td: &TestDaemon, project: &str, text: &str) {
    td.put(
        &format!("/api/projects/{project}/rules"),
        json!({"text": text}),
    )
    .await
    .ok();
}

async fn new_cards(td: &TestDaemon, project: &str, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..n {
        let v = td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": format!("c{i}")}),
            )
            .await
            .ok()
            .clone();
        out.push(v["id"].as_str().unwrap().to_owned());
    }
    out
}

/// Print the engine state, the rules, the event `seq`, and the run rows.
#[allow(clippy::type_complexity)] // Diagnostic SQL rows, printed only on timeout.
async fn dump(st: &Shared, seq: i64) {
    let out = st
        .db
        .read(move |c| {
            let cursor = terrariumd::rules_engine::cursor(c)?;
            let last = terrariumd::events::last_seq(c)?;
            let rules: Vec<(String, String)> = c
                .prepare("SELECT project, updated_at FROM project_rules")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            let ev: Vec<(
                i64,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
            )> = c
                .prepare(
                    "SELECT seq, kind, project, card, actor, at FROM events
                      WHERE seq >= ?1 - 3 ORDER BY seq LIMIT 8",
                )?
                .query_map([seq], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            let runs: Vec<(i64, String, String, String, i64, Option<String>, String)> = c
                .prepare(
                    "SELECT event_seq, rule, lane, state, step, reason, updated_at FROM rule_runs
                      ORDER BY event_seq DESC LIMIT 15",
                )?
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            Ok(format!(
                "cursor={cursor:?} last={last}\nrules={rules:?}\nevents={ev:#?}\nruns={runs:#?}"
            ))
        })
        .await
        .unwrap();
    eprintln!("TER221 DUMP seq={seq} now={}\n{out}", chrono::Utc::now());
}

/// How long the engine may make no progress at all before [`settle`]
/// fails. Under load (a merge job at load average 50) the order test
/// needs minutes to drain its 800 runs: each action is one serialized
/// write, and each comment is a new event. A fixed deadline from the start
/// failed there while the engine still made progress (TER-480), so the
/// deadline only counts time without progress.
const STALL: Duration = Duration::from_secs(120);

/// Wait until the engine read the whole log and ran every pending row.
/// Fail only when neither the cursor nor the count of ended runs moved
/// for [`STALL`]: a stuck engine, not a slow one.
async fn settle(st: &Shared) {
    let mut quiet = 0;
    let start = Instant::now();
    let mut seen = (i64::MIN, i64::MIN);
    let mut moved = Instant::now();
    loop {
        let (cursor, last, pending, ended) = st
            .db
            .read(|c| {
                let cursor = terrariumd::rules_engine::cursor(c)?.unwrap_or(0);
                let last = terrariumd::events::last_seq(c)?;
                let (pending, ended): (i64, i64) = c.query_row(
                    "SELECT count(*) FILTER (WHERE state = 'pending'),
                            count(*) FILTER (WHERE state != 'pending')
                       FROM rule_runs",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                Ok((cursor, last, pending, ended))
            })
            .await
            .unwrap();
        if cursor >= last && pending == 0 {
            quiet += 1;
            if quiet >= 3 {
                if start.elapsed() >= Duration::from_secs(10) {
                    eprintln!(
                        "TER221 settle took {:.1}s (ended runs={ended})",
                        start.elapsed().as_secs_f64()
                    );
                }
                return;
            }
        } else {
            quiet = 0;
        }
        if (cursor, ended) != seen {
            seen = (cursor, ended);
            moved = Instant::now();
        }
        if moved.elapsed() >= STALL {
            dump(st, last).await;
            panic!(
                "no settle: no progress for {}s (after {:.0}s): cursor={cursor} last={last} pending={pending} ended={ended}",
                STALL.as_secs(),
                start.elapsed().as_secs_f64()
            );
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

const LAT: &str = r#"rule "lat" {
  on = ["test.ping"]
  max_per_hour = 600
  card_comment { text = "lat {{event.data.n}}" }
}

rule "noise" {
  on = ["test.noise"]
  max_per_hour = 600
  card_comment { text = "noise {{event.data.n}}" }
}
"#;

/// The end of the run of rule `rule` for event `seq`: (state, plan lag
/// ms, run lag ms) from the stored stamps, or None while pending.
async fn run_of(st: &Shared, seq: i64) -> Option<(String, i64, i64)> {
    st.db
        .read(move |c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT r.state, r.created_at, r.updated_at, e.at
                   FROM rule_runs r JOIN events e ON e.seq = r.event_seq
                  WHERE r.event_seq = ?1 AND r.rule = 'lat'",
                [seq],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?)
        })
        .await
        .unwrap()
        .filter(|(s, ..)| s != "pending")
        .map(|(s, created, updated, at)| {
            let p = |x: &str| chrono::DateTime::parse_from_rfc3339(x).unwrap();
            let plan = (p(&created) - p(&at)).num_milliseconds();
            let run = (p(&updated) - p(&created)).num_milliseconds();
            (s, plan, run)
        })
}

fn report(name: &str, wall: &mut [f64], plan: &mut [f64], run: &mut [f64]) {
    let pct = |v: &mut [f64], q: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let i = ((v.len() as f64 - 1.0) * q).round() as usize;
        v[i]
    };
    let line = |label: &str, v: &mut [f64]| {
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        format!(
            "{label}: n={} mean={mean:.1} p50={:.1} p90={:.1} p99={:.1} max={:.1}",
            v.len(),
            pct(v, 0.5),
            pct(v, 0.9),
            pct(v, 0.99),
            pct(v, 1.0)
        )
    };
    eprintln!("TER221 {name} {}", line("wall_ms", wall));
    eprintln!(
        "TER221 {name} {}",
        line("plan_ms(event.at->run.created)", plan)
    );
    eprintln!(
        "TER221 {name} {}",
        line("run_ms(run.created->run.end)", run)
    );
}

/// `n` pings of rule `lat` on the cards (at most 10 per card: the card
/// rate limit), one at a time, each waited for. A jitter between pings
/// keeps them off the engine's tick.
async fn measure(name: &str, st: &Shared, cards: &[String], n: usize) {
    let (mut wall, mut plan, mut run) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..n {
        let card = &cards[i % cards.len()];
        let t0 = Instant::now();
        let seq = emit(st, "test.ping", "lat", card, i as i64).await;
        let (s, p, r) = loop {
            if let Some(x) = run_of(st, seq).await {
                break x;
            }
            if t0.elapsed() >= Duration::from_secs(60) {
                dump(st, seq).await;
                panic!("run {seq} never ended");
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        assert_eq!(s, "done", "event {seq}");
        wall.push(t0.elapsed().as_secs_f64() * 1000.0);
        plan.push(p as f64);
        run.push(r as f64);
        tokio::time::sleep(Duration::from_millis(7 + (i as u64 * 13) % 41)).await;
    }
    report(name, &mut wall, &mut plan, &mut run);
}

async fn lab() -> (TestDaemon, Vec<String>, Vec<String>) {
    let td = TestDaemon::start().await;
    td.project("lat").await;
    td.project("noise").await;
    put_rules(&td, "lat", LAT).await;
    put_rules(&td, "noise", LAT).await;
    let cards = new_cards(&td, "lat", 10).await;
    let noise = new_cards(&td, "noise", 60).await;
    settle(&state(&td)).await;
    (td, cards, noise)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual latency measurement on a named target"]
async fn latency_idle() {
    let (td, cards, _) = lab().await;
    measure("idle", &state(&td), &cards, 80).await;
}

/// Event load: 8 writers emit `test.noise` events (a matching rule in
/// project `noise`, so the engine plans and runs them too), plus 2 writers
/// of events that match no rule, as fast as the writer lock allows; and
/// CPU hogs on every core.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual load measurement: saturates all available CPUs"]
async fn latency_under_load() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("terrariumd::db_timing=debug")
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
    let (td, cards, noise) = lab().await;
    let st = state(&td);
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for w in 0..10usize {
        let (st, stop, count, noise) = (st.clone(), stop.clone(), count.clone(), noise.clone());
        tasks.push(tokio::spawn(async move {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let kind = if w < 8 { "test.noise" } else { "test.other" };
                let card = &noise[(w * 7 + i) % noise.len()];
                emit(&st, kind, "noise", card, i as i64).await;
                count.fetch_add(1, Ordering::Relaxed);
                i += 1;
                tokio::task::yield_now().await;
            }
        }));
    }
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let hogs: Vec<_> = (0..cpus)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t0 = Instant::now();
    measure(&format!("load(cpus={cpus})"), &st, &cards, 80).await;
    let secs = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    for t in tasks {
        t.await.unwrap();
    }
    for h in hogs {
        h.join().unwrap();
    }
    let backlog = st
        .db
        .read(|c| {
            let cursor = terrariumd::rules_engine::cursor(c)?.unwrap_or(0);
            let last = terrariumd::events::last_seq(c)?;
            let states: Vec<(String, i64)> = c
                .prepare("SELECT state, count(*) FROM rule_runs GROUP BY state")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            Ok((last - cursor, states))
        })
        .await
        .unwrap();
    eprintln!(
        "TER221 load noise_events={} rate={:.0}/s backlog_at_end={} runs={:?}",
        count.load(Ordering::Relaxed),
        count.load(Ordering::Relaxed) as f64 / secs,
        backlog.0,
        backlog.1
    );
    settle(&st).await;
}

const ORDER: &str = r#"rule "first" {
  on = ["test.ping"]
  max_per_hour = 600
  card_comment { text = "a {{event.data.n}}" }
  card_comment { text = "b {{event.data.n}}" }
}

rule "second" {
  on = ["test.ping"]
  max_per_hour = 600
  card_comment { text = "c {{event.data.n}}" }
}
"#;

/// 40 cards, 10 events each, from 4 concurrent writers per card. Each card
/// must show, in event `seq` order, `a n`, `b n`, `c n` for each event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbox_keeps_the_per_card_order() {
    let td = TestDaemon::start().await;
    td.project("ord").await;
    put_rules(&td, "ord", ORDER).await;
    let cards = new_cards(&td, "ord", 40).await;
    let st = state(&td);
    settle(&st).await;
    let mut tasks = Vec::new();
    for (ci, card) in cards.iter().enumerate() {
        for w in 0..4usize {
            let (st, card) = (st.clone(), card.clone());
            // 10 events per card: writers 0 and 1 write 3, writers 2 and 3 write 2.
            let k = if w < 2 { 3 } else { 2 };
            tasks.push(tokio::spawn(async move {
                let mut seqs = Vec::new();
                for j in 0..k {
                    let n = (ci * 100 + w * 10 + j) as i64;
                    seqs.push((emit(&st, "test.ping", "ord", &card, n).await, n));
                    tokio::task::yield_now().await;
                }
                (card, seqs)
            }));
        }
    }
    let mut by_card: std::collections::HashMap<String, Vec<(i64, i64)>> = Default::default();
    for t in tasks {
        let (card, seqs) = t.await.unwrap();
        by_card.entry(card).or_default().extend(seqs);
    }
    settle(&st).await;
    let mut bad = 0;
    let mut runs_done = 0;
    for (card, mut seqs) in by_card {
        seqs.sort();
        let want: Vec<String> = seqs
            .iter()
            .flat_map(|(_, n)| [format!("a `{n}`"), format!("b `{n}`"), format!("c `{n}`")])
            .collect();
        let got: Vec<String> = td.get(&format!("/api/cards/{card}")).await.ok()["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c: &Value| c["body"].as_str().unwrap().to_owned())
            .collect();
        runs_done += got.len();
        if got != want {
            bad += 1;
            eprintln!("TER221 order card {card}\n want {want:?}\n got  {got:?}");
        }
    }
    let states: Vec<(String, i64)> = st
        .db
        .read(|c| {
            Ok(
                c.prepare("SELECT state, count(*) FROM rule_runs GROUP BY state")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?,
            )
        })
        .await
        .unwrap();
    eprintln!(
        "TER221 order cards=40 events=400 comments={runs_done} bad_cards={bad} runs={states:?}"
    );
    assert_eq!(bad, 0);
}
