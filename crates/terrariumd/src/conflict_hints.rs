//! Advisory file overlaps between active card branches. Never gates a move.
use std::collections::BTreeSet;
use std::time::Duration;

use rusqlite::params;

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::{board, mail, repos};
use mail::{Address, Outgoing};

#[derive(Clone)]
struct Card {
    id: String,
    project: String,
    repo: String,
    branch: String,
}

fn active(c: &rusqlite::Connection) -> ApiResult<Vec<Card>> {
    let mut q = c.prepare(
        "SELECT c.id, c.project, c.repo, c.branch FROM cards c
         JOIN board_columns b ON b.id = c.column_id
         WHERE c.archived = 0 AND (b.role IN ('working', 'merge_queue')
         OR b.name = 'merge_queue') AND c.repo IS NOT NULL AND c.branch IS NOT NULL
         ORDER BY c.id",
    )?;
    Ok(q.query_map([], |r| {
        Ok(Card {
            id: r.get(0)?,
            project: r.get(1)?,
            repo: r.get(2)?,
            branch: r.get(3)?,
        })
    })?
    .collect::<Result<_, _>>()?)
}

fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Vec<String> {
    a.intersection(b).cloned().collect()
}

async fn files(dir: &std::path::Path, branch: &str) -> Option<BTreeSet<String>> {
    // Mirror refs are local heads. A three-dot diff uses the merge base with next.
    let range = format!("refs/heads/next...refs/heads/{branch}");
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(["diff", "--name-only", "--no-renames", "-z", &range, "--"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8(out.stdout)
            .ok()?
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Called by a separate timer so network fetches cannot delay agent lifecycle work.
pub async fn pass(state: &Shared) {
    if let Err(e) = check(state).await {
        tracing::warn!("conflict hints: {e}");
    }
}

async fn check(state: &Shared) -> ApiResult<()> {
    let cards = state.db.read(active).await?;
    let mut scanned = Vec::new();
    for card in cards {
        let project = card.project.clone();
        let name = card.repo.clone();
        let repo = match state
            .db
            .read(move |c| repos::resolve(c, &name, Some(&project)))
            .await
        {
            Ok(repo) => repo,
            Err(_) => continue,
        };
        if crate::api::repo_trees::ensure_mirror(state, &repo, false)
            .await
            .is_err()
        {
            continue;
        }
        let dir = crate::api::repo_trees::mirror_dir(state, &repo);
        if let Some(paths) = files(&dir, &card.branch).await {
            scanned.push((card, repo.id, paths));
        }
    }
    for (i, (a, repo, paths)) in scanned.iter().enumerate() {
        for (b, other_repo, other_paths) in &scanned[i + 1..] {
            if a.project != b.project || repo != other_repo {
                continue;
            }
            let common = overlap(paths, other_paths);
            if common.is_empty() {
                continue;
            }
            let a = a.clone();
            let b = b.clone();
            let body = format!(
                "Conflict hint: {} and {} touch the same files: {}. Advisory only. Coordinate work or merge order. Based on branch changes against next.",
                a.id,
                b.id,
                common.join(", ")
            );
            state
                .db
                .write("system", move |t| publish(t, &a, &b, &body))
                .await?;
        }
    }
    Ok(())
}

fn publish(t: &mut crate::db::Tx<'_>, a: &Card, b: &Card, body: &str) -> ApiResult<()> {
    // Persisted deduplication also survives daemon restarts.
    let seen: bool = t.query_row("SELECT EXISTS(SELECT 1 FROM card_comments WHERE card = ?1 AND author = 'system' AND body = ?2)", params![a.id, body], |r| r.get(0))?;
    if seen {
        return Ok(());
    }
    board::system_comment(t, &a.id, body, None)?;
    board::system_comment(t, &b.id, body, None)?;
    let mut recipients = crate::question_guard::leads(t, &a.project, "")?;
    if recipients.is_empty() {
        recipients.push(Address::Operator);
    }
    for to in recipients {
        mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: &to,
                subject: &format!("Conflict hint: {} / {}", a.id, b.id),
                body,
                thread: None,
                in_reply_to: None,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_on_both_cards_and_mails_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("test.db")).unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        db.write("operator", |t| {
            let project = serde_json::from_value(serde_json::json!({"name": "test"})).unwrap();
            crate::store::projects::create(t, &project, "api")?;
            let actor = crate::actor::Actor::Operator;
            let mut cards = Vec::new();
            for title in ["A", "B"] {
                let new = serde_json::from_value(serde_json::json!({"title": title})).unwrap();
                let c = board::create_card(t, &actor, "test", new)?;
                cards.push(Card {
                    id: c.id,
                    project: "test".into(),
                    repo: "r".into(),
                    branch: title.into(),
                });
            }
            publish(t, &cards[0], &cards[1], "Conflict hint: shared.rs")?;
            publish(t, &cards[0], &cards[1], "Conflict hint: shared.rs")?;
            let count: i64 = t.query_row(
                "SELECT count(*) FROM card_comments WHERE body = 'Conflict hint: shared.rs'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 2);
            let count: i64 = t.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE 'Conflict hint:%'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 1);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn exact_paths_only_and_sorted() {
        let a = ["src/a.rs", "src/b.rs", "a.rs"].map(String::from).into();
        let b = ["src/b.rs", "src/a.rs", "b.rs"].map(String::from).into();
        assert_eq!(overlap(&a, &b), ["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn disjoint_and_empty() {
        let a = ["a.rs"].map(String::from).into();
        let b = ["src/a.rs"].map(String::from).into();
        assert!(overlap(&a, &b).is_empty());
        assert!(overlap(&a, &BTreeSet::new()).is_empty());
    }
}
