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
//!     card has ended after it (passed, failed, or cancelled) and it has no
//!     open job, or it leaves the column. Then the next card is admitted.
//!   - `batch`: when no card of the last batch is still in the column, and
//!     `batch_size` cards wait or the first one has waited
//!     `max_wait_seconds`: admit up to `max_batch_cards` (default
//!     `batch_size`) in queue order. The last batch out means the lane is
//!     free, as the `merge-queue-batch` rule did.
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
    project: String,
    wip_limit: Option<i64>,
    release: QueueRelease,
    batch_size: Option<i64>,
    max_wait_seconds: Option<i64>,
    max_batch_cards: Option<i64>,
    research: bool,
}

fn settings(conn: &Connection, column_id: i64) -> ApiResult<Option<Settings>> {
    Ok(conn
        .query_row(
            "SELECT id, project, wip_limit, queue_release, queue_batch_size,
                    queue_max_wait_seconds, queue_max_batch_cards, name
             FROM board_columns WHERE id = ?1",
            [column_id],
            |r| {
                Ok(Settings {
                    id: r.get(0)?,
                    project: r.get(1)?,
                    wip_limit: r.get(2)?,
                    release: QueueRelease::parse(&r.get::<_, String>(3)?)?,
                    batch_size: r.get(4)?,
                    max_wait_seconds: r.get(5)?,
                    max_batch_cards: r.get(6)?,
                    research: r.get::<_, String>(7)?.eq_ignore_ascii_case("research"),
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

/// A job of card `q.card` (its own job, or a batch job that has it).
const JOB_OF_CARD: &str = "(j.card = q.card OR EXISTS (SELECT 1 FROM json_each(j.batch)
                            WHERE json_extract(value, '$.card') = q.card))";

/// Does a card of column `column_id` hold its job slot (release `job`)?
fn job_slot_busy(conn: &Connection, column_id: i64) -> ApiResult<bool> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM card_queue q JOIN cards c ON c.id = q.card
          WHERE q.column_id = ?1 AND q.admitted_at IS NOT NULL AND c.archived = 0
            AND c.column_id = q.column_id
            AND (EXISTS (SELECT 1 FROM jobs j WHERE j.state IN ('queued', 'running')
                           AND {JOB_OF_CARD})
                 OR NOT EXISTS (SELECT 1 FROM jobs j
                           WHERE j.state IN ('passed', 'failed', 'cancelled')
                             AND j.ended_at >= q.admitted_at AND {JOB_OF_CARD})))"
    );
    Ok(conn.query_row(&sql, [column_id], |r| r.get(0))?)
}

/// Is a card of the last batch of column `column_id` still in it?
fn batch_open(conn: &Connection, column_id: i64) -> ApiResult<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM card_queue q JOIN cards c ON c.id = q.card
          WHERE q.column_id = ?1 AND q.admitted_at IS NOT NULL AND c.archived = 0
            AND c.column_id = q.column_id)",
        [column_id],
        |r| r.get(0),
    )?)
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
        // A held ETA that no longer reads (a time in the past is fine) is
        // dropped: the card is admitted all the same.
        if let Ok(when) = terrarium_core::eta::resolve(&eta, chrono::Utc::now()) {
            let when = crate::time_param::stored(when);
            t.execute(
                "UPDATE cards SET eta_at = ?2, eta_set_at = ?3, eta_set_by = ?4 WHERE id = ?1",
                params![row.id, when, at, held_by],
            )?;
            let row = find_row(t, card)?;
            card_event(
                t,
                &row,
                "eta",
                json!({"eta_at": when, "previous": null, "held": true,
                       "labels": json_col::<Vec<String>>(&row.labels)}),
            )?;
        }
    }
    Ok(())
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
    t.execute(
        &format!(
            "DELETE FROM card_queue WHERE rowid IN (
               SELECT q.rowid FROM card_queue q JOIN cards c ON c.id = q.card
                WHERE q.column_id = ?1 AND q.admitted_at IS NOT NULL
                  AND (c.archived = 1 OR c.column_id <> q.column_id
                       OR (?2 AND NOT EXISTS (SELECT 1 FROM jobs j
                                WHERE j.state IN ('queued', 'running') AND {JOB_OF_CARD})
                              AND EXISTS (SELECT 1 FROM jobs j
                                WHERE j.state IN ('passed', 'failed', 'cancelled')
                                  AND j.ended_at >= q.admitted_at AND {JOB_OF_CARD}))))"
        ),
        params![s.id, s.release == QueueRelease::Job],
    )?;
    loop {
        let queue = waiting(t, s.id)?;
        let Some((first, first_at)) = queue.first().cloned() else {
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
                let size = s.batch_size.unwrap_or(1).max(1);
                let waited_out = s.max_wait_seconds.is_some_and(|w| {
                    let cutoff = crate::time_param::stored(
                        chrono::Utc::now() - chrono::Duration::seconds(w),
                    );
                    first_at <= cutoff
                });
                if (queue.len() as i64) < size && !waited_out {
                    return Ok(());
                }
                let mut n = s.max_batch_cards.unwrap_or(size).min(queue.len() as i64);
                if let Some(r) = room {
                    n = n.min(r);
                }
                for (card, _) in queue.iter().take(usize::try_from(n).unwrap_or(0)) {
                    admit(t, card, true)?;
                }
                return Ok(());
            }
        }
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
        "SELECT DISTINCT k.project FROM card_queue q
           JOIN board_columns k ON k.id = q.column_id
          WHERE q.admitted_at IS NULL ORDER BY k.project",
    )?;
    let rows = st
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Release every queue that has a waiting card: the wait of a batch, and
/// any release that no cause triggered. The rule engine's tick runs it.
pub fn sweep_all(t: &mut Tx<'_>) -> ApiResult<()> {
    for project in projects_with_queues(t)? {
        release_project(t, &project)?;
    }
    Ok(())
}
