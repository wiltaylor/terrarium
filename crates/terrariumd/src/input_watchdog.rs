//! The input-box watchdog, daemon side (TER-194, TER-208). An idle agent
//! that keeps unsubmitted text in the input box of its harness takes no
//! turn, and nothing wakes it: its queued turns and mail wait. The runner
//! sees it ([`terrarium_core::Adapter::input_pending`]) and moves the agent
//! to `blocked` with the reason
//! [`terrarium_core::adapter::INPUT_PENDING_REASON`]. The daemon stores the
//! status with that reason, and [`notify`] mails the leads of the agent's
//! project (or the operator, when the project has no other lead), so a
//! stuck agent is seen.
//!
//! The runner flags each stuck box once, and the event pump drops a
//! replayed runner event, so one stuck box gives one mail.

use serde_json::json;
use terrarium_core::AgentStatus;
use terrarium_core::adapter::INPUT_PENDING_REASON;

use crate::db::Tx;
use crate::error::ApiResult;
use crate::events::EventDraft;
use crate::store::agents;
use crate::store::mail::{self, Address, Outgoing};

/// True for the status change of the runner's input-box watchdog.
pub fn is_stuck(to: AgentStatus, reason: Option<&str>) -> bool {
    to == AgentStatus::Blocked && reason == Some(INPUT_PENDING_REASON)
}

/// Mail the leads of agent `id` (or the operator) that its input box holds
/// unsubmitted text, and record an `input_watchdog` event.
pub fn notify(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let a = agents::get(t, id)?.agent;
    let who = format!("{}/{}", a.project, a.name);
    let mut to = crate::question_guard::leads(t, &a.project, id)?;
    if to.is_empty() {
        to.push(Address::Operator);
    }
    let subject = format!("{who} has unsubmitted text in its input box");
    let body = format!(
        "Agent `{who}` is idle, and the input box of its harness holds text that was not \
         submitted. No turn and no mail can reach it until the box is submitted or \
         emptied, so the runner marked it `blocked`.\n\n\
         Look at the screen: `terrarium agent screen {who}`. Submit the text with \
         `terrarium agent keys {who} enter`, or empty the box and send the turn again. \
         The agent goes back to `idle` when the box is empty.",
    );
    let mut thread: Option<String> = None;
    for recipient in &to {
        let m = mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: recipient,
                subject: &subject,
                body: &body,
                thread: thread.as_deref(),
                in_reply_to: None,
            },
        )?;
        thread = Some(m.thread_id);
    }
    let session = agents::current_session(t, id)?;
    t.emit(
        EventDraft::new(
            "input_watchdog",
            format!("{}: unsubmitted text in the input box; leads mailed", a.name),
            json!({
                "reason": INPUT_PENDING_REASON,
                "mailed": to.iter().map(Address::key).collect::<Vec<_>>(),
            }),
        )
        .agent(id.to_owned(), a.name.clone())
        .project(a.project.clone())
        .session(session),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_watchdog_reason_counts() {
        assert!(is_stuck(AgentStatus::Blocked, Some(INPUT_PENDING_REASON)));
        assert!(!is_stuck(AgentStatus::Blocked, Some("stalled")));
        assert!(!is_stuck(AgentStatus::Blocked, None));
        assert!(!is_stuck(AgentStatus::Idle, Some(INPUT_PENDING_REASON)));
    }
}
