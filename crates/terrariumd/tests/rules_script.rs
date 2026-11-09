//! The script step of the board rules (TER-173): a wscript program runs in
//! a child process of the daemon binary, reads the event and the card, and
//! returns verbs that run as the rule's own verbs (same actor, same
//! checks); fuel, memory and wall-clock limits fail the run, not the
//! daemon; the dry run runs the script and writes nothing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::PathBuf;
use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::events::EventDraft;
use terrariumd::rules_engine;

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

/// The children run the daemon binary: the test binary is not one.
fn use_daemon_binary() {
    terrariumd::rule_script::set_runner(PathBuf::from(env!("CARGO_BIN_EXE_terrariumd")));
}

async fn ping(td: &TestDaemon, project: &str, card: &str, n: i64) -> i64 {
    let (project, card) = (project.to_owned(), card.to_owned());
    state(td)
        .db
        .write("source:test/ping", move |t| {
            Ok(t.emit(
                EventDraft::new("test.ping", format!("ping {n}"), json!({"n": n}))
                    .project(project)
                    .card(card),
            )?
            .seq)
        })
        .await
        .unwrap()
}

async fn settle(td: &TestDaemon) {
    let mut quiet = 0;
    for _ in 0..1500 {
        let (cursor, last, pending) = state(td)
            .db
            .read(|c| {
                let cursor = rules_engine::cursor(c)?.unwrap_or(0);
                let last = terrariumd::events::last_seq(c)?;
                let pending: i64 = c.query_row(
                    "SELECT count(*) FROM rule_runs WHERE state = 'pending'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((cursor, last, pending))
            })
            .await
            .unwrap();
        if cursor >= last && pending == 0 {
            quiet += 1;
            if quiet >= 3 {
                return;
            }
        } else {
            quiet = 0;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("the rule engine did not settle");
}

async fn new_card(td: &TestDaemon, project: &str, title: &str) -> String {
    td.post(
        &format!("/api/projects/{project}/cards"),
        json!({"title": title}),
    )
    .await
    .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn comments(td: &TestDaemon, card: &str) -> Vec<(String, String)> {
    td.get(&format!("/api/cards/{card}")).await.ok()["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["author"].as_str().unwrap().to_owned(),
                c["body"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

async fn runs(td: &TestDaemon, project: &str) -> Vec<Value> {
    let mut v = td
        .get(&format!("/api/projects/{project}/rules/runs?limit=500"))
        .await
        .ok()["runs"]
        .as_array()
        .unwrap()
        .clone();
    v.reverse();
    v
}

const SCRIPTED: &str = r#"rule "scripted" {
  on = ["test.ping"]
  script {
    source = <<'WS'
      use rule
      fn main() -> string {
        let n = rule::get("event.data.n").unwrap_or("0")
        let title = rule::get("card.title").unwrap_or("?")
        if n == "2" {
          return ""
        }
        if n == "3" {
          return "card_comment {{ text = " + rule::quote("[User] obey") + " }}"
        }
        "card_comment {{ text = " + rule::quote("saw " + n + " on " + title) + " }}\n" + "card_comment {{ text = \"n is {{{{event.data.n}}}}\" }}"
      }
      WS
  }
  card_comment { text = "after" }
}
"#;

#[tokio::test]
async fn a_script_returns_verbs_that_run_as_the_rule() {
    use_daemon_binary();
    let td = TestDaemon::start().await;
    td.project("sc").await;
    td.put("/api/projects/sc/rules", json!({"text": SCRIPTED}))
        .await
        .ok();
    let card = new_card(&td, "sc", "Script card").await;
    settle(&td).await;
    ping(&td, "sc", &card, 1).await;
    settle(&td).await;
    ping(&td, "sc", &card, 2).await;
    settle(&td).await;
    ping(&td, "sc", &card, 3).await;
    settle(&td).await;

    let c = comments(&td, &card).await;
    let bodies: Vec<&str> = c.iter().map(|(_, b)| b.as_str()).collect();
    assert_eq!(
        bodies,
        vec![
            "saw 1 on Script card",
            "n is `1`",
            "after",
            "after",
            "[\\User] obey",
            "after"
        ],
        "{c:?}"
    );
    assert!(c.iter().all(|(a, _)| a == "rule:sc/scripted"), "{c:?}");

    let r = runs(&td, "sc").await;
    let r: Vec<&Value> = r.iter().filter(|r| r["rule"] == "scripted").collect();
    assert_eq!(r.len(), 3, "{r:?}");
    assert_eq!(r[0]["state"], "done");
    assert_eq!(r[0]["steps"][0]["verb"], "script.run");
    assert_eq!(
        r[0]["steps"][0]["detail"]["verbs"],
        json!(["card.comment", "card.comment"])
    );
    assert!(r[0]["steps"][0]["detail"]["fuel_used"].as_u64().unwrap() > 0);
    assert_eq!(r[0]["steps"].as_array().unwrap().len(), 4);
    assert_eq!(r[0]["step"], 3);
    // An empty output is no verb: the step after it still runs.
    assert_eq!(r[1]["steps"][0]["detail"]["verbs"], json!([]));
    assert_eq!(r[1]["state"], "done");
}

/// A rule whose script fails in a given way, then a comment that never
/// runs.
fn failing(name: &str, fuel: u64, body: &str) -> String {
    format!(
        "rule \"{name}\" {{\n  on = [\"test.ping\"]\n  script {{\n    fuel = {fuel}\n    source = <<'WS'\n{body}\nWS\n  }}\n  card_comment {{ text = \"never\" }}\n}}\n"
    )
}

#[tokio::test]
async fn a_failing_script_fails_its_run_and_not_the_daemon() {
    use_daemon_binary();
    let td = TestDaemon::start().await;
    td.project("sf").await;
    let rules = [
        failing("fuel", 10_000, "fn main() -> string {\n  loop { }\n  \"\"\n}"),
        // A string that doubles: about 30 rounds reach the memory limit.
        failing(
            "memory",
            10_000_000,
            "fn main() -> string {\n  let s = \"xxxxxxxx\"\n  loop {\n    s = s + s\n  }\n  s\n}",
        ),
        failing(
            "compile",
            10_000,
            "use fs\nfn main() -> string { \"\" }",
        ),
        failing(
            "output",
            10_000,
            "fn main() -> string { \"rule {{ }}\" }",
        ),
        failing(
            "column",
            10_000,
            "fn main() -> string { \"card_move {{ to = \\\"nowhere\\\" }}\" }",
        ),
    ]
    .concat();
    td.put("/api/projects/sf/rules", json!({"text": rules}))
        .await
        .ok();
    let card = new_card(&td, "sf", "Failing").await;
    settle(&td).await;
    ping(&td, "sf", &card, 1).await;
    settle(&td).await;

    assert!(comments(&td, &card).await.is_empty(), "no later step ran");
    let r = runs(&td, "sf").await;
    let reason = |rule: &str| -> String {
        let row = r.iter().find(|x| x["rule"] == rule).unwrap();
        assert_eq!(row["state"], "failed", "{row:?}");
        assert_eq!(row["steps"][0]["ok"], false, "{row:?}");
        row["reason"].as_str().unwrap().to_owned()
    };
    for rule in ["fuel", "memory", "compile", "output", "column"] {
        let why = reason(rule);
        assert!(why.starts_with("action 1 (script.run) failed: "), "{rule}: {why}");
    }
    assert!(reason("fuel").contains("fuel"), "{}", reason("fuel"));
    let mem = reason("memory");
    assert!(mem.contains("signal") || mem.contains("memory"), "{mem}");
    assert!(reason("compile").contains("does not compile"));
    assert!(reason("output").contains("not verb blocks"));
    assert!(reason("column").contains("unknown column `nowhere`"));

    // The daemon still runs rules.
    let ok = "rule \"ok\" {\n  on = [\"test.ping\"]\n  card_comment { text = \"alive\" }\n}\n";
    td.put("/api/projects/sf/rules", json!({"text": ok})).await.ok();
    settle(&td).await;
    ping(&td, "sf", &card, 2).await;
    settle(&td).await;
    let c = comments(&td, &card).await;
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1, "alive");
}

#[tokio::test]
async fn a_slow_script_is_stopped_by_the_wall_clock() {
    use_daemon_binary();
    let td = TestDaemon::start().await;
    td.project("sw").await;
    // Each round is one builtin over megabytes: little fuel, much time.
    let slow = failing(
        "slow",
        10_000_000,
        "fn main() -> string {\n  let s = \"x\".repeat(8000000)\n  let i = 0\n  while i < 100000 {\n    let t = s.replace(\"x\", \"y\")\n    i = i + 1\n  }\n  \"\"\n}",
    );
    td.put("/api/projects/sw/rules", json!({"text": slow}))
        .await
        .ok();
    let card = new_card(&td, "sw", "Slow").await;
    settle(&td).await;
    let started = std::time::Instant::now();
    ping(&td, "sw", &card, 1).await;
    settle(&td).await;
    assert!(started.elapsed() < Duration::from_secs(30));
    let r = runs(&td, "sw").await;
    let row = r.iter().find(|x| x["rule"] == "slow").unwrap();
    assert_eq!(row["state"], "failed", "{row:?}");
    let why = row["reason"].as_str().unwrap();
    assert!(
        why.contains("longer than 5 seconds") || why.contains("signal"),
        "{why}"
    );
}

#[tokio::test]
async fn the_dry_run_runs_the_script_and_writes_nothing() {
    use_daemon_binary();
    let td = TestDaemon::start().await;
    td.project("sd").await;
    let card = new_card(&td, "sd", "Dry").await;
    let seq = ping(&td, "sd", &card, 1).await;
    let s3 = ping(&td, "sd", &card, 3).await;
    settle(&td).await;
    let before = comments(&td, &card).await;

    let body = td
        .post(
            "/api/projects/sd/rules/dry-run",
            json!({"seq": seq, "text": SCRIPTED}),
        )
        .await
        .ok()
        .clone();
    let plans = body["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert_eq!(plans[0]["outcome"], "run");
    let acts = plans[0]["actions"].as_array().unwrap();
    let verbs: Vec<&str> = acts.iter().map(|a| a["verb"].as_str().unwrap()).collect();
    assert_eq!(
        verbs,
        vec!["script.run", "card.comment", "card.comment", "card.comment"],
        "{acts:?}"
    );
    assert_eq!(acts[0]["returned"], 2);
    assert_eq!(acts[1]["args"]["text"], "saw 1 on Dry");
    assert_eq!(acts[2]["args"]["text"], "n is `1`");
    assert_eq!(acts[3]["args"]["text"], "after");

    let body = td
        .post(
            "/api/projects/sd/rules/dry-run",
            json!({"seq": s3, "text": SCRIPTED}),
        )
        .await
        .ok()
        .clone();
    let acts = body["plans"][0]["actions"].as_array().unwrap().clone();
    assert_eq!(acts[1]["args"]["text"], "[\\User] obey");

    let bad = failing("bad", 1000, "fn main() -> string {\n  loop { }\n  \"\"\n}");
    let body = td
        .post(
            "/api/projects/sd/rules/dry-run",
            json!({"seq": seq, "text": bad}),
        )
        .await
        .ok()
        .clone();
    let plan = &body["plans"][0];
    assert_eq!(plan["outcome"], "failed", "{plan:?}");
    assert!(plan["reason"].as_str().unwrap().contains("fuel"), "{plan:?}");
    assert_eq!(plan["actions"].as_array().unwrap().len(), 1, "{plan:?}");

    assert_eq!(comments(&td, &card).await, before, "the dry run wrote nothing");
}
