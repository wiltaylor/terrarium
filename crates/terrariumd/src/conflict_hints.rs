//! Advisory file overlaps between active card branches. Never gates a move.
use std::collections::BTreeSet;
use std::time::Duration;

use rusqlite::params;

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::{board, jobs, mail, repos};
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

async fn files(dir: &std::path::Path, base: &str, branch: &str) -> Option<BTreeSet<String>> {
    // Mirror refs are local heads. A three-dot diff uses the merge base.
    let range = format!("refs/heads/{base}...refs/heads/{branch}");
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

async fn local_head(dir: &std::path::Path, branch: &str) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn remote_heads(
    state: &Shared,
    repo: &repos::Repo,
    branch: &str,
) -> Option<(Option<String>, Option<String>)> {
    let secret = crate::secrets::secret_of(state, repo.owner(), repo.credential.as_deref())
        .await
        .ok()?;
    let c = crate::git::Checkout {
        url: repo.url.clone(),
        dest: crate::api::repo_trees::mirror_dir(state, repo),
        branch: None,
        depth: None,
        secret: secret.map(|(_, s)| s),
        store: crate::secrets::RunDirs::new(&state.state_dir),
        deny_local_under: vec![state.state_dir.clone(), state.work_dir.clone()],
        timeout: Duration::from_secs(20),
        hooks: Default::default(),
        origin: Default::default(),
        fetch_from: None,
    };
    let branch = branch.to_owned();
    tokio::task::spawn_blocking(move || {
        Some((
            crate::git::ls_remote(&c, "next").ok()?,
            crate::git::ls_remote(&c, &branch).ok()?,
        ))
    })
    .await
    .ok()?
}

async fn note(state: &Shared, card: &Card, message: &str) -> ApiResult<()> {
    let card = card.id.clone();
    let message = message.to_owned();
    state.db.write("system", move |t| {
        let seen: bool = t.query_row("SELECT EXISTS(SELECT 1 FROM card_comments WHERE card = ?1 AND author = 'system' AND body = ?2)", params![card, message], |r| r.get(0))?;
        if !seen { board::system_comment(t, &card, &message, None)?; }
        Ok(())
    }).await
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
        let project = card.project.clone();
        let configured = state.db.read(move |c| jobs::delivery(c, &project)).await?;
        // DeliveryDef represents integration branches as next_repos, not a free-form branch name.
        if !configured.next_repos.contains(&repo.name) {
            note(state, &card, "Conflict hints unavailable: no integration branch configured for this repo in delivery.next_repos.").await?;
            continue;
        }
        let Some((base, head)) = remote_heads(state, &repo, &card.branch).await else {
            continue;
        };
        let Some(base) = base else {
            note(state, &card, "Conflict hints unavailable: configured integration branch next is missing on the remote.").await?;
            continue;
        };
        // Never use a stale mirror ref for a branch absent from the remote.
        let Some(head) = head else { continue };
        let dir = crate::api::repo_trees::mirror_dir(state, &repo);
        if (local_head(&dir, "next").await.as_ref() != Some(&base)
            || local_head(&dir, &card.branch).await.as_ref() != Some(&head))
            && crate::api::repo_trees::ensure_mirror(state, &repo, true)
                .await
                .is_err()
        {
            continue;
        }
        if let Some(paths) = files(&dir, "next", &card.branch).await {
            scanned.push((card, repo.id, paths));
        }
    }
    publish_overlaps(&state.db, scanned).await
}

async fn publish_overlaps(
    db: &crate::db::Db,
    scanned: Vec<(Card, String, BTreeSet<String>)>,
) -> ApiResult<()> {
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
            db.write("system", move |t| publish(t, &a, &b, &common))
                .await?;
        }
    }
    Ok(())
}

fn publish(t: &mut crate::db::Tx<'_>, a: &Card, b: &Card, common: &[String]) -> ApiResult<()> {
    let mut pair = [&a.id, &b.id];
    pair.sort();
    let paths: BTreeSet<_> = common.iter().cloned().collect();
    let key = serde_json::json!({"cards": pair, "paths": paths});
    // Persisted deduplication also survives daemon restarts.
    let seen = {
        let mut q = t.prepare(
            "SELECT payload FROM events WHERE card = ?1 AND kind = 'card.conflict_hint'",
        )?;
        let values = q
            .query_map([pair[0]], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
            .iter()
            .filter_map(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .any(|v| v["cards"] == key["cards"] && v["paths"] == key["paths"])
    };
    if seen {
        return Ok(());
    }
    let body = format!(
        "Conflict hint: {} and {} share {} files: {}{}. Advisory only. Coordinate work or merge order. Based on branch changes against next.",
        a.id,
        b.id,
        paths.len(),
        paths
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
        if paths.len() > 20 {
            " (first 20 shown)"
        } else {
            ""
        }
    );
    board::system_comment(t, &a.id, &body, None)?;
    board::system_comment(t, &b.id, &body, None)?;
    t.emit(
        crate::events::EventDraft::new("card.conflict_hint", "Shared branch files", key)
            .project(&a.project)
            .card(pair[0]),
    )?;
    let recipients = crate::question_guard::leads(t, &a.project, "")?;
    if recipients.is_empty() {
        tracing::warn!(project = %a.project, "conflict hint has no project lead; cards annotated only");
    }
    for to in recipients {
        mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: &to,
                subject: &format!("Conflict hint: {} / {}", a.id, b.id),
                body: &body,
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
    async fn publishes_on_both_cards_without_operator_mail_and_deduplicates_sets() {
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
            let shared = vec!["shared.rs".to_owned()];
            publish(t, &cards[0], &cards[1], &shared)?;
            publish(t, &cards[1], &cards[0], &shared)?;
            let count: i64 = t.query_row(
                "SELECT count(*) FROM card_comments WHERE body LIKE 'Conflict hint:%'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 2);
            let count: i64 = t.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE 'Conflict hint:%'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 0);
            publish(t, &cards[0], &cards[1], &["shared.rs".into(), "new.rs".into()])?;
            let count: i64 = t.query_row("SELECT count(*) FROM card_comments WHERE body LIKE 'Conflict hint:%'", [], |r| r.get(0))?;
            assert_eq!(count, 4);

            // Selection respects working/merge_queue, archive and missing metadata.
            for (i, c) in cards.iter().enumerate() {
                t.execute("UPDATE cards SET repo = 'r', branch = ?2, column_id = (SELECT id FROM board_columns WHERE project = 'test' AND name = ?3) WHERE id = ?1",
                    params![c.id, c.branch, if i == 0 { "in_progress" } else { "merge_queue" }])?;
            }
            assert_eq!(active(t)?.len(), 2);
            t.execute("UPDATE cards SET archived = 1 WHERE id = ?1", [&cards[0].id])?;
            assert_eq!(active(t)?.len(), 1);
            t.execute("UPDATE cards SET branch = NULL WHERE id = ?1", [&cards[1].id])?;
            assert!(active(t)?.is_empty());
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn diff_uses_merge_base_and_skips_unpushed_branches() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "next"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["commit", "--allow-empty", "-m", "base"]);
        run(&["checkout", "-b", "card"]);
        std::fs::write(dir.path().join("shared.rs"), "card").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "card change"]);
        run(&["checkout", "next"]);
        std::fs::write(dir.path().join("next-only.rs"), "integration").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "next advanced"]);
        let out = std::process::Command::new("git")
            .args(["clone", "--bare"])
            .arg(dir.path())
            .arg(mirror.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        run(&["checkout", "-b", "unpushed"]);
        let paths = files(mirror.path(), "next", "card").await.unwrap();
        assert_eq!(paths, ["shared.rs".into()].into());
        assert!(files(mirror.path(), "next", "unpushed").await.is_none());
        assert!(files(mirror.path(), "missing", "card").await.is_none());
    }

    #[tokio::test]
    async fn check_excludes_other_projects_and_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("test.db")).unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let card = |id: &str, project: &str| Card {
            id: id.into(),
            project: project.into(),
            repo: "r".into(),
            branch: id.into(),
        };
        // These IDs deliberately do not exist. Any attempted publish would fail.
        publish_overlaps(
            &db,
            vec![
                (card("A-1", "a"), "r".into(), ["same.rs".into()].into()),
                (card("B-1", "b"), "r".into(), ["same.rs".into()].into()),
                (
                    card("A-2", "a"),
                    "other-repo".into(),
                    ["same.rs".into()].into(),
                ),
            ],
        )
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
