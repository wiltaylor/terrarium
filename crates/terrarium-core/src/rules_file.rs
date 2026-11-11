//! Board rules files (TER-162, TER-166): one `rules.wcl` for each
//! project.
//!
//! A rule reacts to an event of the log and runs verbs on the board. This
//! module reads and checks a file. It runs nothing: the rule engine
//! (`terrariumd::rules_engine`, TER-168) runs it, with the verbs of
//! `terrariumd::rules_verbs` (TER-169).
//!
//! [`SCHEMA`] is the schema that the daemon ships (`rules.schema.wcl`).
//! [`parse`] checks a file against it, then checks what a schema cannot
//! say:
//!
//! - The file holds plain data only: rule blocks and fields whose values
//!   are literals (a bool, a number, a text, `none`, or a list of these),
//!   and quoted block labels. A `let`, a function call, a function, a
//!   reference, an operator, an interpolated string, a record, a field
//!   for a block slot (computed children), an `import` (imports are off),
//!   a `type`, `union`, `symbol_set`, `namespace`, `use`, table,
//!   connection, or decorator is refused before WCL evaluates anything
//!   ([`forbidden_items`]). So the evaluation is bounded by the file size.
//!   The document reads no file: its loader refuses each read.
//! - The rule names are unique, and a rule has an `on` kind and at least
//!   one verb.
//! - Each `when` has one compare, on a known path
//!   ([`crate::schedule::MatchRule::check`]).
//! - Each `{{...}}` template names a known placeholder ([`check_template`]).
//! - The enumerated arguments (`position`, `kind`, `outcome`) hold a known
//!   value.
//! - With the columns of the project's board, each column argument names
//!   a column: `@<role>` is the column with that role, else the column of
//!   that name; a plain name is a column name.
//! - At most `max_rules` rules.
//!
//! Each error names the file, the line, and the column
//! ([`crate::template_file::FileError`]).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use wcl_lang::{Document, Environment, FileLoader, ast};

use crate::schedule::{MatchOp, MatchRule};
use crate::template_file::{FileError, Problem};

/// The schema of a rules file, as the daemon ships it.
pub const SCHEMA: &str = include_str!("rules.schema.wcl");

/// The built-in rule pack "delivery" (TER-170): the flow of the job queue
/// and of the bound agents as rules, for a project with `board_flow =
/// "rules"`. See [`default_pack`] and [`with_pack`].
pub const DEFAULT_PACK: &str = include_str!("rules.default.wcl");

/// The name of the built-in pack in the docs and the errors.
pub const PACK_FILE: &str = "rules.default.wcl";

/// Opt-in rules for a project that still validates cards (TER-170): the
/// labels, the comment, and the move to `@validating` of a queued validate
/// job. Not in the built-in pack (the operator removed the validating
/// column, 2026-11-07); a project copies them into its `rules.wcl`.
pub const VALIDATE_RULES: &str = include_str!("rules.validate.wcl");

/// Opt-in rules for the merge queue column (TER-172, TER-162 D8, D9): a
/// queued merge job moves its cards to `@merge_queue`, a cancel moves them
/// back to `@review`, and one batch merge of the cards that wait there
/// when 8 wait or 30 minutes after the first one. Not in the built-in pack
/// (a board without that column would fail each run); a project copies
/// them into its `rules.wcl`, with or without `board_flow = rules`.
pub const MERGE_QUEUE_RULES: &str = include_str!("rules.merge_queue.wcl");

/// The name of a rules file in the docs and the errors.
pub const FILE_NAME: &str = "rules.wcl";

/// The largest rules file, in bytes.
pub const MAX_BYTES: usize = 64 * 1024;

/// The highest `max_per_hour` of a rule (as for a trigger).
pub const MAX_PER_HOUR: u32 = 600;

/// The default of the most rules in one project's file.
pub const DEFAULT_MAX_RULES: usize = 50;

/// The type of an argument of a verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arg {
    Bool,
    /// Text; templates allowed.
    Text,
    /// A list of texts; templates allowed.
    List,
    /// A whole number, at least 1.
    Count,
    /// One column.
    Column,
    /// A list of columns.
    Columns,
    /// One of the given words.
    OneOf(&'static [&'static str]),
    /// The source of a script ([`SCRIPT_VERB`]): a text of 1 to
    /// [`MAX_SCRIPT_BYTES`] bytes, without templates (the script reads its
    /// values with `rule::get`).
    Code,
}

/// One verb: its block kind in the file, its name in the API, and its
/// arguments. The schema (`rules.schema.wcl`) has one block type for each,
/// with the same fields (a test checks that they agree).
#[derive(Debug)]
pub struct Verb {
    /// The block kind, for example `card_move`.
    pub kind: &'static str,
    /// The verb, for example `card.move`.
    pub name: &'static str,
    args: &'static [(&'static str, Arg)],
    /// The arguments that the verb needs.
    required: &'static [&'static str],
}

impl Verb {
    /// The names of the arguments, in the order of the schema.
    pub fn arg_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.args.iter().map(|(k, _)| *k)
    }

    /// The names of the arguments that name a column (one or a list).
    pub fn column_args(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.args
            .iter()
            .filter(|(_, a)| matches!(a, Arg::Column | Arg::Columns))
            .map(|(k, _)| *k)
    }
}

/// The verbs, in the order of the schema.
pub const VERBS: &[Verb] = &[
    Verb {
        kind: "version_cut",
        name: "version.cut",
        args: &[("min_cards", Arg::Count), ("max_wait_minutes", Arg::Count)],
        required: &["min_cards", "max_wait_minutes"],
    },
    Verb {
        kind: "group_open",
        name: "group.open",
        args: &[
            ("key", Arg::Text),
            ("title", Arg::Text),
            ("column", Arg::Column),
            ("done_when", Arg::Column),
            ("eta", Arg::Text),
            ("close_after", Arg::Text),
        ],
        required: &["key", "title", "column", "done_when"],
    },
    Verb {
        kind: "group_add",
        name: "group.add",
        args: &[("key", Arg::Text)],
        required: &["key"],
    },
    Verb {
        kind: "group_remove",
        name: "group.remove",
        args: &[("key", Arg::Text)],
        required: &["key"],
    },
    Verb {
        kind: "group_status",
        name: "group.status",
        args: &[
            ("key", Arg::Text),
            ("text", Arg::Text),
            ("eta", Arg::Text),
            ("step", Arg::Text),
            (
                "state",
                Arg::OneOf(&["pending", "running", "done", "failed"]),
            ),
        ],
        required: &["key"],
    },
    Verb {
        kind: "group_close",
        name: "group.close",
        args: &[
            ("key", Arg::Text),
            ("comment", Arg::Text),
            ("all_done", Arg::Bool),
        ],
        required: &["key"],
    },
    Verb {
        kind: "card_move",
        name: "card.move",
        args: &[
            ("to", Arg::Column),
            ("from", Arg::Columns),
            ("comment", Arg::Text),
            ("position", Arg::OneOf(&["top", "bottom"])),
        ],
        required: &["to"],
    },
    Verb {
        kind: "card_wait_action",
        name: "card.wait_action",
        args: &[],
        required: &[],
    },
    Verb {
        kind: "card_label",
        name: "card.label",
        args: &[("add", Arg::List), ("remove", Arg::List)],
        required: &[],
    },
    Verb {
        kind: "card_comment",
        name: "card.comment",
        args: &[("text", Arg::Text)],
        required: &["text"],
    },
    Verb {
        kind: "card_assign",
        name: "card.assign",
        args: &[("who", Arg::Text)],
        required: &["who"],
    },
    Verb {
        kind: "card_prepend",
        name: "card.prepend",
        args: &[("text", Arg::Text)],
        required: &["text"],
    },
    Verb {
        kind: "mail",
        name: "mail",
        args: &[
            ("to", Arg::List),
            ("subject", Arg::Text),
            ("body", Arg::Text),
        ],
        required: &["to", "subject"],
    },
    Verb {
        kind: "card_blocked_mail",
        name: "card.blocked_mail",
        args: &[],
        required: &[],
    },
    Verb {
        kind: "card_done_mail",
        name: "card.done_mail",
        args: &[],
        required: &[],
    },
    Verb {
        kind: "job_queue",
        name: "job.queue",
        args: &[
            ("kind", Arg::OneOf(&["validate", "merge", "batch"])),
            ("from", Arg::Column),
            ("limit", Arg::Count),
            ("min_cards", Arg::Count),
            ("max_wait_minutes", Arg::Count),
        ],
        required: &["kind"],
    },
    Verb {
        kind: "job_cancel",
        name: "job.cancel",
        args: &[],
        required: &[],
    },
    Verb {
        kind: "job_rerun",
        name: "job.rerun",
        args: &[("max_attempts", Arg::Count)],
        required: &[],
    },
    Verb {
        kind: "job_split",
        name: "job.split",
        args: &[("after_fixes", Arg::Count)],
        required: &[],
    },
    Verb {
        kind: "job_fix",
        name: "job.fix",
        args: &[
            ("to", Arg::Column),
            ("labels", Arg::List),
            ("max", Arg::Count),
        ],
        required: &[],
    },
    Verb {
        kind: "agent_park",
        name: "agent.park",
        args: &[("reason", Arg::Text)],
        required: &[],
    },
    Verb {
        kind: "agent_unpark",
        name: "agent.unpark",
        args: &[("turn", Arg::Text)],
        required: &[],
    },
    Verb {
        kind: "agent_end",
        name: "agent.end",
        args: &[
            ("outcome", Arg::OneOf(&["done", "cancelled"])),
            ("result", Arg::Text),
        ],
        required: &["outcome"],
    },
    Verb {
        kind: "script",
        name: SCRIPT_VERB,
        args: &[("source", Arg::Code), ("fuel", Arg::Count)],
        required: &["source"],
    },
];

/// The verb of a script step (TER-173, TER-162 D1): a wscript program for
/// what WCL cannot say. The engine runs it in a child process
/// (`terrariumd::rule_script`) with fuel, a memory limit and a wall-clock
/// limit, with a read-only view of the event, the card and the job. It
/// returns verb blocks in the WCL syntax of this file; [`script_actions`]
/// checks them as the verbs of a rule, and the engine runs them in place
/// of the step, with the same actor, checks and dry run.
pub const SCRIPT_VERB: &str = "script.run";

/// The largest script source, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 16 * 1024;

/// The fuel (wscript instructions) of a script step without `fuel`.
pub const DEFAULT_SCRIPT_FUEL: u64 = 100_000;

/// The highest `fuel` of a script step.
pub const MAX_SCRIPT_FUEL: u64 = 10_000_000;

/// The largest output of a script (its verb blocks), in bytes.
pub const MAX_SCRIPT_OUTPUT: usize = 16 * 1024;

/// The most verbs that one script step returns.
pub const MAX_SCRIPT_ACTIONS: usize = 20;

/// The block kind of a rule.
const RULE: &str = "rule";
/// The block kind of a condition on the event.
const WHEN: &str = "when";
/// The block kind of a condition on the current state of the run's card.
const IF_CARD: &str = "if_card";
/// The block kind of a condition on the current state of the event's job.
const IF_JOB: &str = "if_job";

/// The states of a job, for `if_job.state`.
pub const JOB_STATES: &[&str] = &["queued", "running", "passed", "failed", "cancelled"];
/// The outcomes of a job, for `if_job.outcome` (`none`: no outcome).
pub const JOB_OUTCOMES: &[&str] = &["red", "conflict", "error", "none"];

/// The placeholders after `card.`.
pub const CARD_FIELDS: &[&str] = &["id", "title", "column", "assignee", "labels", "project"];
/// The group containing the event's card (or the group card itself).
pub const GROUP_FIELDS: &[&str] = &[
    "id",
    "key",
    "title",
    "column",
    "status",
    "done",
    "total",
    "eta_at",
    "failed_step",
];
/// The placeholders after `job.`.
pub const JOB_FIELDS: &[&str] = &[
    "group_key",
    "id",
    "kind",
    "state",
    "outcome",
    "attempt",
    "by_agent",
    "repo",
    "commit",
    "branch",
    "end_comment",
    "failure_block",
    "next_jobs",
    "queue_comment",
    "rerun_comment",
    "split_comment",
    "fail_comment",
    "fail_mail",
    "infra_comment",
];

/// The placeholders whose values the daemon makes: IDs, and the texts of
/// the job queue that the coded flow writes as they are (`board_flow =
/// code`). [`render`] does not quote them, so the built-in pack writes the
/// same comments and mails as the coded flow. Every other value is quoted
/// in a text that an agent reads.
///
/// Each of these is one line of daemon fields, or a fixed text around one
/// (`job.fail_mail`): a branch name has no space and no `[`, so none of
/// them can hold a tag such as `[User]`. The engine fills the `job.*` ones
/// from the stored job, never from event data (any event source can write
/// a `job.verdict`). `job.failure_block` is not here: it holds the tail of
/// the job output, untrusted text, so it is quoted data in a comment, a
/// mail, or a turn. Only `card_prepend` writes it as it is, into the card
/// body (which agents read as data).
pub const RAW_FIELDS: &[&str] = &[
    "job.group_key",
    "card.id",
    "card.project",
    "card.column",
    "job.id",
    "job.kind",
    "job.state",
    "job.outcome",
    "job.attempt",
    "job.end_comment",
    "job.next_jobs",
    "job.queue_comment",
    "job.rerun_comment",
    "job.split_comment",
    "job.fail_comment",
    "job.fail_mail",
    "job.infra_comment",
    "rule.name",
];

/// One column of the project's board, for the column check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardColumn {
    pub name: String,
    pub role: Option<String>,
}

/// What [`parse`] checks a file against, beyond the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    /// The columns of the project's board. `None`: no column check.
    pub columns: Option<Vec<BoardColumn>>,
    /// The most rules in the file.
    pub max_rules: usize,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            columns: None,
            max_rules: DEFAULT_MAX_RULES,
        }
    }
}

/// One verb of a rule, with its arguments as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// The verb, for example `card.move`.
    pub verb: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub args: Map<String, Value>,
}

/// A condition on the current state of the run's card (`if_card`). Each
/// field that is set must hold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CardCondition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key_prefix: Option<String>,
    /// In one of these columns (`@<role>` or a name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub column: Vec<String>,
    /// In none of these columns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_column: Vec<String>,
    /// Has every one of these labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has_labels: Vec<String>,
    /// Has none of these labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lacks_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

/// A condition on the current state of the event's job (`if_job`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobCondition {
    /// The job is in one of these states ([`JOB_STATES`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<String>,
    /// The job has one of these outcomes ([`JOB_OUTCOMES`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcome: Vec<String>,
    /// The job is (or is not) a half of a split batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<bool>,
    /// Two or more cards of the job merged (did not conflict), or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<bool>,
    /// A rerun of the job is queued or ran, or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun: Option<bool>,
    /// The job was split in halves, or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halves: Option<bool>,
}

/// One rule, read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    /// Event kind globs.
    pub on: Vec<String>,
    /// Every condition must hold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<MatchRule>,
    /// Conditions on the current state of the run's card: every one must
    /// hold when the run starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub if_card: Vec<CardCondition>,
    /// Conditions on the current state of the event's job.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub if_job: Vec<JobCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_hour: Option<u32>,
    /// The verbs, in the order of the file.
    pub actions: Vec<Action>,
    /// The line of the rule in the file (1-based).
    pub line: usize,
}

/// A rules file, read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
}

/// The built-in pack, read once. It is checked by a test on every build,
/// so a failure here is a bug of the daemon: the pack is then empty.
pub fn default_pack() -> &'static RuleSet {
    static PACK: std::sync::OnceLock<RuleSet> = std::sync::OnceLock::new();
    PACK.get_or_init(|| {
        let ctx = Context {
            columns: None,
            max_rules: usize::MAX,
        };
        parse(DEFAULT_PACK, PACK_FILE, &ctx).unwrap_or_default()
    })
}

/// The rules that a project with the built-in pack runs: the rules of
/// `pack` in their order, each replaced by the rule of `own` with its
/// name (a rule with only `enabled = false` turns it off), then the other
/// rules of `own` in their order.
pub fn with_pack(pack: &RuleSet, own: &RuleSet) -> RuleSet {
    let mut rules: Vec<Rule> = pack
        .rules
        .iter()
        .map(|p| {
            own.rules
                .iter()
                .find(|r| r.name == p.name)
                .cloned()
                .unwrap_or_else(|| p.clone())
        })
        .collect();
    rules.extend(
        own.rules
            .iter()
            .filter(|r| !pack.rules.iter().any(|p| p.name == r.name))
            .cloned(),
    );
    RuleSet { rules }
}

/// The line and the column (1-based) of the byte `offset` of `text`.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit_once('\n')
        .map_or(before, |(_, l)| l)
        .chars()
        .count()
        + 1;
    (line, column)
}

/// A problem at the byte `offset` of `text`. An offset past the end of
/// the text (in the schema that [`parse`] puts after it) counts as the end.
fn problem_at(text: &str, offset: Option<usize>, message: impl Into<String>) -> Problem {
    let (line, column) = offset.map_or((0, 0), |o| position(text, o));
    Problem {
        line,
        column,
        message: message.into(),
    }
}

/// The offset of the first label of a WCL diagnostic.
fn label_offset(d: &dyn miette::Diagnostic) -> Option<usize> {
    d.labels()?.next().map(|l| l.offset())
}

/// A loader that reads no file: imports are off.
fn no_files() -> FileLoader {
    std::sync::Arc::new(|p: &std::path::Path| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "imports are off in {FILE_NAME}: cannot read `{}`",
                p.display()
            ),
        ))
    })
}

/// The block kinds of a rules file. A field of one of these names would
/// fill a block slot with an expression (computed children), which breaks
/// the file order of the verbs.
fn is_block_kind(name: &str) -> bool {
    matches!(name, RULE | "rules" | WHEN | IF_CARD | IF_JOB) || VERBS.iter().any(|v| v.kind == name)
}

/// Is `e` a number literal?
fn is_number(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    matches!(
        e,
        E::I8(_)
            | E::I16(_)
            | E::I32(_)
            | E::I64(_)
            | E::I128(_)
            | E::Isize(_)
            | E::U8(_)
            | E::U16(_)
            | E::U32(_)
            | E::U64(_)
            | E::U128(_)
            | E::Usize(_)
            | E::F32(_)
            | E::F64(_)
    )
}

/// What in `e` is not plain data, for the error. `None`: `e` is a literal
/// (a bool, a number, a negative number, a text, `none`, or a list of
/// these). Nothing else is allowed, so WCL evaluates no call, function,
/// reference, or operator of a rules file.
fn non_literal(e: &ast::Expr) -> Option<&'static str> {
    use ast::Expr as E;
    match e {
        E::Bool(_) | E::Utf8(_) | E::Ascii(_) | E::None => None,
        e if is_number(e) => None,
        E::Unary {
            op: ast::UnaryOp::Neg,
            operand,
            ..
        } if is_number(operand) => None,
        E::Paren { inner, .. } => non_literal(inner),
        E::ListLit { elements, .. } => elements.iter().find_map(non_literal),
        E::Call { .. } => Some("a function call"),
        E::Function(_) => Some("a function"),
        E::Identifier(..) | E::SelfKw(_) | E::ParentKw(_) | E::Member { .. } => Some("a reference"),
        E::InterpolatedString { .. } => Some("an interpolated string"),
        E::Binary { .. } | E::Unary { .. } => Some("an operator"),
        _ => Some("a computed value"),
    }
}

/// A problem for each value of `e` that is not plain data.
fn check_literal(text: &str, e: &ast::Expr, at: usize, out: &mut Vec<Problem>) {
    if let Some(what) = non_literal(e) {
        out.push(problem_at(
            text,
            Some(at),
            format!(
                "{what} is not allowed in {FILE_NAME}: a value is a literal \
                 (a bool, a number, a quoted text, none, or a list of these)"
            ),
        ));
    }
}

/// The items of the file that a rules file may not hold, as problems. A
/// rules file holds plain data only (see the module docs): this runs on
/// the parsed text, before WCL evaluates anything.
fn forbidden_items(text: &str, items: &[ast::Item], out: &mut Vec<Problem>) {
    for item in items {
        let what = match item {
            ast::Item::Field(f) => {
                if let Some(d) = f.decorators.first() {
                    out.push(problem_at(
                        text,
                        Some(d.span.start),
                        format!("a decorator is not allowed in {FILE_NAME}"),
                    ));
                }
                if is_block_kind(&f.name) {
                    out.push(problem_at(
                        text,
                        Some(f.span.start),
                        format!(
                            "a field `{0}` is not allowed in {FILE_NAME}: write a `{0} {{ ... }}` block",
                            f.name
                        ),
                    ));
                    continue;
                }
                check_literal(text, &f.expr, f.span.start, out);
                continue;
            }
            ast::Item::Let(_) => "a `let` binding (a rules file holds plain data only)",
            ast::Item::Block(b) => {
                if let Some(d) = b.decorators.first() {
                    out.push(problem_at(
                        text,
                        Some(d.span.start),
                        format!("a decorator is not allowed in {FILE_NAME}"),
                    ));
                }
                let bad_form = if b.slot_decl.is_some() {
                    Some("a slot declaration")
                } else if b.conditional {
                    Some("a conditional block")
                } else if !b.kind_ns.is_empty() {
                    Some("a namespaced block kind")
                } else {
                    None
                };
                if let Some(what) = bad_form {
                    out.push(problem_at(
                        text,
                        Some(b.span.start),
                        format!("{what} is not allowed in {FILE_NAME}"),
                    ));
                }
                for l in &b.labels {
                    check_literal(text, l, b.span.start, out);
                }
                forbidden_items(text, &b.items, out);
                continue;
            }
            ast::Item::Import(_) => "an import (imports are off)",
            ast::Item::TypeDecl(_) => "a type declaration (the daemon ships the schema)",
            ast::Item::InterfaceDecl(_) => "an interface declaration",
            ast::Item::UnionDecl(_) => "a union declaration",
            ast::Item::NamespaceDecl(_) => "a namespace",
            ast::Item::UseDecl(_) => "a use declaration",
            ast::Item::SymbolSetDecl(_) => "a symbol_set declaration",
            ast::Item::Table(_) => "a table",
            ast::Item::ConnectionDecl(_) | ast::Item::Connection(_) => "a connection",
            _ => "this item",
        };
        out.push(problem_at(
            text,
            Some(item.span().start),
            format!("{what} is not allowed in {FILE_NAME}"),
        ));
    }
}

/// Check one template: each `{{...}}` names `event.<path>`
/// ([`crate::template`]), `card.<field>` ([`CARD_FIELDS`]), `job.<field>`
/// ([`JOB_FIELDS`]), or `rule.name`.
pub fn check_template(template: &str) -> Result<(), String> {
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            return Err(format!("`{{{{` without `}}}}` in `{template}`"));
        };
        let inner = after[..close].trim();
        let known = if inner.starts_with("event.") {
            crate::template::check(&format!("{{{{{inner}}}}}")).is_ok()
        } else if let Some(f) = inner.strip_prefix("card.") {
            CARD_FIELDS.contains(&f)
        } else if let Some(f) = inner.strip_prefix("job.") {
            JOB_FIELDS.contains(&f)
        } else if let Some(f) = inner.strip_prefix("group.") {
            GROUP_FIELDS.contains(&f)
        } else {
            inner == "rule.name"
        };
        if !known {
            return Err(format!(
                "unknown placeholder `{{{{{inner}}}}}`: use event.<kind|summary|project|seq|actor|data.path>, \
                 card.<{}>, job.<{}>, group.<{}>, or rule.name",
                CARD_FIELDS.join("|"),
                JOB_FIELDS.join("|"),
                GROUP_FIELDS.join("|")
            ));
        }
        rest = &after[close + 2..];
    }
    Ok(())
}

/// Does `reference` name a column of `columns`? `@<x>`: the column with
/// the role `x`, else the column named `x`. A plain name: the column of
/// that name.
pub fn column_exists(columns: &[BoardColumn], reference: &str) -> bool {
    match reference.strip_prefix('@') {
        Some(x) => columns
            .iter()
            .any(|c| c.role.as_deref() == Some(x) || c.name == x),
        None => columns.iter().any(|c| c.name == reference),
    }
}

/// A WCL value as JSON. Texts, identifiers, and symbols are strings.
fn json_of(v: &wcl_lang::Value) -> Value {
    use wcl_lang::Value as W;
    match v {
        W::Utf8(s) | W::Ascii(s) | W::Identifier(s) | W::Symbol(s) => Value::String(s.clone()),
        W::List(items) => Value::Array(items.iter().map(json_of).collect()),
        W::None => Value::Null,
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

/// The texts of a JSON value: a string, or the strings of a list.
fn texts(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// The checks of `job_queue` across its arguments: a batch needs `from`;
/// `limit`, `min_cards`, and `max_wait_minutes` are for a batch only;
/// `max_wait_minutes` needs `min_cards` (without it the batch goes at
/// once); `min_cards` is at most `limit`.
fn check_batch_args(args: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    let batch = args.get("kind").and_then(Value::as_str) == Some("batch");
    if batch && !args.contains_key("from") {
        out.push("job_queue of kind batch needs `from` (a column)".to_owned());
    }
    if !batch {
        for key in ["from", "limit", "min_cards", "max_wait_minutes"] {
            if args.contains_key(key) {
                out.push(format!("job_queue.{key} is for kind batch only"));
            }
        }
        return out;
    }
    if args.contains_key("max_wait_minutes") && !args.contains_key("min_cards") {
        out.push(
            "job_queue.max_wait_minutes needs `min_cards` (without it the batch goes at once)"
                .to_owned(),
        );
    }
    if let (Some(min), Some(limit)) = (
        args.get("min_cards").and_then(Value::as_i64),
        args.get("limit").and_then(Value::as_i64),
    ) && min > limit
    {
        out.push(format!(
            "job_queue.min_cards ({min}) is more than its limit ({limit}): the batch would never go"
        ));
    }
    out
}

/// Check the value of one argument. `Err` is the message.
fn check_arg(ctx: &Context, arg: Arg, v: &Value) -> Result<(), String> {
    match arg {
        Arg::Text | Arg::List => {
            for t in texts(v) {
                check_template(t)?;
            }
            Ok(())
        }
        Arg::Code => match v.as_str() {
            Some(s) if s.trim().is_empty() => Err("is empty".into()),
            Some(s) if s.len() > MAX_SCRIPT_BYTES => Err(format!(
                "has {} bytes; a script has at most {MAX_SCRIPT_BYTES}",
                s.len()
            )),
            Some(_) => Ok(()),
            None => Err("must be a text".into()),
        },
        Arg::Bool => {
            if v.is_boolean() {
                Ok(())
            } else {
                Err("must be a boolean".into())
            }
        }
        Arg::Count => match v.as_i64() {
            Some(n) if n >= 1 => Ok(()),
            _ => Err(format!("needs a whole number of 1 or more, not `{v}`")),
        },
        Arg::OneOf(words) => match v.as_str() {
            Some(s) if words.contains(&s) => Ok(()),
            _ => Err(format!("needs one of {}, not `{v}`", words.join(", "))),
        },
        Arg::Column | Arg::Columns => {
            let names = texts(v);
            if names.iter().any(|n| n.trim().is_empty()) {
                return Err("names an empty column".into());
            }
            if let Some(cols) = &ctx.columns {
                for n in names {
                    if !column_exists(cols, n) {
                        let have: Vec<String> = cols
                            .iter()
                            .map(|c| match &c.role {
                                Some(r) => format!("{} (@{r})", c.name),
                                None => c.name.clone(),
                            })
                            .collect();
                        return Err(format!(
                            "unknown column `{n}`: the board has {}",
                            have.join(", ")
                        ));
                    }
                }
            }
            Ok(())
        }
    }
}

/// Read one `when` block as a match rule.
fn read_when(path: String, fields: &Map<String, Value>) -> Result<MatchRule, String> {
    let compares: Vec<(&str, MatchOp)> = [
        ("eq", MatchOp::Eq),
        ("ne", MatchOp::Ne),
        ("glob", MatchOp::Glob),
        ("one_of", MatchOp::In),
    ]
    .into_iter()
    .filter(|(k, _)| fields.get(*k).is_some_and(|v| !v.is_null()))
    .collect();
    let [(key, op)] = compares[..] else {
        return Err(format!(
            "`when \"{path}\"` needs exactly one of eq, ne, glob, one_of"
        ));
    };
    let m = MatchRule {
        path,
        op,
        value: fields.get(key).cloned().unwrap_or(Value::Null),
    };
    m.check()?;
    Ok(m)
}

/// The texts of the field `key` of `fields`; absent: empty.
fn text_list(fields: &Map<String, Value>, key: &str) -> Vec<String> {
    fields
        .get(key)
        .map(|v| texts(v).into_iter().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Read one `if_card` block.
fn read_if_card(ctx: &Context, fields: &Map<String, Value>) -> Result<CardCondition, String> {
    for key in ["column", "not_column"] {
        if let Some(v) = fields.get(key).filter(|v| !v.is_null()) {
            check_arg(ctx, Arg::Columns, v).map_err(|e| format!("if_card.{key} {e}"))?;
        }
    }
    let c = CardCondition {
        group_key_prefix: fields
            .get("group_key_prefix")
            .and_then(Value::as_str)
            .map(str::to_owned),
        column: text_list(fields, "column"),
        not_column: text_list(fields, "not_column"),
        has_labels: text_list(fields, "has_labels"),
        lacks_labels: text_list(fields, "lacks_labels"),
        assigned: fields.get("assigned").and_then(Value::as_bool),
        archived: fields.get("archived").and_then(Value::as_bool),
    };
    if c.has_labels
        .iter()
        .chain(&c.lacks_labels)
        .any(|l| l.trim().is_empty() || l.contains("{{"))
    {
        return Err("if_card labels are plain, non-empty texts".into());
    }
    if c == CardCondition::default() {
        return Err("if_card needs at least one field".into());
    }
    Ok(c)
}

/// Read one `if_job` block.
fn read_if_job(fields: &Map<String, Value>) -> Result<JobCondition, String> {
    let flag = |k: &str| fields.get(k).and_then(Value::as_bool);
    let c = JobCondition {
        state: text_list(fields, "state"),
        outcome: text_list(fields, "outcome"),
        split: flag("split"),
        batch: flag("batch"),
        rerun: flag("rerun"),
        halves: flag("halves"),
    };
    if let Some(s) = c.state.iter().find(|s| !JOB_STATES.contains(&s.as_str())) {
        return Err(format!(
            "if_job.state `{s}` is not one of {}",
            JOB_STATES.join(", ")
        ));
    }
    if let Some(o) = c
        .outcome
        .iter()
        .find(|o| !JOB_OUTCOMES.contains(&o.as_str()))
    {
        return Err(format!(
            "if_job.outcome `{o}` is not one of {}",
            JOB_OUTCOMES.join(", ")
        ));
    }
    if c == JobCondition::default() {
        return Err("if_job needs at least one field".into());
    }
    Ok(c)
}

/// Every column that `rule` names: the column arguments of its verbs and
/// the columns of its `if_card` conditions, in file order. The engine
/// checks them against the board when a run starts (the board may have
/// changed since the file was checked).
pub fn column_refs(rule: &Rule) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &rule.if_card {
        out.extend(c.column.iter().chain(&c.not_column).cloned());
    }
    for a in &rule.actions {
        let Some(verb) = VERBS.iter().find(|v| v.name == a.verb) else {
            continue;
        };
        for (key, arg) in verb.args {
            if matches!(arg, Arg::Column | Arg::Columns)
                && let Some(v) = a.args.get(*key)
            {
                out.extend(texts(v).into_iter().map(str::to_owned));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.clone()));
    out
}

/// The column of `columns` that `reference` names ([`column_exists`]):
/// `@<x>` is the column with the role `x`, else the column named `x`.
pub fn resolve_column<'a>(columns: &'a [BoardColumn], reference: &str) -> Option<&'a BoardColumn> {
    match reference.strip_prefix('@') {
        Some(x) => columns
            .iter()
            .find(|c| c.role.as_deref() == Some(x))
            .or_else(|| columns.iter().find(|c| c.name == x)),
        None => columns.iter().find(|c| c.name == reference),
    }
}

/// Fill a checked template ([`check_template`]): each `{{...}}` is
/// replaced by `lookup` of its inner name (`event.kind`, `card.id`,
/// `job.id`, `rule.name`, ...); `None` is empty. With `quoted`, each
/// value is quoted data ([`crate::template::quote`]): for text that an
/// agent reads, since a value can come from an event or a card title. The
/// values of [`RAW_FIELDS`] are never quoted: the daemon makes them.
pub fn render(template: &str, lookup: &dyn Fn(&str) -> Option<String>, quoted: bool) -> String {
    render_with(template, lookup, quoted).0
}

/// [`render`], and whether a placeholder of the template had no value.
pub fn render_with(
    template: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    quoted: bool,
) -> (String, bool) {
    let mut empty = false;
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            return (out, empty);
        };
        let name = after[..close].trim();
        let v = lookup(name).unwrap_or_default();
        empty |= v.is_empty();
        if quoted && !RAW_FIELDS.contains(&name) {
            out.push_str(&crate::template::quote(&v));
        } else {
            out.push_str(&v);
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    (out, empty)
}

/// The fields of a block as JSON, with the offset of each.
fn block_fields(b: &wcl_lang::Block<'_>) -> Vec<(String, Value, usize)> {
    b.fields()
        .filter_map(|f| {
            let v = f.value().ok()?;
            Some((f.name().to_owned(), json_of(v), f.span().start))
        })
        .collect()
}

/// Read and check a rules file. `file` names the file in the errors.
pub fn parse(text: &str, file: &str, ctx: &Context) -> Result<RuleSet, FileError> {
    let fail = |problems: Vec<Problem>| FileError {
        file: file.to_owned(),
        problems,
    };
    if text.len() > MAX_BYTES {
        return Err(fail(vec![problem_at(
            text,
            None,
            format!(
                "the file has {} bytes; a {FILE_NAME} has at most {MAX_BYTES}",
                text.len()
            ),
        )]));
    }
    // The items first: no import, no declaration.
    match wcl_lang::parse_for_edit(text, file) {
        Ok(src) => {
            let mut problems = Vec::new();
            forbidden_items(text, &src.items, &mut problems);
            if !problems.is_empty() {
                return Err(fail(problems));
            }
        }
        Err(e) => {
            let mut problems: Vec<Problem> = e
                .syntax_errors()
                .map(|s| problem_at(text, Some(s.span.offset()), s.message.clone()))
                .collect();
            if problems.is_empty() {
                problems.push(problem_at(text, None, e.to_string()));
            }
            return Err(fail(problems));
        }
    }
    // The schema goes after the text, so the offsets of the text stay.
    let source = format!("{text}\n{SCHEMA}");
    let doc =
        match Document::open_at_with_loader(&source, file, None, &Environment::new(), no_files()) {
            Ok(d) => d,
            Err(e) => {
                let mut problems: Vec<Problem> = e
                    .syntax_errors()
                    .map(|s| problem_at(text, Some(s.span.offset()), s.message.clone()))
                    .collect();
                if problems.is_empty() {
                    problems.push(problem_at(text, None, e.to_string()));
                }
                return Err(fail(problems));
            }
        };
    let mut problems: Vec<Problem> = doc
        .schema_errors()
        .iter()
        .map(|e| problem_at(text, label_offset(e), e.to_string()))
        .collect();
    if !problems.is_empty() {
        return Err(fail(problems));
    }
    let mut rules: Vec<Rule> = Vec::new();
    for b in doc.blocks().filter(|b| b.kind() == RULE) {
        let at = b.span().start;
        let name = match b.labels() {
            Ok(l) => l.first().map(json_of).and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            }),
            Err(e) => {
                problems.push(problem_at(text, Some(at), e.to_string()));
                continue;
            }
        };
        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            problems.push(problem_at(text, Some(at), "a rule needs a name"));
            continue;
        };
        if rules.iter().any(|r| r.name == name) {
            problems.push(problem_at(
                text,
                Some(at),
                format!("a second rule named `{name}`"),
            ));
            continue;
        }
        let mut rule = Rule {
            name: name.clone(),
            description: None,
            enabled: true,
            on: Vec::new(),
            when: Vec::new(),
            if_card: Vec::new(),
            if_job: Vec::new(),
            max_per_hour: None,
            actions: Vec::new(),
            line: position(text, at).0,
        };
        for f in b.fields() {
            let v = match f.value() {
                Ok(v) => json_of(v),
                Err(e) => {
                    problems.push(problem_at(text, Some(f.span().start), e.to_string()));
                    continue;
                }
            };
            let fat = Some(f.span().start);
            match (f.name(), v) {
                (_, Value::Null) => {}
                ("description", Value::String(s)) => rule.description = Some(s),
                ("enabled", Value::Bool(e)) => rule.enabled = e,
                ("on", v) => {
                    rule.on = texts(&v).into_iter().map(str::to_owned).collect();
                    if rule.on.iter().any(|k| k.trim().is_empty()) {
                        problems.push(problem_at(
                            text,
                            fat,
                            format!("rule `{name}`: `on` has an empty kind"),
                        ));
                    }
                }
                ("max_per_hour", v) => match v.as_u64() {
                    Some(n) if (1..=MAX_PER_HOUR as u64).contains(&n) => {
                        rule.max_per_hour = Some(n as u32);
                    }
                    _ => problems.push(problem_at(
                        text,
                        fat,
                        format!("rule `{name}`: `max_per_hour` is 1 to {}", MAX_PER_HOUR),
                    )),
                },
                // The schema check reported any other field.
                _ => {}
            }
        }
        if rule.on.is_empty() && rule.enabled {
            problems.push(problem_at(
                text,
                Some(at),
                format!("rule `{name}`: `on` needs at least one event kind"),
            ));
        }
        for child in b.blocks() {
            let cat = child.span().start;
            let fields = block_fields(&child);
            let map: Map<String, Value> = fields
                .iter()
                .map(|(k, v, _)| (k.clone(), v.clone()))
                .collect();
            if child.kind() == WHEN {
                let path = match child.labels() {
                    Ok(l) => l.first().map(json_of),
                    Err(_) => None,
                };
                let Some(Value::String(path)) = path else {
                    problems.push(problem_at(
                        text,
                        Some(cat),
                        format!("rule `{name}`: `when` needs a path"),
                    ));
                    continue;
                };
                match read_when(path, &map) {
                    Ok(m) => rule.when.push(m),
                    Err(e) => {
                        problems.push(problem_at(text, Some(cat), format!("rule `{name}`: {e}")))
                    }
                }
                continue;
            }
            if child.kind() == IF_CARD {
                match read_if_card(ctx, &map) {
                    Ok(c) => rule.if_card.push(c),
                    Err(e) => {
                        problems.push(problem_at(text, Some(cat), format!("rule `{name}`: {e}")))
                    }
                }
                continue;
            }
            if child.kind() == IF_JOB {
                match read_if_job(&map) {
                    Ok(c) => rule.if_job.push(c),
                    Err(e) => {
                        problems.push(problem_at(text, Some(cat), format!("rule `{name}`: {e}")))
                    }
                }
                continue;
            }
            let Some(verb) = VERBS.iter().find(|v| v.kind == child.kind()) else {
                // The schema check reported an unknown block kind.
                continue;
            };
            let mut args = Map::new();
            for (key, v, fat) in fields {
                if v.is_null() {
                    continue;
                }
                let Some((_, arg)) = verb.args.iter().find(|(k, _)| *k == key) else {
                    continue;
                };
                if let Err(e) = check_arg(ctx, *arg, &v) {
                    problems.push(problem_at(
                        text,
                        Some(fat),
                        format!("rule `{name}`: {}.{key} {e}", verb.kind),
                    ));
                }
                args.insert(key, v);
            }
            for key in verb.required {
                if !args.contains_key(*key) {
                    problems.push(problem_at(
                        text,
                        Some(cat),
                        format!("rule `{name}`: {} needs `{key}`", verb.kind),
                    ));
                }
            }
            if verb.kind == "job_queue" {
                for e in check_batch_args(&args) {
                    problems.push(problem_at(text, Some(cat), format!("rule `{name}`: {e}")));
                }
            }
            if verb.name == SCRIPT_VERB
                && let Some(f) = args.get("fuel").and_then(Value::as_u64)
                && f > MAX_SCRIPT_FUEL
            {
                problems.push(problem_at(
                    text,
                    Some(cat),
                    format!("rule `{name}`: script.fuel is at most {MAX_SCRIPT_FUEL}, not {f}"),
                ));
            }
            if verb.kind == "card_label" && args.is_empty() {
                problems.push(problem_at(
                    text,
                    Some(cat),
                    format!("rule `{name}`: card_label needs `add` or `remove`"),
                ));
            }
            rule.actions.push(Action {
                verb: verb.name.to_owned(),
                args,
            });
        }
        if rule.actions.is_empty() && rule.enabled {
            problems.push(problem_at(
                text,
                Some(at),
                format!("rule `{name}`: a rule needs at least one verb"),
            ));
        }
        rules.push(rule);
    }
    if rules.len() > ctx.max_rules {
        problems.push(problem_at(
            text,
            None,
            format!(
                "{} rules; a project has at most {} (TERRARIUM_MAX_RULES_PER_PROJECT)",
                rules.len(),
                ctx.max_rules
            ),
        ));
    }
    if !problems.is_empty() {
        return Err(fail(problems));
    }
    Ok(RuleSet { rules })
}

/// The first line of the rule around the output of a script in
/// [`script_actions`].
const SCRIPT_WRAP: &str = "rule \"script\" {\n  on = [\"script\"]\n";

/// The verbs that a script returned: `output` holds verb blocks in the
/// syntax of a rules file (for example `card_move { to = "@review" }`).
/// They are checked as the verbs of one rule ([`parse`], with the columns
/// of `ctx`), and nothing else may be in the output: no second rule, no
/// condition, no field of the rule, no script step. An empty output is
/// no verb. `Err` is the message, with the lines of the output.
pub fn script_actions(output: &str, ctx: &Context) -> Result<Vec<Action>, String> {
    if output.len() > MAX_SCRIPT_OUTPUT {
        return Err(format!(
            "the script returned {} bytes; a script returns at most {MAX_SCRIPT_OUTPUT}",
            output.len()
        ));
    }
    if output.trim().is_empty() {
        return Ok(Vec::new());
    }
    let text = format!("{SCRIPT_WRAP}{output}\n}}\n");
    let ctx = Context {
        columns: ctx.columns.clone(),
        max_rules: 1,
    };
    let lines = SCRIPT_WRAP.matches('\n').count();
    let set = parse(&text, "script output", &ctx).map_err(|e| {
        let problems: Vec<String> = e
            .problems
            .iter()
            .take(5)
            .map(|p| match p.line.checked_sub(lines) {
                Some(l) if l >= 1 => format!("line {l}: {}", p.message),
                _ => p.message.clone(),
            })
            .collect();
        format!(
            "the script output is not verb blocks: {}",
            problems.join("; ")
        )
    })?;
    let [rule] = set.rules.as_slice() else {
        return Err("the script output holds a rule; it holds verb blocks only".into());
    };
    let plain = Rule {
        actions: rule.actions.clone(),
        line: rule.line,
        ..Rule {
            name: "script".into(),
            description: None,
            enabled: true,
            on: vec!["script".into()],
            when: Vec::new(),
            if_card: Vec::new(),
            if_job: Vec::new(),
            max_per_hour: None,
            actions: Vec::new(),
            line: 0,
        }
    };
    if *rule != plain {
        return Err(
            "the script output sets a field or a condition of the rule; it holds verb blocks only"
                .into(),
        );
    }
    if rule.actions.iter().any(|a| a.verb == SCRIPT_VERB) {
        return Err("a script cannot return a script step".into());
    }
    if rule.actions.len() > MAX_SCRIPT_ACTIONS {
        return Err(format!(
            "the script returned {} verbs; a script returns at most {MAX_SCRIPT_ACTIONS}",
            rule.actions.len()
        ));
    }
    Ok(rule.actions.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> Context {
        let cols = [
            ("backlog", None),
            ("ready", None),
            ("in_progress", Some("working")),
            ("validating", Some("validating")),
            ("review", None),
            ("blocked", Some("blocked")),
            ("done", None),
        ];
        Context {
            columns: Some(
                cols.iter()
                    .map(|(n, r)| BoardColumn {
                        name: (*n).to_owned(),
                        role: r.map(str::to_owned),
                    })
                    .collect(),
            ),
            max_rules: DEFAULT_MAX_RULES,
        }
    }

    const SCRIPTED: &str = r#"rule "scripted" {
  on = ["card"]
  script {
    fuel = 5000
    source = <<'WS'
      use rule
      fn main() -> string {
        let t = "{{event.not.a.template}}"
        "card_comment {{ text = \"x\" }}"
      }
      WS
  }
  card_comment { text = "after" }
}
"#;

    #[test]
    fn a_script_step_reads_with_its_source_as_it_is() {
        let set = parse(SCRIPTED, "rules.wcl", &board()).unwrap();
        let r = &set.rules[0];
        assert_eq!(r.actions.len(), 2);
        assert_eq!(r.actions[0].verb, SCRIPT_VERB);
        assert_eq!(r.actions[0].args["fuel"], serde_json::json!(5000));
        let src = r.actions[0].args["source"].as_str().unwrap();
        assert!(src.starts_with("use rule\n"), "{src:?}");
        assert!(src.contains("{{event.not.a.template}}"), "{src:?}");
        assert_eq!(r.actions[1].verb, "card.comment");
    }

    #[test]
    fn a_script_step_has_bounds() {
        let big = SCRIPTED.replace("fuel = 5000", "fuel = 10000001");
        let e = parse(&big, "rules.wcl", &board()).unwrap_err().to_string();
        assert!(e.contains("script.fuel is at most 10000000"), "{e}");
        let empty = "rule \"s\" {\n  on = [\"card\"]\n  script { source = \"  \" }\n}\n";
        let e = parse(empty, "rules.wcl", &board()).unwrap_err().to_string();
        assert!(e.contains("script.source is empty"), "{e}");
        let long = format!(
            "rule \"s\" {{\n  on = [\"card\"]\n  script {{ source = \"{}\" }}\n}}\n",
            "x".repeat(MAX_SCRIPT_BYTES + 1)
        );
        let e = parse(&long, "rules.wcl", &board()).unwrap_err().to_string();
        assert!(e.contains("a script has at most"), "{e}");
        let none = "rule \"s\" {\n  on = [\"card\"]\n  script { fuel = 10 }\n}\n";
        assert!(parse(none, "rules.wcl", &board()).is_err());
    }

    #[test]
    fn script_output_is_checked_as_verbs() {
        let ctx = board();
        let a = script_actions(
            "card_move { to = \"review\" }\ncard_label { add = [\"x\"] }",
            &ctx,
        )
        .unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].verb, "card.move");
        assert_eq!(a[1].args["add"], serde_json::json!(["x"]));
        assert!(script_actions("  \n", &ctx).unwrap().is_empty());

        let bad = [
            ("card_move { to = \"nowhere\" }", "unknown column `nowhere`"),
            ("card_comment { text = \"{{nope}}\" }", "line 1"),
            ("card_move { to = 3 }", "not verb blocks"),
            ("let x = 1", "not verb blocks"),
            (
                "}\nrule \"two\" {\n on = [\"x\"]\n card_comment { text = \"t\" }",
                "not verb blocks",
            ),
            (
                "when \"kind\" { eq = \"x\" }\ncard_comment { text = \"t\" }",
                "verb blocks only",
            ),
            ("enabled = false", "verb blocks only"),
            (
                "description = \"d\"\ncard_comment { text = \"t\" }",
                "verb blocks only",
            ),
            ("script { source = \"x\" }", "cannot return a script step"),
            ("card_comment { text = 1 2 }", "not verb blocks"),
        ];
        for (out, want) in bad {
            let e = script_actions(out, &ctx).unwrap_err();
            assert!(e.contains(want), "{out:?}: {e}");
        }
        let many = "card_comment { text = \"t\" }\n".repeat(MAX_SCRIPT_ACTIONS + 1);
        let e = script_actions(&many, &ctx).unwrap_err();
        assert!(e.contains("at most 20"), "{e}");
        let huge = "x".repeat(MAX_SCRIPT_OUTPUT + 1);
        let e = script_actions(&huge, &ctx).unwrap_err();
        assert!(e.contains("returns at most"), "{e}");
    }

    const GOOD: &str = r#"// The flow of a validate job.
rule "validate-queued" {
  description = "A queued validate job moves its card to validating."
  on = ["job"]
  when "data.change" { eq = "queued" }
  when "data.kind" { one_of = ["validate"] }
  card_label { add = ["validation:queued", "by:{{job.by_agent}}"] }
  card_move {
    to = "@validating"
    comment = "Job queue: validate job {{job.id}} queued by {{event.actor}}"
  }
}

rule "validate-pass" {
  on = ["job.verdict"]
  enabled = false
  max_per_hour = 60
  when "data.verdict" { eq = "passed" }
  card_label { remove = ["validation:running", "validation:queued"] }
  card_move { to = "review"  from = ["@validating"]  position = "top" }
  card_assign { who = "none" }
  agent_end { outcome = "done" }
}
"#;

    #[test]
    fn group_close_all_done_requires_a_boolean() {
        let err = errors(
            "rule \"close\" { on = [\"card\"] group_close { key = \"batch\" all_done = \"yes\" } }",
        );
        assert!(err.contains("all_done"), "{err}");
    }

    fn errors(text: &str) -> String {
        parse(text, "rules.wcl", &board()).unwrap_err().to_string()
    }

    #[test]
    fn a_good_file_reads_in_order() {
        let set = parse(GOOD, "rules.wcl", &board()).expect("good file");
        assert_eq!(set.rules.len(), 2);
        let q = &set.rules[0];
        assert_eq!(q.name, "validate-queued");
        assert!(q.enabled);
        assert_eq!(q.on, vec!["job"]);
        assert_eq!(q.line, 2);
        assert_eq!(q.when.len(), 2);
        assert_eq!(q.when[1].op, MatchOp::In);
        let verbs: Vec<&str> = q.actions.iter().map(|a| a.verb.as_str()).collect();
        assert_eq!(verbs, vec!["card.label", "card.move"]);
        assert_eq!(q.actions[1].args["to"], "@validating");
        let p = &set.rules[1];
        assert!(!p.enabled);
        assert_eq!(p.max_per_hour, Some(60));
        let verbs: Vec<&str> = p.actions.iter().map(|a| a.verb.as_str()).collect();
        assert_eq!(
            verbs,
            vec!["card.label", "card.move", "card.assign", "agent.end"]
        );
    }

    #[test]
    fn an_empty_file_has_no_rules() {
        let set = parse("// nothing yet\n", "rules.wcl", &board()).expect("empty file");
        assert!(set.rules.is_empty());
    }

    #[test]
    fn an_unknown_verb_is_refused_at_its_line() {
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_frobnicate { to = \"review\" }\n}\n";
        let e = errors(text);
        assert!(e.contains("card_frobnicate"), "{e}");
        assert!(e.starts_with("rules.wcl:3:"), "{e}");
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { to = \"review\"  colour = \"red\" }\n}\n";
        let e = errors(text);
        assert!(e.contains("colour"), "{e}");
    }

    #[test]
    fn a_missing_required_argument_is_refused() {
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { comment = \"x\" }\n}\n";
        let e = errors(text);
        assert!(e.contains("card_move needs `to`"), "{e}");
        assert!(e.starts_with("rules.wcl:3:"), "{e}");
    }

    #[test]
    fn an_unknown_column_is_refused() {
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { to = \"accepted\" }\n}\n";
        let e = errors(text);
        assert!(e.contains("unknown column `accepted`"), "{e}");
        assert!(e.starts_with("rules.wcl:3:"), "{e}");
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { to = \"@merge_queue\" }\n}\n";
        assert!(errors(text).contains("unknown column `@merge_queue`"));
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { to = \"review\"  from = [\"@nope\"] }\n}\n";
        assert!(errors(text).contains("unknown column `@nope`"));
        // A role and a name both resolve; without a board, nothing is checked.
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { to = \"@working\" }\n}\n";
        parse(text, "rules.wcl", &board()).expect("role");
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { to = \"@review\" }\n}\n";
        parse(text, "rules.wcl", &board()).expect("name fallback");
        let text = "rule \"r\" {\n  on = [\"card\"]\n  card_move { to = \"accepted\" }\n}\n";
        parse(text, "rules.wcl", &Context::default()).expect("no board");
    }

    #[test]
    fn an_import_is_refused() {
        let e = errors("import \"./other.wcl\"\n");
        assert!(e.contains("an import (imports are off)"), "{e}");
        assert!(e.starts_with("rules.wcl:1:1:"), "{e}");
        let e = errors("import <terrarium/rules.wcl>\n");
        assert!(e.contains("imports are off"), "{e}");
        // An import inside a rule too.
        let e =
            errors("rule \"r\" {\n  import \"./x.wcl\"\n  on = [\"card\"]\n  job_cancel {}\n}\n");
        assert!(e.contains("imports are off"), "{e}");
    }

    #[test]
    fn declarations_and_decorators_are_refused() {
        let e = errors("type Rule { x: utf8 }\n");
        assert!(e.contains("a type declaration"), "{e}");
        let e = errors("symbol_set S { a }\n");
        assert!(e.contains("symbol_set"), "{e}");
        let e = errors("@schemaless\nrule \"r\" {\n  anything = 1\n}\n");
        assert!(e.contains("decorator"), "{e}");
    }

    #[test]
    fn only_plain_data_is_allowed() {
        let rule = |body: &str| format!("rule \"r\" {{\n  on = [\"card\"]\n{body}\n}}\n");
        // A function call, at a field and inside a verb.
        let e = errors(&rule("  description = upper(\"x\")\n  job_cancel {}"));
        assert!(e.contains("a function call is not allowed"), "{e}");
        assert!(e.starts_with("rules.wcl:3:"), "{e}");
        let big = "  card_comment { text = join(map(range(0, 1048576), fn(i: i64) -> utf8 { repeat(\"x\", 67108864) }), \"\") }";
        let e = errors(&rule(big));
        assert!(e.contains("a function call is not allowed"), "{e}");
        // A let, at the top and in a rule.
        let e = errors(&format!("let x = \"y\"\n{}", rule("  job_cancel {}")));
        assert!(e.contains("a `let` binding"), "{e}");
        assert!(e.starts_with("rules.wcl:1:1:"), "{e}");
        let e = errors(&rule("  let x = 1\n  job_cancel {}"));
        assert!(e.contains("a `let` binding"), "{e}");
        // A lambda, a reference, an operator, an interpolation.
        let e = errors(&rule(
            "  description = fn(i: i64) -> i64 { i }\n  job_cancel {}",
        ));
        assert!(e.contains("a function is not allowed"), "{e}");
        let e = errors(&rule("  description = name\n  job_cancel {}"));
        assert!(e.contains("a reference is not allowed"), "{e}");
        let e = errors(&rule("  max_per_hour = 2 * 3\n  job_cancel {}"));
        assert!(e.contains("an operator is not allowed"), "{e}");
        let e = errors(&rule("  on = [\"a\", upper(\"b\")]\n  job_cancel {}"));
        assert!(e.contains("a function call is not allowed"), "{e}");
        // (Refused, or a syntax error: either way an error.)
        errors(&rule("  card_comment { text = $\"${1 + 1}\" }"));
        // Computed children: a field for a verb slot, or for the rules.
        let e = errors(&rule("  card_move = []\n  job_cancel {}"));
        assert!(e.contains("a field `card_move` is not allowed"), "{e}");
        let e = errors("rules = []\n");
        assert!(e.contains("a field `rules` is not allowed"), "{e}");
        // A computed label.
        errors("rule upper(\"r\") {\n  on = [\"card\"]\n  job_cancel {}\n}\n");
        // Literals still read: a negative number fails the range check only.
        let e = errors(&rule("  max_per_hour = -1\n  job_cancel {}"));
        assert!(e.contains("`max_per_hour` is 1 to"), "{e}");
    }

    #[test]
    fn the_verbs_keep_the_file_order() {
        // Not the order of the schema: grouping by kind would fail here.
        let text = "rule \"r\" {\n  on = [\"card\"]\n  agent_end { outcome = \"done\" }\n  card_move { to = \"review\" }\n  card_label { add = [\"a\"] }\n  card_move { to = \"done\" }\n  card_comment { text = \"x\" }\n}\n";
        let set = parse(text, "rules.wcl", &board()).expect("good file");
        let verbs: Vec<&str> = set.rules[0]
            .actions
            .iter()
            .map(|a| a.verb.as_str())
            .collect();
        assert_eq!(
            verbs,
            vec![
                "agent.end",
                "card.move",
                "card.label",
                "card.move",
                "card.comment"
            ]
        );
        assert_eq!(set.rules[0].actions[1].args["to"], "review");
        assert_eq!(set.rules[0].actions[3].args["to"], "done");
    }

    #[test]
    fn templates_are_checked() {
        let text =
            "rule \"r\" {\n  on = [\"card\"]\n  card_comment { text = \"{{job.nope}}\" }\n}\n";
        let e = errors(text);
        assert!(e.contains("unknown placeholder `{{job.nope}}`"), "{e}");
        check_template("{{event.data.verdict}} {{card.title}} {{rule.name}}").expect("known");
        assert!(check_template("{{event.nope}}").is_err());
        assert!(check_template("{{card.id").is_err());
    }

    #[test]
    fn rule_checks() {
        // Two rules of one name.
        let text = "rule \"r\" {\n  on = [\"card\"]\n  job_cancel {}\n}\nrule \"r\" {\n  on = [\"card\"]\n  job_cancel {}\n}\n";
        assert!(errors(text).contains("a second rule named `r`"));
        // No verb.
        let text = "rule \"r\" {\n  on = [\"card\"]\n}\n";
        assert!(errors(text).contains("at least one verb"));
        // No event kind.
        let text = "rule \"r\" {\n  on = []\n  job_cancel {}\n}\n";
        assert!(errors(text).contains("at least one event kind"));
        // A when with two compares, or a bad path.
        let text = "rule \"r\" {\n  on = [\"card\"]\n  when \"data.to\" { eq = \"a\"  ne = \"b\" }\n  job_cancel {}\n}\n";
        assert!(errors(text).contains("exactly one of"));
        let text =
            "rule \"r\" {\n  on = [\"card\"]\n  when \"nope\" { eq = \"a\" }\n  job_cancel {}\n}\n";
        assert!(errors(text).contains("bad path `nope`"));
        // A bad word.
        let text = "rule \"r\" {\n  on = [\"job\"]\n  job_queue { kind = \"deploy\" }\n}\n";
        assert!(errors(text).contains("needs one of validate, merge, batch"));
        let text = "rule \"r\" {\n  on = [\"job\"]\n  job_queue { kind = \"batch\" }\n}\n";
        assert!(errors(text).contains("needs `from`"));
        // An unknown top-level field.
        assert!(!errors("limit = 3\n").is_empty());
    }

    #[test]
    fn the_cap_holds() {
        let mut text = String::new();
        for i in 0..3 {
            text.push_str(&format!(
                "rule \"r{i}\" {{\n  on = [\"card\"]\n  job_cancel {{}}\n}}\n"
            ));
        }
        let ctx = Context {
            columns: None,
            max_rules: 2,
        };
        let e = parse(&text, "rules.wcl", &ctx).unwrap_err().to_string();
        assert!(e.contains("at most 2"), "{e}");
        let big = format!("// {}\n", "x".repeat(MAX_BYTES));
        let e = parse(&big, "rules.wcl", &ctx).unwrap_err().to_string();
        assert!(e.contains("at most"), "{e}");
    }

    #[test]
    fn the_schema_and_the_verb_table_agree() {
        let doc = Document::open(SCHEMA, "rules.schema.wcl").expect("schema opens");
        assert!(doc.schema_errors().is_empty(), "{:?}", doc.schema_errors());
        for v in VERBS {
            let t = doc
                .block_schema(v.kind)
                .unwrap_or_else(|| panic!("no schema for {}", v.kind));
            let fields: Vec<&str> = t.fields().map(|f| f.name()).collect();
            let args: Vec<&str> = v.args.iter().map(|(k, _)| *k).collect();
            assert_eq!(fields, args, "{}", v.kind);
            let required: Vec<&str> = t
                .fields()
                .filter(|f| !f.optional())
                .map(|f| f.name())
                .collect();
            assert_eq!(required, v.required, "{}", v.kind);
        }
        let rule = doc.block_schema(RULE).expect("rule schema");
        let kinds: Vec<String> = rule
            .fields()
            .filter_map(|f| f.children_block_kind())
            .filter(|k| k != WHEN && k != IF_CARD && k != IF_JOB)
            .collect();
        let verbs: Vec<&str> = VERBS.iter().map(|v| v.kind).collect();
        assert_eq!(kinds, verbs);
    }

    #[test]
    fn state_conditions_read_and_check() {
        let text = r#"rule "r" {
  on = ["job"]
  if_card {
    column = ["@validating", "review"]
    lacks_labels = ["hold"]
    assigned = true
  }
  if_card { not_column = ["done"] }
  if_job { state = ["passed"]  outcome = ["none"] }
  card_comment { text = "ok" }
}
"#;
        let set = parse(text, "rules.wcl", &board()).unwrap();
        let r = &set.rules[0];
        assert_eq!(r.if_card.len(), 2);
        assert_eq!(r.if_card[0].column, vec!["@validating", "review"]);
        assert_eq!(r.if_card[0].lacks_labels, vec!["hold"]);
        assert_eq!(r.if_card[0].assigned, Some(true));
        assert_eq!(r.if_card[1].not_column, vec!["done"]);
        assert_eq!(r.if_job[0].state, vec!["passed"]);
        assert_eq!(r.actions.len(), 1, "a condition is not a verb");
        assert_eq!(
            column_refs(r),
            vec!["@validating", "review", "done"],
            "the columns of the conditions"
        );

        let bad = |cond: &str| {
            errors(&format!(
                "rule \"r\" {{\n  on = [\"job\"]\n  {cond}\n  card_comment {{ text = \"x\" }}\n}}\n"
            ))
        };
        let e = bad(r#"if_card { column = ["nowhere"] }"#);
        assert!(e.contains("unknown column `nowhere`"), "{e}");
        let e = bad("if_card {}");
        assert!(e.contains("at least one field"), "{e}");
        let e = bad(r#"if_card { has_labels = ["{{card.id}}"] }"#);
        assert!(e.contains("plain"), "{e}");
        let e = bad(r#"if_job { state = ["done"] }"#);
        assert!(e.contains("if_job.state `done`"), "{e}");
        let e = bad(r#"if_job { outcome = ["green"] }"#);
        assert!(e.contains("if_job.outcome `green`"), "{e}");
        let e = bad("if_job {}");
        assert!(e.contains("if_job needs at least one field"), "{e}");
        let e = bad(r#"if_card { colour = ["x"] }"#);
        assert!(!e.is_empty(), "an unknown field is a schema error");
        let e = bad(r#"if_card = [1]"#);
        assert!(e.contains("field `if_card` is not allowed"), "{e}");
    }

    #[test]
    fn column_refs_take_the_verb_columns() {
        let text = r#"rule "r" {
  on = ["card"]
  card_move { to = "@review"  from = ["@validating", "ready"] }
  job_queue { kind = "batch"  from = "review" }
  card_comment { text = "x" }
}
"#;
        let set = parse(text, "rules.wcl", &board()).unwrap();
        assert_eq!(
            column_refs(&set.rules[0]),
            vec!["@review", "@validating", "ready", "review"]
        );
        let cols = board().columns.unwrap();
        assert_eq!(
            resolve_column(&cols, "@working").unwrap().name,
            "in_progress"
        );
        assert_eq!(resolve_column(&cols, "@review").unwrap().name, "review");
        assert_eq!(resolve_column(&cols, "review").unwrap().name, "review");
        assert!(resolve_column(&cols, "@merge_queue").is_none());
    }

    #[test]
    fn render_fills_and_quotes() {
        let lookup = |k: &str| match k {
            "card.id" => Some("TER-1".to_owned()),
            "event.actor" => Some("a`b".to_owned()),
            _ => None,
        };
        assert_eq!(
            render(
                "{{card.id}} by {{ event.actor }}, {{job.id}}.",
                &lookup,
                false
            ),
            "TER-1 by a`b, ."
        );
        // An ID that the daemon makes is never quoted (RAW_FIELDS).
        assert_eq!(
            render("{{card.id}} by {{event.actor}}", &lookup, true),
            "TER-1 by `a'b`"
        );
        assert_eq!(render("open {{card.id", &lookup, false), "open {{card.id");
        assert_eq!(
            render_with("by:{{job.by_agent}}", &lookup, false),
            ("by:".to_owned(), true),
            "an empty placeholder is reported"
        );
        assert_eq!(
            render_with("{{card.id}}", &lookup, false),
            ("TER-1".to_owned(), false)
        );
    }

    /// The columns of a new board (`store::board::DEFAULT_COLUMNS`).
    fn default_board() -> Context {
        let cols = [
            ("backlog", None),
            ("ready", None),
            ("in_progress", Some("working")),
            ("validating", Some("validating")),
            ("review", Some("review")),
            ("merge_queue", Some("merge_queue")),
            ("accepted", Some("accepted")),
            ("blocked", Some("blocked")),
            ("done", None),
        ];
        Context {
            columns: Some(
                cols.iter()
                    .map(|(n, r)| BoardColumn {
                        name: (*n).to_owned(),
                        role: r.map(str::to_owned),
                    })
                    .collect(),
            ),
            max_rules: DEFAULT_MAX_RULES,
        }
    }

    #[test]
    fn the_default_pack_reads_against_the_default_board() {
        let set = parse(DEFAULT_PACK, PACK_FILE, &default_board())
            .unwrap_or_else(|e| panic!("the pack does not read: {e}"));
        assert_eq!(&set, default_pack(), "default_pack() reads the same file");
        assert!(set.rules.len() <= DEFAULT_MAX_RULES);
        let names: Vec<&str> = set.rules.iter().map(|r| r.name.as_str()).collect();
        for want in [
            "merge-queued",
            "job-started",
            "job-requeued",
            "validate-pass",
            "merge-pass",
            "job-rerun-error",
            "job-rerun-red",
            "job-rerun-note",
            "merge-red-group",
            "merge-red-split-note",
            "merge-red-fix-note",
            "job-fail-conflict",
            "job-moved",
            "job-fail-red",
            "job-fail-error",
            "job-cancelled",
            "agent-park",
            "agent-unpark",
            "agent-end-done",
            "agent-end-archived",
            "agent-end-merged",
            "card-blocked-mail",
            "card-done-mail",
        ] {
            assert!(names.contains(&want), "no rule `{want}` in {names:?}");
        }
        let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
        // No rule moves a card to validating or labels a queued validate
        // (no validating column, operator 2026-11-07).
        assert!(
            !names.iter().any(|n| n.starts_with("validate-queued")),
            "{names:?}"
        );
        assert!(
            set.rules
                .iter()
                .flat_map(column_refs)
                .all(|c| c != "@validating" && c != "validating"),
            "the pack names no validating column"
        );
        // The opt-in validate rules read too; their note runs before the
        // move, so a card that the move puts in validating gets one
        // comment, not two.
        let v = parse(VALIDATE_RULES, "rules.validate.wcl", &default_board())
            .unwrap_or_else(|e| panic!("the validate rules do not read: {e}"));
        let vn: Vec<&str> = v.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            vn,
            [
                "validate-queued",
                "validate-queued-note",
                "validate-queued-move"
            ]
        );
        // The rerun decides before its note and before the final failure.
        assert!(at("job-rerun-red") < at("job-rerun-note"));
        assert!(at("job-rerun-note") < at("job-fail-red"));
        assert!(at("merge-red-group") < at("merge-red-split-note"));
        // Every rule may run often: a busy board needs more than the
        // default of 30 runs an hour.
        assert!(set.rules.iter().all(|r| r.max_per_hour == Some(600)));
        // The red merge group: a fix card first, a split after 3 (MEM-84).
        let group = set
            .rules
            .iter()
            .find(|r| r.name == "merge-red-group")
            .unwrap();
        assert_eq!(group.actions[0].verb, "job.fix");
        assert_eq!(group.actions[0].args["max"], 3);
        assert_eq!(group.actions[1].verb, "job.split");
        assert_eq!(group.actions[1].args["after_fixes"], 3);
        // No verb of the pack validates a card (no card validate step).
        assert!(
            set.rules
                .iter()
                .flat_map(|r| &r.actions)
                .all(|a| a.verb != "job.queue"),
            "the pack queues no job"
        );
    }

    #[test]
    fn a_project_rule_replaces_or_turns_off_a_pack_rule() {
        let own = parse(
            r#"rule "job-started" { enabled = false }
rule "validate-pass" {
  on = ["job.verdict"]
  when "data.verdict" { eq = "passed" }
  card_comment { text = "passed" }
}
rule "mine" {
  on = ["card"]
  card_comment { text = "x" }
}
"#,
            "rules.wcl",
            &default_board(),
        )
        .expect("a disabled rule needs no `on` and no verb");
        assert!(!own.rules[0].enabled);
        assert!(own.rules[0].on.is_empty() && own.rules[0].actions.is_empty());
        let pack = default_pack();
        let set = with_pack(pack, &own);
        assert_eq!(set.rules.len(), pack.rules.len() + 1);
        let names: Vec<&str> = set.rules.iter().map(|r| r.name.as_str()).collect();
        let pack_names: Vec<&str> = pack.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            &names[..pack_names.len()],
            &pack_names[..],
            "the pack order stays"
        );
        assert_eq!(names.last(), Some(&"mine"));
        let started = set.rules.iter().find(|r| r.name == "job-started").unwrap();
        assert!(!started.enabled);
        let pass = set
            .rules
            .iter()
            .find(|r| r.name == "validate-pass")
            .unwrap();
        assert_eq!(pass.actions.len(), 1);
        assert_eq!(pass.actions[0].verb, "card.comment");
        // An enabled rule still needs `on` and a verb.
        let e = errors("rule \"r\" { enabled = true }\n");
        assert!(e.contains("at least one event kind"), "{e}");
        assert!(e.contains("at least one verb"), "{e}");
    }

    #[test]
    fn the_merge_queue_rules_read_and_hold_the_batch_numbers() {
        let set = parse(MERGE_QUEUE_RULES, "rules.merge_queue.wcl", &default_board())
            .unwrap_or_else(|e| panic!("the merge queue rules do not read: {e}"));
        let names: Vec<&str> = set.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "merge-queue-move",
                "merge-queue-cancel",
                "merge-queue-batch"
            ]
        );
        let batch = &set.rules[2].actions[0];
        assert_eq!(batch.verb, "job.queue");
        assert_eq!(batch.args["kind"], "batch");
        assert_eq!(batch.args["from"], "@merge_queue");
        // terrarium starts with 4 cards, or 30 minutes after the first
        // one, at most 8 (TER-162 D9, TER-644).
        assert_eq!(batch.args["limit"], 8);
        assert_eq!(batch.args["min_cards"], 4);
        assert_eq!(batch.args["max_wait_minutes"], 30);
        // A board without the column refuses the file.
        let e = parse(MERGE_QUEUE_RULES, "rules.wcl", &board()).unwrap_err();
        assert!(
            e.to_string().contains("unknown column `@merge_queue`"),
            "{e}"
        );
    }

    #[test]
    fn batch_arguments_are_checked_together() {
        let rule = |args: &str| {
            format!("rule \"r\" {{\n  on = [\"card\"]\n  job_queue {{ {args} }}\n}}\n")
        };
        let e = errors(&rule("kind = \"merge\"  limit = 2"));
        assert!(e.contains("job_queue.limit is for kind batch only"), "{e}");
        let e = errors(&rule("kind = \"validate\"  min_cards = 2"));
        assert!(
            e.contains("job_queue.min_cards is for kind batch only"),
            "{e}"
        );
        let e = errors(&rule(
            "kind = \"batch\"  from = \"review\"  max_wait_minutes = 30",
        ));
        assert!(e.contains("max_wait_minutes needs `min_cards`"), "{e}");
        let e = errors(&rule(
            "kind = \"batch\"  from = \"review\"  limit = 4  min_cards = 8",
        ));
        assert!(
            e.contains("min_cards (8) is more than its limit (4)"),
            "{e}"
        );
        let e = errors(&rule("kind = \"batch\"  from = \"review\"  min_cards = 0"));
        assert!(e.contains("1 or more"), "{e}");
        parse(
            &rule("kind = \"batch\"  from = \"review\"  min_cards = 3  max_wait_minutes = 5"),
            "rules.wcl",
            &board(),
        )
        .expect("a batch without a limit takes min_cards and a wait");
    }

    #[test]
    fn job_conditions_on_the_queue() {
        let set = parse(
            r#"rule "r" {
  on = ["job.verdict"]
  if_job {
    split = false
    batch = true
    rerun = false
    halves = true
  }
  job_fix { max = 3 }
  job_split { after_fixes = 3 }
  agent_park { reason = "card in {{card.column}}" }
  agent_end {
    outcome = "done"
    result = "merged ({{job.id}})"
  }
}
"#,
            "rules.wcl",
            &board(),
        )
        .unwrap();
        let c = &set.rules[0].if_job[0];
        assert_eq!(
            (c.split, c.batch, c.rerun, c.halves),
            (Some(false), Some(true), Some(false), Some(true))
        );
        let e = errors("rule \"r\" {\n  on = [\"job\"]\n  job_fix { max = 0 }\n}\n");
        assert!(e.contains("1 or more"), "{e}");
    }
}
