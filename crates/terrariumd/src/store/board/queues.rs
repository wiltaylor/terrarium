//! The queues of the board columns (TER-1114, kanban rework C3, D3 of the
//! handoff, docs/api.md §8.2.2). Only a project with `board_flow = columns`
//! has them. Every other project keeps the `409 wip_limit` of a full column.
//!
//! - **Enter.** A move into a column that cannot take the card now is
//!   accepted: the card is in the column and waits in its queue (a
//!   `card_queue` row with no `admitted_at`). The move checks run as
//!   before (rights, final column, dependencies, the comment), and the ETA
//!   is cleared. The `moved` event has `queued: true`, so the column's
//!   `lands` rules do not run yet. A claim, a card wait's move, and the
//!   `card.move` verb all go through the same move.
//! - **Admit.** A queued card gets a slot in queue order (first in, first
//!   out; a new card never overtakes one that waits). Admission writes the
//!   `card` event `admitted`, and the `lands` and `count_reached` rules of
//!   the column run on it. An ETA set while the card waited is set then.
//! - **The release** ([`QueueRelease`]). The capacity is the column's
//!   `wip_limit` (none: no limit) in each mode.
//!   - `wip`: admit while the admitted cards are fewer than the limit.
//!   - `job`: one job at a time in the column (TER-1112 Q4). A card admitted
//!     by `job` holds the job slot from its admission until a job of the
//!     card that was queued after it has ended (passed, failed, or
//!     cancelled) and it has no open job, or it leaves the column. Then the
//!     next card is admitted. A job from the previous column does not free
//!     the slot (TER-1204). A failed job ends only when the rules have
//!     acted on it, so its rerun keeps the slot (TER-1250, `RERUN_SETTLED`).
//!   - `batch`: when the last batch no longer holds the lane (an admitted
//!     card with an open job, or in [`BATCH_GRACE_SECONDS`] after its
//!     admission or job end; a stale card goes back to the queue), and
//!     `batch_size` cards wait or one has waited `max_wait_seconds`: admit
//!     up to `max_batch_cards` (default `batch_size`) in queue order. The
//!     merge_queue column counts and admits only the cards that a merge
//!     takes now, and a ready release-blocker alone, at once, as the
//!     `merge-queue-batch` rule did ([`merge_candidates`]).
//! - **When.** [`release`] runs in the transaction of its cause: a move into
//!   or out of the column, an archive, a job end ([`on_job_end`]), and a
//!   change of the column's WIP limit or queue options. The rule engine's
//!   tick calls [`sweep_all`] for the `max_wait_seconds` of a batch and to
//!   repair a release that no cause triggered.
use super::*;

use crate::store::board::column_options::QueueRelease;
use terrarium_core::config::BoardFlow;

/// Do the column queues run on the board of `project`?
pub(crate) fn active(conn: &Connection, project: &str) -> ApiResult<bool> {
    Ok(crate::store::projects::board_flow(conn, project)? == BoardFlow::Columns)
}

/// The place of a card in the queue of its column, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardQueue {
    pub column: String,
    /// When the card entered the queue.
    pub since: String,
    /// 1: the card is admitted next.
    pub place: i64,
    /// An ETA set while the card waits: it is set when the card is admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_eta: Option<String>,
}

struct Settings {
    id: i64,
    name: String,
    project: String,
    wip_limit: Option<i64>,
    release: QueueRelease,
    batch_size: Option<i64>,
    max_wait_seconds: Option<i64>,
    max_batch_cards: Option<i64>,
    research: bool,
    /// The merge queue column (role or name `merge_queue`): its batch
    /// waits while a merge job of the project holds the merge lane.
    merge_lane: bool,
}

fn settings(conn: &Connection, column_id: i64) -> ApiResult<Option<Settings>> {
    Ok(conn
        .query_row(
            "SELECT id, project, wip_limit, queue_release, queue_batch_size,
                    queue_max_wait_seconds, queue_max_batch_cards, name,
                    coalesce(role, name) = 'merge_queue'
             FROM board_columns WHERE id = ?1",
            [column_id],
            |r| {
                Ok(Settings {
                    id: r.get(0)?,
                    name: r.get(7)?,
                    project: r.get(1)?,
                    wip_limit: r.get(2)?,
                    release: QueueRelease::parse(&r.get::<_, String>(3)?)?,
                    batch_size: r.get(4)?,
                    max_wait_seconds: r.get(5)?,
                    max_batch_cards: r.get(6)?,
                    research: r.get::<_, String>(7)?.eq_ignore_ascii_case("research"),
                    merge_lane: r.get(8)?,
                })
            },
        )
        .optional()?)
}

/// The cards that wait in the queue of column `column_id`, in queue
/// order: `(card, queued_at)`.
fn waiting(conn: &Connection, column_id: i64) -> ApiResult<Vec<(String, String)>> {
    let mut st = conn.prepare(
        "SELECT q.card, q.queued_at FROM card_queue q JOIN cards c ON c.id = q.card
          WHERE q.column_id = ?1 AND q.admitted_at IS NULL AND c.archived = 0
          ORDER BY q.seq",
    )?;
    let rows = st
        .query_map([column_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The cards of column `column_id` that are admitted (in the column, not
/// archived, not waiting in its queue).
pub(crate) fn admitted_count(conn: &Connection, column_id: i64) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM cards c WHERE c.column_id = ?1 AND c.archived = 0
           AND c.kind = 'card'
           AND NOT EXISTS (SELECT 1 FROM card_queue q
                            WHERE q.card = c.id AND q.admitted_at IS NULL)",
        [column_id],
        |r| r.get(0),
    )?)
}

/// The cards that wait in the queue of column `column_id`.
pub(crate) fn queued_count(conn: &Connection, column_id: i64) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM card_queue q JOIN cards c ON c.id = q.card
          WHERE q.column_id = ?1 AND q.admitted_at IS NULL AND c.archived = 0",
        [column_id],
        |r| r.get(0),
    )?)
}

/// The waiting cards of each column of `project` that has any, by column
/// ID: one statement for a whole board (the board summary has a statement
/// budget).
pub(crate) fn queued_by_column(
    conn: &Connection,
    project: &str,
) -> ApiResult<std::collections::HashMap<i64, i64>> {
    let mut st = conn.prepare(
        "SELECT q.column_id, count(*) FROM card_queue q
           JOIN board_columns k ON k.id = q.column_id
           JOIN cards c ON c.id = q.card
          WHERE k.project = ?1 AND q.admitted_at IS NULL AND c.archived = 0
          GROUP BY q.column_id",
    )?;
    let rows = st
        .query_map([project], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// A job of card `q.card` (its own job, or a batch job that has it).
const JOB_OF_CARD: &str = "(j.card = q.card OR EXISTS (SELECT 1 FROM json_each(j.batch)
                            WHERE json_extract(value, '$.card') = q.card))";

/// The rerun of ended job `j` is settled (TER-1250): `j` did not fail, or
/// the board rules have acted on its end. A failed job frees the `job`
/// slot only when its `job.verdict` is written (by `after_end`, after the
/// end; a kind with no verdict, or 10 minutes after the end, without
/// one), the rule engine's cursor is past its `job` and `job.verdict`
/// events, and no rule run of them is pending. A rerun that a rule queued
/// is then an open job, so the slot stays held until the rerun ends.
const RERUN_SETTLED: &str = "(j.state <> 'failed'
    OR ((j.kind IN ('e2e', 'release')
         OR j.ended_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-600 seconds')
         OR EXISTS (SELECT 1 FROM events e WHERE e.card = j.card AND e.kind = 'job.verdict'
                      AND json_extract(e.payload, '$.job') = j.id))
        AND NOT EXISTS (SELECT 1 FROM events e
                         WHERE e.seq > coalesce((SELECT seq FROM rule_cursor WHERE id = 1), e.seq)
                           AND e.kind IN ('job', 'job.verdict')
                           AND coalesce(json_extract(e.payload, '$.job'),
                                        json_extract(e.payload, '$.id')) = j.id)
        AND NOT EXISTS (SELECT 1 FROM rule_runs r JOIN events e ON e.seq = r.event_seq
                         WHERE r.state = 'pending' AND e.kind IN ('job', 'job.verdict')
                           AND coalesce(json_extract(e.payload, '$.job'),
                                        json_extract(e.payload, '$.id')) = j.id)))";

/// A job of card `q.card` that was queued at or after its admission, has
/// ended, and whose rerun is settled ([`RERUN_SETTLED`]): it frees the
/// `job` slot. An older job (one still running from the previous column)
/// does not (TER-1204).
fn slot_job_ended() -> String {
    format!(
        "EXISTS (SELECT 1 FROM jobs j
                  WHERE j.state IN ('passed', 'failed', 'cancelled')
                    AND j.created_at >= q.admitted_at
                    AND j.ended_at >= q.admitted_at
                    AND {JOB_OF_CARD}
                    AND {RERUN_SETTLED})"
    )
}

/// Does a card of column `column_id` hold its job slot (release `job`)?
fn job_slot_busy(conn: &Connection, column_id: i64) -> ApiResult<bool> {
    let ended = slot_job_ended();
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM card_queue q JOIN cards c ON c.id = q.card
          WHERE q.column_id = ?1 AND q.admitted_at IS NOT NULL AND c.archived = 0
            AND c.column_id = q.column_id
            AND (EXISTS (SELECT 1 FROM jobs j WHERE j.state IN ('queued', 'running')
                           AND {JOB_OF_CARD})
                 OR NOT {ended}))"
    );
    Ok(conn.query_row(&sql, [column_id], |r| r.get(0))?)
}

/// How long an admitted card of a `batch` column holds the lane with no
/// open job (TER-1204): the time for its `lands` rules to queue the job,
/// counted from its admission or from the end of its last job (for the
/// rules that move it on). After it, the card is stale.
pub const BATCH_GRACE_SECONDS: i64 = 120;

/// A job of card `c.id` (its own job, or a batch job that has it).
const JOB_OF_C: &str = "(j.card = c.id OR EXISTS (SELECT 1 FROM json_each(j.batch)
                         WHERE json_extract(value, '$.card') = c.id))";

/// The admitted cards of column `?1` that are stale in its batch, in
/// board order: no open job, and neither admitted nor with a job end at
/// or after the cutoff `?2`. A card with no queue row (it was in the
/// column before the `batch` release, or before the conversion of the
/// board) has no admission time. Only a job queued at or after the
/// admission counts (the batch job comes after it), so the jobs are read
/// by `jobs_project_created_at`, and the open ones by `jobs_project_state`
/// (TER-1204).
fn stale_sql() -> String {
    format!(
        "SELECT c.id FROM cards c
          WHERE c.column_id = ?1 AND c.archived = 0 AND c.kind = 'card'
            AND NOT EXISTS (SELECT 1 FROM card_queue q
                             WHERE q.card = c.id AND q.admitted_at IS NULL)
            AND coalesce((SELECT q.admitted_at FROM card_queue q WHERE q.card = c.id), '') < ?2
            AND NOT EXISTS (SELECT 1 FROM jobs j INDEXED BY jobs_project_state
                             WHERE j.project = c.project AND j.state IN ('queued', 'running')
                               AND {JOB_OF_C})
            AND NOT EXISTS (SELECT 1 FROM jobs j INDEXED BY jobs_project_created_at
                             WHERE j.project = c.project
                               AND j.created_at >= coalesce(
                                     (SELECT q.admitted_at FROM card_queue q WHERE q.card = c.id), '')
                               AND j.ended_at >= ?2
                               AND j.state IN ('passed', 'failed', 'cancelled')
                               AND {JOB_OF_C})
          ORDER BY c.position"
    )
}

fn stale_in_batch(conn: &Connection, column_id: i64, cutoff: &str) -> ApiResult<Vec<String>> {
    let mut st = conn.prepare(&stale_sql())?;
    let rows = st
        .query_map(params![column_id, cutoff], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn grace_cutoff() -> String {
    crate::time_param::stored(chrono::Utc::now() - chrono::Duration::seconds(BATCH_GRACE_SECONDS))
}

/// Does a batch still hold the lane of `batch` column `column_id`? An
/// admitted card holds it while it has an open job, or in the grace time
/// after its admission or its last job end ([`BATCH_GRACE_SECONDS`]). A
/// card that the batch job left out does not hold it after that, so it
/// never stops the queue (TER-1204).
fn batch_open(conn: &Connection, column_id: i64) -> ApiResult<bool> {
    let stale = stale_in_batch(conn, column_id, &grace_cutoff())?.len() as i64;
    Ok(admitted_count(conn, column_id)? > stale)
}

/// Put the stale admitted cards of `batch` column `column_id` back in its
/// queue, first, in board order (TER-1204). Their wait starts again, so a
/// card that no batch takes is tried once per `max_wait_seconds`, and an
/// ETA is held for the next admission.
fn return_stale(t: &mut Tx<'_>, column_id: i64) -> ApiResult<()> {
    let stale = stale_in_batch(t, column_id, &grace_cutoff())?;
    if stale.is_empty() {
        return Ok(());
    }
    let first: i64 = t.query_row(
        "SELECT coalesce(min(seq), 1) FROM card_queue WHERE column_id = ?1",
        [column_id],
        |r| r.get(0),
    )?;
    let at = now();
    let n = stale.len() as i64;
    for (i, card) in stale.iter().enumerate() {
        t.execute(
            "INSERT OR REPLACE INTO card_queue (card, column_id, seq, queued_at, held_eta, held_eta_by)
             SELECT id, ?2, ?3, ?4, eta_at, eta_set_by FROM cards WHERE id = ?1",
            params![card, column_id, first - n + i as i64, at],
        )?;
        t.execute(
            "UPDATE cards SET eta_at = NULL, eta_set_at = NULL, eta_set_by = NULL WHERE id = ?1",
            [card],
        )?;
        let row = find_row(t, card)?;
        card_event(
            t,
            &row,
            "queue_returned",
            json!({"reason": "no open job in the batch", "assignee": row.assignee}),
        )?;
    }
    Ok(())
}

/// [`merge_candidates`]: the cards a batch takes, and a release-blocker.
type Candidates = (Vec<(String, String)>, Option<String>);

/// The waiting cards of merge_queue column `s` that a batch merge takes
/// now, as `merge-queue-batch` counts them (TER-1204): only cards that can
/// go ([`crate::rules_verbs::merge_ready`]) with a repo in `next_repos`,
/// and of one repo, the repo of the first one, in queue order:
/// `(card, queued_at)`. The others wait, and never hold the lane. The
/// second value is the first such card with the release-blocker label,
/// of any repo: it goes alone, at once (TER-607).
fn merge_candidates(
    conn: &Connection,
    s: &Settings,
    queue: &[(String, String)],
) -> ApiResult<Candidates> {
    let served = crate::store::jobs::delivery(conn, &s.project)?.next_repos;
    // A project with no `next` repo has no merge lane: the column is a
    // plain `batch` column.
    if served.is_empty() {
        return Ok((queue.to_vec(), None));
    }
    let cards: std::collections::HashMap<String, Card> =
        selection_cards(conn, &s.project, &s.name)?
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
    let mut repo: Option<String> = None;
    let mut out = Vec::new();
    let mut blocker = None;
    for (id, at) in queue {
        let Some(c) = cards.get(id) else { continue };
        let Ok(r) = crate::rules_verbs::merge_ready(conn, c)? else {
            continue;
        };
        if !served.contains(&r) {
            continue;
        }
        if blocker.is_none()
            && c.labels
                .iter()
                .any(|l| l == crate::rules_verbs::RELEASE_BLOCKER)
        {
            blocker = Some(id.clone());
        }
        if repo.get_or_insert_with(|| r.clone()) == &r {
            out.push((id.clone(), at.clone()));
        }
    }
    Ok((out, blocker))
}

/// The free places of the column: `None` without a WIP limit.
fn room(conn: &Connection, s: &Settings) -> ApiResult<Option<i64>> {
    let Some(limit) = s.wip_limit else {
        return Ok(None);
    };
    let helpers = if s.research {
        crate::research_helpers::running(conn, &s.project)?
    } else {
        0
    };
    Ok(Some(limit - admitted_count(conn, s.id)? - helpers))
}

/// Must a card that moves into column `column_id` now wait in its queue?
/// Only on a `columns` board; reads only.
pub(super) fn must_queue(conn: &Connection, project: &str, column_id: i64) -> ApiResult<bool> {
    if !active(conn, project)? {
        return Ok(false);
    }
    let Some(s) = settings(conn, column_id)? else {
        return Ok(false);
    };
    if queued_count(conn, s.id)? > 0 || room(conn, &s)?.is_some_and(|r| r <= 0) {
        return Ok(true);
    }
    Ok(match s.release {
        QueueRelease::Wip => false,
        QueueRelease::Job => job_slot_busy(conn, s.id)?,
        // A card enters a batch column only with a batch.
        QueueRelease::Batch => true,
    })
}

/// Put card `card` in the queue of column `column_id`, last.
pub(super) fn enqueue(t: &mut Tx<'_>, card: &str, column_id: i64) -> ApiResult<()> {
    let seq: i64 = t.query_row(
        "SELECT coalesce(max(seq), 0) + 1 FROM card_queue WHERE column_id = ?1",
        [column_id],
        |r| r.get(0),
    )?;
    t.execute(
        "INSERT OR REPLACE INTO card_queue (card, column_id, seq, queued_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![card, column_id, seq, now()],
    )?;
    Ok(())
}

/// Card `card` moved straight into column `column_id` (no wait): under the
/// release `job` it takes the free job slot, as an admitted card does.
pub(super) fn landed(t: &mut Tx<'_>, card: &str, column_id: i64) -> ApiResult<()> {
    let Some(s) = settings(t, column_id)? else {
        return Ok(());
    };
    if s.release != QueueRelease::Job || !active(t, &s.project)? {
        return Ok(());
    }
    let at = now();
    t.execute(
        "INSERT OR REPLACE INTO card_queue (card, column_id, seq, queued_at, admitted_at)
         VALUES (?1, ?2, (SELECT coalesce(max(seq), 0) + 1 FROM card_queue WHERE column_id = ?2),
                 ?3, ?3)",
        params![card, column_id, at],
    )?;
    Ok(())
}

/// Take card `card` out of any queue (it leaves its column or is
/// archived). Returns whether it was waiting (not admitted).
pub(super) fn leave(t: &mut Tx<'_>, card: &str) -> ApiResult<bool> {
    let waited: Option<bool> = t
        .query_row(
            "SELECT admitted_at IS NULL FROM card_queue WHERE card = ?1",
            [card],
            |r| r.get(0),
        )
        .optional()?;
    if waited.is_some() {
        t.execute("DELETE FROM card_queue WHERE card = ?1", [card])?;
    }
    Ok(waited.unwrap_or(false))
}

/// Is card `card` waiting in a queue?
pub(crate) fn is_waiting(conn: &Connection, card: &str) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM card_queue WHERE card = ?1 AND admitted_at IS NULL",
            [card],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

/// The queue place of card `card`, when it waits.
pub(crate) fn of_card(conn: &Connection, card: &str) -> ApiResult<Option<CardQueue>> {
    Ok(conn
        .query_row(
            "SELECT k.name, q.queued_at, q.held_eta,
                    (SELECT count(*) FROM card_queue o JOIN cards oc ON oc.id = o.card
                      WHERE o.column_id = q.column_id AND o.admitted_at IS NULL
                        AND oc.archived = 0 AND o.seq < q.seq) + 1
             FROM card_queue q JOIN board_columns k ON k.id = q.column_id
             WHERE q.card = ?1 AND q.admitted_at IS NULL",
            [card],
            |r| {
                Ok(CardQueue {
                    column: r.get(0)?,
                    since: r.get(1)?,
                    held_eta: r.get(2)?,
                    place: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Keep ETA `eta` (as given: a time or a duration) of waiting card `card`
/// for its admission; `None` drops a held ETA. `false`: the card does not
/// wait.
pub(super) fn hold_eta(t: &mut Tx<'_>, card: &str, eta: Option<&str>, by: &str) -> ApiResult<bool> {
    let n = t.execute(
        "UPDATE card_queue SET held_eta = ?2, held_eta_by = ?3
          WHERE card = ?1 AND admitted_at IS NULL",
        params![card, eta, eta.map(|_| by)],
    )?;
    Ok(n > 0)
}

/// Admit card `card` to its column: `keep` holds its row for a `job` slot
/// or a `batch`.
fn admit(t: &mut Tx<'_>, card: &str, keep: bool) -> ApiResult<()> {
    let (queued_at, held, held_by): (String, Option<String>, Option<String>) = t.query_row(
        "SELECT queued_at, held_eta, held_eta_by FROM card_queue WHERE card = ?1",
        [card],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let at = now();
    if keep {
        t.execute(
            "UPDATE card_queue SET admitted_at = ?2, held_eta = NULL, held_eta_by = NULL
              WHERE card = ?1",
            params![card, at],
        )?;
    } else {
        t.execute("DELETE FROM card_queue WHERE card = ?1", [card])?;
    }
    let row = find_row(t, card)?;
    card_event(
        t,
        &row,
        "admitted",
        json!({"queued_at": queued_at, "assignee": row.assignee,
               "to_count": admitted_count(t, row.column_id)?}),
    )?;
    if let Some(eta) = held {
        set_held(t, card, &eta, held_by.as_deref())?;
    }
    Ok(())
}

/// The held ETA of waiting card `card` and who set it.
pub(super) fn held_of(
    conn: &Connection,
    card: &str,
) -> ApiResult<Option<(String, Option<String>)>> {
    Ok(conn
        .query_row(
            "SELECT held_eta, held_eta_by FROM card_queue
              WHERE card = ?1 AND admitted_at IS NULL AND held_eta IS NOT NULL",
            [card],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Set held ETA `eta` (a time or a duration, counted from now) of card
/// `card`, by `by`. A held ETA that no longer reads (a time that has
/// passed by now) is dropped.
pub(super) fn set_held(t: &mut Tx<'_>, card: &str, eta: &str, by: Option<&str>) -> ApiResult<()> {
    let Ok(when) = terrarium_core::eta::resolve(eta, chrono::Utc::now()) else {
        return Ok(());
    };
    let when = crate::time_param::stored(when);
    t.execute(
        "UPDATE cards SET eta_at = ?2, eta_set_at = ?3, eta_set_by = ?4 WHERE id = ?1",
        params![card, when, now(), by],
    )?;
    let row = find_row(t, card)?;
    card_event(
        t,
        &row,
        "eta",
        json!({"eta_at": when, "previous": null, "held": true,
               "labels": json_col::<Vec<String>>(&row.labels)}),
    )
}

/// Admit the cards that the queue of column `column_id` releases now.
/// Does nothing on a board that is not `columns`.
pub fn release(t: &mut Tx<'_>, column_id: i64) -> ApiResult<()> {
    let Some(s) = settings(t, column_id)? else {
        return Ok(());
    };
    if !active(t, &s.project)? {
        return Ok(());
    }
    // A `job` slot that is free again, or a row of a card that left: the row
    // goes, so the table holds only the cards that wait or hold a slot.
    let ended = slot_job_ended();
    t.execute(
        &format!(
            "DELETE FROM card_queue WHERE rowid IN (
               SELECT q.rowid FROM card_queue q JOIN cards c ON c.id = q.card
                WHERE q.column_id = ?1 AND q.admitted_at IS NOT NULL
                  AND (c.archived = 1 OR c.column_id <> q.column_id
                       OR (?2 AND NOT EXISTS (SELECT 1 FROM jobs j
                                WHERE j.state IN ('queued', 'running') AND {JOB_OF_CARD})
                              AND {ended})))"
        ),
        params![s.id, s.release == QueueRelease::Job],
    )?;
    if s.release == QueueRelease::Batch {
        return_stale(t, s.id)?;
    }
    loop {
        let queue = waiting(t, s.id)?;
        let Some((first, _)) = queue.first().cloned() else {
            return Ok(());
        };
        let room = room(t, &s)?;
        if room.is_some_and(|r| r <= 0) {
            return Ok(());
        }
        match s.release {
            QueueRelease::Wip => admit(t, &first, false)?,
            QueueRelease::Job => {
                if job_slot_busy(t, s.id)? {
                    return Ok(());
                }
                admit(t, &first, true)?;
            }
            QueueRelease::Batch => {
                if batch_open(t, s.id)? {
                    return Ok(());
                }
                // The merge lane of the project is busy (a direct merge):
                // the batch waits for its end, as the rule did (TER-644).
                if s.merge_lane && crate::store::jobs::open_merge(t, &s.project)?.is_some() {
                    return Ok(());
                }
                let ready = if s.merge_lane {
                    let (ready, blocker) = merge_candidates(t, &s, &queue)?;
                    if let Some(card) = blocker {
                        admit(t, &card, true)?;
                        return Ok(());
                    }
                    ready
                } else {
                    queue
                };
                let size = s.batch_size.unwrap_or(1).max(1);
                let waited_out = s.max_wait_seconds.is_some_and(|w| {
                    let cutoff = crate::time_param::stored(
                        chrono::Utc::now() - chrono::Duration::seconds(w),
                    );
                    // The longest wait: a card put back (stale) is first
                    // in the queue, but its wait started again.
                    ready
                        .iter()
                        .map(|q| q.1.as_str())
                        .min()
                        .is_some_and(|m| m <= cutoff.as_str())
                });
                if ready.is_empty() || ((ready.len() as i64) < size && !waited_out) {
                    return Ok(());
                }
                let mut n = s.max_batch_cards.unwrap_or(size).min(ready.len() as i64);
                if let Some(r) = room {
                    n = n.min(r);
                }
                for (card, _) in ready.iter().take(usize::try_from(n).unwrap_or(0)) {
                    admit(t, card, true)?;
                }
                return Ok(());
            }
        }
    }
}

/// [`release`] for the column of card `card`: its reviewed commit is
/// recorded now, so a merge_queue batch may take it (TER-1204).
pub fn release_card_column(t: &mut Tx<'_>, card: &str) -> ApiResult<()> {
    let column: Option<i64> = t
        .query_row(
            "SELECT column_id FROM cards WHERE id = ?1 AND archived = 0",
            [card],
            |r| r.get(0),
        )
        .optional()?;
    match column {
        Some(c) => release(t, c),
        None => Ok(()),
    }
}

/// [`release`] for each column of the board of `project`.
pub fn release_project(t: &mut Tx<'_>, project: &str) -> ApiResult<()> {
    if !active(t, project)? {
        return Ok(());
    }
    for c in column_rows(t, project)? {
        release(t, c.id)?;
    }
    Ok(())
}

/// A job ended: release the columns of its cards (the `job` slot).
pub fn on_job_end(t: &mut Tx<'_>, job: &str) -> ApiResult<()> {
    let columns: Vec<i64> = {
        let mut st = t.prepare(
            "SELECT DISTINCT c.column_id FROM cards c, jobs j
              WHERE j.id = ?1 AND c.archived = 0
                AND (c.id = j.card OR EXISTS (SELECT 1 FROM json_each(j.batch)
                                             WHERE json_extract(value, '$.card') = c.id))",
        )?;
        st.query_map([job], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for c in columns {
        release(t, c)?;
    }
    Ok(())
}

/// The projects with a card in a queue ([`sweep_all`]).
fn projects_with_queues(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut st = conn.prepare(
        "SELECT k.project FROM card_queue q
           JOIN board_columns k ON k.id = q.column_id
          WHERE q.admitted_at IS NULL
         UNION
         SELECT k.project FROM board_columns k JOIN cards c ON c.column_id = k.id
          WHERE k.queue_release = 'batch' AND c.archived = 0 AND c.kind = 'card'
         ORDER BY 1",
    )?;
    let rows = st
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Release every queue that has a waiting card, and every `batch` column
/// with a card in it: the wait of a batch, the stale cards of a batch
/// (TER-1204), and any release that no cause triggered. The rule engine's
/// tick runs it.
pub fn sweep_all(t: &mut Tx<'_>) -> ApiResult<()> {
    for project in projects_with_queues(t)? {
        release_project(t, &project)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The stale test of a batch reads jobs by index, never by a scan
    /// (rev-1204b finding 5). Run with `--nocapture` for the plan.
    #[test]
    fn the_stale_test_reads_jobs_by_index() {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        let mut q = c
            .prepare(&format!("EXPLAIN QUERY PLAN {}", stale_sql()))
            .unwrap();
        let plans: Vec<String> = q
            .query_map(params![1, "2026-11-14T00:00:00.000Z"], |r| r.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        println!("{}", plans.join("\n"));
        // `SCAN json_each` (a batch list) is fine; a scan of a table is not.
        let scans = |t: &str| {
            plans
                .iter()
                .any(|p| p == &format!("SCAN {t}") || p.starts_with(&format!("SCAN {t} ")))
        };
        assert!(!scans("j") && !scans("c") && !scans("q"), "{plans:?}");
        assert!(
            plans.iter().any(|p| p.contains("jobs_project_created_at")),
            "{plans:?}"
        );
        assert!(
            plans.iter().any(|p| p.contains("jobs_project_state")),
            "{plans:?}"
        );
    }
}
