//! The reviewed commit of each card in the merge queue (TER-553).
//!
//! A move into the merge queue (the review accept) opens a round with no
//! commit. The daemon reads the head of the card branch on its remote right
//! after the move ([`crate::jobs::record_accepted_heads`]) and records it
//! here. A move out of the merge queue ends the round. A merge job takes
//! the recorded commit of each card, so a push after the accept never
//! reaches `next` unreviewed: the card leaves the merge with the outcome
//! `moved` and goes back to review.

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Tx, now};
use crate::error::ApiResult;

/// The name of the merge queue column when no column has the role.
const QUEUE_NAME: &str = "merge_queue";

/// Is a column with `name` and `role` the merge queue? The role, else the
/// name of a column without a role (as `conflict_hints`).
pub fn is_queue(name: &str, role: Option<&str>) -> bool {
    match role {
        Some(r) => r == crate::store::board::MERGE_QUEUE,
        None => name == QUEUE_NAME,
    }
}

/// The card moved from one column to another (not a reorder): open a new
/// round on a move into the merge queue, end it on a move out.
pub fn on_move(
    t: &Tx<'_>,
    card: &str,
    from: (&str, Option<&str>),
    to: (&str, Option<&str>),
) -> ApiResult<()> {
    let (was, is) = (is_queue(from.0, from.1), is_queue(to.0, to.1));
    if is && !was {
        t.execute(
            "INSERT INTO card_accepted_heads (card, round, accepted_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(card) DO UPDATE SET
               round = excluded.round, accepted_at = excluded.accepted_at,
               branch = NULL, commit_sha = NULL, recorded_at = NULL, note = NULL,
               tried_at = NULL",
            params![card, ulid::Ulid::new().to_string(), now()],
        )?;
    } else if was && !is {
        t.execute("DELETE FROM card_accepted_heads WHERE card = ?1", [card])?;
    }
    Ok(())
}

/// A round whose head is not recorded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub card: String,
    pub round: String,
}

/// The rounds without a recorded commit, oldest first. A round whose last
/// read failed at or after `retry_before` waits.
pub fn pending(conn: &Connection, retry_before: &str) -> ApiResult<Vec<Pending>> {
    let mut q = conn.prepare(
        "SELECT card, round FROM card_accepted_heads
         WHERE commit_sha IS NULL AND (tried_at IS NULL OR tried_at < ?1)
         ORDER BY accepted_at, card",
    )?;
    let rows = q
        .query_map([retry_before], |r| {
            Ok(Pending {
                card: r.get(0)?,
                round: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Record the head `commit` of `branch` for round `round` of `card`. Only
/// the open round, and only once: a late read of an earlier round, or a
/// second read of this one, changes nothing. True when it recorded.
pub fn record(t: &Tx<'_>, card: &str, round: &str, branch: &str, commit: &str) -> ApiResult<bool> {
    let n = t.execute(
        "UPDATE card_accepted_heads SET branch = ?3, commit_sha = ?4, recorded_at = ?5,
                note = NULL
         WHERE card = ?1 AND round = ?2 AND commit_sha IS NULL",
        params![card, round, branch, commit, now()],
    )?;
    Ok(n == 1)
}

/// Record `commit` of `branch` in the open round of `card`, in the write
/// of the move that opened it (the head was read just before). True when
/// it recorded.
pub fn record_open(t: &Tx<'_>, card: &str, branch: &str, commit: &str) -> ApiResult<bool> {
    let n = t.execute(
        "UPDATE card_accepted_heads SET branch = ?2, commit_sha = ?3, recorded_at = ?4,
                note = NULL
         WHERE card = ?1 AND commit_sha IS NULL",
        params![card, branch, commit, now()],
    )?;
    Ok(n == 1)
}

/// Note why the head of the open round could not be read.
pub fn note(t: &Tx<'_>, card: &str, round: &str, why: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE card_accepted_heads SET note = ?3, tried_at = ?4
         WHERE card = ?1 AND round = ?2 AND commit_sha IS NULL",
        params![card, round, why, now()],
    )?;
    Ok(())
}

/// The recorded head of a card in the merge queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// `None` while the read is pending.
    pub commit: Option<String>,
    pub branch: Option<String>,
    /// Why the read failed so far.
    pub note: Option<String>,
}

/// The round of `card`, if it is in the merge queue.
pub fn get(conn: &Connection, card: &str) -> ApiResult<Option<Head>> {
    Ok(conn
        .query_row(
            "SELECT commit_sha, branch, note FROM card_accepted_heads WHERE card = ?1",
            [card],
            |r| {
                Ok(Head {
                    commit: r.get(0)?,
                    branch: r.get(1)?,
                    note: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// The reviewed commit of `card` on `branch`: recorded, and for the branch
/// that the merge takes now (a branch changed after the accept has none).
pub fn commit_for(conn: &Connection, card: &str, branch: &str) -> ApiResult<Option<String>> {
    Ok(get(conn, card)?
        .filter(|h| h.branch.as_deref() == Some(branch))
        .and_then(|h| h.commit))
}

/// Why a card has no reviewed commit for `branch` (for a left-out reason).
pub fn missing_reason(conn: &Connection, card: &str, branch: &str) -> ApiResult<String> {
    Ok(match get(conn, card)? {
        None => "no reviewed commit: the card was not accepted into the merge queue".to_owned(),
        Some(h) if h.commit.is_none() => match h.note {
            Some(n) => format!("the reviewed commit is not recorded yet: {n}"),
            None => "the reviewed commit is not recorded yet".to_owned(),
        },
        Some(h) => format!(
            "the reviewed commit is of branch `{}`, the card branch is now `{branch}`",
            h.branch.unwrap_or_default()
        ),
    })
}
