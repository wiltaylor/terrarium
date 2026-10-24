//! Each command against a mock transport: the exact requests, and the
//! output.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use clap::Parser;
use serde_json::{Value, json};

use crate::api::{ServerError, exit, exit_code_of};
use crate::cli::Cli;
use crate::ctx::{AgentEnv, Ctx};
use crate::transport::mock::MockTransport;
use crate::transport::{ApiRequest, Method};

const ME: &str = "01AGENTAGENTAGENTAGENTAGEN";

struct Run {
    result: anyhow::Result<()>,
    out: String,
    reqs: Vec<ApiRequest>,
}

async fn exec(agent: bool, args: &[&str], replies: Vec<(u16, Value)>, stdin: Option<&str>) -> Run {
    let cli = Cli::try_parse_from(std::iter::once("terrarium").chain(args.iter().copied()))
        .unwrap_or_else(|e| panic!("{args:?}: {e}"));
    let mock = MockTransport::new();
    for (s, b) in replies {
        mock.reply(s, b);
    }
    let env = agent.then(|| AgentEnv {
        agent: ME.into(),
        socket: "/nonexistent.sock".into(),
    });
    let mut ctx = Ctx::for_test(Arc::new(mock.clone()), env);
    if agent {
        ctx = ctx.with_identity(json!({
            "agent": ME, "name": "worker", "project": "terra",
            "harness": "pi", "status": "idle", "roles": []
        }));
    }
    if let Some(s) = stdin {
        ctx = ctx.with_stdin(s);
    }
    ctx.json = cli.json;
    ctx.project = cli.project.clone();
    let result = super::run(&ctx, &cli.command).await;
    Run {
        result,
        out: ctx.out.captured(),
        reqs: mock.requests(),
    }
}

type Want = (
    Method,
    &'static str,
    Vec<(&'static str, &'static str)>,
    Option<Value>,
);

fn req(m: Method, path: &'static str) -> Want {
    (m, path, vec![], None)
}

fn check(args: &[&str], got: &[ApiRequest], want: &[Want]) {
    assert_eq!(
        got.len(),
        want.len(),
        "{args:?}: got {:#?}",
        got.iter()
            .map(|r| format!("{} {}", r.method.as_str(), r.path_and_query()))
            .collect::<Vec<_>>()
    );
    for (g, (m, path, q, body)) in got.iter().zip(want) {
        assert_eq!(g.method, *m, "{args:?} method");
        assert_eq!(g.path, *path, "{args:?} path");
        let q: Vec<(String, String)> = q
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(g.query, q, "{args:?} query of {path}");
        assert_eq!(&g.body, body, "{args:?} body of {path}");
    }
}

/// Run one case; it must succeed.
async fn ok(agent: bool, args: &[&str], replies: Vec<(u16, Value)>, want: Vec<Want>) -> String {
    ok_in(agent, args, replies, None, want).await
}

async fn ok_in(
    agent: bool,
    args: &[&str],
    replies: Vec<(u16, Value)>,
    stdin: Option<&str>,
    want: Vec<Want>,
) -> String {
    let r = exec(agent, args, replies, stdin).await;
    if let Err(e) = &r.result {
        panic!("{args:?} failed: {e:#}");
    }
    check(args, &r.reqs, &want);
    r.out
}

use Method::{Delete, Get, Patch, Post, Put};

#[tokio::test]
async fn daemon_and_system() {
    let out = ok(
        false,
        &["daemon", "status"],
        vec![(200, json!({"version": "0.1.0", "uptime": 65}))],
        vec![req(Get, "/api/health")],
    )
    .await;
    assert_eq!(out, "terrariumd 0.1.0 running at mock (up 1m5s)\n");
    let out = ok(
        false,
        &["daemon", "reload"],
        vec![(
            200,
            json!({"projects": {"added": [], "updated": [], "removed": []},
                   "agents": {"added": ["t/h"], "updated": [], "removed": []}, "errors": ["bad.toml"]}),
        )],
        vec![req(Post, "/api/reload")],
    )
    .await;
    assert!(out.contains("agents: added t/h"), "{out}");
    assert!(out.contains("error: bad.toml"), "{out}");
    ok(
        false,
        &["targets", "ls"],
        vec![],
        vec![req(Get, "/api/targets")],
    )
    .await;
    ok(
        false,
        &["-p", "terra", "targets", "ls"],
        vec![],
        vec![req(Get, "/api/projects/terra/targets")],
    )
    .await;
    ok(
        false,
        &["audit", "--action", "agent.start", "--limit", "10"],
        vec![],
        vec![(
            Get,
            "/api/audit",
            vec![("action", "agent.start"), ("limit", "10")],
            None,
        )],
    )
    .await;
    let out = ok(
        false,
        &["events", "--kind", "status", "--limit", "3", "--since", "4"],
        vec![(
            200,
            json!([{"seq": 5, "at": "2026-10-24T06:36:54.499Z", "kind": "status",
                    "agent": "worker", "summary": "worker: starting -> idle"}]),
        )],
        vec![(
            Get,
            "/api/events",
            vec![("kind", "status"), ("since", "4"), ("limit", "3")],
            None,
        )],
    )
    .await;
    assert!(out.contains("2026-10-24 06:36  status"), "{out}");
    assert!(out.contains("worker: starting -> idle"), "{out}");
    let out = ok(false, &["whoami"], vec![], vec![]).await;
    assert_eq!(out, "operator\n");
    let out = ok(true, &["whoami"], vec![], vec![]).await;
    assert!(
        out.contains("agent: worker") && out.contains("project: terra"),
        "{out}"
    );
}

#[tokio::test]
async fn projects_and_repos() {
    ok(
        false,
        &["project", "ls"],
        vec![],
        vec![req(Get, "/api/projects")],
    )
    .await;
    let out = ok(
        false,
        &["project", "show", "ops"],
        vec![
            (200, json!({"name": "ops", "card_prefix": "OPS", "source": "api"})),
            (200, json!([{"id": "A1", "name": "lead", "project": "ops", "harness": "claude", "status": "idle"}])),
            (200, json!([])),
            (200, json!([])),
        ],
        vec![
            req(Get, "/api/projects/ops"),
            req(Get, "/api/projects/ops/agents"),
            req(Get, "/api/projects/ops/repos"),
            req(Get, "/api/projects/ops/targets"),
        ],
    )
    .await;
    assert!(out.contains("project: ops  prefix: OPS"), "{out}");
    assert!(out.contains("lead"), "{out}");
    ok(
        false,
        &[
            "project",
            "new",
            "ops",
            "-d",
            "Operations",
            "--prefix",
            "OPS",
        ],
        vec![(201, json!({"name": "ops", "card_prefix": "OPS"}))],
        vec![(
            Post,
            "/api/projects",
            vec![],
            Some(json!({"name": "ops", "description": "Operations", "card_prefix": "OPS"})),
        )],
    )
    .await;
    ok(
        false,
        &["project", "set", "ops", "-d", "X"],
        vec![(200, json!({"name": "ops"}))],
        vec![(
            Patch,
            "/api/projects/ops",
            vec![],
            Some(json!({"description": "X"})),
        )],
    )
    .await;
    ok(
        false,
        &["project", "rm", "ops", "--confirm"],
        vec![(204, Value::Null)],
        vec![(Delete, "/api/projects/ops", vec![("confirm", "true")], None)],
    )
    .await;
    ok(
        false,
        &["project", "repos", "ops"],
        vec![],
        vec![req(Get, "/api/projects/ops/repos")],
    )
    .await;
    ok(
        false,
        &[
            "-p",
            "ops",
            "project",
            "repo-add",
            "https://x/infra.git",
            "--branch",
            "main",
            "--credential",
            "gh",
        ],
        vec![(201, json!({"name": "infra", "project": "ops"}))],
        vec![(
            Post,
            "/api/projects/ops/repos",
            vec![],
            Some(
                json!({"url": "https://x/infra.git", "default_branch": "main", "credential": "gh"}),
            ),
        )],
    )
    .await;
    // Agent mode: the project comes from whoami.
    ok(
        true,
        &["project", "show"],
        vec![],
        vec![
            req(Get, "/api/projects/terra"),
            req(Get, "/api/projects/terra/agents"),
            req(Get, "/api/projects/terra/repos"),
            req(Get, "/api/projects/terra/targets"),
        ],
    )
    .await;
    ok(
        true,
        &["repo", "ls"],
        vec![],
        vec![req(Get, "/api/projects/terra/repos")],
    )
    .await;
    ok(
        true,
        &["repo", "checkout", "terrarium", "--branch", "main"],
        vec![
            (200, json!([{"id": "R1", "name": "terrarium"}])),
            (200, json!({})),
        ],
        vec![
            req(Get, "/api/projects/terra/repos"),
            (
                Post,
                "/api/repos/R1/checkout",
                vec![],
                Some(json!({"branch": "main"})),
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn repo_checkout_awaits_the_server() {
    let r = exec(
        true,
        &["repo", "checkout", "terrarium"],
        vec![
            (200, json!([])),
            (
                404,
                json!({"error": {"code": "not_found", "message": "no such API route"}}),
            ),
        ],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    assert!(e.to_string().starts_with("awaiting server"), "{e}");
    assert_eq!(exit_code_of(&e), exit::USAGE);
}

#[tokio::test]
async fn repo_checkout_prints_the_result() {
    let out = ok(
        false,
        &[
            "-p",
            "terra",
            "repo",
            "checkout",
            "terrarium",
            "--agent",
            "worker",
            "--depth",
            "1",
        ],
        vec![
            (200, json!([{"id": "R1", "name": "terrarium"}])),
            (
                200,
                json!({"repo": "terrarium", "repo_id": "R1", "project": "terra", "agent": "worker",
                       "agent_id": "A1", "path": "terrarium", "action": "cloned",
                       "head": "abc123", "branch": "main"}),
            ),
        ],
        vec![
            req(Get, "/api/projects/terra/repos"),
            (
                Post,
                "/api/repos/R1/checkout",
                vec![],
                Some(json!({"depth": 1, "agent": "worker"})),
            ),
        ],
    )
    .await;
    assert_eq!(
        out,
        "cloned terrarium in work/terrarium of worker\nHEAD abc123 (main)\n"
    );
    // A daemon that answers 501 not_available: exit 2 with its message.
    let r = exec(
        true,
        &["repo", "checkout", "terrarium", "--target", "box"],
        vec![
            (200, json!([{"id": "R1", "name": "terrarium"}])),
            (
                501,
                json!({"error": {"code": "not_available", "message": "host agents arrive in a later phase"}}),
            ),
        ],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    assert!(e.to_string().contains("later phase"), "{e}");
    assert_eq!(exit_code_of(&e), exit::USAGE);
    // A missing repo is a server error, not "awaiting server".
    let r = exec(
        true,
        &["repo", "checkout", "nope"],
        vec![
            (200, json!([])),
            (
                404,
                json!({"error": {"code": "not_found", "message": "no repo `nope`"}}),
            ),
        ],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    assert_eq!(exit_code_of(&e), exit::SERVER);
}

#[test]
fn credential_helper_input_parses_to_the_blank_line() {
    let m = super::projects::parse_helper_input(
        "protocol=https\nhost=github.com\npath=a/b.git\nwwwauth[]=Basic x=y\n\nhost=ignored\n",
    );
    assert_eq!(m["protocol"], "https");
    assert_eq!(m["host"], "github.com");
    assert_eq!(m["path"], "a/b.git");
    assert_eq!(m["wwwauth[]"], "Basic x=y");
    assert_eq!(m.len(), 4);
    assert!(super::projects::parse_helper_input("").is_empty());
}

#[tokio::test]
async fn repo_credential_speaks_the_helper_protocol() {
    let out = ok_in(
        true,
        &["repo", "credential", "get"],
        vec![(200, json!({"username": "bot", "password": "tok"}))],
        Some("protocol=https\nhost=git.example.com\npath=team/hub.git\n\n"),
        vec![(
            Post,
            "/api/repos/credential",
            vec![],
            Some(json!({"protocol": "https", "host": "git.example.com", "path": "team/hub.git"})),
        )],
    )
    .await;
    assert_eq!(out, "username=bot\npassword=tok\n");
    // 404: no output and exit 0, so git falls back.
    let out = ok_in(
        true,
        &["repo", "credential", "get"],
        vec![(
            404,
            json!({"error": {"code": "not_found", "message": "no credential"}}),
        )],
        Some("protocol=https\nhost=x\n"),
        vec![(
            Post,
            "/api/repos/credential",
            vec![],
            Some(json!({"protocol": "https", "host": "x"})),
        )],
    )
    .await;
    assert_eq!(out, "");
    // `store` and `erase` do nothing; neither does a request without a host.
    for (op, input) in [
        ("store", "protocol=https\nhost=x\npassword=p\n"),
        ("erase", ""),
        ("get", "protocol=https\n"),
    ] {
        let out = ok_in(
            true,
            &["repo", "credential", op],
            vec![],
            Some(input),
            vec![],
        )
        .await;
        assert_eq!(out, "");
    }
    // Another error is an error.
    let r = exec(
        true,
        &["repo", "credential", "get"],
        vec![(
            429,
            json!({"error": {"code": "rate_limited", "message": "slow down"}}),
        )],
        Some("protocol=https\nhost=x\n"),
    )
    .await;
    assert!(r.result.is_err());
    assert_eq!(r.out, "");
}

#[tokio::test]
async fn a_missing_project_is_a_usage_error() {
    let r = exec(false, &["board"], vec![], None).await;
    let e = r.result.unwrap_err();
    assert_eq!(exit_code_of(&e), exit::USAGE);
    assert!(r.reqs.is_empty());
}

#[tokio::test]
async fn agents() {
    ok(
        false,
        &["-p", "terra", "agent", "ls"],
        vec![],
        vec![(Get, "/api/agents", vec![("project", "terra")], None)],
    )
    .await;
    ok(
        true,
        &["agents", "ls"],
        vec![],
        vec![req(Get, "/api/agents")],
    )
    .await;
    ok(
        true,
        &["agents", "ls", "--all-projects"],
        vec![(200, json!([{"name": "terra"}, {"name": "ops"}]))],
        vec![
            req(Get, "/api/projects"),
            req(Get, "/api/projects/terra/agents"),
            req(Get, "/api/projects/ops/agents"),
        ],
    )
    .await;
    ok(
        false,
        &["agent", "show", "terra/worker"],
        vec![(200, json!({"id": "A1", "name": "worker"}))],
        vec![req(Get, "/api/agents/terra%2Fworker")],
    )
    .await;
    let out = ok(
        false,
        &[
            "-p",
            "terra",
            "agent",
            "new",
            "worker",
            "--harness",
            "pi",
            "--model",
            "m",
            "--mail-to",
            "*",
            "--target",
            "buildbox",
        ],
        vec![(
            201,
            json!({"id": "A1", "name": "worker", "project": "terra", "harness": "pi"}),
        )],
        vec![(
            Post,
            "/api/agents",
            vec![],
            Some(
                json!({"name": "worker", "project": "terra", "harness": "pi", "model": "m",
                        "policy": {"targets": ["buildbox"], "mail_to": ["*"]}}),
            ),
        )],
    )
    .await;
    assert_eq!(out, "created agent terra/worker (pi) A1\n");
    ok(
        false,
        &[
            "agent",
            "set",
            "w",
            "--prompt",
            "lead",
            "--policy",
            r#"{"targets":[]}"#,
        ],
        vec![(200, json!({}))],
        vec![(
            Patch,
            "/api/agents/w",
            vec![],
            Some(json!({"prompt": "lead", "policy": {"targets": []}})),
        )],
    )
    .await;
    ok(
        false,
        &["agent", "rm", "w", "--confirm"],
        vec![(204, Value::Null)],
        vec![(Delete, "/api/agents/w", vec![("confirm", "true")], None)],
    )
    .await;
    for action in ["start", "stop", "restart"] {
        let path: &'static str = match action {
            "start" => "/api/agents/w/start",
            "stop" => "/api/agents/w/stop",
            _ => "/api/agents/w/restart",
        };
        let out = ok(
            false,
            &["agent", action, "w"],
            vec![(
                202,
                json!({"name": "w", "project": "terra", "status": "starting"}),
            )],
            vec![req(Post, path)],
        )
        .await;
        assert_eq!(out, format!("{action}: terra/w is starting\n"));
    }
    ok(
        false,
        &["agent", "send", "w", "run the tests"],
        vec![(202, json!({"accepted": true}))],
        vec![(
            Post,
            "/api/agents/w/send",
            vec![],
            Some(json!({"text": "run the tests"})),
        )],
    )
    .await;
    ok_in(
        false,
        &["agent", "send", "w", "-"],
        vec![(202, json!({"accepted": true}))],
        Some("from stdin"),
        vec![(
            Post,
            "/api/agents/w/send",
            vec![],
            Some(json!({"text": "from stdin"})),
        )],
    )
    .await;
    ok(
        false,
        &[
            "agent",
            "answer",
            "w",
            "--choice",
            "1",
            "--request-id",
            "r1",
        ],
        vec![(202, json!({}))],
        vec![(
            Post,
            "/api/agents/w/answer",
            vec![],
            Some(json!({"answer": {"kind": "choice", "value": 1}, "request_id": "r1"})),
        )],
    )
    .await;
    ok(
        false,
        &["agent", "answer", "w", "--text", "y"],
        vec![(202, json!({}))],
        vec![(
            Post,
            "/api/agents/w/answer",
            vec![],
            Some(json!({"text": "y"})),
        )],
    )
    .await;
    ok(
        false,
        &["agent", "logs", "w", "--kind", "status", "--limit", "5"],
        vec![],
        vec![(
            Get,
            "/api/agents/w/events",
            vec![("kind", "status"), ("limit", "5")],
            None,
        )],
    )
    .await;
    ok(
        false,
        &["agent", "sessions", "w"],
        vec![],
        vec![req(Get, "/api/agents/w/sessions")],
    )
    .await;
    let out = ok(
        false,
        &["agent", "roles", "get", "w"],
        vec![(200, json!([]))],
        vec![req(Get, "/api/agents/w/roles")],
    )
    .await;
    assert_eq!(out, "member\n");
    ok(
        false,
        &["agent", "roles", "set", "w", "project_admin"],
        vec![(200, json!(["project_admin"]))],
        vec![(
            Put,
            "/api/agents/w/roles",
            vec![],
            Some(json!({"roles": ["project_admin"]})),
        )],
    )
    .await;
}

#[tokio::test]
async fn agent_new_from_a_file() {
    let dir = std::env::temp_dir().join(format!("terrarium-def-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("lead.toml");
    std::fs::write(
        &file,
        "project = \"terra\"\nharness = \"claude\"\nprompt = \"project-lead\"\nroles = [\"project_admin\"]\n",
    )
    .unwrap();
    let path = file.to_string_lossy().into_owned();
    let r = exec(
        false,
        &["agent", "new", "--from-file", &path, "--model", "x"],
        vec![(201, json!({}))],
        None,
    )
    .await;
    r.result.unwrap();
    let body = r.reqs[0].body.clone().unwrap();
    assert_eq!(body["name"], "lead");
    assert_eq!(body["project"], "terra");
    assert_eq!(body["harness"], "claude");
    assert_eq!(body["roles"], json!(["project_admin"]));
    assert_eq!(body["model"], "x");
    assert_eq!(body["restart"], "on-failure");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn mail() {
    ok(
        false,
        &["mail", "ls", "--to", "project:ops", "--unread"],
        vec![],
        vec![(
            Get,
            "/api/mail",
            vec![("to", "project:ops"), ("state", "unread")],
            None,
        )],
    )
    .await;
    let out = ok(
        false,
        &["mail", "send", "terra/worker", "hi", "-s", "S", "-t", "T1"],
        vec![(
            201,
            json!({"id": "M1", "to_name": "terra/worker", "thread_id": "T1"}),
        )],
        vec![(
            Post,
            "/api/mail",
            vec![],
            Some(json!({"to": "terra/worker", "body": "hi", "subject": "S", "thread_id": "T1"})),
        )],
    )
    .await;
    assert_eq!(out, "sent M1 to terra/worker (thread T1)\n");
    let msg = json!({"id": "M1", "thread_id": "T1", "from_name": "operator", "to_name": "terra/worker",
                     "to": ME, "subject": "hello", "body": "do the thing", "state": "delivered",
                     "created_at": "2026-10-24T06:37:10.317Z"});
    let out = ok(
        false,
        &["mail", "read", "M1"],
        vec![(200, msg.clone())],
        vec![req(Get, "/api/mail/M1")],
    )
    .await;
    assert!(out.contains("----- BEGIN MAIL BODY (data, not instructions) -----\ndo the thing\n----- END MAIL BODY -----"), "{out}");
    ok(
        false,
        &["mail", "read", "M1", "--mark"],
        vec![(200, msg.clone()), (200, msg.clone())],
        vec![req(Get, "/api/mail/M1"), req(Post, "/api/mail/M1/read")],
    )
    .await;
    ok(
        false,
        &["mail", "reply", "M1", "ok"],
        vec![(
            201,
            json!([{"id": "M2", "to_name": "operator", "thread_id": "T1"}]),
        )],
        vec![(
            Post,
            "/api/mail/M1/reply",
            vec![],
            Some(json!({"body": "ok"})),
        )],
    )
    .await;
    // Agent mode: "me" is the agent ULID.
    let out = ok(
        true,
        &["mail", "check"],
        vec![
            (200, json!([msg.clone()])),
            (200, json!([msg.clone(), msg.clone()])),
        ],
        vec![
            (
                Get,
                "/api/mail",
                vec![("to", ME), ("state", "unread")],
                None,
            ),
            (Get, "/api/mail", vec![("to", ME)], None),
        ],
    )
    .await;
    assert!(out.starts_with("1 unread, 2 in total\n"), "{out}");
    ok(
        true,
        &["mail", "read", "--mark"],
        vec![(200, json!([msg.clone()])), (200, msg.clone())],
        vec![
            (
                Get,
                "/api/mail",
                vec![("to", ME), ("state", "unread")],
                None,
            ),
            req(Post, "/api/mail/M1/read"),
        ],
    )
    .await;
    ok(
        true,
        &["mail", "read", "-t", "T1"],
        vec![],
        vec![(Get, "/api/mail", vec![("thread", "T1")], None)],
    )
    .await;
    ok_in(
        true,
        &["mail", "send", "project:ops", "-s", "bug", "-"],
        vec![(201, json!({}))],
        Some("details"),
        vec![(
            Post,
            "/api/mail",
            vec![],
            Some(json!({"to": "project:ops", "body": "details", "subject": "bug"})),
        )],
    )
    .await;
}

#[tokio::test]
async fn mail_wait_long_polls_until_timeout() {
    let r = exec(true, &["mail", "wait", "--timeout", "1"], vec![], None).await;
    r.result.unwrap();
    assert!(!r.reqs.is_empty());
    assert_eq!(r.reqs[0].query_value("wait"), Some("1"));
    assert_eq!(r.reqs[0].query_value("to"), Some(ME));
    assert!(r.out.contains("no mail after 1s"), "{}", r.out);
    let out = ok(
        true,
        &["mail", "wait", "--timeout", "30"],
        vec![(200, json!([{"id": "M9", "subject": "hey"}]))],
        vec![(
            Get,
            "/api/mail",
            vec![("to", ME), ("state", "unread"), ("wait", "30")],
            None,
        )],
    )
    .await;
    assert!(out.contains("M9"), "{out}");
}

#[tokio::test]
async fn inbox() {
    ok(
        false,
        &["-p", "ops", "inbox", "ls", "--unread"],
        vec![],
        vec![(
            Get,
            "/api/projects/ops/inbox",
            vec![("unread", "true")],
            None,
        )],
    )
    .await;
    ok(
        false,
        &["-p", "ops", "inbox", "read", "M1"],
        vec![(200, json!({"id": "M1", "state": "read"}))],
        vec![req(Get, "/api/projects/ops/inbox/M1")],
    )
    .await;
    ok(
        false,
        &["-p", "ops", "inbox", "read", "M1", "--mark"],
        vec![
            (200, json!({"id": "M1", "state": "delivered"})),
            (200, json!({})),
        ],
        vec![
            req(Get, "/api/projects/ops/inbox/M1"),
            req(Post, "/api/projects/ops/inbox/M1/read"),
        ],
    )
    .await;
    ok(
        false,
        &["-p", "ops", "inbox", "reply", "M1", "accepted"],
        vec![(201, json!([]))],
        vec![(
            Post,
            "/api/projects/ops/inbox/M1/reply",
            vec![],
            Some(json!({"body": "accepted"})),
        )],
    )
    .await;
    ok(
        true,
        &["inbox", "ls"],
        vec![],
        vec![req(Get, "/api/projects/terra/inbox")],
    )
    .await;
    ok(
        true,
        &["inbox", "send", "hello", "-s", "hi"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/projects/terra/inbox",
            vec![],
            Some(json!({"body": "hello", "subject": "hi"})),
        )],
    )
    .await;
}

#[tokio::test]
async fn board_and_cards() {
    let out = ok(
        false,
        &["-p", "terra", "board"],
        vec![(
            200,
            json!({"project": "terra", "card_prefix": "TER",
              "columns": [{"name": "backlog", "count": 1}, {"name": "in_progress", "wip_limit": 2, "count": 0}],
              "cards": [{"id": "TER-1", "column": "backlog", "title": "Parser", "assignee_name": "terra/worker"}]}),
        )],
        vec![req(Get, "/api/projects/terra/board")],
    )
    .await;
    assert!(
        out.contains("== backlog (1)\n  TER-1     Parser  @terra/worker"),
        "{out}"
    );
    assert!(out.contains("== in_progress (0, wip 2)"), "{out}");
    ok(
        false,
        &["-p", "terra", "card", "ls", "--mine", "--column", "ready"],
        vec![],
        vec![(
            Get,
            "/api/projects/terra/cards",
            vec![("column", "ready"), ("assignee", "me")],
            None,
        )],
    )
    .await;
    ok(
        true,
        &["card", "ls", "--unassigned"],
        vec![],
        vec![(
            Get,
            "/api/projects/terra/cards",
            vec![("unassigned", "true")],
            None,
        )],
    )
    .await;
    let out = ok(
        false,
        &["card", "show", "TER-1"],
        vec![(
            200,
            json!({"id": "TER-1", "title": "Parser", "column": "blocked", "body": "See spec.",
                   "comments": [{"author_name": "terra/worker", "body": "need grammar", "created_at": "2026-10-24T06:37:10.357Z"}],
                   "links": [{"kind": "branch", "ref": "feat/p"}], "blocked_by": ["TER-0"]}),
        )],
        vec![req(Get, "/api/cards/TER-1")],
    )
    .await;
    assert!(out.contains("TER-1  Parser  [blocked]"), "{out}");
    assert!(out.contains("blocked by: TER-0"), "{out}");
    assert!(out.contains("branch: feat/p"), "{out}");
    assert!(out.contains("    need grammar"), "{out}");
    ok(
        false,
        &[
            "-p",
            "terra",
            "card",
            "new",
            "Ship it",
            "--label",
            "a",
            "--depends-on",
            "TER-1",
            "--column",
            "blocked",
            "-m",
            "why",
        ],
        vec![(
            201,
            json!({"id": "TER-2", "title": "Ship it", "column": "blocked"}),
        )],
        vec![(
            Post,
            "/api/projects/terra/cards",
            vec![],
            Some(
                json!({"title": "Ship it", "column": "blocked", "labels": ["a"],
                        "depends_on": ["TER-1"], "comment": "why"}),
            ),
        )],
    )
    .await;
    ok_in(
        true,
        &["card", "new", "T", "--body", "-"],
        vec![(201, json!({}))],
        Some("long body"),
        vec![(
            Post,
            "/api/projects/terra/cards",
            vec![],
            Some(json!({"title": "T", "body": "long body"})),
        )],
    )
    .await;
    ok(
        true,
        &["card", "new", "--from-mail", "M1"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/projects/terra/cards",
            vec![],
            Some(json!({"from_mail": "M1"})),
        )],
    )
    .await;
    ok(
        false,
        &["card", "edit", "TER-1", "--title", "T", "--repo", "none"],
        vec![(200, json!({}))],
        vec![(
            Patch,
            "/api/cards/TER-1",
            vec![],
            Some(json!({"title": "T", "repo": null})),
        )],
    )
    .await;
    ok(
        false,
        &["card", "claim", "TER-1"],
        vec![(200, json!({}))],
        vec![req(Post, "/api/cards/TER-1/claim")],
    )
    .await;
    ok(
        false,
        &[
            "card",
            "move",
            "TER-1",
            "review",
            "-m",
            "look at the parser",
        ],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/cards/TER-1/move",
            vec![],
            Some(json!({"column": "review", "comment": "look at the parser"})),
        )],
    )
    .await;
    ok(
        false,
        &["card", "assign", "TER-1", "none"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/cards/TER-1/assign",
            vec![],
            Some(json!({"assignee": null})),
        )],
    )
    .await;
    ok(
        false,
        &["card", "assign", "TER-1", "worker"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/cards/TER-1/assign",
            vec![],
            Some(json!({"assignee": "worker"})),
        )],
    )
    .await;
    ok_in(
        false,
        &["card", "comment", "TER-1", "-"],
        vec![(201, json!({}))],
        Some("progress"),
        vec![(
            Post,
            "/api/cards/TER-1/comments",
            vec![],
            Some(json!({"body": "progress"})),
        )],
    )
    .await;
    ok(
        false,
        &["card", "link", "TER-1", "--branch", "feat/x"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/cards/TER-1/links",
            vec![],
            Some(json!({"kind": "branch", "ref": "feat/x"})),
        )],
    )
    .await;
    ok(
        false,
        &["card", "link", "TER-1", "--thread", "T1"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/cards/TER-1/links",
            vec![],
            Some(json!({"kind": "thread", "ref": "T1"})),
        )],
    )
    .await;
    ok(
        false,
        &["card", "rm", "TER-1"],
        vec![(204, Value::Null)],
        vec![req(Delete, "/api/cards/TER-1")],
    )
    .await;
    ok(
        false,
        &["card", "events", "TER-1"],
        vec![],
        vec![req(Get, "/api/cards/TER-1/events")],
    )
    .await;
}

#[tokio::test]
async fn card_wait_long_polls_for_an_assignment() {
    let out = ok(
        true,
        &["card", "wait", "--mine", "--timeout", "40"],
        vec![
            (200, json!([])),
            (
                200,
                json!([{"id": "TER-4", "title": "New work", "column": "ready"}]),
            ),
        ],
        vec![
            (
                Get,
                "/api/projects/terra/cards",
                vec![("assignee", "me")],
                None,
            ),
            (
                Get,
                "/api/projects/terra/cards",
                vec![("assignee", "me"), ("wait", "40")],
                None,
            ),
        ],
    )
    .await;
    assert_eq!(out, "assigned to you: TER-4: New work [ready]\n");
}

#[tokio::test]
async fn card_wait_watches_card_events_after_now() {
    let out = ok(
        true,
        &["card", "wait", "--mine", "--timeout", "30"],
        vec![
            (200, json!([{"id": "TER-1"}])),
            (200, json!([{"seq": 4}, {"seq": 5}])),
            (
                200,
                json!([{"seq": 6, "kind": "card", "agent_id": "someone-else"},
                         {"seq": 7, "kind": "card", "agent_id": ME, "summary": "TER-3 assigned"}]),
            ),
        ],
        vec![
            (
                Get,
                "/api/projects/terra/cards",
                vec![("assignee", "me")],
                None,
            ),
            (
                Get,
                "/api/events",
                vec![("kind", "card"), ("project", "terra")],
                None,
            ),
            (
                Get,
                "/api/events",
                vec![
                    ("kind", "card"),
                    ("project", "terra"),
                    ("since", "5"),
                    ("wait", "30"),
                ],
                None,
            ),
        ],
    )
    .await;
    assert!(out.contains("TER-3 assigned"), "{out}");
    assert!(!out.contains("someone-else"), "{out}");
}

#[tokio::test]
async fn memory() {
    ok(
        true,
        &["mem", "search", "parser", "--tag", "t", "--limit", "5"],
        vec![],
        vec![(
            Get,
            "/api/projects/terra/memories",
            vec![("q", "parser"), ("tag", "t"), ("limit", "5")],
            None,
        )],
    )
    .await;
    let out = ok(
        true,
        &["mem", "show", "MEM-1"],
        vec![(
            200,
            json!({"depth": 1,
                   "nodes": [{"id": "MEM-2", "title": "X is Z", "superseded_by": "MEM-3"},
                             {"id": "MEM-1", "title": "X is Y", "body": "because", "tags": ["a"],
                              "sources": [{"kind": "card", "ref": "TER-1"}]}],
                   "edges": [{"from": "MEM-1", "to": "MEM-4", "relation": "supports"},
                             {"from": "MEM-1", "to": "MEM-2", "relation": "contradicts"}]}),
        )],
        vec![(Get, "/api/memories/MEM-1/graph", vec![("depth", "1")], None)],
    )
    .await;
    assert!(out.contains("MEM-1  X is Y"), "{out}");
    assert!(out.contains("card:TER-1"), "{out}");
    // Disagreement comes first.
    assert!(
        out.contains("links:\n  MEM-1 -[contradicts]-> MEM-2\n  MEM-1 -[supports]-> MEM-4"),
        "{out}"
    );
    assert!(out.contains("MEM-2  X is Z  (superseded)"), "{out}");
    ok(
        true,
        &["mem", "show", "MEM-1", "--depth", "0"],
        vec![(200, json!({}))],
        vec![req(Get, "/api/memories/MEM-1")],
    )
    .await;
    let new_body = json!({"title": "X is Y", "body": "why", "tags": ["a"],
        "sources": [{"kind": "card", "ref": "TER-1"}], "links": [{"to": "MEM-2", "relation": "supports"}]});
    let args = [
        "mem",
        "new",
        "X is Y",
        "--body",
        "-",
        "--tag",
        "a",
        "--source",
        "card:TER-1",
        "--link",
        "MEM-2:supports",
    ];
    let out = ok_in(
        true,
        &args,
        vec![
            (200, json!([{"id": "MEM-9", "title": "X might be Y"}])),
            (201, json!({"id": "MEM-10", "title": "X is Y"})),
        ],
        Some("why"),
        vec![
            (
                Get,
                "/api/projects/terra/memories",
                vec![("q", "X is Y"), ("limit", "5")],
                None,
            ),
            (
                Post,
                "/api/projects/terra/memories",
                vec![],
                Some(new_body.clone()),
            ),
        ],
    )
    .await;
    assert!(
        out.contains("similar memories") && out.contains("MEM-9"),
        "{out}"
    );
    assert!(out.ends_with("created MEM-10: X is Y\n"), "{out}");
    // With --json the CLI skips the duplicate check.
    let mut jargs = vec!["--json"];
    jargs.extend(args);
    ok_in(
        true,
        &jargs,
        vec![(201, json!({}))],
        Some("why"),
        vec![(Post, "/api/projects/terra/memories", vec![], Some(new_body))],
    )
    .await;
    ok(
        true,
        &["mem", "link", "MEM-1", "MEM-2", "supports", "--note", "n"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/memories/MEM-1/links",
            vec![],
            Some(json!({"to": "MEM-2", "relation": "supports", "note": "n"})),
        )],
    )
    .await;
    ok(
        true,
        &["mem", "unlink", "MEM-1", "MEM-2", "supports"],
        vec![(204, Value::Null)],
        vec![(
            Delete,
            "/api/memories/MEM-1/links",
            vec![],
            Some(json!({"to": "MEM-2", "relation": "supports"})),
        )],
    )
    .await;
    ok(
        true,
        &["mem", "edit", "MEM-1", "--tag", "x"],
        vec![(200, json!({}))],
        vec![(
            Patch,
            "/api/memories/MEM-1",
            vec![],
            Some(json!({"tags": ["x"]})),
        )],
    )
    .await;
    ok(
        true,
        &["mem", "supersede", "MEM-1", "MEM-3"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/memories/MEM-1/supersede",
            vec![],
            Some(json!({"by": "MEM-3"})),
        )],
    )
    .await;
    ok(
        true,
        &["mem", "neighbors", "MEM-1", "--relation", "contradicts"],
        vec![(200, json!({}))],
        vec![(
            Get,
            "/api/memories/MEM-1/graph",
            vec![("depth", "1"), ("relation", "contradicts")],
            None,
        )],
    )
    .await;
    ok(
        true,
        &["mem", "ls", "--mine", "--recent", "5"],
        vec![],
        vec![(
            Get,
            "/api/projects/terra/memories",
            vec![("author", ME), ("limit", "5")],
            None,
        )],
    )
    .await;
    ok(
        false,
        &["mem", "rm", "MEM-1"],
        vec![(204, Value::Null)],
        vec![req(Delete, "/api/memories/MEM-1")],
    )
    .await;
}

#[tokio::test]
async fn wiki() {
    let out = ok(
        true,
        &["wiki", "ls", "reports"],
        vec![(
            200,
            json!([{"path": "index"}, {"path": "reports"}, {"path": "reports/perf"}, {"path": "reportsx"}]),
        )],
        vec![req(Get, "/api/projects/terra/wiki")],
    )
    .await;
    assert!(
        out.contains("reports/perf") && !out.contains("reportsx") && !out.contains("index"),
        "{out}"
    );
    let out = ok(
        true,
        &["wiki", "show", "reports/perf", "--rev", "2"],
        vec![(200, json!({"path": "reports/perf", "rev": 2, "author_name": "terra/worker", "body": "# Perf\n"}))],
        vec![req(Get, "/api/wiki/terra%2Freports%2Fperf/revisions/2")],
    )
    .await;
    assert_eq!(out, "reports/perf  rev 2  by terra/worker\n# Perf\n");
    ok(
        true,
        &["wiki", "search", "perf"],
        vec![],
        vec![(Get, "/api/projects/terra/wiki", vec![("q", "perf")], None)],
    )
    .await;
    let out = ok_in(
        true,
        &["wiki", "new", "reports/perf", "--body", "-", "-m", "first"],
        vec![(
            201,
            json!({"path": "reports/perf", "rev": 1, "url": "http://dash/wiki/terra/reports/perf"}),
        )],
        Some("# Perf"),
        vec![(
            Post,
            "/api/projects/terra/wiki",
            vec![],
            Some(json!({"path": "reports/perf", "body": "# Perf", "summary": "first"})),
        )],
    )
    .await;
    assert!(out.contains("http://dash/wiki/terra/reports/perf"), "{out}");
    ok(
        true,
        &[
            "wiki", "edit", "index", "--body", "new", "--base", "3", "-m", "s",
        ],
        vec![(200, json!({}))],
        vec![(
            Put,
            "/api/wiki/terra%2Findex",
            vec![],
            Some(json!({"body": "new", "base": 3, "summary": "s"})),
        )],
    )
    .await;
    ok(
        true,
        &["wiki", "section", "index", "Status", "--body", "x"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/wiki/terra%2Findex/section",
            vec![],
            Some(json!({"heading": "Status", "body": "x"})),
        )],
    )
    .await;
    ok(
        true,
        &["wiki", "append", "index", "--body", "x", "-m", "more"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/wiki/terra%2Findex/append",
            vec![],
            Some(json!({"body": "x", "summary": "more"})),
        )],
    )
    .await;
    ok(
        true,
        &["wiki", "history", "index"],
        vec![],
        vec![req(Get, "/api/wiki/terra%2Findex/revisions")],
    )
    .await;
    ok(
        true,
        &["wiki", "diff", "index", "1", "2"],
        vec![(200, json!({"diff": "-a\n+b\n"}))],
        vec![(
            Get,
            "/api/wiki/terra%2Findex/diff",
            vec![("a", "1"), ("b", "2")],
            None,
        )],
    )
    .await;
    ok(
        true,
        &["wiki", "mv", "a", "b"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/wiki/terra%2Fa/move",
            vec![],
            Some(json!({"path": "b"})),
        )],
    )
    .await;
    ok(
        true,
        &["wiki", "rm", "a"],
        vec![(204, Value::Null)],
        vec![req(Delete, "/api/wiki/terra%2Fa")],
    )
    .await;
    ok(
        true,
        &["wiki", "restore", "a", "2"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/wiki/terra%2Fa/restore",
            vec![],
            Some(json!({"rev": 2})),
        )],
    )
    .await;
    ok(
        true,
        &["wiki", "backlinks", "a"],
        vec![],
        vec![req(Get, "/api/wiki/terra%2Fa/backlinks")],
    )
    .await;
}

#[tokio::test]
async fn a_stale_wiki_edit_prints_the_diff_from_the_details() {
    let r = exec(
        true,
        &["wiki", "edit", "index", "--body", "new", "--base", "3", "-m", "s"],
        vec![(
            409,
            json!({"error": {"code": "stale_base", "message": "page `index` is at rev 5, not your base rev 3",
                             "details": {"base": 3, "current": 5, "diff": "-old\n+theirs\n"}}}),
        )],
        None,
    )
    .await;
    assert!(r.result.is_err());
    assert_eq!(r.reqs.len(), 1);
    assert_eq!(r.out, "changes since rev 3 (now rev 5):\n-old\n+theirs\n");
}

#[tokio::test]
async fn a_stale_wiki_edit_prints_the_diff() {
    let r = exec(
        true,
        &["wiki", "edit", "index", "--body", "new", "--base", "3"],
        vec![
            (409, json!({"error": {"code": "conflict", "message": "base rev 3 is stale; the page is at rev 5"}})),
            (200, json!({"current_rev": 5})),
            (200, json!({"diff": "+someone else's line\n"})),
        ],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    assert_eq!(e.to_string(), "base rev 3 is stale; the page is at rev 5");
    assert!(
        r.out
            .contains("changes since rev 3 (now rev 5):\n+someone else's line"),
        "{}",
        r.out
    );
    assert_eq!(
        r.reqs[2].query,
        vec![("a".into(), "3".into()), ("b".into(), "5".into())]
    );
}

#[tokio::test]
async fn artifacts_and_versions() {
    let dir = std::env::temp_dir().join(format!("terrarium-push-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("report.html");
    std::fs::write(&file, "<p>ok</p>").unwrap();
    let path = file.to_string_lossy().into_owned();
    let r = exec(
        true,
        &["artifact", "push", &path, "--card", "TER-1"],
        vec![(201, json!({"id": "ART-1", "name": "report.html", "kind": "file", "url": "http://a/ART-1/report.html"}))],
        None,
    )
    .await;
    r.result.unwrap();
    assert_eq!(r.reqs[0].path, "/api/projects/terra/artifacts");
    assert_eq!(
        r.reqs[0].body,
        Some(
            json!({"name": "report.html", "content_type": "text/html", "kind": "file",
                    "data_base64": "PHA+b2s8L3A+", "attach": {"kind": "card", "ref": "TER-1"}})
        )
    );
    assert!(r.out.contains("http://a/ART-1/report.html"), "{}", r.out);
    std::fs::remove_dir_all(&dir).ok();

    ok(
        true,
        &["artifact", "ls", "--page", "index"],
        vec![],
        vec![(
            Get,
            "/api/projects/terra/artifacts",
            vec![("page", "index")],
            None,
        )],
    )
    .await;
    let out = ok(
        true,
        &["artifact", "url", "ART-1"],
        vec![(200, json!({"url": "http://a/1", "exp": 1}))],
        vec![req(Post, "/api/artifacts/ART-1/url")],
    )
    .await;
    assert_eq!(out, "http://a/1\n");
    ok(
        true,
        &["artifact", "rm", "ART-1"],
        vec![(204, Value::Null)],
        vec![req(Delete, "/api/artifacts/ART-1")],
    )
    .await;

    ok(
        true,
        &["version", "new", "v1", "--notes", "n", "--repo", "terrarium", "--ref", "abc", "--card", "TER-1"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/projects/terra/versions",
            vec![],
            Some(json!({"name": "v1", "notes": "n", "repo": "terrarium", "git_ref": "abc", "cards": ["TER-1"]})),
        )],
    )
    .await;
    ok(
        true,
        &["version", "ls"],
        vec![],
        vec![req(Get, "/api/projects/terra/versions")],
    )
    .await;
    ok(
        true,
        &["version", "show", "v1"],
        vec![(200, json!({}))],
        vec![req(Get, "/api/versions/terra%2Fv1")],
    )
    .await;
    ok(
        true,
        &["version", "attach", "v1", "ART-1"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/artifacts/ART-1/links",
            vec![],
            Some(json!({"kind": "version", "ref": "v1"})),
        )],
    )
    .await;
    for (action, path) in [
        ("submit", "/api/versions/terra%2Fv1/submit"),
        ("release", "/api/versions/terra%2Fv1/release"),
    ] {
        ok(
            true,
            &["version", action, "v1"],
            vec![(200, json!({}))],
            vec![(Post, path, vec![], Some(json!({})))],
        )
        .await;
    }
    ok(
        false,
        &["-p", "terra", "version", "reject", "v1", "-m", "no"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/versions/terra%2Fv1/reject",
            vec![],
            Some(json!({"comment": "no"})),
        )],
    )
    .await;
    let out = ok(
        true,
        &["version", "wait", "v1", "--timeout", "20"],
        vec![
            (200, json!([])),
            (200, json!({"name": "v1", "status": "in_review"})),
            (200, json!([{"name": "v1", "status": "approved"}])),
        ],
        vec![
            (
                Get,
                "/api/projects/terra/versions",
                vec![("name", "v1"), ("status", "approved"), ("wait", "20")],
                None,
            ),
            req(Get, "/api/versions/terra%2Fv1"),
            (
                Get,
                "/api/projects/terra/versions",
                vec![("name", "v1"), ("status", "approved"), ("wait", "20")],
                None,
            ),
        ],
    )
    .await;
    assert!(out.starts_with("v1  [approved]"), "{out}");
    // A rejection ends the wait too.
    let out = ok(
        true,
        &["version", "wait", "v1", "--timeout", "100"],
        vec![
            (200, json!([])),
            (200, json!({"name": "v1", "status": "rejected"})),
        ],
        vec![
            (
                Get,
                "/api/projects/terra/versions",
                vec![("name", "v1"), ("status", "approved"), ("wait", "30")],
                None,
            ),
            req(Get, "/api/versions/terra%2Fv1"),
        ],
    )
    .await;
    assert!(out.starts_with("v1  [rejected]"), "{out}");
}

#[tokio::test]
async fn usage_and_status() {
    ok(
        true,
        &["usage"],
        vec![(200, json!([]))],
        vec![(Get, "/api/usage", vec![("agent", ME)], None)],
    )
    .await;
    ok(
        true,
        &["usage", "project"],
        vec![(200, json!([]))],
        vec![(Get, "/api/usage", vec![("project", "terra")], None)],
    )
    .await;
    ok(
        true,
        &["usage", "subscriptions"],
        vec![(200, json!([]))],
        vec![req(Get, "/api/subscriptions")],
    )
    .await;
    let out = ok(
        true,
        &["usage", "subscriptions"],
        vec![(
            200,
            json!([{"name": "anthropic-max", "provider": "anthropic"}]),
        )],
        vec![req(Get, "/api/subscriptions")],
    )
    .await;
    assert!(
        out.contains("NAME") && out.contains("anthropic-max"),
        "{out}"
    );
    ok(
        true,
        &["status", "set", "working", "--note", "on TER-1"],
        vec![(200, json!({}))],
        vec![(
            Post,
            "/api/agents/01AGENTAGENTAGENTAGENTAGEN/status",
            vec![],
            Some(json!({"status": "working", "note": "on TER-1"})),
        )],
    )
    .await;
}

#[tokio::test]
async fn admin() {
    ok(
        true,
        &[
            "admin",
            "agent",
            "new",
            "helper",
            "--harness",
            "claude",
            "--prompt",
            "leader-worker",
            "--role",
            "project_admin",
        ],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/agents",
            vec![],
            Some(
                json!({"name": "helper", "project": "terra", "harness": "claude",
                        "prompt": "leader-worker", "roles": ["project_admin"]}),
            ),
        )],
    )
    .await;
    ok(
        true,
        &["admin", "agent", "stop", "helper"],
        vec![(202, json!({}))],
        vec![req(Post, "/api/agents/helper/stop")],
    )
    .await;
    ok(
        true,
        &[
            "admin",
            "agent",
            "set",
            "helper",
            "--subscription",
            "openai-codex",
        ],
        vec![(200, json!({}))],
        vec![(
            Patch,
            "/api/agents/helper",
            vec![],
            Some(json!({"subscription": "openai-codex"})),
        )],
    )
    .await;
    ok(
        true,
        &["admin", "project", "new", "x"],
        vec![(201, json!({}))],
        vec![(Post, "/api/projects", vec![], Some(json!({"name": "x"})))],
    )
    .await;
    ok(
        true,
        &["admin", "role", "grant", "helper", "project_admin"],
        vec![
            (200, json!(["sysadmin"])),
            (200, json!(["sysadmin", "project_admin"])),
        ],
        vec![
            req(Get, "/api/agents/helper/roles"),
            (
                Put,
                "/api/agents/helper/roles",
                vec![],
                Some(json!({"roles": ["sysadmin", "project_admin"]})),
            ),
        ],
    )
    .await;
    let out = ok(
        true,
        &["admin", "role", "revoke", "helper", "sysadmin"],
        vec![(200, json!({"roles": ["sysadmin"]})), (200, json!([]))],
        vec![
            req(Get, "/api/agents/helper/roles"),
            (
                Put,
                "/api/agents/helper/roles",
                vec![],
                Some(json!({"roles": []})),
            ),
        ],
    )
    .await;
    assert_eq!(out, "helper: member\n");
    ok(
        true,
        &[
            "admin",
            "target",
            "new",
            "box",
            "--kind",
            "ssh",
            "--address",
            "a@b",
        ],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/targets",
            vec![],
            Some(json!({"name": "box", "kind": "ssh", "address": "a@b", "project": "terra"})),
        )],
    )
    .await;
    ok(
        true,
        &["admin", "target", "new", "box", "--kind", "ssh", "--global"],
        vec![(201, json!({}))],
        vec![(
            Post,
            "/api/targets",
            vec![],
            Some(json!({"name": "box", "kind": "ssh"})),
        )],
    )
    .await;
    ok(
        true,
        &["admin", "host", "ls"],
        vec![],
        vec![req(Get, "/api/hosts")],
    )
    .await;
    ok(
        true,
        &["admin", "audit", "--since", "2026-10-24T00:00:00Z"],
        vec![],
        vec![(
            Get,
            "/api/audit",
            vec![("since", "2026-10-24T00:00:00Z"), ("limit", "100")],
            None,
        )],
    )
    .await;
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[tokio::test]
async fn exec_streams_and_passes_the_exit_code() {
    let poll = |since: &'static str| {
        (
            Get,
            "/api/targets/box/sessions/S1/output",
            vec![("since", since), ("wait", "25")],
            None,
        )
    };
    let r = exec(
        true,
        &[
            "exec", "box", "--cwd", "/srv", "-e", "A=1", "--", "sh", "-c", "x",
        ],
        vec![
            (201, json!({"session_id": "S1"})),
            (200, json!({"accepted": true})),
            (
                200,
                json!({"chunks": [{"stream": "stdout", "data": b64(b"hi\n")},
                                    {"stream": "stderr", "data": b64(b"err")}],
                         "next": 6, "done": false}),
            ),
            (
                200,
                json!({"chunks": [], "next": 6, "done": true,
                         "end": {"type": "exit", "code": 3, "reason": "exited"}}),
            ),
        ],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    assert_eq!(exit_code_of(&e), 3);
    assert_eq!(r.out, "hi\nerr");
    check(
        &["exec"],
        &r.reqs,
        &[
            (
                Post,
                "/api/targets/box/exec",
                vec![],
                Some(json!({"cmd": ["sh", "-c", "x"], "cwd": "/srv", "env": {"A": "1"}})),
            ),
            (
                Post,
                "/api/targets/box/sessions/S1/input",
                vec![],
                Some(json!({"eof": true})),
            ),
            poll("0"),
            poll("6"),
        ],
    );
    // Exit 0 is success; no command is a usage error.
    ok(
        true,
        &["exec", "box", "--", "true"],
        vec![
            (201, json!({"session_id": "S1"})),
            (200, json!({})),
            (
                200,
                json!({"chunks": [], "next": 0, "done": true, "end": {"type": "exit", "code": 0}}),
            ),
        ],
        vec![
            (
                Post,
                "/api/targets/box/exec",
                vec![],
                Some(json!({"cmd": ["true"]})),
            ),
            (
                Post,
                "/api/targets/box/sessions/S1/input",
                vec![],
                Some(json!({"eof": true})),
            ),
            poll("0"),
        ],
    )
    .await;
    let r = exec(true, &["exec", "box"], vec![], None).await;
    assert_eq!(exit_code_of(&r.result.unwrap_err()), exit::USAGE);
    assert!(r.reqs.is_empty());
    // A run that failed on the host is a server error.
    let r = exec(
        true,
        &["exec", "box", "--", "nope"],
        vec![
            (201, json!({"session_id": "S1"})),
            (200, json!({})),
            (200, json!({"chunks": [], "next": 0, "done": true,
                         "end": {"type": "error", "code": "spawn_failed", "message": "no such file"}})),
        ],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    assert!(e.to_string().contains("no such file"), "{e}");
    assert_eq!(exit_code_of(&e), exit::SERVER);
}

#[tokio::test]
async fn cp_pushes_and_pulls_in_chunks_with_sha256() {
    use sha2::{Digest, Sha256};
    let dir = std::env::temp_dir().join(format!("terrarium-cp-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("in.bin");
    let data: Vec<u8> = (0..(super::remote::CHUNK + 10))
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(&src, &data).unwrap();
    let digest: String = Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let src_s = src.to_str().unwrap().to_owned();
    let r = exec(
        true,
        &["cp", &src_s, "box:/srv/out/"],
        vec![
            (201, json!({"session_id": "C1"})),
            (200, json!({})),
            (200, json!({})),
            (200, json!({})),
            (
                200,
                json!({"chunks": [], "next": 0, "done": true,
                         "end": {"type": "cp", "bytes": data.len(), "sha256": digest}}),
            ),
        ],
        None,
    )
    .await;
    r.result.unwrap();
    assert_eq!(r.reqs.len(), 5);
    let open = r.reqs[0].body.clone().unwrap();
    assert_eq!(open["direction"], "push");
    assert_eq!(open["path"], "/srv/out/in.bin");
    let first = r.reqs[1].body.as_ref().unwrap()["data"]
        .as_str()
        .unwrap()
        .len();
    assert_eq!(first, b64(&data[..super::remote::CHUNK]).len());
    assert_eq!(
        r.reqs[3].body,
        Some(json!({"finish": {"bytes": data.len(), "sha256": digest}}))
    );
    assert!(r.out.contains("copied"), "{}", r.out);

    // Pull: the bytes land in place only when the digest matches.
    let dst = dir.join("back.bin");
    let dst_s = dst.to_str().unwrap().to_owned();
    let replies = |sha: &str| {
        vec![
            (201, json!({"session_id": "C2"})),
            (
                200,
                json!({"chunks": [{"stream": "file", "data": b64(&data)}], "next": data.len(), "done": true,
                         "end": {"type": "cp", "bytes": data.len(), "sha256": sha}}),
            ),
        ]
    };
    let r = exec(
        true,
        &["cp", "box:/srv/f.bin", &dst_s],
        replies(&digest),
        None,
    )
    .await;
    r.result.unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), data);
    assert_eq!(
        r.reqs[0].body,
        Some(json!({"direction": "pull", "path": "/srv/f.bin"}))
    );
    let bad = dir.join("bad.bin");
    let r = exec(
        true,
        &["cp", "box:/srv/f.bin", bad.to_str().unwrap()],
        replies("00"),
        None,
    )
    .await;
    assert!(r.result.is_err());
    assert!(!bad.exists());
    // Both or neither side remote: a usage error.
    let r = exec(true, &["cp", "a", "b"], vec![], None).await;
    assert_eq!(exit_code_of(&r.result.unwrap_err()), exit::USAGE);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn remote_paths() {
    use super::remote::split_remote;
    assert_eq!(split_remote("box:/tmp/a"), Some(("box", "/tmp/a")));
    assert_eq!(split_remote("./a:b"), None);
    assert_eq!(split_remote("/tmp/x"), None);
    assert_eq!(split_remote("box:"), None);
}

#[tokio::test]
async fn agent_targets_and_hosts() {
    let out = ok(
        true,
        &["targets", "ls"],
        vec![(
            200,
            json!([{"name": "box", "kind": "host", "host": "lab1", "host_online": true}]),
        )],
        vec![(Get, "/api/targets", vec![("usable", "true")], None)],
    )
    .await;
    assert!(out.contains("online"), "{out}");
    let out = ok(
        false,
        &["admin", "host", "new", "lab1"],
        vec![(201, json!({"name": "lab1", "token": "thk_abc"}))],
        vec![(Post, "/api/hosts", vec![], Some(json!({"name": "lab1"})))],
    )
    .await;
    assert_eq!(out, "thk_abc\n");
    ok(
        false,
        &["admin", "host", "rm", "lab1"],
        vec![(204, Value::Null)],
        vec![req(Delete, "/api/hosts/lab1")],
    )
    .await;
    ok(
        false,
        &[
            "admin",
            "target",
            "verb",
            "win",
            "snapshot",
            "--snapshot",
            "clean",
        ],
        vec![(200, json!({"state": "running"}))],
        vec![(
            Post,
            "/api/targets/win/snapshot",
            vec![],
            Some(json!({"name": "clean"})),
        )],
    )
    .await;
    ok(
        true,
        &[
            "admin",
            "target",
            "new",
            "d",
            "--kind",
            "docker",
            "--host",
            "lab1",
            "--image",
            "alpine",
            "--ephemeral",
        ],
        vec![(201, json!({"name": "d"}))],
        vec![(
            Post,
            "/api/targets",
            vec![],
            Some(
                json!({"name": "d", "kind": "docker", "host": "lab1", "image": "alpine",
                        "ephemeral": true, "project": "terra"}),
            ),
        )],
    )
    .await;
}

#[tokio::test]
async fn agent_attach_is_not_available_through_the_tunnel() {
    // The tunnel does not carry the PTY WebSocket.
    let r = exec(true, &["admin", "agent", "attach", "w"], vec![], None).await;
    assert_eq!(exit_code_of(&r.result.unwrap_err()), exit::USAGE);
}

#[tokio::test]
async fn policy_refusals_keep_the_message() {
    let r = exec(
        true,
        &["mail", "send", "ops/dev", "hi"],
        vec![(
            403,
            json!({"error": {"code": "forbidden", "message":
                "`ops/dev` is outside project `terrarium`; add \"ops/dev\" to policy.cross_project, or write to the project inbox `project:ops`"}}),
        )],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    assert_eq!(exit_code_of(&e), exit::REFUSED);
    assert!(
        e.to_string()
            .starts_with("`ops/dev` is outside project `terrarium`")
    );
    let r = exec(
        true,
        &["card", "claim", "TER-2"],
        vec![(
            409,
            json!({"error": {"code": "dependencies_unmet", "message": "TER-2 depends on TER-1 that are not done",
                             "details": {"blocked_by": ["TER-1"]}}}),
        )],
        None,
    )
    .await;
    let e = r.result.unwrap_err();
    let s = e.downcast_ref::<ServerError>().unwrap();
    assert_eq!(s.code, "dependencies_unmet");
    assert_eq!(s.details, Some(json!({"blocked_by": ["TER-1"]})));
    assert_eq!(exit_code_of(&e), exit::SERVER);
}

#[tokio::test]
async fn json_prints_the_server_answer() {
    let out = ok(
        false,
        &["--json", "card", "show", "TER-1"],
        vec![(200, json!({"id": "TER-1", "title": "x"}))],
        vec![req(Get, "/api/cards/TER-1")],
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v, json!({"id": "TER-1", "title": "x"}));
}
