//! The structured rules of a board column (TER-1120, kanban rework C1b):
//! the rules in the column options, with stable IDs and order, the same
//! versioning, checks, audit, and rights as the options, the catalog, and
//! no change to the legacy rules.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.post(
        "/api/projects",
        json!({"name": "proj", "card_prefix": "TER"}),
    )
    .await
    .ok();
    td.agent("proj", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("proj", "w1", json!({})).await;
    td.project("ops").await;
    td.agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    td.project("other").await;
    td.agent("other", "boss", json!({"roles": ["project_admin"]}))
        .await;
    td
}

fn path(column: &str) -> String {
    format!("/api/projects/proj/board/columns/{column}/options")
}

async fn options(td: &TestDaemon, column: &str) -> Value {
    td.get(&path(column)).await.ok().clone()
}

/// The `PUT` body of `o` with the rule list `rules`.
fn with_rules(o: &Value, rules: Value) -> Value {
    json!({"base_version": o["version"], "checks": o["checks"], "queue": o["queue"],
           "rules": rules})
}

async fn count(td: &TestDaemon, path: &str) -> usize {
    td.get(path).await.ok().as_array().unwrap().len()
}

fn mail_rule(name: &str) -> Value {
    json!({"name": name, "trigger": {"kind": "lands"},
           "actions": [{"verb": "mail", "args": {"to": ["lead"], "subject": "{{card.id}} landed"}}]})
}

fn merge_rule() -> Value {
    json!({"name": "merge-pass", "description": "a passed merge moves the card on",
           "trigger": {"kind": "job_result", "events": ["ended"], "job_kinds": ["merge"]},
           "if_job": [{"state": ["passed"]}],
           "actions": [{"verb": "card.move", "args": {"to": "@accepted"}}]})
}

fn names(o: &Value) -> Vec<String> {
    o["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn rules_round_trip_with_stable_ids_and_order() {
    let td = setup().await;
    let o = options(&td, "review").await;
    assert_eq!(o["rules"], json!([]));
    let r = td
        .put(
            &path("review"),
            with_rules(&o, json!([merge_rule(), mail_rule("note")])),
        )
        .await
        .ok()
        .clone();
    assert_eq!(r["version"], 2);
    assert_eq!(names(&r), ["merge-pass", "note"]);
    let ids: Vec<String> = r["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    // The GET gives the same rules, with the defaults filled in.
    let got = options(&td, "review").await;
    assert_eq!(got["rules"], r["rules"]);
    assert_eq!(got["rules"][1]["enabled"], true);
    assert_eq!(
        got["rules"][0]["trigger"],
        json!({"kind": "job_result", "events": ["ended"], "job_kinds": ["merge"]})
    );
    // Reorder, rename, and turn one off: the IDs stay.
    let mut a = got["rules"][0].clone();
    let mut b = got["rules"][1].clone();
    b["name"] = "renamed".into();
    a["enabled"] = false.into();
    let r = td
        .put(&path("review"), with_rules(&got, json!([b, a])))
        .await
        .ok()
        .clone();
    assert_eq!(r["version"], 3);
    assert_eq!(names(&r), ["renamed", "merge-pass"]);
    assert_eq!(r["rules"][0]["id"], ids[1].as_str());
    assert_eq!(r["rules"][1]["id"], ids[0].as_str());
    assert_eq!(r["rules"][1]["enabled"], false);
    // A PUT without `rules` (the C1a body) keeps them.
    let mut keep = with_rules(&r, Value::Null);
    keep.as_object_mut().unwrap().remove("rules");
    keep["checks"]["require_comment_on_enter"] = true.into();
    let k = td.put(&path("review"), keep).await.ok().clone();
    assert_eq!(k["version"], 4);
    assert_eq!(k["rules"], r["rules"]);
    // The same name on another column is another rule.
    let v = options(&td, "validating").await;
    let other = td
        .put(
            &path("validating"),
            with_rules(&v, json!([mail_rule("note")])),
        )
        .await
        .ok()
        .clone();
    assert_ne!(other["rules"][0]["id"], ids[1].as_str());
    // Remove one: the other stays with its ID.
    let k = options(&td, "review").await;
    let left = json!([k["rules"][1].clone()]);
    let r = td
        .put(&path("review"), with_rules(&k, left))
        .await
        .ok()
        .clone();
    assert_eq!(names(&r), ["merge-pass"]);
    assert_eq!(r["rules"][0]["id"], ids[0].as_str());
    assert_eq!(options(&td, "validating").await["rules"][0]["name"], "note");
}

#[tokio::test]
async fn a_rule_write_is_versioned_audited_and_stale_writes_nothing() {
    let td = setup().await;
    let events = "/api/events?kind=board&project=proj";
    let audits = "/api/audit?action=board.column_options";
    let (e0, a0) = (count(&td, events).await, count(&td, audits).await);
    let o = options(&td, "review").await;
    let r = td
        .put(&path("review"), with_rules(&o, json!([merge_rule()])))
        .await
        .ok()
        .clone();
    assert_eq!(r["version"], 2);
    assert_eq!(count(&td, events).await, e0 + 1);
    assert_eq!(count(&td, audits).await, a0 + 1);
    let audit = td.get(audits).await.ok().clone();
    let row = audit.as_array().unwrap().last().unwrap().to_string();
    assert!(
        row.contains("\"before\"") && row.contains("merge-pass"),
        "{row}"
    );
    let ev = td.get(events).await.ok().clone();
    let last = ev.as_array().unwrap().last().unwrap().to_string();
    assert!(last.contains("\"rule_count\":1"), "{last}");
    // The old base is stale: 409, and nothing changes.
    let stale = with_rules(&o, json!([mail_rule("late")]));
    let s = td.put(&path("review"), stale).await;
    s.err(409, "stale_base");
    assert_eq!(s.body["error"]["details"]["version"], 2);
    let now = options(&td, "review").await;
    assert_eq!(now["version"], 2);
    assert_eq!(names(&now), ["merge-pass"]);
    assert_eq!(count(&td, events).await, e0 + 1);
    assert_eq!(count(&td, audits).await, a0 + 1);
    // The same rules again change nothing: the version stays, no audit.
    let same = with_rules(&now, now["rules"].clone());
    assert_eq!(td.put(&path("review"), same).await.ok()["version"], 2);
    assert_eq!(count(&td, audits).await, a0 + 1);
    assert_eq!(count(&td, events).await, e0 + 1);
}

#[tokio::test]
async fn bad_rules_are_refused_with_the_path_and_nothing_is_written() {
    let td = setup().await;
    let o = options(&td, "review").await;
    let first = td
        .put(&path("review"), with_rules(&o, json!([merge_rule()])))
        .await
        .ok()
        .clone();
    let id = first["rules"][0]["id"].clone();
    let v = options(&td, "validating").await;
    let vr = td
        .put(&path("validating"), with_rules(&v, json!([mail_rule("v")])))
        .await
        .ok()
        .clone();
    let foreign = vr["rules"][0]["id"].clone();
    let mut bad_column = merge_rule();
    bad_column["actions"][0]["args"]["to"] = "nowhere".into();
    let mut bad_trigger = mail_rule("t");
    bad_trigger["trigger"] = json!({"kind": "timer", "every_seconds": 1});
    let mut unknown_id = mail_rule("u");
    unknown_id["id"] = "01NOPE".into();
    let mut foreign_id = mail_rule("f");
    foreign_id["id"] = foreign;
    let mut again = merge_rule();
    again["id"] = id.clone();
    let mut again2 = mail_rule("twice");
    again2["id"] = id.clone();
    for (rules, at) in [
        (json!([bad_column]), "rules[0].actions[0].args.to"),
        (
            json!([mail_rule("ok"), bad_trigger]),
            "rules[1].trigger.every_seconds",
        ),
        (json!([unknown_id]), "rules[0].id"),
        (json!([foreign_id]), "rules[0].id"),
        (json!([again, again2]), "rules[1].id"),
        (json!([mail_rule("a"), mail_rule("a")]), "rules[1].name"),
        (
            json!([{"name": "x", "trigger": {"kind": "lands"}, "actions": [], "colour": 1}]),
            "rules[0].colour",
        ),
        (
            json!([{"name": "x", "trigger": {"kind": "lands"},
                 "actions": [{"verb": "card.teleport"}]}]),
            "rules[0].actions[0]",
        ),
    ] {
        let r = td
            .put(&path("review"), with_rules(&first, rules.clone()))
            .await;
        r.err(422, "invalid");
        assert_eq!(
            r.body["error"]["details"]["path"], at,
            "{rules}: {}",
            r.body
        );
        assert!(r.body["error"]["details"]["problems"].is_array());
    }
    // A bad rule list writes nothing: not the checks of the same PUT.
    let mut b = with_rules(&first, json!([bad_trigger_rule()]));
    b["checks"]["require_comment_on_enter"] = true.into();
    td.put(&path("review"), b).await.err(422, "invalid");
    let now = options(&td, "review").await;
    assert_eq!(now["version"], 2);
    assert_eq!(now["checks"]["require_comment_on_enter"], false);
    assert_eq!(now["rules"], first["rules"]);
}

fn bad_trigger_rule() -> Value {
    json!({"name": "b", "trigger": {"kind": "count_reached", "count": 0},
           "actions": [{"verb": "card.done_mail"}]})
}

#[tokio::test]
async fn only_the_operator_and_the_own_project_admin_write_rules() {
    let td = setup().await;
    let o = options(&td, "review").await;
    let body = |v: i64| {
        let mut b = with_rules(&o, json!([mail_rule("note")]));
        b["base_version"] = v.into();
        b
    };
    for who in ["proj/w1", "ops/root", "other/boss"] {
        td.put_as(who, &path("review"), body(1))
            .await
            .err(403, "forbidden");
    }
    assert_eq!(options(&td, "review").await["rules"], json!([]));
    let r = td.put_as("proj/lead", &path("review"), body(1)).await;
    assert_eq!(names(r.ok()), ["note"]);
    let mut b = body(2);
    b["rules"] = json!([]);
    assert_eq!(td.put(&path("review"), b).await.ok()["rules"], json!([]));
    // Readers read the rules: a member, and a sysadmin of another project.
    for who in ["proj/w1", "ops/root"] {
        let g = td.get_as(who, &path("review")).await;
        assert_eq!(g.ok()["version"], 3, "{who}");
        assert!(g.ok()["rules"].is_array(), "{who}");
    }
}

#[tokio::test]
async fn the_catalog_lists_the_model_for_every_caller() {
    let td = setup().await;
    let c = td
        .get_as("proj/w1", "/api/rules/catalog")
        .await
        .ok()
        .clone();
    let kinds: Vec<&str> = c["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "lands",
            "leaves",
            "job_result",
            "timer",
            "count_reached",
            "wait_met",
            "archived"
        ]
    );
    assert!(
        c["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["verb"] == "card.move")
    );
    assert_eq!(c["limits"]["max_rules_per_column"], 50);
}

#[tokio::test]
async fn a_rename_keeps_the_rules_a_removed_column_drops_them_and_legacy_rules_stay() {
    let td = setup().await;
    let legacy = td.get("/api/projects/proj/rules").await.ok().clone();
    let board = "/api/projects/proj/board";
    let cols = |qa: Option<&str>| {
        let mut c = vec![
            json!({"name": "backlog"}),
            json!({"name": "ready"}),
            json!({"name": "in_progress", "role": "working"}),
        ];
        match qa {
            Some("qa") => c.push(json!({"name": "qa"})),
            Some(n) => c.push(json!({"name": n, "rename_from": "qa"})),
            None => {}
        }
        c.extend([
            json!({"name": "review", "role": "review"}),
            json!({"name": "validating", "role": "validating"}),
            json!({"name": "merge_queue", "role": "merge_queue"}),
            json!({"name": "accepted", "role": "accepted"}),
            json!({"name": "blocked", "role": "blocked"}),
            json!({"name": "done"}),
        ]);
        json!({"columns": c})
    };
    td.patch(board, cols(Some("qa"))).await.ok();
    let o = options(&td, "qa").await;
    let r = td
        .put(&path("qa"), with_rules(&o, json!([mail_rule("note")])))
        .await
        .ok()
        .clone();
    let id = r["rules"][0]["id"].clone();
    td.patch(board, cols(Some("quality"))).await.ok();
    let q = options(&td, "quality").await;
    assert_eq!(q["rules"][0]["id"], id);
    assert_eq!(q["version"], 2);
    // The column goes, and its rules with it.
    td.patch(board, cols(None)).await.ok();
    let left: i64 = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM board_column_rules", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(left, 0);
    // The legacy rules of the project did not change.
    assert_eq!(td.get("/api/projects/proj/rules").await.ok(), &legacy);
}
