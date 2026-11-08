//! Reproducible Inbox load at approximately today's counts and ten times that.
#![allow(clippy::unwrap_used)]
use crate::common::TestDaemon;
use rusqlite::params;
use std::time::{Duration, Instant};

#[tokio::test]
async fn inbox_live_and_tenfold_load() {
    for scale in [1, 10] {
        let td = TestDaemon::start().await;
        td.project("p").await;
        let agent = td.agent("p", "lead", serde_json::json!({})).await;
        let agent = agent["id"].as_str().unwrap().to_owned();
        let db = &td.daemon.as_ref().unwrap().state.db;
        db.write("operator", move |t| {
            let at = "2026-11-08T00:00:00Z";
            let body = "Representative message content. ".repeat(100);
            for i in 0..317 * scale {
                let (to, kind) = if i < 257 * scale { ("operator", "operator") } else { ("project:p", "project") };
                t.execute("INSERT INTO messages (id,thread_id,from_agent,to_agent,subject,body,created_at,from_kind,to_kind) VALUES (?1,?1,?2,?3,'Inbox performance',?4,?5,'agent',?6)", params![format!("mail-{i:06}"), agent, to, body, at, kind])?;
            }
            for i in 0..100 * scale {
                let id = format!("thread-{i:06}");
                t.execute("INSERT INTO threads (id,topic,scope,opened_by,agent,status,created_at,updated_at) VALUES (?1,'Performance','p','operator',?2,'waiting_operator',?3,?3)", params![id, agent, at])?;
                for j in 0..20 {
                    t.execute("INSERT INTO thread_messages (id,thread,author,kind,text,at) VALUES (?1,?2,?3,'message',?4,?5)", params![format!("{id}-{j}"), id, agent, body, at])?;
                }
            }
            for i in 0..30 * scale {
                t.execute("INSERT INTO questions (id,agent,project,text,thread_id,created_at,updated_at,kind) VALUES (?1,?2,'p','Question',?1,?3,?3,?4)", params![format!("question-{i:06}"), agent, at, if i % 3 == 0 { "notice" } else { "question" }])?;
            }
            Ok(())
        }).await.unwrap();
        db.read(move |c| {
            for sql in [
                "SELECT * FROM messages WHERE to_kind = 'operator' AND state IN ('queued','delivered') ORDER BY id DESC LIMIT 1000",
                "SELECT count(*), max(at) FROM thread_messages WHERE thread = 'thread-000001'",
                "SELECT count(*) FROM questions WHERE thread = 'thread-000001' AND status = 'open'",
            ] {
                let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                let plan = st.query_map([], |r| r.get::<_, String>(3))?.collect::<Result<Vec<_>, _>>()?;
                println!("scale={scale} SQL={sql} plan={plan:?}");
            }
            Ok(())
        }).await.unwrap();
        for path in [
            "/api/inbox",
            "/api/inbox/counts",
            "/api/threads",
            "/api/mail?to=project:p",
        ] {
            let mut times = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                let response = td.get(path).await;
                assert_eq!(response.status, 200, "{}", response.body);
                times.push(start.elapsed());
            }
            times.sort();
            println!(
                "scale={scale} {path} median={:?} max={:?}",
                times[2], times[4]
            );
            assert!(times[2] < Duration::from_secs(2), "{path}: {times:?}");
        }
        for action in ["ack", "answer"] {
            let mut times = Vec::new();
            for i in 0..5 {
                let index = i * 3 + usize::from(action == "answer");
                let path = format!("/api/questions/question-{index:06}/{action}");
                let body = if action == "answer" {
                    serde_json::json!({"answer": "Accepted"})
                } else {
                    serde_json::json!({})
                };
                let start = Instant::now();
                let response = td.post(&path, body).await;
                assert_eq!(response.status, 200, "{}", response.body);
                times.push(start.elapsed());
            }
            times.sort();
            println!(
                "scale={scale} POST {action} median={:?} max={:?}",
                times[2], times[4]
            );
            assert!(times[2] < Duration::from_secs(2), "{action}: {times:?}");
        }
        let response = td.get("/api/inbox/counts?project=p").await;
        let counts = response.ok();
        assert_eq!(counts["mail"], 257 * scale);
        assert_eq!(counts["thread"], 100 * scale);
        assert_eq!(counts["open"], (257 + 100 + 30) * scale - 10);
    }
}
