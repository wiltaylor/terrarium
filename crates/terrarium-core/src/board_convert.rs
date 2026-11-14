//! The conversion of a board to column rules (TER-1117, kanban rework C5,
//! D5 of the handoff in TER-1117).
//!
//! The upgrade converts every board: the rules that the board runs today
//! (the built-in pack mapped to the board, with its `rules.wcl`, see
//! [`crate::rules_tools::migrate`]) become rules of its columns, and the
//! `card_dispatcher` and `review_dispatcher` settings become
//! `card.assign` and `review.start` rules (D6). Nothing here reads the
//! database: the daemon gives the rules and the columns, and stores the
//! result (`terrariumd::board_convert`).
//!
//! Each rule goes to the columns where it fires today (docs/api.md
//! §19.14):
//!
//! - A `card` rule on `moved` or `created` becomes a `lands` rule of each
//!   column that its `data.to`, `data.to_role` and `if_card.column` allow.
//!   The `data.to*` conditions go (the column is the condition, and the
//!   `admitted` event of a column queue has no `to`). `data.change` stays;
//!   on a column that can queue, `moved` and `created` also match
//!   `admitted`. A condition on `data.from*` stays: a card that returns
//!   from a wait column lands where it returns to (rev-1215 note 1: a card
//!   that only waited in a queue never leaves it).
//! - A `card` rule on `wait_met` or `archived` gets that trigger on each
//!   column that its `if_card.column` allows (every column but `done` for
//!   `wait_met`).
//! - A `job` or `job.verdict` rule becomes a `job_result` rule, with a copy
//!   on every column but `done`: a job's cards can be in any column (a
//!   merge can be queued from any column, rev-1215 note 3).
//! - A `rule.hourly` rule becomes a `timer` of 3600 seconds on the first
//!   column. A timer run has no card, so it runs once an hour.
//! - A rule with `version.cut` also gets a `timer` of 60 seconds (the
//!   4-hour cut, TER-1199). A batch `job.queue` with `min_cards` or
//!   `max_wait_minutes` (`merge-queue-batch`) becomes the batch queue of
//!   its column and a `lands` rule that queues the admitted cards.
//!
//! A rule that fits none of these is not converted: the report names it
//! with the reason, and the conversion goes on (D5: no fallback).

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::board_rules::{ColumnRule, JOB_EVENTS, JOB_KINDS, Trigger};
use crate::rules_file::{Action, BoardColumn, Rule, resolve_column};
use crate::schedule::{MatchOp, MatchRule};

/// The template map of the `card_dispatcher` setting
/// (`terrariumd::card_dispatcher::template`), as `card.assign.templates`.
pub const DISPATCH_TEMPLATES: &[&str] = &[
    "type:small=card-small",
    "type:impl=card-impl",
    "type:hard=card-impl-hard",
    "type:impl-hard=card-impl-hard",
];
/// The template and the rounds of the `review_dispatcher` setting.
pub const REVIEW_TEMPLATE: &str = "card-review";
pub const REVIEW_MAX_ROUNDS: u64 = 3;
/// The period of the timers that replace a setting's tick or the legacy
/// batch and version timers: the shortest that a column timer has.
pub const TICK_SECONDS: u64 = crate::board_rules::TIMER_MIN_SECONDS;
/// The period of a converted `rule.hourly` rule.
pub const HOURLY_SECONDS: u64 = 3600;
/// The suffix of the timer rule that goes with a converted rule.
pub const TIMER_SUFFIX: &str = "-timer";

/// One column of the board to convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub role: Option<String>,
    /// The column has a WIP limit or a `job` release: a card that moves in
    /// can wait in its queue, and lands on its `admitted` event.
    pub queues: bool,
}

/// The dispatcher settings of the project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dispatchers {
    pub card: Option<String>,
    pub review: Option<String>,
}

/// The batch queue of a column, from a batch `job.queue` rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchQueue {
    pub column: String,
    pub batch_size: i64,
    pub max_wait_seconds: i64,
    pub max_batch_cards: i64,
}

/// What happened to one rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub rule: String,
    /// `converted`, `off` (the rule was off: nothing to convert), or
    /// `not_converted`.
    pub result: &'static str,
    /// The trigger of the new rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<&'static str>,
    /// The columns that got a copy, in board order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// The result of [`convert`].
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Conversion {
    /// The new rules of each column, in board order. A column with no new
    /// rule is left out.
    pub columns: Vec<(String, Vec<ColumnRule>)>,
    /// The batch queue to set, if a rule asked for one.
    pub batch: Option<BatchQueue>,
    pub report: Vec<Entry>,
}

impl Conversion {
    /// The number of new rules.
    pub fn rule_count(&self) -> usize {
        self.columns.iter().map(|(_, r)| r.len()).sum()
    }

    fn push(&mut self, column: &str, rule: ColumnRule) {
        match self.columns.iter_mut().find(|(c, _)| c == column) {
            Some((_, v)) => v.push(rule),
            None => self.columns.push((column.to_owned(), vec![rule])),
        }
    }

    /// Put the columns in board order.
    fn sort(&mut self, board: &[Column]) {
        let pos = |c: &str| board.iter().position(|b| b.name == c).unwrap_or(usize::MAX);
        self.columns.sort_by_key(|(c, _)| pos(c));
    }
}

fn texts(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// The values that `m` allows, for `eq` and `in`. `None` for another op.
fn allowed(m: &MatchRule) -> Option<Vec<String>> {
    match m.op {
        MatchOp::Eq | MatchOp::In => Some(texts(&m.value)),
        _ => None,
    }
}

fn when_on<'a>(rule: &'a Rule, path: &str) -> Vec<&'a MatchRule> {
    rule.when.iter().filter(|m| m.path == path).collect()
}

/// The values of `data.change` that the rule allows. `None`: no condition,
/// or one that is not `eq` or `in`.
fn changes(rule: &Rule) -> Option<Vec<String>> {
    let ms = when_on(rule, "data.change");
    if ms.is_empty() {
        return None;
    }
    let mut out: Option<Vec<String>> = None;
    for m in ms {
        let v = allowed(m)?;
        out = Some(match out {
            None => v,
            Some(o) => o.into_iter().filter(|x| v.contains(x)).collect(),
        });
    }
    out
}

fn board_columns(board: &[Column]) -> Vec<BoardColumn> {
    board
        .iter()
        .map(|c| BoardColumn {
            name: c.name.clone(),
            role: c.role.clone(),
        })
        .collect()
}

/// The columns of `refs` (`@<role>` or names) on the board.
fn resolved(board: &[Column], refs: &[String]) -> Vec<String> {
    let cols = board_columns(board);
    refs.iter()
        .filter_map(|r| resolve_column(&cols, r).map(|c| c.name.clone()))
        .collect()
}

/// Does column `c` have the role `role`? The pack's bound agent rules
/// also take a column with no role by its name (docs/api.md §6.10).
fn has_role(rule: &Rule, c: &Column, role: &str) -> bool {
    match c.role.as_deref() {
        Some(r) => r == role,
        None => matches!(rule.name.as_str(), "agent-park" | "agent-unpark") && c.name == role,
    }
}

/// The columns that the `if_card` conditions of `rule` allow, of `cands`.
fn by_if_card(board: &[Column], rule: &Rule, mut cands: Vec<String>) -> Vec<String> {
    for c in &rule.if_card {
        if !c.column.is_empty() {
            let keep = resolved(board, &c.column);
            cands.retain(|x| keep.contains(x));
        }
        if !c.not_column.is_empty() {
            let drop = resolved(board, &c.not_column);
            cands.retain(|x| !drop.contains(x));
        }
    }
    cands
}

/// The columns that a card rule lands on: those that its `data.to`,
/// `data.to_role` and `if_card` allow. `Err`: a condition that cannot be
/// placed (a glob).
fn landing(board: &[Column], rule: &Rule) -> Result<Vec<String>, String> {
    let mut cands: Vec<&Column> = board.iter().collect();
    for m in &rule.when {
        let role = match m.path.as_str() {
            "data.to" => false,
            "data.to_role" => true,
            _ => continue,
        };
        let matches = |c: &Column, v: &str| {
            if role {
                has_role(rule, c, v)
            } else {
                c.name == v
            }
        };
        match m.op {
            MatchOp::Eq | MatchOp::In => {
                let v = texts(&m.value);
                cands.retain(|c| v.iter().any(|x| matches(c, x)));
            }
            MatchOp::Ne => {
                let v = texts(&m.value);
                cands.retain(|c| !v.iter().any(|x| matches(c, x)));
            }
            MatchOp::Glob => {
                return Err(format!(
                    "a glob on `{}` names no column: convert it by hand",
                    m.path
                ));
            }
        }
    }
    let names = cands.into_iter().map(|c| c.name.clone()).collect();
    Ok(by_if_card(board, rule, names))
}

fn is_done(c: &str) -> bool {
    c == "done"
}

fn description(rule: &Rule, origin: &str) -> Option<String> {
    let from = format!("Converted from the {origin} rule `{}` (TER-1117).", rule.name);
    let text = match &rule.description {
        Some(d) if !d.trim().is_empty() => format!("{} {from}", d.trim()),
        _ => from,
    };
    Some(text.chars().take(crate::board_rules::MAX_DESCRIPTION_CHARS).collect())
}

fn column_rule(rule: &Rule, name: &str, origin: &str, trigger: Trigger) -> ColumnRule {
    ColumnRule {
        id: None,
        name: name.to_owned(),
        description: description(rule, origin),
        enabled: true,
        trigger,
        when: rule.when.clone(),
        if_card: rule.if_card.clone(),
        if_job: rule.if_job.clone(),
        max_per_hour: rule.max_per_hour,
        actions: rule.actions.clone(),
    }
}

/// `rule` with the conditions on `data.to` and `data.to_role` taken out,
/// and `data.change` widened to `admitted` on a column that queues.
fn landed(mut r: ColumnRule, queues: bool) -> ColumnRule {
    r.when.retain(|m| m.path != "data.to" && m.path != "data.to_role");
    if queues {
        for m in r.when.iter_mut().filter(|m| m.path == "data.change") {
            if let Some(mut v) = allowed(m)
                && v.iter().any(|x| x == "moved" || x == "created")
                && !v.iter().any(|x| x == "admitted")
            {
                v.push("admitted".into());
                m.op = MatchOp::In;
                m.value = json!(v);
            }
        }
    }
    r
}

/// The `job_result` trigger of a job rule: the events of its `data.change`
/// (all job events without one), `verdict` for `job.verdict`, and the
/// kinds of its `data.kind`.
fn job_trigger(rule: &Rule) -> Trigger {
    let mut events = Vec::new();
    if rule.on.iter().any(|k| k == "job") {
        let all: Vec<String> = JOB_EVENTS
            .iter()
            .filter(|e| **e != "verdict")
            .map(|e| (*e).to_owned())
            .collect();
        match changes(rule) {
            Some(v) => events.extend(all.into_iter().filter(|e| v.contains(e))),
            None => events.extend(all),
        }
    }
    if rule.on.iter().any(|k| k == "job.verdict") {
        events.push("verdict".into());
    }
    let mut job_kinds: Vec<String> = Vec::new();
    for m in when_on(rule, "data.kind") {
        if let Some(v) = allowed(m) {
            job_kinds = v.into_iter().filter(|k| JOB_KINDS.contains(&k.as_str())).collect();
        }
    }
    Trigger::JobResult { events, job_kinds }
}

fn verb_args<'a>(rule: &'a Rule, verb: &str) -> Option<&'a Map<String, Value>> {
    rule.actions.iter().find(|a| a.verb == verb).map(|a| &a.args)
}

/// A batch `job.queue` that waits for a count or a time (the legacy batch
/// timer, `merge-queue-batch`): the queue of its `from` column.
fn batch_queue(board: &[Column], rule: &Rule) -> Option<BatchQueue> {
    let args = verb_args(rule, "job.queue")?;
    if args.get("kind").and_then(Value::as_str) != Some("batch") {
        return None;
    }
    let min = args.get("min_cards").and_then(Value::as_i64);
    let wait = args.get("max_wait_minutes").and_then(Value::as_i64);
    if min.is_none() && wait.is_none() {
        return None;
    }
    let from = args.get("from").and_then(Value::as_str)?;
    let column = resolved(board, &[from.to_owned()]).into_iter().next()?;
    let limit = args.get("limit").and_then(Value::as_i64);
    let batch_size = min.unwrap_or(1).max(1);
    // Without a wait, a batch waits up to 30 days (the most a queue takes).
    let max_wait_seconds = wait.map_or(2_592_000, |m| (m * 60).clamp(1, 2_592_000));
    let max_batch_cards = limit.unwrap_or(batch_size).max(batch_size);
    Some(BatchQueue {
        column,
        batch_size,
        max_wait_seconds,
        max_batch_cards,
    })
}

/// The `job.queue` of a batch rule without the arguments of the legacy
/// timer: the column's queue admits the batch (TER-1199).
fn plain_batch(rule: &Rule) -> Vec<Action> {
    rule.actions
        .iter()
        .map(|a| {
            let mut a = a.clone();
            if a.verb == "job.queue" {
                for k in ["min_cards", "max_wait_minutes", "limit"] {
                    a.args.remove(k);
                }
            }
            a
        })
        .collect()
}

/// Convert `rules` (in the order that the engine runs them, each with its
/// origin: `pack`, `override` or `own`) to rules of the columns of
/// `board`, and the dispatcher settings `d` to rules. A rule that is off
/// is reported and not converted.
pub fn convert(rules: &[(Rule, &str)], board: &[Column], d: &Dispatchers) -> Conversion {
    let mut out = Conversion::default();
    // The batch queue first: it makes its column one that queues.
    let mut board: Vec<Column> = board.to_vec();
    for (r, _) in rules.iter().filter(|(r, _)| r.enabled) {
        if let Some(q) = batch_queue(&board, r)
            && out.batch.is_none()
        {
            if let Some(c) = board.iter_mut().find(|c| c.name == q.column) {
                c.queues = true;
            }
            out.batch = Some(q);
        }
    }
    let queues = |c: &str| board.iter().any(|b| b.name == c && b.queues);
    let all: Vec<String> = board.iter().map(|c| c.name.clone()).collect();
    let not_done: Vec<String> = all.iter().filter(|c| !is_done(c)).cloned().collect();
    for (rule, origin) in rules {
        let mut entry = Entry {
            rule: rule.name.clone(),
            result: "converted",
            trigger: None,
            columns: Vec::new(),
            note: String::new(),
        };
        if !rule.enabled {
            entry.result = "off";
            entry.note = "the rule is off: nothing to convert".into();
            out.report.push(entry);
            continue;
        }
        let on: Vec<&str> = rule.on.iter().map(String::as_str).collect();
        let is_job = !on.is_empty() && on.iter().all(|k| *k == "job" || *k == "job.verdict");
        if is_job {
            let trigger = job_trigger(rule);
            if matches!(&trigger, Trigger::JobResult { events, .. } if events.is_empty()) {
                entry.result = "not_converted";
                entry.note = "its `data.change` names no job event".into();
                out.report.push(entry);
                continue;
            }
            // Its `if_card` stays and holds at the run, as in the pack.
            let cols = not_done.clone();
            for c in &cols {
                out.push(c, column_rule(rule, &rule.name, origin, trigger.clone()));
            }
            entry.trigger = Some("job_result");
            entry.columns = cols;
        } else if on == ["rule.hourly"] {
            let Some(first) = all.first() else {
                continue;
            };
            let mut r = column_rule(
                rule,
                &rule.name,
                origin,
                Trigger::Timer {
                    every_seconds: HOURLY_SECONDS,
                },
            );
            r.when.clear();
            out.push(first, r);
            entry.trigger = Some("timer");
            entry.columns = vec![first.clone()];
            entry.note = "every 3600 s (the hourly event)".into();
        } else if on == ["card"] {
            let Some(change) = changes(rule) else {
                entry.result = "not_converted";
                entry.note =
                    "a card rule needs a `data.change` condition with `eq` or `one_of`".into();
                out.report.push(entry);
                continue;
            };
            let lands = change.iter().all(|c| c == "moved" || c == "created");
            if lands {
                if let Some(q) = batch_queue(&board, rule) {
                    let mut r = column_rule(rule, &rule.name, origin, Trigger::Lands);
                    r.when.clear();
                    r.actions = plain_batch(rule);
                    out.push(&q.column, r);
                    entry.trigger = Some("lands");
                    entry.columns = vec![q.column.clone()];
                    entry.note = format!(
                        "the column queue releases the batch: {} cards, or after {} s, at most {}",
                        q.batch_size, q.max_wait_seconds, q.max_batch_cards
                    );
                    out.report.push(entry);
                    continue;
                }
                let cols = match landing(&board, rule) {
                    Ok(c) => c,
                    Err(e) => {
                        entry.result = "not_converted";
                        entry.note = e;
                        out.report.push(entry);
                        continue;
                    }
                };
                for c in &cols {
                    let r = landed(column_rule(rule, &rule.name, origin, Trigger::Lands), queues(c));
                    out.push(c, r);
                }
                entry.trigger = Some("lands");
                if verb_args(rule, "version.cut").is_some() {
                    for c in &cols {
                        let mut r = column_rule(
                            rule,
                            &format!("{}{TIMER_SUFFIX}", rule.name),
                            origin,
                            Trigger::Timer {
                                every_seconds: TICK_SECONDS,
                            },
                        );
                        r.when.clear();
                        out.push(c, r);
                    }
                    entry.note = format!("and a timer every {TICK_SECONDS} s for the wait");
                }
                entry.columns = cols;
            } else if change == ["wait_met"] || change == ["archived"] {
                let (trigger, kind, base) = if change == ["wait_met"] {
                    (Trigger::WaitMet, "wait_met", not_done.clone())
                } else {
                    (Trigger::Archived, "archived", all.clone())
                };
                let cols = by_if_card(&board, rule, base);
                for c in &cols {
                    out.push(c, column_rule(rule, &rule.name, origin, trigger.clone()));
                }
                entry.trigger = Some(kind);
                entry.columns = cols;
            } else {
                entry.result = "not_converted";
                entry.note = format!(
                    "no column trigger for the card change(s) {}",
                    change.join(", ")
                );
            }
        } else {
            entry.result = "not_converted";
            entry.note = format!("no column trigger for the event(s) {}", rule.on.join(", "));
        }
        if entry.result == "converted" && entry.columns.is_empty() {
            entry.note = "no column of this board matches: the rule never ran here".into();
        }
        out.report.push(entry);
    }
    dispatchers(&mut out, &board, d);
    out.sort(&board);
    out
}

fn setting_rule(name: &str, text: &str, trigger: Trigger, action: Action) -> ColumnRule {
    ColumnRule {
        id: None,
        name: name.to_owned(),
        description: Some(text.to_owned()),
        enabled: true,
        trigger,
        when: Vec::new(),
        if_card: Vec::new(),
        if_job: Vec::new(),
        max_per_hour: Some(crate::rules_file::MAX_PER_HOUR),
        actions: vec![action],
    }
}

fn args(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// The rules of the dispatcher settings (D6).
fn dispatchers(out: &mut Conversion, board: &[Column], d: &Dispatchers) {
    let cols = board_columns(board);
    if let Some(owner) = &d.card {
        let ready = resolve_column(&cols, "ready").map(|c| c.name.clone());
        let working = resolve_column(&cols, "@working")
            .filter(|c| c.role.as_deref() == Some("working"))
            .map(|c| c.name.clone());
        let mut entry = Entry {
            rule: "card_dispatcher".into(),
            result: "converted",
            trigger: Some("lands"),
            columns: Vec::new(),
            note: format!("owner `{owner}`, the type: map as `templates`"),
        };
        match (ready, working) {
            (Some(ready), Some(working)) => {
                let assign = Action {
                    verb: "card.assign".into(),
                    args: args(json!({"templates": DISPATCH_TEMPLATES, "owner": owner,
                                      "from": ready,
                                      "limit": crate::rules_file::MAX_ASSIGN_LIMIT})),
                };
                let text = "Start a card agent for the top ready cards while the working column \
                            has room (the card_dispatcher setting, TER-1117).";
                out.push(
                    &ready,
                    setting_rule("dispatch", text, Trigger::Lands, assign.clone()),
                );
                out.push(
                    &ready,
                    setting_rule(
                        &format!("dispatch{TIMER_SUFFIX}"),
                        text,
                        Trigger::Timer {
                            every_seconds: TICK_SECONDS,
                        },
                        assign.clone(),
                    ),
                );
                out.push(
                    &working,
                    setting_rule("dispatch-on-free-slot", text, Trigger::Leaves, assign),
                );
                entry.columns = vec![ready, working];
            }
            _ => {
                entry.result = "not_converted";
                entry.note = "the board has no `ready` column or no `working` column".into();
            }
        }
        out.report.push(entry);
    }
    if let Some(owner) = &d.review {
        let mut entry = Entry {
            rule: "review_dispatcher".into(),
            result: "converted",
            trigger: Some("lands"),
            columns: Vec::new(),
            note: format!("owner `{owner}`, template `{REVIEW_TEMPLATE}`, {REVIEW_MAX_ROUNDS} rounds"),
        };
        match crate::rules_file::review_column(&cols).map(|c| c.name.clone()) {
            Some(review) => {
                let start = |from: bool| {
                    let mut a = json!({"owner": owner, "template": REVIEW_TEMPLATE,
                                       "max_rounds": REVIEW_MAX_ROUNDS});
                    if from {
                        a["from"] = json!(review);
                    }
                    Action {
                        verb: "review.start".into(),
                        args: args(a),
                    }
                };
                let text = "Start a review agent for a card in review (the review_dispatcher \
                            setting, TER-1117).";
                out.push(
                    &review,
                    setting_rule("review-start", text, Trigger::Lands, start(false)),
                );
                out.push(
                    &review,
                    setting_rule(
                        &format!("review-start{TIMER_SUFFIX}"),
                        text,
                        Trigger::Timer {
                            every_seconds: TICK_SECONDS,
                        },
                        start(true),
                    ),
                );
                entry.columns = vec![review];
            }
            None => {
                entry.result = "not_converted";
                entry.note = "the board has no review column".into();
            }
        }
        out.report.push(entry);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::board_rules::read_rules;
    use crate::rules_file::{self, Context, RuleSet, default_pack};

    fn col(name: &str, role: Option<&str>, queues: bool) -> Column {
        Column {
            name: name.into(),
            role: role.map(str::to_owned),
            queues,
        }
    }

    /// The board of project terrarium (2026-11-14).
    fn terrarium_board() -> Vec<Column> {
        vec![
            col("backlog", None, false),
            col("research", None, true),
            col("ready", None, false),
            col("in_progress", Some("working"), true),
            col("review", Some("review"), false),
            col("merge_queue", Some("merge_queue"), false),
            col("accepted", Some("accepted"), false),
            col("blocked", Some("blocked"), false),
            col("done", None, false),
        ]
    }

    fn pack() -> Vec<(Rule, &'static str)> {
        default_pack().rules.iter().map(|r| (r.clone(), "pack")).collect()
    }

    fn merge_queue_rules() -> RuleSet {
        rules_file::parse(
            rules_file::MERGE_QUEUE_RULES,
            "rules.merge_queue.wcl",
            &Context::default(),
        )
        .unwrap()
    }

    fn rules_of<'a>(c: &'a Conversion, column: &str) -> Vec<&'a ColumnRule> {
        c.columns
            .iter()
            .find(|(n, _)| n == column)
            .map(|(_, r)| r.iter().collect())
            .unwrap_or_default()
    }

    fn names(c: &Conversion, column: &str) -> Vec<String> {
        rules_of(c, column).iter().map(|r| r.name.clone()).collect()
    }

    /// Every column's new rules pass the checks of a `PUT` on a columns
    /// board.
    fn check_all(c: &Conversion, board: &[Column]) {
        let ctx = Context {
            columns: Some(board_columns(board)),
            max_rules: usize::MAX,
            columns_flow: true,
        };
        for (column, rules) in &c.columns {
            let values: Vec<Value> = rules.iter().map(|r| serde_json::to_value(r).unwrap()).collect();
            if let Err(p) = read_rules(&ctx, &values) {
                panic!("{column}: {p:?}");
            }
            assert!(rules.len() <= crate::board_rules::MAX_RULES_PER_COLUMN, "{column}");
        }
        assert!(c.rule_count() <= crate::board_rules::MAX_RULES_PER_PROJECT);
    }

    #[test]
    fn the_pack_on_the_terrarium_board_converts_every_rule() {
        let board = terrarium_board();
        let c = convert(&pack(), &board, &Dispatchers::default());
        check_all(&c, &board);
        let not: Vec<_> = c.report.iter().filter(|e| e.result != "converted").collect();
        assert!(not.is_empty(), "{not:?}");
        assert!(c.batch.is_none());
        // The triage mails on backlog, the hourly one as a timer.
        let backlog = names(&c, "backlog");
        for n in ["triage-new-backlog", "triage-moved-backlog", "triage-hourly"] {
            assert!(backlog.contains(&n.to_owned()), "{backlog:?}");
        }
        let hourly = rules_of(&c, "backlog")
            .into_iter()
            .find(|r| r.name == "triage-hourly")
            .unwrap();
        assert_eq!(hourly.trigger, Trigger::Timer { every_seconds: 3600 });
        // The cut on accepted, and its timer.
        let acc = names(&c, "accepted");
        assert!(acc.contains(&"version-cut".to_owned()) && acc.contains(&"version-cut-timer".to_owned()));
        let cut = rules_of(&c, "accepted")
            .into_iter()
            .find(|r| r.name == "version-cut")
            .unwrap();
        assert!(cut.when.iter().all(|m| m.path != "data.to_role"), "{cut:?}");
        // The job rules on every column but done.
        for col in board.iter().filter(|c| c.name != "done") {
            let n = names(&c, &col.name);
            for j in ["merge-pass", "job-fail-red", "merge-queued", "agent-end-merged"] {
                assert!(n.contains(&j.to_owned()), "{}: {n:?}", col.name);
            }
        }
        assert!(!names(&c, "done").contains(&"merge-pass".to_owned()));
        // A returned card leaves its merge group where it lands.
        for col in ["in_progress", "ready", "blocked", "backlog"] {
            assert!(names(&c, col).contains(&"merge-group-returned".to_owned()), "{col}");
        }
        assert!(!names(&c, "review").contains(&"merge-group-returned".to_owned()));
        // Park on the wait columns; unpark where a card comes back to.
        for col in ["review", "merge_queue", "accepted"] {
            assert!(names(&c, col).contains(&"agent-park".to_owned()), "{col}");
            assert!(!names(&c, col).contains(&"agent-unpark".to_owned()), "{col}");
        }
        for col in ["backlog", "research", "ready", "in_progress", "blocked"] {
            assert!(names(&c, col).contains(&"agent-unpark".to_owned()), "{col}");
        }
        assert!(!names(&c, "done").contains(&"agent-unpark".to_owned()));
        assert_eq!(names(&c, "done").iter().filter(|n| n.starts_with("agent-end")).count(), 2);
        // A move into a column that queues also lands on `admitted`.
        let unpark = rules_of(&c, "in_progress")
            .into_iter()
            .find(|r| r.name == "agent-unpark")
            .unwrap();
        let change = unpark.when.iter().find(|m| m.path == "data.change").unwrap();
        assert_eq!(change.value, json!(["moved", "admitted"]));
        assert!(unpark.when.iter().any(|m| m.path == "data.from_role"));
        assert!(unpark.when.iter().all(|m| !m.path.starts_with("data.to")));
        let ready_unpark = rules_of(&c, "ready")
            .into_iter()
            .find(|r| r.name == "agent-unpark")
            .unwrap();
        let change = ready_unpark.when.iter().find(|m| m.path == "data.change").unwrap();
        assert_eq!(change.value, json!("moved"));
    }

    #[test]
    fn the_merge_queue_rules_become_the_batch_queue() {
        let board = terrarium_board();
        let mut rules = pack();
        rules.extend(merge_queue_rules().rules.into_iter().map(|r| (r, "own")));
        let c = convert(&rules, &board, &Dispatchers::default());
        check_all(&c, &board);
        assert_eq!(
            c.batch,
            Some(BatchQueue {
                column: "merge_queue".into(),
                batch_size: 4,
                max_wait_seconds: 1800,
                max_batch_cards: 8
            })
        );
        let batch = rules_of(&c, "merge_queue")
            .into_iter()
            .find(|r| r.name == "merge-queue-batch")
            .unwrap();
        assert_eq!(batch.trigger, Trigger::Lands);
        assert_eq!(batch.actions[0].args, args(json!({"kind": "batch", "from": "@merge_queue"})));
        // The batch column queues: a park there lands on `admitted` too.
        let park = rules_of(&c, "merge_queue")
            .into_iter()
            .find(|r| r.name == "agent-park")
            .unwrap();
        assert_eq!(park.when[0].value, json!(["moved", "admitted"]));
        assert!(names(&c, "review").contains(&"merge-queue-move".to_owned()));
    }

    #[test]
    fn the_dispatchers_become_assign_and_review_rules() {
        let board = terrarium_board();
        let d = Dispatchers {
            card: Some("lead".into()),
            review: Some("lead".into()),
        };
        let c = convert(&[], &board, &d);
        check_all(&c, &board);
        assert_eq!(names(&c, "ready"), vec!["dispatch", "dispatch-timer"]);
        assert_eq!(names(&c, "in_progress"), vec!["dispatch-on-free-slot"]);
        assert_eq!(names(&c, "review"), vec!["review-start", "review-start-timer"]);
        let a = &rules_of(&c, "ready")[0].actions[0];
        assert_eq!(a.verb, "card.assign");
        assert_eq!(a.args["owner"], "lead");
        assert_eq!(a.args["templates"], json!(DISPATCH_TEMPLATES));
        let r = &rules_of(&c, "review")[1].actions[0];
        assert_eq!(r.args["from"], "review");
        assert_eq!(r.args["max_rounds"], 3);
    }

    #[test]
    fn rules_that_fit_no_trigger_are_reported_and_the_rest_converts() {
        let board = terrarium_board();
        let text = r#"
rule "on-mail" {
  on = ["mail"]
  mail { to = ["operator"] subject = "x" }
}
rule "any-card" {
  on = ["card"]
  card_comment { text = "x" }
}
rule "glob" {
  on = ["card"]
  when "data.change" { eq = "moved" }
  when "data.to" { glob = "re*" }
  card_comment { text = "x" }
}
rule "off" {
  enabled = false
  on = ["card"]
  when "data.change" { eq = "moved" }
  card_comment { text = "x" }
}
"#;
        let own = rules_file::parse(text, "rules.wcl", &Context::default()).unwrap();
        let rules: Vec<_> = own.rules.into_iter().map(|r| (r, "own")).collect();
        let c = convert(&rules, &board, &Dispatchers::default());
        let res: Vec<_> = c.report.iter().map(|e| (e.rule.as_str(), e.result)).collect();
        assert_eq!(
            res,
            vec![
                ("on-mail", "not_converted"),
                ("any-card", "not_converted"),
                ("glob", "not_converted"),
                ("off", "off")
            ]
        );
        assert_eq!(c.rule_count(), 0);
    }

    #[test]
    fn a_second_conversion_is_the_same() {
        let board = terrarium_board();
        let a = convert(&pack(), &board, &Dispatchers::default());
        let b = convert(&pack(), &board, &Dispatchers::default());
        assert_eq!(a, b);
    }
}
