//! The system architect role (TER-47, docs/api.md §3): an agent that reads
//! what a sysadmin reads, in every project, and writes only as a member of
//! its own project. With the fake supervisor; no harness runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

/// The ULID of an agent from its create answer.
fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

struct World {
    td: TestDaemon,
    /// `p/arch`: architect, in project `p`.
    arch: String,
    /// `q/w`: a member of the other project `q`.
    qw: String,
    /// `q/lead`: project_admin of `q`.
    qlead: String,
    /// A card of `q`, made by `q/w`.
    qcard: String,
    /// A mail between `q/w` and `q/lead`.
    qmail: String,
}

async fn world() -> World {
    let td = TestDaemon::start().await;
    for p in ["p", "q", "ops"] {
        td.project(p).await;
    }
    let arch = id(&td.agent("p", "arch", json!({"roles": ["architect"]})).await);
    td.agent("p", "pw", json!({})).await;
    let qw = id(&td.agent("q", "w", json!({})).await);
    let qlead = id(&td
        .agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await);
    td.agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    let qcard = td
        .post_as(&qw, "/api/projects/q/cards", json!({"title": "Q work"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let qmail = td
        .post_as(
            &qw,
            "/api/mail",
            json!({"to": "q/lead", "subject": "s", "body": "inside q"}),
        )
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post_as(
        &qw,
        "/api/mail",
        json!({"to": "project:q", "subject": "i", "body": "to the q inbox"}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets",
        json!({"name": "gh", "kind": "token", "fields": {"value": "ghp_R4nd0mT0k3nV4lu3Xy9Qz7Lm2Pd"}}),
    )
    .await
    .ok();
    td.post(
        "/api/targets",
        json!({"name": "box", "kind": "ssh", "address": "u@h", "project": "q"}),
    )
    .await
    .ok();
    World {
        td,
        arch,
        qw,
        qlead,
        qcard,
        qmail,
    }
}

/// The role is shown, granted only by the operator, and not with sysadmin.
#[tokio::test]
async fn only_the_operator_grants_architect_and_not_with_sysadmin() {
    let w = world().await;
    let a =
        w.td.get(&format!("/api/agents/{}", w.arch))
            .await
            .ok()
            .clone();
    assert_eq!(a["roles"], json!(["architect"]));
    let pw = w.td.get("/api/agents/p%2Fpw").await.ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A sysadmin agent and a project_admin cannot grant it; the operator can.
    w.td.put_as(
        "ops/root",
        &format!("/api/agents/{pw}/roles"),
        json!({"roles": ["architect"]}),
    )
    .await
    .err(403, "forbidden");
    w.td.put_as(
        &w.qlead,
        &format!("/api/agents/{}/roles", w.qw),
        json!({"roles": ["architect"]}),
    )
    .await
    .err(403, "forbidden");
    let r =
        w.td.put(
            &format!("/api/agents/{pw}/roles"),
            json!({"roles": ["architect", "project_admin"]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // An architect cannot grant roles either, nor revoke its own.
    w.td.put_as(
        &w.arch,
        &format!("/api/agents/{pw}/roles"),
        json!({"roles": []}),
    )
    .await
    .err(403, "forbidden");
    // Not with sysadmin.
    w.td.put(
        &format!("/api/agents/{pw}/roles"),
        json!({"roles": ["architect", "sysadmin"]}),
    )
    .await
    .err(422, "invalid");
}

/// Reads in another project and of the whole system: 200.
#[tokio::test]
async fn an_architect_reads_every_project() {
    let w = world().await;
    let get = |path: String| {
        let td = &w.td;
        let arch = w.arch.clone();
        async move { td.get_as(&arch, &path).await }
    };
    for path in [
        "/api/projects/q/board".to_owned(),
        "/api/projects/q/cards".to_owned(),
        format!("/api/cards/{}", w.qcard),
        "/api/projects/q/repos".to_owned(),
        format!("/api/agents/{}", w.qw),
        format!("/api/agents/{}/sessions", w.qw),
        format!("/api/mail/{}", w.qmail),
        "/api/projects/q/inbox".to_owned(),
        "/api/audit".to_owned(),
        "/api/questions".to_owned(),
        "/api/targets".to_owned(),
        "/api/templates".to_owned(),
    ] {
        let r = get(path.clone()).await;
        assert_eq!(r.status, 200, "{path}: {}", r.body);
    }
    // The lists cover the other project.
    let projects = get("/api/projects".into()).await.ok().clone();
    assert!(projects.to_string().contains("\"q\""), "{projects}");
    let agents = get("/api/agents".into()).await.ok().clone();
    assert!(agents.to_string().contains(&w.qw), "{agents}");
    let mail = get("/api/mail".into()).await.ok().clone();
    assert!(mail.to_string().contains(&w.qmail), "{mail}");
    let cards = get("/api/cards?columns=backlog".into()).await.ok().clone();
    assert!(cards.to_string().contains(&w.qcard), "{cards}");
    let targets = get("/api/targets".into()).await.ok().clone();
    assert!(targets.to_string().contains("\"q-box\""), "{targets}");
    // Secret metadata, never a value.
    let secrets = get("/api/secrets".into()).await.ok().clone();
    assert!(secrets.to_string().contains("\"gh\""), "{secrets}");
    assert!(!secrets.to_string().contains("ghp_R4nd0m"), "{secrets}");
}

/// Writes outside the own project: 403 (or 404 where the route hides).
#[tokio::test]
async fn an_architect_writes_nothing_in_another_project() {
    let w = world().await;
    let td = &w.td;
    let a = &w.arch;
    let card = format!("/api/cards/{}", w.qcard);
    let refused = |r: &common::Resp, what: &str| {
        assert!(
            r.status == 403 || r.status == 404,
            "{what}: {} {}",
            r.status,
            r.body
        );
    };
    refused(
        &td.post_as(a, "/api/projects/q/cards", json!({"title": "x"}))
            .await,
        "card create",
    );
    refused(
        &td.post_as(a, &format!("{card}/move"), json!({"column": "ready"}))
            .await,
        "card move",
    );
    refused(
        &td.patch_as(a, &card, json!({"title": "y"})).await,
        "card edit",
    );
    refused(
        &td.post_as(a, &format!("{card}/comments"), json!({"body": "z"}))
            .await,
        "comment",
    );
    refused(
        &td.post_empty_as(a, &format!("{card}/claim")).await,
        "claim",
    );
    refused(
        &td.post_as(a, &format!("{card}/assign"), json!({"assignee": "me"}))
            .await,
        "assign",
    );
    refused(
        &td.post_as(
            a,
            "/api/projects/q/memories",
            json!({"title": "A claim", "body": "b"}),
        )
        .await,
        "memory",
    );
    refused(
        &td.post_as(
            a,
            "/api/projects/q/wiki",
            json!({"path": "plan", "body": "x", "summary": "s"}),
        )
        .await,
        "wiki page",
    );
    refused(
        &td.post_as(
            a,
            &format!("/api/mail/{}/reply", w.qmail),
            json!({"body": "r"}),
        )
        .await,
        "reply in a q thread",
    );
    refused(
        &td.post_as(
            a,
            "/api/mail",
            json!({"to": "q/w", "subject": "s", "body": "b"}),
        )
        .await,
        "mail to q/w",
    );
    refused(
        &td.post_empty_as(a, &format!("/api/agents/{}/start", w.qw))
            .await,
        "agent start",
    );
    refused(
        &td.post_as(
            a,
            &format!("/api/agents/{}/send", w.qw),
            json!({"text": "do it"}),
        )
        .await,
        "agent send",
    );
    refused(
        &td.post_as(
            a,
            &format!("/api/agents/{}/keys", w.qw),
            json!({"keys": ["Escape"]}),
        )
        .await,
        "agent keys",
    );
    refused(
        &td.post_as(a, "/api/targets/q-box/leases", json!({"agent": "q/w"}))
            .await,
        "lease",
    );
    // Exec on a target the architect can see: 403, not a hidden 404.
    td.post_as(a, "/api/targets/q-box/exec", json!({"cmd": ["true"]}))
        .await
        .err(403, "forbidden");
    refused(
        &td.post_as(a, "/api/projects", json!({"name": "new"})).await,
        "project create",
    );
    refused(&td.post_empty_as(a, "/api/reload").await, "reload");
    // The secret value: operator only.
    td.post_as(a, "/api/secrets/global/gh/reveal", json!({"password": "x"}))
        .await
        .err(403, "forbidden");
    // The terminal attach takes input: refused also as a view.
    let ws = td.ws(&format!("/api/agents/{}/pty", w.qw), Some(a)).await;
    match ws {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
            assert_eq!(r.status().as_u16(), 403);
        }
        Err(e) => panic!("pty: {e}"),
        Ok(_) => panic!("pty: the terminal of q/w opened for the architect"),
    }
    // Nothing changed in q.
    let c = td.get(&card).await.ok().clone();
    assert_eq!(c["title"], "Q work");
    assert_eq!(c["column"], "backlog");
    assert!(c["comments"].as_array().unwrap().is_empty(), "{c}");
}

/// Every write handler behind `api::agents::load` (which takes the read
/// right) has a second check that refuses an architect: exactly 403 for an
/// agent of another project, and nothing changes.
#[tokio::test]
async fn an_architect_controls_no_agent_of_another_project() {
    use hyper::Method;
    let w = world().await;
    let td = &w.td;
    let a = w.arch.as_str();
    let q = format!("/api/agents/{}", w.qw);
    let before = td.get(&q).await.ok().clone();
    let calls: Vec<(&str, Method, String, Option<Value>)> = vec![
        (
            "update",
            Method::PATCH,
            q.clone(),
            Some(json!({"model": "x"})),
        ),
        ("remove", Method::DELETE, format!("{q}?confirm=true"), None),
        (
            "swap",
            Method::POST,
            format!("{q}/subscription"),
            Some(json!({"subscription": "s"})),
        ),
        ("start", Method::POST, format!("{q}/start"), None),
        ("stop", Method::POST, format!("{q}/stop"), None),
        ("restart", Method::POST, format!("{q}/restart"), None),
        ("pause", Method::POST, format!("{q}/pause"), None),
        ("unpause", Method::POST, format!("{q}/unpause"), None),
        ("compact", Method::POST, format!("{q}/compact"), None),
        (
            "new-session",
            Method::POST,
            format!("{q}/new-session"),
            None,
        ),
        (
            "send",
            Method::POST,
            format!("{q}/send"),
            Some(json!({"text": "do it"})),
        ),
        (
            "keys",
            Method::POST,
            format!("{q}/keys"),
            Some(json!({"keys": ["esc"]})),
        ),
        ("interrupt", Method::POST, format!("{q}/interrupt"), None),
        (
            "answer",
            Method::POST,
            format!("{q}/answer"),
            Some(json!({"text": "yes"})),
        ),
        (
            "roles",
            Method::PUT,
            format!("{q}/roles"),
            Some(json!({"roles": ["project_admin"]})),
        ),
        (
            "attachment",
            Method::POST,
            format!("{q}/attachments"),
            Some(json!({"name": "a.png", "data": "iVBORw0KGgo="})),
        ),
        (
            "spawn",
            Method::POST,
            format!("{q}/spawn"),
            Some(json!({"template": "t", "task": "x"})),
        ),
        (
            "wait",
            Method::POST,
            format!("{q}/waits"),
            Some(json!({"kind": "time", "for": "1m"})),
        ),
        (
            "question",
            Method::POST,
            format!("{q}/questions"),
            Some(json!({"text": "t"})),
        ),
        (
            "status",
            Method::POST,
            format!("{q}/status"),
            Some(json!({"status": "idle"})),
        ),
    ];
    for (what, m, path, body) in calls {
        let r = td.req(m, &path, body, Some(a)).await;
        assert_eq!(r.status, 403, "{what}: {}", r.body);
    }
    let after = td.get(&q).await.ok().clone();
    assert_eq!(before["status"], after["status"]);
    assert_eq!(before["model"], after["model"]);
    assert_eq!(after["roles"], json!([]));
}

/// In its own project, an architect writes as a member.
#[tokio::test]
async fn an_architect_writes_as_a_member_in_its_own_project() {
    let w = world().await;
    let td = &w.td;
    let a = &w.arch;
    let k = td
        .post_as(
            a,
            "/api/projects/p/cards",
            json!({"title": "Plan the split"}),
        )
        .await;
    assert_eq!(k.status, 201, "{}", k.body);
    let kid = k.body["id"].as_str().unwrap().to_owned();
    td.post_as(
        a,
        &format!("/api/cards/{kid}/comments"),
        json!({"body": "Step one"}),
    )
    .await
    .ok();
    let m = td
        .post_as(
            a,
            "/api/projects/p/memories",
            json!({"title": "The split needs two crates", "body": "b"}),
        )
        .await;
    assert!(m.status == 200 || m.status == 201, "{}", m.body);
    let p = td
        .post_as(
            a,
            "/api/projects/p/wiki",
            json!({"path": "plans/split", "body": "# Split", "summary": "first"}),
        )
        .await;
    assert!(p.status == 200 || p.status == 201, "{}", p.body);
    let mail = td
        .post_as(
            a,
            "/api/mail",
            json!({"to": "operator", "subject": "plan", "body": "ready"}),
        )
        .await;
    assert!(mail.status == 200 || mail.status == 201, "{}", mail.body);
    // As a member: no agent control in the own project either.
    let pw = td.get("/api/agents/p%2Fpw").await.ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = td
        .post_empty_as(a, &format!("/api/agents/{pw}/start"))
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
}
