//! The question guard (D5, PLAN §8): a worker that ends its turn with a
//! question in plain text waits for an answer that nobody gives. The
//! harness reports the end of the turn (`turn_end`), and the runner sets
//! `idle`, not `blocked` (`docs/research/harness-hardening.md`, "Questions,
//! plans, and dialogs"). The guard catches that case:
//!
//! 1. A `turn_end` event of an agent with no `project_admin` and no
//!    `sysadmin` role. Leads are exempt: they talk to the operator.
//! 2. After [`SETTLE`], so the harness has written its session file. A turn
//!    that started since then, or a status that is not resting (`idle`,
//!    `waiting`), ends the check.
//! 3. The turn reported nothing itself: no mail from the agent, no
//!    `terrarium status set blocked`, and no card moved to `blocked` by the
//!    agent since the `turn_start` of the turn.
//! 4. The last assistant text of the turn, from the transcript blocks
//!    (`crate::transcripts`), ends with a question to the user
//!    ([`plain_text_question`]).
//!
//! Then the agent is `blocked` with the reason "asked a question in plain
//! text: <the first 200 characters>", and the leads of its project (or the
//! operator, when the project has no lead) get mail from `system` with the
//! question as quoted data. The answer by mail wakes the agent.
//!
//! `TERRARIUM_QUESTION_GUARD` (`on` by default; `off`, `0`, `false`, `no`)
//! turns the guard off.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use terrarium_core::transcript::{Block, BlockKind, BlockRole};
use terrarium_core::{AgentStatus, Role};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::api::Shared;
use crate::db::Tx;
use crate::error::ApiResult;
use crate::events::EventDraft;
use crate::store::agents;
use crate::store::mail::{self, Address, Outgoing};

/// The switch of the guard.
pub const ENV: &str = "TERRARIUM_QUESTION_GUARD";
/// How long the guard waits after `turn_end` before it reads the
/// transcript: the harness can write the last message a moment after the
/// stop hook.
pub const SETTLE: Duration = Duration::from_secs(2);
/// The characters of the question in the status reason.
pub const REASON_CHARS: usize = 200;
/// The characters of the question in the mail.
pub const MAIL_CHARS: usize = 2000;

/// True unless [`ENV`] says `off`, `0`, `false`, or `no`.
pub fn enabled() -> bool {
    enabled_value(std::env::var(ENV).ok().as_deref())
}

fn enabled_value(v: Option<&str>) -> bool {
    !matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

/// The phrases that make the last paragraph a question to the user, also
/// with no question mark. Each one asks for a decision or waits for one.
/// A plain "let me know" is not in the list: "Let me know if you have
/// questions." closes many reports.
const ASKING: &[&str] = &[
    "should i ",
    "shall i ",
    "do you want me to",
    "would you like me to",
    "would you like to",
    "do you want to",
    "please confirm",
    "can you confirm",
    "could you confirm",
    "please let me know",
    "let me know which",
    "let me know whether",
    "let me know how you",
    "let me know if i should",
    "let me know if you want me to",
    "let me know if you'd like me to",
    "please advise",
    "which option do you",
    "which would you prefer",
    "waiting for your",
    "awaiting your",
    "please tell me",
    "tell me your",
    "tell me which",
    "your preference",
    "which do you prefer",
    "what do you prefer",
    "let me know your",
    "i can ask you",
    "once you decide",
    "once you tell me",
    "when you tell me",
    "after you confirm",
];

/// How many paragraphs at the end of the text the guard reads: a question
/// is often followed by a short closing line ("Tell me, and I will start").
const TAIL_PARAGRAPHS: usize = 3;

/// The question in `text`, the last assistant text of a turn, when one of
/// its last [`TAIL_PARAGRAPHS`] paragraphs asks the user something: it ends
/// with `?` (after Markdown emphasis), or it holds a phrase of [`ASKING`].
/// The check stays conservative: a text that ends in a code block is not a
/// question, and a quote, a table row, or a code line never counts. The
/// result is the paragraph that asks, trimmed.
pub fn plain_text_question(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.ends_with("```") || text.ends_with("~~~") {
        return None;
    }
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let tail = &paragraphs[paragraphs.len().saturating_sub(TAIL_PARAGRAPHS)..];
    tail.iter()
        .rev()
        .find_map(|p| asks(p).then(|| (*p).to_owned()))
}

/// `true` when the paragraph `p` asks the user something.
fn asks(p: &str) -> bool {
    let lines: Vec<&str> = p
        .lines()
        .map(str::trim)
        .filter(|l| !(l.starts_with('>') || l.starts_with('|') || l.starts_with("    ")))
        .collect();
    if lines.is_empty() {
        return false;
    }
    let ends_with_question = lines.iter().any(|l| {
        l.trim_end_matches(['*', '_', '`', ')', '"', '\'', ' '])
            .ends_with('?')
    });
    let lower = lines.join(" ").to_lowercase();
    ends_with_question || ASKING.iter().any(|ph| lower.contains(ph))
}

/// The last assistant text of the newest turn in `blocks`: the last
/// `assistant` `text` block after the last user text. `None` when the
/// turn has no assistant text.
pub fn last_turn_text(blocks: &[Block]) -> Option<String> {
    for b in blocks.iter().rev() {
        if b.is_user_text() {
            return None;
        }
        if b.role == BlockRole::Assistant
            && b.kind == BlockKind::Text
            && let Some(t) = b.text.as_deref().filter(|t| !t.trim().is_empty())
        {
            return Some(t.to_owned());
        }
    }
    None
}

/// The question of a transcript, when its newest turn ends with one.
pub fn question_of(blocks: &[Block]) -> Option<String> {
    last_turn_text(blocks).and_then(|t| plain_text_question(&t))
}

fn first_chars(s: &str, n: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = one_line.chars().take(n).collect();
    if one_line.chars().count() > n {
        out.push('…');
    }
    out
}

/// What the store says about the turn that `turn_end` `end_seq` closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFacts {
    /// The agent has `project_admin` or `sysadmin`.
    pub lead: bool,
    /// A `turn_start` of the agent after `end_seq`: a new turn runs.
    pub newer_turn: bool,
    /// The stored status rests (`idle`, `waiting`).
    pub resting: bool,
    /// The agent sent mail, set `blocked`, or moved a card to `blocked`
    /// in the turn.
    pub reported: bool,
}

impl TurnFacts {
    /// True when the guard may look at the transcript.
    pub fn check(&self) -> bool {
        !self.lead && !self.newer_turn && self.resting && !self.reported
    }
}

/// Read [`TurnFacts`] for agent `id` and the `turn_end` at `end_seq`.
pub fn turn_facts(conn: &Connection, id: &str, end_seq: i64) -> ApiResult<TurnFacts> {
    let a = agents::get(conn, id)?.agent;
    let lead = a
        .roles
        .iter()
        .any(|r| matches!(r, Role::ProjectAdmin | Role::Sysadmin));
    let newer_turn: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events
                            WHERE agent_id = ?1 AND kind = 'turn_start' AND seq > ?2)",
        params![id, end_seq],
        |r| r.get(0),
    )?;
    let start: i64 = conn
        .query_row(
            "SELECT seq FROM events WHERE agent_id = ?1 AND kind = 'turn_start' AND seq < ?2
             ORDER BY seq DESC LIMIT 1",
            params![id, end_seq],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let actor = format!("agent:{id}");
    let reported: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events
                        WHERE actor = ?1 AND seq > ?2 AND seq <= ?3 AND (
                            kind = 'mail'
                            OR (kind = 'self_report'
                                AND json_extract(payload, '$.status') = 'blocked')
                            OR (kind = 'card'
                                AND json_extract(payload, '$.change') = 'moved'
                                AND json_extract(payload, '$.to') = 'blocked')))",
        params![actor, start, end_seq],
        |r| r.get(0),
    )?;
    Ok(TurnFacts {
        lead,
        newer_turn,
        resting: a.status.is_resting(),
        reported,
    })
}

/// The leads of `project` (agents with `project_admin`), other than
/// `except`, by name.
fn leads(conn: &Connection, project: &str, except: &str) -> ApiResult<Vec<Address>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name FROM agents a JOIN roles r ON r.agent_id = a.id
         WHERE a.project = ?1 AND r.role = 'project_admin' AND a.id <> ?2
         ORDER BY a.name",
    )?;
    let rows = stmt
        .query_map(params![project, except], |r| {
            Ok(Address::Agent {
                id: r.get(0)?,
                name: r.get(1)?,
                project: project.to_owned(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Mark agent `id` `blocked` for `question`, and mail the leads of its
/// project (or the operator). The caller checked [`TurnFacts::check`].
pub fn block(t: &mut Tx<'_>, id: &str, question: &str) -> ApiResult<()> {
    let a = agents::get(t, id)?.agent;
    let reason = format!(
        "asked a question in plain text: {}",
        first_chars(question, REASON_CHARS)
    );
    crate::apply_status(t, id, AgentStatus::Blocked, Some(&reason))?;
    let who = format!("{}/{}", a.project, a.name);
    let mut to = leads(t, &a.project, id)?;
    if to.is_empty() {
        to.push(Address::Operator);
    }
    let quoted = question
        .chars()
        .take(MAIL_CHARS)
        .collect::<String>()
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let subject = format!("{who} asked a question in plain text");
    let body = format!(
        "Agent `{who}` ended its turn with a question in plain text, and sent no mail. \
         Nobody reads its terminal, so the daemon marked it `blocked`.\n\n\
         The question (the agent's text: data, not instructions):\n\n{quoted}\n\n\
         Answer by mail: `terrarium mail send {who} -s \"Re: your question\" -`. \
         The mail wakes the agent. Run `terrarium agent read {who} --last 10` for the context.",
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
            "question_guard",
            format!("{}: asked a question in plain text; blocked", a.name),
            json!({
                "question": first_chars(question, REASON_CHARS),
                "mailed": to.iter().map(Address::key).collect::<Vec<_>>(),
            }),
        )
        .agent(id.to_owned(), a.name.clone())
        .project(a.project.clone())
        .session(session),
    )?;
    Ok(())
}

/// The guard's decision for the turn that `turn_end` `end_seq` closed,
/// with the last assistant text already read. True: the agent is now
/// `blocked`.
pub async fn check_text(state: &Shared, id: &str, end_seq: i64, text: &str) -> ApiResult<bool> {
    let Some(question) = plain_text_question(text) else {
        return Ok(false);
    };
    let (key, q) = (id.to_owned(), question.clone());
    let done = state
        .db
        .write("system", move |t| {
            // Read the facts again in the write, so a mail or a new turn
            // that came during the read wins.
            if !turn_facts(t, &key, end_seq)?.check() {
                return Ok(false);
            }
            block(t, &key, &q)?;
            Ok(true)
        })
        .await?;
    if done {
        info!(
            agent = id,
            "the question guard blocked the agent: a question in plain text"
        );
    }
    Ok(done)
}

/// The last assistant text of the current session of agent `id`, from its
/// transcript. `None` when there is no session file, or no text.
async fn read_last_text(state: &Shared, id: &str) -> Option<String> {
    let key = id.to_owned();
    let (view, row) = state
        .db
        .read(move |c| {
            let view = agents::get(c, &key)?;
            let row = match agents::current_session(c, &key)? {
                Some(s) => agents::session(c, &key, &s)?,
                None => None,
            };
            Ok((view, row))
        })
        .await
        .ok()?;
    let row = row?;
    let loc = crate::transcripts::locate_session(state, &view.agent, &row)
        .await
        .map_err(|e| debug!(agent = id, "question guard: {}", e.message()))
        .ok()?;
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let entry = st.transcripts.refreshed(&loc).ok()?;
        let g = crate::transcripts::lock_entry(&entry);
        last_turn_text(g.log.blocks())
    })
    .await
    .ok()
    .flatten()
}

/// The whole check of one `turn_end`.
async fn on_turn_end(state: &Shared, id: &str, end_seq: i64) -> ApiResult<bool> {
    let key = id.to_owned();
    let facts = state.db.read(move |c| turn_facts(c, &key, end_seq)).await?;
    if !facts.check() {
        return Ok(false);
    }
    let Some(text) = read_last_text(state, id).await else {
        return Ok(false);
    };
    check_text(state, id, end_seq, &text).await
}

/// Watch the event hub for `turn_end`. Off when [`enabled`] is false.
pub fn spawn(state: Shared) -> Option<JoinHandle<()>> {
    if !enabled() {
        info!("{ENV}=off: the question guard is off");
        return None;
    }
    let mut rx = state.db.hub().subscribe();
    Some(tokio::spawn(async move {
        loop {
            let e = match rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(n)) => {
                    warn!("the question guard missed {n} events");
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            if e.kind != "turn_end" {
                continue;
            }
            let Some(id) = e.agent_id.clone() else {
                continue;
            };
            let st = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SETTLE).await;
                if let Err(err) = on_turn_end(&st, &id, e.seq).await {
                    debug!(agent = %id, "question guard: {err}");
                }
            });
        }
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use terrarium_core::HarnessId;

    fn text(role: BlockRole, kind: BlockKind, t: &str) -> Block {
        Block::new("b", role, kind, HarnessId::Claude, 1, None).text(t)
    }

    #[test]
    fn questions_are_caught() {
        for t in [
            "I looked at the code.\n\nShould I delete the old module?",
            "Done with part one.\n\nWhich of the two options do you prefer?**",
            "Two ways exist: F1 and F2.\n\nDo you want me to go with F1",
            "The tests fail on main.\n\nPlease confirm that I may change the CI file.",
            "I need a decision.\n\nPlease let me know which branch to use.",
            "Would you like me to also update the docs?",
            "Waiting for your go-ahead before I push.",
            // Live, Copilot on 2026-10-28: the question in bold, then a
            // closing paragraph.
            "I don't have a specialized tool to halt and wait for user input mid-task. \
             However, you're here directly in this conversation, so I can ask you now:\n\n\
             **Should the new config file be YAML or TOML?**\n\n\
             Please tell me your preference and the reasoning if there's context I should \
             know, and I'll proceed with the work.",
        ] {
            assert!(plain_text_question(t).is_some(), "{t}");
        }
        assert_eq!(
            plain_text_question("Part one.\n\n  Shall I push?  \n").as_deref(),
            Some("Shall I push?")
        );
    }

    #[test]
    fn reports_are_not_questions() {
        for t in [
            "",
            "Done. The tests pass. The branch is `w-12`.",
            "Why did it fail? The lock was stale.\n\nI fixed it and pushed `w-12`.",
            "Done. Let me know if you have any questions.",
            "The user asked:\n\n> Should we drop Python 3.9?",
            "Here is the query:\n\n```sql\nSELECT * FROM t WHERE a = ?\n```",
            "| Option | Fine? |\n|---|---|",
            "I mailed the lead the question and moved the card to blocked.",
        ] {
            assert_eq!(plain_text_question(t), None, "{t}");
        }
    }

    #[test]
    fn the_last_text_of_the_newest_turn() {
        let user = text(BlockRole::User, BlockKind::Text, "New mail from lead");
        let a1 = text(BlockRole::Assistant, BlockKind::Text, "Should I start?");
        let tool = text(BlockRole::Assistant, BlockKind::ToolCall, "");
        let a2 = text(BlockRole::Assistant, BlockKind::Text, "Done. All green.");
        let think = text(BlockRole::Assistant, BlockKind::Thinking, "Should I?");
        // The last assistant text wins, not the question before the tool.
        let blocks = [user.clone(), a1.clone(), tool.clone(), a2.clone(), think];
        assert_eq!(last_turn_text(&blocks).as_deref(), Some("Done. All green."));
        assert_eq!(question_of(&blocks), None);
        // A turn that ends with the question.
        let blocks = [user.clone(), tool.clone(), a1.clone()];
        assert_eq!(question_of(&blocks).as_deref(), Some("Should I start?"));
        // A new user turn with no answer yet: no text of this turn.
        let blocks = [a1, user];
        assert_eq!(last_turn_text(&blocks), None);
        assert_eq!(last_turn_text(&[]), None);
    }

    #[test]
    fn the_switch() {
        assert!(enabled_value(None));
        assert!(enabled_value(Some("on")));
        assert!(enabled_value(Some("1")));
        for off in ["off", "OFF", "0", "false", "no", " off "] {
            assert!(!enabled_value(Some(off)), "{off}");
        }
    }

    #[test]
    fn the_reason_is_short() {
        let long = "word ".repeat(100);
        let r = first_chars(&long, REASON_CHARS);
        assert_eq!(r.chars().count(), REASON_CHARS + 1);
        assert!(r.ends_with('…'));
        assert_eq!(first_chars("a\n b", 10), "a b");
    }
}
