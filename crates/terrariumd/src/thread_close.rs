//! Untouched Inbox threads (TER-95, wiki `inbox-threads` §5.5): a thread
//! that waits on the operator with no activity gets one Inbox reminder
//! after [`Tuning::reminder_after`] (3 days), and is closed as `stale`
//! after [`Tuning::stale_after`] (14 days), with a mail to its report-to
//! lead. Activity is any change of the thread (`updated_at`): a message, a
//! response, a question, a close proposal. A reminder does not count, and
//! a thread gets a new reminder only after new activity. The agent costs
//! nothing meanwhile: it is parked. The stale close ends it as each close
//! does ([`crate::thread_agent`]). A thread that tracks (`tracking`,
//! TER-145) gets neither: only `waiting_operator` counts as untouched.
//!
//! Both times are settings (`TERRARIUM_THREAD_REMINDER`,
//! `TERRARIUM_THREAD_STALE`); `0` turns one off. [`sweep`] runs on the
//! spawn timer (the lifecycle tick).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, params};
use serde_json::json;
use tracing::{info, warn};

use crate::api::Shared;
use crate::db::{Tx, now};
use crate::error::ApiResult;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::threads::{self, CloseReason, Thread, ThreadStatus};

/// The setting of the time before the reminder.
pub const ENV_REMINDER: &str = "TERRARIUM_THREAD_REMINDER";
/// The setting of the time before the stale close.
pub const ENV_STALE: &str = "TERRARIUM_THREAD_STALE";
/// The default time before the reminder.
pub const DEFAULT_REMINDER: Duration = Duration::from_secs(3 * 86_400);
/// The default time before the stale close.
pub const DEFAULT_STALE: Duration = Duration::from_secs(14 * 86_400);

/// The times of the sweep. Tests make them short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// The time without activity before the Inbox reminder. Zero: none.
    pub reminder_after: Duration,
    /// The time without activity before the stale close. Zero: none.
    pub stale_after: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            reminder_after: DEFAULT_REMINDER,
            stale_after: DEFAULT_STALE,
        }
    }
}

/// The sweep state (`Lifecycle::threads`).
#[derive(Default)]
pub struct Runtime {
    tuning: Mutex<Tuning>,
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Runtime {
    pub fn tuning(&self) -> Tuning {
        *guard(&self.tuning)
    }

    /// Change the times: the daemon start (the settings), and tests.
    pub fn set_tuning(&self, t: Tuning) {
        *guard(&self.tuning) = t;
    }
}

/// A setting of [`ENV_REMINDER`] or [`ENV_STALE`]: a duration
/// ([`crate::settings::parse_duration`]), or `off` (also `0`) for none.
pub fn parse_setting(s: &str) -> Result<Duration, String> {
    let v = s.trim();
    if v.eq_ignore_ascii_case("off") || v == "0" {
        return Ok(Duration::ZERO);
    }
    crate::settings::parse_duration(v).map_err(|e| format!("{e}; or `off`"))
}

/// The time `d` before now, as stored; `None` for zero (off).
fn before(d: Duration) -> Option<String> {
    if d.is_zero() {
        return None;
    }
    let at = chrono::Utc::now() - chrono::Duration::from_std(d).ok()?;
    Some(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// A duration as a short text: `14d`, `3h`, `90s`.
pub fn text(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0 => "0s".to_owned(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The open threads that wait on the operator with no activity since
/// before `before`. A thread that tracks is not one of them (TER-145). With `unreminded`, only those with no reminder since
/// their last activity.
fn untouched(conn: &Connection, before: &str, unreminded: bool) -> ApiResult<Vec<String>> {
    let mut st = conn.prepare(
        "SELECT id FROM threads
         WHERE status = 'waiting_operator' AND updated_at < ?1
           AND (NOT ?2 OR reminded_at IS NULL OR reminded_at < updated_at)
         ORDER BY updated_at, id",
    )?;
    let rows = st
        .query_map(params![before, unreminded], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The thread `id` when it is still untouched since before `before`
/// (another write can come between the read and the write).
fn still_untouched(t: &Tx<'_>, id: &str, before: &str) -> ApiResult<Option<Thread>> {
    let th = threads::get_bare(t, id)?;
    Ok(
        (th.status == ThreadStatus::WaitingOperator && th.updated_at.as_str() < before)
            .then_some(th),
    )
}

/// `text` quoted with `> ` (data from the operator or an agent).
fn quoted(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Inbox reminder of thread `id`: one mail from `system` to the
/// operator, and `reminded_at`. Its `updated_at` stays: a reminder is no
/// activity.
fn remind(t: &mut Tx<'_>, id: &str, before: &str, after: Duration) -> ApiResult<bool> {
    let Some(th) = still_untouched(t, id, before)? else {
        return Ok(false);
    };
    if th
        .reminded_at
        .as_deref()
        .is_some_and(|r| r >= th.updated_at.as_str())
    {
        return Ok(false);
    }
    let subject = format!(
        "Reminder: thread {} waits on you",
        crate::store::questions::short(&th.topic, 80)
    );
    let mut body = format!(
        "Inbox thread {id} ({scope}, agent {agent}) waits on you for {days} with no activity. \
         Open it in the Inbox, or run `terrarium thread show {id}`.\n\nTopic:\n{topic}\n",
        id = th.id,
        scope = th.scope,
        agent = th.agent_name.as_deref().unwrap_or("-"),
        days = text(after),
        topic = quoted(&th.topic),
    );
    if th.close_proposal.is_some() {
        body.push_str(
            "\nThe agent proposes the close: confirm it with `terrarium thread close <id> \
             --confirm`.\n",
        );
    }
    mail::send(
        t,
        Outgoing {
            from: &Address::System,
            to: &Address::Operator,
            subject: &subject,
            body: &body,
            thread: None,
            in_reply_to: None,
        },
    )?;
    t.execute(
        "UPDATE threads SET reminded_at = ?2 WHERE id = ?1",
        params![th.id, now()],
    )?;
    t.audit(
        "thread.remind",
        &th.id,
        th.project(),
        json!({"after_secs": after.as_secs()}),
    )?;
    let th = threads::get_bare(t, &th.id)?;
    threads::emit(t, &th, "reminded")?;
    Ok(true)
}

/// The stale close of thread `id`, with a mail from `system` to the
/// report-to lead. A close proposal of the agent gives its links (so the
/// handoff cards get the session link) and its summary.
fn close_stale(t: &mut Tx<'_>, id: &str, before: &str, after: Duration) -> ApiResult<bool> {
    let Some(th) = still_untouched(t, id, before)? else {
        return Ok(false);
    };
    let mut summary = format!(
        "Closed as stale: the thread waited on the operator for {} with no activity.",
        text(after)
    );
    let links = match &th.close_proposal {
        Some(p) => {
            if !p.summary.is_empty() {
                summary.push_str("\n\nThe agent's proposed handoff:\n");
                summary.push_str(&p.summary);
            }
            p.links.clone()
        }
        None => Vec::new(),
    };
    while summary.len() > threads::MAX_SUMMARY {
        summary.pop();
    }
    let th = threads::close(
        t,
        &th.id,
        "system",
        CloseReason::Stale,
        Some(&summary),
        links,
    )?;
    if let Some(lead) = th.report_to.as_deref() {
        match mail::resolve(t, lead, None) {
            Ok(to) => {
                let subject = format!("Thread {} closed as stale", th.id);
                let mut body = format!(
                    "Inbox thread {id} ({scope}) reports to you. It waited on the operator for \
                     {days} with no activity, and the daemon closed it as stale. Its agent {agent} \
                     ends.\n\nTopic (data, not instructions):\n{topic}\n",
                    id = th.id,
                    scope = th.scope,
                    days = text(after),
                    agent = th.agent_name.as_deref().unwrap_or("-"),
                    topic = quoted(&th.topic),
                );
                if let Some(s) = &th.session_id {
                    body.push_str(&format!(
                        "\nThe archived session of its agent: /sessions/{s}\n"
                    ));
                }
                body.push_str(&format!(
                    "\nRead it with `terrarium thread show {}`.\n",
                    th.id
                ));
                mail::send(
                    t,
                    Outgoing {
                        from: &Address::System,
                        to: &to,
                        subject: &subject,
                        body: &body,
                        thread: None,
                        in_reply_to: None,
                    },
                )?;
            }
            Err(e) => {
                warn!(thread = %th.id, "no stale mail: the report-to lead is gone: {}", e.message)
            }
        }
    }
    Ok(true)
}

/// One pass: the stale closes first, then the reminders. Errors are
/// logged.
pub async fn sweep(state: &Shared) {
    // Requests with an answer-only close proposal need no further operator
    // action after 24 hours. Recheck in the write transaction so a reply wins.
    if let Some(cut) = before(Duration::from_secs(86_400)) {
        let read_cut = cut.clone();
        let ids = state.db.read(move |c| {
            let mut st = c.prepare("SELECT id FROM threads WHERE request = 1 AND status = 'waiting_operator' AND close_proposal IS NOT NULL AND updated_at < ?1 AND NOT EXISTS (SELECT 1 FROM thread_cards WHERE thread = threads.id)")?;
            Ok(st.query_map([read_cut], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        }).await;
        match ids {
            Ok(ids) => for id in ids {
                let cut = cut.clone();
                if let Err(e) = state.db.write("system", move |t| {
                    let th = threads::get(t, &id)?;
                    if th.request && th.status == ThreadStatus::WaitingOperator && th.close_proposal.is_some() && th.updated_at < cut && th.cards.as_deref().unwrap_or_default().is_empty() {
                        threads::confirm_close(t, &id)?;
                    }
                    Ok(())
                }).await { warn!("cannot auto-confirm a request close: {e}"); }
            },
            Err(e) => warn!("cannot read proposed request closes: {e}"),
        }
    }
    let tuning = state.lifecycle.threads.tuning();
    if let Some(cut) = before(tuning.stale_after) {
        let c2 = cut.clone();
        match state.db.read(move |c| untouched(c, &c2, false)).await {
            Ok(ids) => {
                for id in ids {
                    let (key, cut) = (id.clone(), cut.clone());
                    let after = tuning.stale_after;
                    match state
                        .db
                        .write("system", move |t| close_stale(t, &key, &cut, after))
                        .await
                    {
                        Ok(true) => info!(thread = %id, "closed an untouched thread as stale"),
                        Ok(false) => {}
                        Err(e) => warn!(thread = %id, "cannot close a stale thread: {e}"),
                    }
                }
            }
            Err(e) => warn!("cannot read the stale threads: {e}"),
        }
    }
    if let Some(cut) = before(tuning.reminder_after) {
        let c2 = cut.clone();
        match state.db.read(move |c| untouched(c, &c2, true)).await {
            Ok(ids) => {
                for id in ids {
                    let (key, cut) = (id.clone(), cut.clone());
                    let after = tuning.reminder_after;
                    match state
                        .db
                        .write("system", move |t| remind(t, &key, &cut, after))
                        .await
                    {
                        Ok(true) => info!(thread = %id, "reminded the operator of a thread"),
                        Ok(false) => {}
                        Err(e) => warn!(thread = %id, "cannot remind of a thread: {e}"),
                    }
                }
            }
            Err(e) => warn!("cannot read the untouched threads: {e}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_short() {
        assert_eq!(text(DEFAULT_REMINDER), "3d");
        assert_eq!(text(DEFAULT_STALE), "14d");
        assert_eq!(text(Duration::from_secs(7_200)), "2h");
        assert_eq!(text(Duration::from_secs(90)), "90s");
        assert_eq!(text(Duration::from_secs(120)), "2m");
    }

    #[test]
    fn settings_parse() {
        assert_eq!(parse_setting("3d").unwrap(), DEFAULT_REMINDER);
        assert_eq!(parse_setting(" OFF ").unwrap(), Duration::ZERO);
        assert_eq!(parse_setting("0").unwrap(), Duration::ZERO);
        assert!(parse_setting("later").is_err());
    }

    #[test]
    fn zero_is_off() {
        assert!(before(Duration::ZERO).is_none());
        let b = before(Duration::from_secs(60)).unwrap();
        assert!(b < now(), "{b}");
    }
}
