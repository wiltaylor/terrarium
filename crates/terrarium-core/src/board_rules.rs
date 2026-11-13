//! The structured rules of a board column (TER-1120, kanban rework C1b).
//!
//! Each rule belongs to one column (D2). It has a trigger, conditions, and
//! ordered actions, as JSON. The daemon stores the rules of a column with
//! its options (`/api/projects/{name}/board/columns/{column}/options`) and
//! checks each write with [`read_rules`]. [`catalog`] lists the triggers,
//! the conditions, the verbs, and the limits for an editor.
//!
//! The conditions and the verbs are those of the legacy rules file
//! ([`crate::rules_file`]), with the same checks: [`rules_file::check_action`],
//! [`rules_file::check_card_condition`], [`rules_file::check_job_condition`],
//! and [`MatchRule::check`]. This module runs nothing: the engine still runs
//! the legacy rules (C2, TER-1113, switches over).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::rules_file::{self, Action, CardCondition, Context, JobCondition};
use crate::schedule::{MatchOp, MatchRule};

/// The version of the stored rule model. A later change of the model
/// raises it and migrates the stored rules.
pub const MODEL_VERSION: u32 = 1;

/// The most rules of one column.
pub const MAX_RULES_PER_COLUMN: usize = 50;
/// The most column rules of one project.
pub const MAX_RULES_PER_PROJECT: usize = 300;
/// The most actions of one rule.
pub const MAX_ACTIONS: usize = 20;
/// The largest rule, as compact JSON, in bytes.
pub const MAX_RULE_BYTES: usize = 32 * 1024;
/// The longest rule name, in characters.
pub const MAX_NAME_CHARS: usize = 64;
/// The longest description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
/// The shortest and the longest period of a `timer` trigger, in seconds.
pub const TIMER_MIN_SECONDS: u64 = 60;
pub const TIMER_MAX_SECONDS: u64 = 30 * 24 * 3600;
/// The highest `count` of a `count_reached` trigger.
pub const COUNT_MAX: u32 = 1000;

/// The events of a job for a `job_result` trigger: the `change` of a `job`
/// event (`queued`, `started`, `requeued`, `cancelled`, `ended`), and
/// `verdict`, the `job.verdict` event (TER-1112 lead decision Q2: all job
/// lifecycle events).
pub const JOB_EVENTS: &[&str] = &[
    "queued",
    "started",
    "requeued",
    "verdict",
    "cancelled",
    "ended",
];
/// The kinds of a job, for `job_result.job_kinds`.
pub const JOB_KINDS: &[&str] = &["validate", "merge", "e2e", "release"];

/// The trigger of a column rule: when it runs (D2, and Q2 for `wait_met`
/// and `archived`). JSON: `{"kind": "lands"}`, `{"kind": "job_result",
/// "events": ["ended"], "job_kinds": ["merge"]}`, `{"kind": "timer",
/// "every_seconds": 3600}`, `{"kind": "count_reached", "count": 4}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// A card enters the column (after the column's queue, C3).
    Lands,
    /// A card leaves the column.
    Leaves,
    /// While a card is in the column: an event of its job.
    JobResult {
        /// [`JOB_EVENTS`], at least one.
        events: Vec<String>,
        /// [`JOB_KINDS`]; empty: every kind.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        job_kinds: Vec<String>,
    },
    /// While a card is in the column: every `every_seconds`.
    Timer { every_seconds: u64 },
    /// The column holds `count` cards.
    CountReached { count: u32 },
    /// While a card is in the column: its wait is met.
    WaitMet,
    /// A card in the column is archived.
    Archived,
}

impl Trigger {
    /// The `kind` of the trigger.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Lands => "lands",
            Self::Leaves => "leaves",
            Self::JobResult { .. } => "job_result",
            Self::Timer { .. } => "timer",
            Self::CountReached { .. } => "count_reached",
            Self::WaitMet => "wait_met",
            Self::Archived => "archived",
        }
    }
}

/// One rule of a column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnRule {
    /// The stable ID that the daemon gives a new rule. A write names the
    /// ID of each rule it keeps; a rule without one is new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Unique in the column: a-z, A-Z, 0-9, `.`, `_`, `-`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub trigger: Trigger,
    /// Conditions on the event: every one must hold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<MatchRule>,
    /// Conditions on the current state of the card: every one must hold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub if_card: Vec<CardCondition>,
    /// Conditions on the current state of the job (`job_result` only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub if_job: Vec<JobCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_hour: Option<u32>,
    /// The verbs, in order.
    pub actions: Vec<Action>,
}

fn enabled() -> bool {
    true
}

/// One problem of a rule: the JSON path (`rules[2].actions[0].args.to`)
/// and the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// The JSON type of a field of the rule model.
#[derive(Clone, Copy)]
enum Ty {
    Str,
    Bool,
    /// A whole number from 0 to the bound.
    UInt(u64),
    /// A list of texts.
    StrList,
    /// A list (its items are checked on their own).
    List,
    Object,
    /// One of the words.
    OneOf(&'static [&'static str]),
    Any,
}

/// Whether a field must be there, and whether `null` is its absence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    Required,
    /// May be absent; `null` is a wrong type.
    Optional,
    /// May be absent or `null`.
    Nullable,
}

type Field = (&'static str, Ty, Need);

const OPS: &[&str] = &["eq", "ne", "glob", "in"];
const U32: u64 = u32::MAX as u64;

const RULE_FIELDS: &[Field] = &[
    ("id", Ty::Str, Need::Nullable),
    ("name", Ty::Str, Need::Required),
    ("description", Ty::Str, Need::Nullable),
    ("enabled", Ty::Bool, Need::Optional),
    ("trigger", Ty::Object, Need::Required),
    ("when", Ty::List, Need::Optional),
    ("if_card", Ty::List, Need::Optional),
    ("if_job", Ty::List, Need::Optional),
    ("max_per_hour", Ty::UInt(U32), Need::Nullable),
    ("actions", Ty::List, Need::Required),
];
const WHEN_FIELDS: &[Field] = &[
    ("path", Ty::Str, Need::Required),
    ("op", Ty::OneOf(OPS), Need::Optional),
    ("value", Ty::Any, Need::Required),
];
const IF_CARD_FIELDS: &[Field] = &[
    ("group_key_prefix", Ty::Str, Need::Nullable),
    ("column", Ty::StrList, Need::Optional),
    ("not_column", Ty::StrList, Need::Optional),
    ("has_labels", Ty::StrList, Need::Optional),
    ("any_labels", Ty::StrList, Need::Optional),
    ("lacks_labels", Ty::StrList, Need::Optional),
    ("assigned", Ty::Bool, Need::Nullable),
    ("archived", Ty::Bool, Need::Nullable),
];
const IF_JOB_FIELDS: &[Field] = &[
    ("state", Ty::StrList, Need::Optional),
    ("outcome", Ty::StrList, Need::Optional),
    ("split", Ty::Bool, Need::Nullable),
    ("batch", Ty::Bool, Need::Nullable),
    ("rerun", Ty::Bool, Need::Nullable),
    ("halves", Ty::Bool, Need::Nullable),
];
const ACTION_FIELDS: &[Field] = &[
    ("verb", Ty::Str, Need::Required),
    ("args", Ty::Object, Need::Optional),
];

/// The fields of a trigger of kind `kind`, after `kind`; `None`: no such
/// kind.
fn trigger_fields(kind: &str) -> Option<&'static [Field]> {
    Some(match kind {
        "lands" | "leaves" | "wait_met" | "archived" => &[],
        "job_result" => &[
            ("events", Ty::StrList, Need::Required),
            ("job_kinds", Ty::StrList, Need::Optional),
        ],
        "timer" => &[("every_seconds", Ty::UInt(u64::MAX), Need::Required)],
        "count_reached" => &[("count", Ty::UInt(U32), Need::Required)],
        _ => return None,
    })
}

const TRIGGER_KINDS: &[&str] = &[
    "lands",
    "leaves",
    "job_result",
    "timer",
    "count_reached",
    "wait_met",
    "archived",
];

/// The problem of the value `v` for the type `ty`, if any.
fn type_problem(v: &Value, ty: Ty) -> Option<String> {
    let ok = match ty {
        Ty::Str => v.is_string(),
        Ty::Bool => v.is_boolean(),
        Ty::UInt(max) => v.as_u64().is_some_and(|n| n <= max),
        Ty::StrList => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
        Ty::List => v.is_array(),
        Ty::Object => v.is_object(),
        Ty::OneOf(words) => v.as_str().is_some_and(|s| words.contains(&s)),
        Ty::Any => true,
    };
    if ok {
        return None;
    }
    Some(match ty {
        Ty::Str => "must be a text".into(),
        Ty::Bool => "must be true or false".into(),
        Ty::UInt(max) => format!("must be a whole number from 0 to {max}"),
        Ty::StrList => "must be a list of texts".into(),
        Ty::List => "must be a list".into(),
        Ty::Object => "must be an object".into(),
        Ty::OneOf(words) => format!("must be one of {}", words.join(", ")),
        Ty::Any => unreachable!(),
    })
}

/// A problem for each field of the object `v` (at `path`) that is not in
/// `fields`, is missing though required, or has the wrong type; and for a
/// value that is not an object.
fn check_fields(v: &Value, path: &str, fields: &[Field], out: &mut Vec<Problem>) {
    let Some(o) = v.as_object() else {
        out.push(Problem {
            path: path.to_owned(),
            message: "must be an object".into(),
        });
        return;
    };
    let known: Vec<&str> = fields.iter().map(|(k, _, _)| *k).collect();
    for k in o.keys().filter(|k| !known.contains(&k.as_str())) {
        out.push(Problem {
            path: format!("{path}.{k}"),
            message: format!("unknown field `{k}`; known: {}", known.join(", ")),
        });
    }
    for (k, ty, need) in fields {
        let at = format!("{path}.{k}");
        match o.get(*k) {
            None if *need == Need::Required => out.push(Problem {
                path: at,
                message: "is missing".into(),
            }),
            None => {}
            Some(Value::Null) if *need == Need::Nullable => {}
            Some(v) => {
                if let Some(message) = type_problem(v, *ty) {
                    out.push(Problem { path: at, message });
                }
            }
        }
    }
}

/// The items of the list `key` of `o`, each with its path.
fn items<'a>(o: &'a Map<String, Value>, key: &str, path: &str) -> Vec<(String, &'a Value)> {
    match o.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (format!("{path}.{key}[{i}]"), v))
            .collect(),
        _ => Vec::new(),
    }
}

/// The shape of one rule, before serde reads it: unknown fields (serde
/// would drop them, and a typo would then change what the rule does),
/// missing fields, and wrong types, each at its own path.
fn shape(v: &Value, path: &str, out: &mut Vec<Problem>) {
    check_fields(v, path, RULE_FIELDS, out);
    let Some(o) = v.as_object() else {
        return;
    };
    if let Some(t) = o.get("trigger").filter(|t| t.is_object()) {
        let tp = format!("{path}.trigger");
        match t.get("kind").and_then(Value::as_str) {
            Some(kind) => match trigger_fields(kind) {
                Some(fields) => {
                    let mut all: Vec<Field> = vec![("kind", Ty::Str, Need::Required)];
                    all.extend_from_slice(fields);
                    check_fields(t, &tp, &all, out);
                }
                None => out.push(Problem {
                    path: format!("{tp}.kind"),
                    message: format!(
                        "unknown trigger `{kind}`: use one of {}",
                        TRIGGER_KINDS.join(", ")
                    ),
                }),
            },
            None => out.push(Problem {
                path: format!("{tp}.kind"),
                message: format!("needs one of {}", TRIGGER_KINDS.join(", ")),
            }),
        }
    }
    for (key, fields) in [
        ("when", WHEN_FIELDS),
        ("if_card", IF_CARD_FIELDS),
        ("if_job", IF_JOB_FIELDS),
        ("actions", ACTION_FIELDS),
    ] {
        for (p, item) in items(o, key, path) {
            check_fields(item, &p, fields, out);
        }
    }
}

/// Is `name` a rule name: 1 to [`MAX_NAME_CHARS`] of a-z, A-Z, 0-9, `.`,
/// `_`, `-`?
fn good_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Check one rule that was read. `path` is its JSON path. `ctx.columns`
/// are the columns of the board: each column that it names must exist.
pub fn check_rule(ctx: &Context, rule: &ColumnRule, path: &str) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut bad = |at: String, message: String| out.push(Problem { path: at, message });
    if !good_name(&rule.name) {
        bad(
            format!("{path}.name"),
            format!(
                "`{}` is not a rule name: 1 to {MAX_NAME_CHARS} of a-z, A-Z, 0-9, `.`, `_`, `-`",
                rule.name
            ),
        );
    }
    if let Some(d) = &rule.description
        && d.chars().count() > MAX_DESCRIPTION_CHARS
    {
        bad(
            format!("{path}.description"),
            format!("has more than {MAX_DESCRIPTION_CHARS} characters"),
        );
    }
    let tp = format!("{path}.trigger");
    match &rule.trigger {
        Trigger::JobResult { events, job_kinds } => {
            if events.is_empty() {
                bad(
                    format!("{tp}.events"),
                    format!("needs at least one of {}", JOB_EVENTS.join(", ")),
                );
            }
            for (key, list, known) in [
                ("events", events, JOB_EVENTS),
                ("job_kinds", job_kinds, JOB_KINDS),
            ] {
                for (i, v) in list.iter().enumerate() {
                    if !known.contains(&v.as_str()) {
                        bad(
                            format!("{tp}.{key}[{i}]"),
                            format!("`{v}` is not one of {}", known.join(", ")),
                        );
                    } else if list[..i].contains(v) {
                        bad(format!("{tp}.{key}[{i}]"), format!("`{v}` twice"));
                    }
                }
            }
        }
        Trigger::Timer { every_seconds } => {
            if !(TIMER_MIN_SECONDS..=TIMER_MAX_SECONDS).contains(every_seconds) {
                bad(
                    format!("{tp}.every_seconds"),
                    format!("must be from {TIMER_MIN_SECONDS} to {TIMER_MAX_SECONDS}"),
                );
            }
        }
        Trigger::CountReached { count } => {
            if !(1..=COUNT_MAX).contains(count) {
                bad(
                    format!("{tp}.count"),
                    format!("must be from 1 to {COUNT_MAX}"),
                );
            }
        }
        Trigger::Lands | Trigger::Leaves | Trigger::WaitMet | Trigger::Archived => {}
    }
    for (i, m) in rule.when.iter().enumerate() {
        if let Err(e) = m.check() {
            bad(format!("{path}.when[{i}]"), e);
        }
    }
    for (i, c) in rule.if_card.iter().enumerate() {
        if let Err(e) = rules_file::check_card_condition(ctx, c) {
            bad(format!("{path}.if_card[{i}]"), e);
        }
    }
    if !rule.if_job.is_empty() && !matches!(rule.trigger, Trigger::JobResult { .. }) {
        bad(
            format!("{path}.if_job"),
            format!(
                "only a `job_result` trigger has a job; this rule's trigger is `{}`",
                rule.trigger.kind()
            ),
        );
    }
    for (i, c) in rule.if_job.iter().enumerate() {
        if let Err(e) = rules_file::check_job_condition(c) {
            bad(format!("{path}.if_job[{i}]"), e);
        }
    }
    if let Some(n) = rule.max_per_hour
        && !(1..=rules_file::MAX_PER_HOUR).contains(&n)
    {
        bad(
            format!("{path}.max_per_hour"),
            format!("must be from 1 to {}", rules_file::MAX_PER_HOUR),
        );
    }
    if rule.actions.is_empty() && rule.enabled {
        bad(
            format!("{path}.actions"),
            "an enabled rule needs at least one action".into(),
        );
    }
    if rule.actions.len() > MAX_ACTIONS {
        bad(
            format!("{path}.actions"),
            format!(
                "{} actions; a rule has at most {MAX_ACTIONS}",
                rule.actions.len()
            ),
        );
    }
    for (i, a) in rule.actions.iter().enumerate() {
        for (arg, e) in rules_file::check_action(ctx, a) {
            let at = match arg {
                Some(k) => format!("{path}.actions[{i}].args.{k}"),
                None => format!("{path}.actions[{i}]"),
            };
            bad(at, e);
        }
    }
    out
}

/// Read and check the rules of one column, in order. `values` is the JSON
/// list; the problems name `rules[i]...`. `ctx.columns` are the columns of
/// the board; `ctx.max_rules` is the most rules of the column. The IDs are
/// not checked here: the daemon knows which ones exist.
pub fn read_rules(ctx: &Context, values: &[Value]) -> Result<Vec<ColumnRule>, Vec<Problem>> {
    let mut problems = Vec::new();
    let mut rules = Vec::new();
    let max = ctx.max_rules.min(MAX_RULES_PER_COLUMN);
    if values.len() > max {
        problems.push(Problem {
            path: "rules".into(),
            message: format!("{} rules; a column has at most {max}", values.len()),
        });
    }
    for (i, v) in values.iter().enumerate() {
        let path = format!("rules[{i}]");
        let size = v.to_string().len();
        if size > MAX_RULE_BYTES {
            problems.push(Problem {
                path,
                message: format!("has {size} bytes; a rule has at most {MAX_RULE_BYTES}"),
            });
            continue;
        }
        let mut shape_problems = Vec::new();
        shape(v, &path, &mut shape_problems);
        if !shape_problems.is_empty() {
            problems.extend(shape_problems);
            continue;
        }
        let rule: ColumnRule = match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(e) => {
                problems.push(Problem {
                    path,
                    message: e.to_string(),
                });
                continue;
            }
        };
        problems.extend(check_rule(ctx, &rule, &path));
        if rules.iter().any(|r: &ColumnRule| r.name == rule.name) {
            problems.push(Problem {
                path: format!("{path}.name"),
                message: format!("a second rule named `{}` in the column", rule.name),
            });
        }
        rules.push(rule);
    }
    if problems.is_empty() {
        Ok(rules)
    } else {
        Err(problems)
    }
}

/// The catalog of the rule model, for an editor and the CLI (`GET
/// /api/rules/catalog`): the triggers, the conditions, the verbs with their
/// arguments, the placeholders of a text, and the limits.
pub fn catalog() -> Value {
    let field = |name: &str, kind: &str, values: &[&str], min: Option<u64>, max: Option<u64>| {
        let mut f = json!({"name": name, "type": kind, "required": true});
        if !values.is_empty() {
            f["values"] = json!(values);
        }
        if let Some(m) = min {
            f["min"] = m.into();
        }
        if let Some(m) = max {
            f["max"] = m.into();
        }
        f
    };
    let mut job_kinds = field("job_kinds", "one_of_list", JOB_KINDS, None, None);
    job_kinds["required"] = false.into();
    let triggers = json!([
        {"kind": "lands", "description": "a card enters the column (after the column's queue)",
         "fields": []},
        {"kind": "leaves", "description": "a card leaves the column", "fields": []},
        {"kind": "job_result",
         "description": "while a card is in the column: an event of its job",
         "fields": [field("events", "one_of_list", JOB_EVENTS, Some(1), None), job_kinds]},
        {"kind": "timer", "description": "while a card is in the column: every `every_seconds`",
         "fields": [field("every_seconds", "count", &[], Some(TIMER_MIN_SECONDS),
                          Some(TIMER_MAX_SECONDS))]},
        {"kind": "count_reached", "description": "the column holds `count` cards",
         "fields": [field("count", "count", &[], Some(1), Some(COUNT_MAX.into()))]},
        {"kind": "wait_met", "description": "while a card is in the column: its wait is met",
         "fields": []},
        {"kind": "archived", "description": "a card in the column is archived", "fields": []},
    ]);
    let ops: Vec<&str> = [MatchOp::Eq, MatchOp::Ne, MatchOp::Glob, MatchOp::In]
        .iter()
        .map(|o| o.as_str())
        .collect();
    let verbs: Vec<Value> = rules_file::VERBS
        .iter()
        .map(|v| json!({"verb": v.name, "args": v.arg_specs().collect::<Vec<_>>()}))
        .collect();
    json!({
        "model_version": MODEL_VERSION,
        "triggers": triggers,
        "conditions": {
            "when": {
                "description": "a condition on the event; every one must hold",
                "paths": ["project", "kind", "actor", "summary", "data.<path>"],
                "ops": ops,
            },
            "if_card": {
                "description": "a condition on the current card; each field that is set must hold",
                "fields": [
                    {"name": "group_key_prefix", "type": "text"},
                    {"name": "column", "type": "columns"},
                    {"name": "not_column", "type": "columns"},
                    {"name": "has_labels", "type": "list"},
                    {"name": "any_labels", "type": "list"},
                    {"name": "lacks_labels", "type": "list"},
                    {"name": "assigned", "type": "bool"},
                    {"name": "archived", "type": "bool"},
                ],
            },
            "if_job": {
                "description": "a condition on the current job (job_result only)",
                "fields": [
                    {"name": "state", "type": "one_of_list", "values": rules_file::JOB_STATES},
                    {"name": "outcome", "type": "one_of_list", "values": rules_file::JOB_OUTCOMES},
                    {"name": "split", "type": "bool"},
                    {"name": "batch", "type": "bool"},
                    {"name": "rerun", "type": "bool"},
                    {"name": "halves", "type": "bool"},
                ],
            },
        },
        "actions": verbs,
        "placeholders": {
            "event": ["kind", "summary", "project", "seq", "actor", "data.<path>"],
            "card": rules_file::CARD_FIELDS,
            "job": rules_file::JOB_FIELDS,
            "group": rules_file::GROUP_FIELDS,
            "rule": ["name"],
        },
        "limits": {
            "max_rules_per_column": MAX_RULES_PER_COLUMN,
            "max_rules_per_project": MAX_RULES_PER_PROJECT,
            "max_actions": MAX_ACTIONS,
            "max_rule_bytes": MAX_RULE_BYTES,
            "max_name_chars": MAX_NAME_CHARS,
            "max_description_chars": MAX_DESCRIPTION_CHARS,
            "max_per_hour": rules_file::MAX_PER_HOUR,
        },
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::rules_file::BoardColumn;

    fn ctx() -> Context {
        let col = |n: &str, r: Option<&str>| BoardColumn {
            name: n.into(),
            role: r.map(Into::into),
        };
        Context {
            columns: Some(vec![
                col("backlog", None),
                col("in_progress", Some("working")),
                col("review", Some("review")),
                col("blocked", Some("blocked")),
                col("done", None),
            ]),
            max_rules: MAX_RULES_PER_COLUMN,
            columns_flow: false,
        }
    }

    fn paths(values: Value) -> Vec<String> {
        read_rules(&ctx(), values.as_array().unwrap())
            .unwrap_err()
            .into_iter()
            .map(|p| p.path)
            .collect()
    }

    fn merge_pass() -> Value {
        json!({
            "name": "merge-pass",
            "description": "a passed merge job moves the card on",
            "trigger": {"kind": "job_result", "events": ["ended"], "job_kinds": ["merge"]},
            "when": [{"path": "data.state", "op": "eq", "value": "passed"}],
            "if_card": [{"column": ["@review"], "lacks_labels": ["hold"]}],
            "if_job": [{"state": ["passed"]}],
            "max_per_hour": 600,
            "actions": [
                {"verb": "card.move", "args": {"to": "done", "comment": "{{job.end_comment}}"}},
                {"verb": "card.label", "args": {"remove": ["merging"]}},
            ],
        })
    }

    #[test]
    fn every_trigger_reads_and_round_trips() {
        let rules = json!([
            merge_pass(),
            {"name": "lands", "trigger": {"kind": "lands"},
             "actions": [{"verb": "card.done_mail"}]},
            {"name": "leaves", "trigger": {"kind": "leaves"},
             "actions": [{"verb": "agent.unpark", "args": {"turn": "back"}}]},
            {"name": "hourly", "trigger": {"kind": "timer", "every_seconds": 3600},
             "actions": [{"verb": "mail", "args": {"to": ["lead"], "subject": "{{card.id}}"}}]},
            {"name": "four", "trigger": {"kind": "count_reached", "count": 4},
             "actions": [{"verb": "job.queue", "args": {"kind": "batch", "from": "review"}}]},
            {"name": "met", "trigger": {"kind": "wait_met"},
             "actions": [{"verb": "card.wait_action"}]},
            {"name": "gone", "enabled": false, "trigger": {"kind": "archived"}, "actions": []},
        ]);
        let read = read_rules(&ctx(), rules.as_array().unwrap()).unwrap();
        assert_eq!(read.len(), 7);
        assert!(read[0].enabled && !read[6].enabled);
        assert_eq!(
            read[3].trigger,
            Trigger::Timer {
                every_seconds: 3600
            }
        );
        assert_eq!(read[0].when[0].op, MatchOp::Eq);
        // Written out and read again, each rule is the same.
        let out = serde_json::to_value(&read).unwrap();
        assert_eq!(out[1]["trigger"], json!({"kind": "lands"}));
        assert_eq!(
            out[0]["trigger"],
            json!({"kind": "job_result", "events": ["ended"], "job_kinds": ["merge"]})
        );
        let again = read_rules(&ctx(), out.as_array().unwrap()).unwrap();
        assert_eq!(again, read);
    }

    #[test]
    fn bad_rules_are_refused_with_their_paths() {
        let with = |f: &dyn Fn(&mut Value)| {
            let mut r = merge_pass();
            f(&mut r);
            json!([r])
        };
        let cases: Vec<(Value, &str)> = vec![
            (with(&|r| r["name"] = "has space".into()), "rules[0].name"),
            (with(&|r| r["surprise"] = 1.into()), "rules[0].surprise"),
            (
                with(&|r| r["trigger"] = json!({"kind": "sometimes"})),
                "rules[0].trigger.kind",
            ),
            (
                with(&|r| r["trigger"] = json!({"kind": "lands", "events": ["ended"]})),
                "rules[0].trigger.events",
            ),
            (
                with(&|r| r["trigger"]["events"] = json!([])),
                "rules[0].trigger.events",
            ),
            (
                with(&|r| r["trigger"]["events"] = json!(["ended", "exploded"])),
                "rules[0].trigger.events[1]",
            ),
            (
                with(&|r| r["trigger"]["job_kinds"] = json!(["merge", "merge"])),
                "rules[0].trigger.job_kinds[1]",
            ),
            (
                with(&|r| r["trigger"] = json!({"kind": "timer", "every_seconds": 5})),
                "rules[0].trigger.every_seconds",
            ),
            (
                with(&|r| r["trigger"] = json!({"kind": "count_reached", "count": 0})),
                "rules[0].trigger.count",
            ),
            (
                with(&|r| r["when"][0]["path"] = "nowhere".into()),
                "rules[0].when[0]",
            ),
            (
                with(&|r| r["when"][0]["colour"] = "red".into()),
                "rules[0].when[0].colour",
            ),
            (
                with(&|r| r["if_card"][0]["column"] = json!(["nope"])),
                "rules[0].if_card[0]",
            ),
            (
                with(&|r| r["if_card"][0]["has_label"] = json!(["x"])),
                "rules[0].if_card[0].has_label",
            ),
            (
                with(&|r| r["if_job"][0]["state"] = json!(["exploded"])),
                "rules[0].if_job[0]",
            ),
            (
                with(&|r| r["trigger"] = json!({"kind": "lands"})),
                "rules[0].if_job",
            ),
            (
                with(&|r| r["max_per_hour"] = 601.into()),
                "rules[0].max_per_hour",
            ),
            (with(&|r| r["actions"] = json!([])), "rules[0].actions"),
            (
                with(&|r| r["actions"][0]["verb"] = "card.teleport".into()),
                "rules[0].actions[0]",
            ),
            (
                with(&|r| r["actions"][0]["args"]["to"] = "nowhere".into()),
                "rules[0].actions[0].args.to",
            ),
            (
                with(&|r| r["actions"][0]["args"]["speed"] = 9.into()),
                "rules[0].actions[0].args.speed",
            ),
            (
                with(&|r| {
                    r["actions"][0]["args"]
                        .as_object_mut()
                        .unwrap()
                        .remove("to");
                }),
                "rules[0].actions[0].args.to",
            ),
            (
                with(&|r| r["actions"][0]["args"]["comment"] = "{{card.colour}}".into()),
                "rules[0].actions[0].args.comment",
            ),
            (
                with(&|r| r["actions"][1]["args"] = json!({})),
                "rules[0].actions[1]",
            ),
            (with(&|r| r["trigger"] = json!("lands")), "rules[0].trigger"),
            // A missing field or a wrong type is named at its own path.
            (
                with(&|r| {
                    r.as_object_mut().unwrap().remove("trigger");
                }),
                "rules[0].trigger",
            ),
            (
                with(&|r| {
                    r["trigger"].as_object_mut().unwrap().remove("events");
                }),
                "rules[0].trigger.events",
            ),
            (
                with(&|r| r["trigger"] = json!({"kind": "timer", "every_seconds": "1h"})),
                "rules[0].trigger.every_seconds",
            ),
            (
                with(&|r| r["trigger"] = json!({"kind": "count_reached", "count": -1})),
                "rules[0].trigger.count",
            ),
            (with(&|r| r["enabled"] = "yes".into()), "rules[0].enabled"),
            (with(&|r| r["name"] = 7.into()), "rules[0].name"),
            (
                with(&|r| r["max_per_hour"] = 1.5.into()),
                "rules[0].max_per_hour",
            ),
            (with(&|r| r["actions"] = json!({})), "rules[0].actions"),
            (
                with(&|r| r["actions"][0]["args"] = json!(["to"])),
                "rules[0].actions[0].args",
            ),
            (
                with(&|r| {
                    r["actions"][0].as_object_mut().unwrap().remove("verb");
                }),
                "rules[0].actions[0].verb",
            ),
            (
                with(&|r| r["when"][0]["op"] = "like".into()),
                "rules[0].when[0].op",
            ),
            (
                with(&|r| {
                    r["when"][0].as_object_mut().unwrap().remove("value");
                }),
                "rules[0].when[0].value",
            ),
            (
                with(&|r| r["if_card"][0]["assigned"] = "no".into()),
                "rules[0].if_card[0].assigned",
            ),
            (
                with(&|r| r["if_card"][0]["has_labels"] = json!([1])),
                "rules[0].if_card[0].has_labels",
            ),
            (
                with(&|r| r["if_job"][0]["split"] = 1.into()),
                "rules[0].if_job[0].split",
            ),
            (with(&|r| r["when"] = json!([3])), "rules[0].when[0]"),
            // The limits of one rule: actions, and bytes.
            (
                with(&|r| {
                    r["actions"] =
                        Value::from(vec![json!({"verb": "card.done_mail"}); MAX_ACTIONS + 1])
                }),
                "rules[0].actions",
            ),
            (
                with(&|r| r["description"] = "x".repeat(MAX_RULE_BYTES).into()),
                "rules[0]",
            ),
        ];
        for (rules, want) in cases {
            let got = paths(rules.clone());
            assert!(
                got.iter().any(|p| p == want),
                "{want} not in {got:?}: {rules}"
            );
        }
        // Two rules of one name.
        assert_eq!(
            paths(json!([merge_pass(), merge_pass()])),
            ["rules[1].name"]
        );
        // Too many rules for the column.
        let many: Vec<Value> = (0..=MAX_RULES_PER_COLUMN)
            .map(|i| {
                let mut r = merge_pass();
                r["name"] = format!("r{i}").into();
                r
            })
            .collect();
        assert_eq!(paths(Value::from(many)), ["rules"]);
    }

    #[test]
    fn a_rule_at_the_limits_reads() {
        let mut r = merge_pass();
        r["actions"] = Value::from(vec![json!({"verb": "card.done_mail"}); MAX_ACTIONS]);
        assert!(read_rules(&ctx(), &[r]).is_ok());
        // Under the byte limit, a long description is refused only by its
        // own limit.
        let mut r = merge_pass();
        r["description"] = "x".repeat(MAX_DESCRIPTION_CHARS + 1).into();
        assert_eq!(paths(json!([r])), ["rules[0].description"]);
        let rules: Vec<Value> = (0..MAX_RULES_PER_COLUMN)
            .map(|i| {
                let mut r = merge_pass();
                r["name"] = format!("r{i}").into();
                r
            })
            .collect();
        assert!(read_rules(&ctx(), &rules).is_ok());
    }

    #[test]
    fn the_catalog_lists_every_trigger_and_verb() {
        let c = catalog();
        let kinds: Vec<&str> = c["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, TRIGGER_KINDS);
        for k in TRIGGER_KINDS {
            assert!(trigger_fields(k).is_some(), "{k}");
        }
        assert_eq!(
            c["actions"].as_array().unwrap().len(),
            rules_file::VERBS.len()
        );
        let mv = c["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["verb"] == "card.move")
            .unwrap();
        assert_eq!(
            mv["args"][0],
            json!({"name": "to", "type": "column", "required": true})
        );
        assert_eq!(mv["args"][3]["values"], json!(["top", "bottom"]), "{mv}");
        assert_eq!(
            c["conditions"]["when"]["ops"],
            json!(["eq", "ne", "glob", "in"])
        );
        assert_eq!(c["model_version"], MODEL_VERSION);
    }
}
