//! Tools for the board rules (TER-171): the migration of a project to the
//! built-in pack, the lint, the switch of one rule on or off in the text
//! of a `rules.wcl`, and a rule as WCL text.
//!
//! Nothing here reads the database or runs a rule. The daemon calls these
//! with the board columns and the parsed files (`crate::rules_file`).
//!
//! - [`migrate`] maps the pack to the columns of one board. A move to a
//!   column that the board lacks becomes a comment (the coded flow wrote
//!   a comment when such a move failed), a column name of a `when` on
//!   `data.to` or `data.from` that the board lacks maps to the column with
//!   that role, and a rule that can no longer act is turned off. Each
//!   changed pack rule becomes an override in the project's file.
//! - [`lint`] finds the columns that a running rule names and the board
//!   lacks (the run would fail), and the columns that a rule moves cards
//!   into but no rule moves them out of.
//! - [`set_enabled`] edits the text of a file so that one rule is on or
//!   off, and keeps the rest of the text as it is.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::rules_file::{self, Action, BoardColumn, CardCondition, Rule, RuleSet, VERBS};
use crate::schedule::{MatchOp, MatchRule};

// ------------------------------------------------------------ WCL text

/// `s` as a WCL string literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON value of a rule as a WCL literal.
fn literal(v: &Value) -> String {
    match v {
        Value::String(s) => quote(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(literal).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Null => "none".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Object(_) => "none".to_owned(),
    }
}

/// The fields of a small block on one line: `{ a = 1  b = "x" }`.
fn inline(fields: &[(String, String)]) -> String {
    if fields.is_empty() {
        return "{}".to_owned();
    }
    let parts: Vec<String> = fields.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    format!("{{ {} }}", parts.join("  "))
}

fn list(items: &[String]) -> String {
    literal(&Value::Array(
        items.iter().cloned().map(Value::String).collect(),
    ))
}

fn card_fields(c: &CardCondition) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(prefix) = &c.group_key_prefix {
        out.push(("group_key_prefix".to_owned(), quote(prefix)));
    }
    for (k, v) in [
        ("column", &c.column),
        ("not_column", &c.not_column),
        ("has_labels", &c.has_labels),
        ("lacks_labels", &c.lacks_labels),
    ] {
        if !v.is_empty() {
            out.push((k.to_owned(), list(v)));
        }
    }
    for (k, v) in [("assigned", c.assigned), ("archived", c.archived)] {
        if let Some(b) = v {
            out.push((k.to_owned(), b.to_string()));
        }
    }
    out
}

/// The arguments of `a` in the order of the schema, then any other.
fn action_fields(a: &Action) -> Vec<(String, String)> {
    let order: Vec<&str> = VERBS
        .iter()
        .find(|v| v.name == a.verb)
        .map(|v| v.arg_names().collect())
        .unwrap_or_default();
    let mut keys: Vec<&String> = a.args.keys().collect();
    keys.sort_by_key(|k| order.iter().position(|o| o == k).unwrap_or(usize::MAX));
    keys.into_iter()
        .map(|k| (k.clone(), literal(&a.args[k])))
        .collect()
}

/// A rule as the text of a `rule` block of a `rules.wcl`. [`crate::rules_file::parse`]
/// reads it back as the same rule (apart from `line`). A rule that is off
/// and has no `on` and no verb is the short form that turns a pack rule
/// off.
pub fn to_wcl(rule: &Rule) -> String {
    let mut out = format!("rule {} {{\n", quote(&rule.name));
    if let Some(d) = &rule.description {
        out.push_str(&format!("  description = {}\n", quote(d)));
    }
    if !rule.enabled {
        out.push_str("  enabled = false\n");
    }
    if !rule.on.is_empty() {
        out.push_str(&format!("  on = {}\n", list(&rule.on)));
    }
    if let Some(n) = rule.max_per_hour {
        out.push_str(&format!("  max_per_hour = {n}\n"));
    }
    for m in &rule.when {
        let key = match m.op {
            MatchOp::Eq => "eq",
            MatchOp::Ne => "ne",
            MatchOp::Glob => "glob",
            MatchOp::In => "one_of",
        };
        out.push_str(&format!(
            "  when {} {}\n",
            quote(&m.path),
            inline(&[(key.to_owned(), literal(&m.value))])
        ));
    }
    for c in &rule.if_card {
        out.push_str(&format!("  if_card {}\n", inline(&card_fields(c))));
    }
    for c in &rule.if_job {
        let mut f = Vec::new();
        if !c.state.is_empty() {
            f.push(("state".to_owned(), list(&c.state)));
        }
        if !c.outcome.is_empty() {
            f.push(("outcome".to_owned(), list(&c.outcome)));
        }
        for (k, v) in [
            ("split", c.split),
            ("batch", c.batch),
            ("rerun", c.rerun),
            ("halves", c.halves),
        ] {
            if let Some(b) = v {
                f.push((k.to_owned(), b.to_string()));
            }
        }
        out.push_str(&format!("  if_job {}\n", inline(&f)));
    }
    for a in &rule.actions {
        let kind = VERBS
            .iter()
            .find(|v| v.name == a.verb)
            .map_or(a.verb.as_str(), |v| v.kind);
        out.push_str(&format!("  {kind} {}\n", inline(&action_fields(a))));
    }
    out.push_str("}\n");
    out
}

// ------------------------------------------------------- the effective set

/// Where a rule of the effective set comes from.
pub const ORIGIN_PACK: &str = "pack";
/// A rule of the project's file with the name of a pack rule.
pub const ORIGIN_OVERRIDE: &str = "override";
/// A rule of the project's file.
pub const ORIGIN_OWN: &str = "own";

/// One rule as the engine runs it, with where it comes from and its text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectiveRule {
    #[serde(flatten)]
    pub rule: Rule,
    /// [`ORIGIN_PACK`], [`ORIGIN_OVERRIDE`], or [`ORIGIN_OWN`].
    pub origin: &'static str,
    /// The rule as WCL ([`to_wcl`]).
    pub wcl: String,
}

/// The rules that the engine runs for a project, in its order: with the
/// pack (`board_flow = rules`) the rules of [`rules_file::with_pack`],
/// else the project's own rules.
pub fn effective(pack: Option<&RuleSet>, own: &RuleSet) -> Vec<EffectiveRule> {
    let set = match pack {
        Some(p) => rules_file::with_pack(p, own),
        None => own.clone(),
    };
    set.rules
        .into_iter()
        .map(|rule| {
            let in_pack = pack.is_some_and(|p| p.rules.iter().any(|r| r.name == rule.name));
            let in_own = own.rules.iter().any(|r| r.name == rule.name);
            let origin = match (in_pack, in_own) {
                (true, true) => ORIGIN_OVERRIDE,
                (true, false) => ORIGIN_PACK,
                _ => ORIGIN_OWN,
            };
            let wcl = to_wcl(&rule);
            EffectiveRule { rule, origin, wcl }
        })
        .collect()
}

// ------------------------------------------------------------ migration

/// What [`migrate`] did with one pack rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub rule: String,
    /// `mapped` (an override with the board's columns), `disabled` (an
    /// override that turns it off), or `kept` (the project's file has a
    /// rule of that name already; it stays).
    pub change: &'static str,
    pub reasons: Vec<String>,
}

/// The result of [`migrate`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Migration {
    /// The new text of the project's `rules.wcl`: its old text, then the
    /// overrides. The old text when nothing changes.
    pub text: String,
    /// One entry for each pack rule that needs a change or that the file
    /// replaces already.
    pub changes: Vec<Change>,
    /// The overrides, in the order of the pack.
    pub overrides: Vec<Rule>,
}

/// The first line of the overrides that [`migrate`] appends.
pub const MIGRATION_HEADER: &str = "// The built-in pack mapped to this board (terrarium rule migrate, TER-171).\n\
// A rule here with the name of a pack rule replaces it.\n";

/// The column of `columns` with the name `name`, else the one with the
/// role `name`.
fn column_named_or_role<'a>(columns: &'a [BoardColumn], name: &str) -> Option<&'a BoardColumn> {
    columns
        .iter()
        .find(|c| c.name == name)
        .or_else(|| columns.iter().find(|c| c.role.as_deref() == Some(name)))
}

fn resolves(columns: &[BoardColumn], reference: &str) -> bool {
    rules_file::resolve_column(columns, reference).is_some()
}

/// A column name of a `when`: kept when the board has it, mapped to the
/// column with that role, else `None`. `reasons` gets the mapping.
fn map_name(columns: &[BoardColumn], name: &str, reasons: &mut Vec<String>) -> Option<String> {
    let c = column_named_or_role(columns, name)?;
    if c.name != name {
        reasons.push(format!(
            "the column `{name}` is `{}` on this board (role {name})",
            c.name
        ));
    }
    Some(c.name.clone())
}

/// `rule` with the columns of the board, or `None` when it needs no change.
/// The second value is true when the rule can no longer act and is off.
fn map_rule(rule: &Rule, columns: &[BoardColumn]) -> Option<(Rule, bool, Vec<String>)> {
    let mut out = rule.clone();
    let mut reasons: Vec<String> = Vec::new();
    let mut off = false;

    // The column names of a `when` on the move of a card.
    let mut when = Vec::new();
    for m in &rule.when {
        if !matches!(
            m.path.as_str(),
            "data.to" | "data.from" | "data.to_role" | "data.from_role"
        ) {
            when.push(m.clone());
            continue;
        }
        match (m.op, &m.value) {
            (MatchOp::Eq | MatchOp::Ne, Value::String(name)) => {
                match map_name(columns, name, &mut reasons) {
                    Some(n) => when.push(MatchRule {
                        value: Value::String(if m.path.ends_with("_role") {
                            name.clone()
                        } else {
                            n
                        }),
                        ..m.clone()
                    }),
                    None if m.op == MatchOp::Eq => {
                        reasons.push(format!(
                            "the board has no column `{name}`: `{} eq {name}` never holds",
                            m.path
                        ));
                        off = true;
                    }
                    None => reasons.push(format!(
                        "the board has no column `{name}`: `{} ne {name}` always holds, dropped",
                        m.path
                    )),
                }
            }
            (MatchOp::In, Value::Array(items)) => {
                let mut names: Vec<Value> = Vec::new();
                for v in items {
                    let Some(name) = v.as_str() else {
                        names.push(v.clone());
                        continue;
                    };
                    match map_name(columns, name, &mut reasons) {
                        Some(n) => {
                            let n = Value::String(if m.path.ends_with("_role") {
                                name.to_owned()
                            } else {
                                n
                            });
                            if !names.contains(&n) {
                                names.push(n);
                            }
                        }
                        None => reasons.push(format!(
                            "the board has no column `{name}`: dropped from `{}`",
                            m.path
                        )),
                    }
                }
                if names.is_empty() {
                    reasons.push(format!("no column of `{} one_of` is on the board", m.path));
                    off = true;
                } else {
                    when.push(MatchRule {
                        value: Value::Array(names),
                        ..m.clone()
                    });
                }
            }
            _ => when.push(m.clone()),
        }
    }
    out.when = when;

    // The columns of the conditions on the card.
    let mut conds = Vec::new();
    for c in &rule.if_card {
        let mut c2 = c.clone();
        c2.column.retain(|r| resolves(columns, r));
        c2.not_column.retain(|r| resolves(columns, r));
        for r in c.column.iter().chain(&c.not_column) {
            if !resolves(columns, r) {
                reasons.push(format!(
                    "the board has no column `{r}`: dropped from if_card"
                ));
            }
        }
        if !c.column.is_empty() && c2.column.is_empty() {
            reasons.push("no column of if_card.column is on the board".to_owned());
            off = true;
        }
        if c2 != CardCondition::default() {
            conds.push(c2);
        }
    }
    out.if_card = conds;

    // The column arguments of the verbs.
    let mut actions = Vec::new();
    for a in &rule.actions {
        // The delivery pack's merge group needs both its home and its
        // completion anchor. Omit the whole lifecycle on older boards,
        // not just group_open, leaving the existing delivery flow intact.
        if a.verb.starts_with("group.")
            && matches!(
                a.args.get("key").and_then(Value::as_str),
                Some("{{job.group_key}}" | "{{group.key}}")
            )
            && (!resolves(columns, "@review") || !resolves(columns, "@accepted"))
        {
            reasons
                .push("merge groups need review and accepted columns: group action dropped".into());
            continue;
        }
        let Some(verb) = VERBS.iter().find(|v| v.name == a.verb) else {
            actions.push(a.clone());
            continue;
        };
        let mut a2 = a.clone();
        let mut keep = true;
        for key in verb.column_args() {
            let Some(v) = a.args.get(key) else { continue };
            match v {
                Value::String(r) if !resolves(columns, r) => {
                    if a.verb == "card.move" && key == "to" {
                        // A comment has no `from`: it would reach the
                        // cards that the move skips, so a move with a
                        // `from` is dropped (TER-226).
                        let from = a.args.get("from").is_some_and(|f| match f {
                            Value::Array(items) => !items.is_empty(),
                            Value::Null => false,
                            _ => true,
                        });
                        match a.args.get("comment").and_then(Value::as_str) {
                            Some(_) if from => {
                                reasons.push(format!(
                                    "the board has no column `{r}`: the move has a `from`, which a comment cannot keep, so it is dropped"
                                ));
                                keep = false;
                            }
                            Some(text) if !text.is_empty() => {
                                let mut args = Map::new();
                                args.insert("text".to_owned(), Value::String(text.to_owned()));
                                a2 = Action {
                                    verb: "card.comment".to_owned(),
                                    args,
                                };
                                reasons.push(format!(
                                    "the board has no column `{r}`: the move becomes its comment"
                                ));
                            }
                            _ => {
                                reasons.push(format!(
                                    "the board has no column `{r}`: the move is dropped"
                                ));
                                keep = false;
                            }
                        }
                        break;
                    }
                    reasons.push(format!(
                        "the board has no column `{r}`: {} is dropped",
                        verb.kind
                    ));
                    keep = false;
                    break;
                }
                Value::Array(items) => {
                    let have: Vec<Value> = items
                        .iter()
                        .filter(|i| i.as_str().is_none_or(|r| resolves(columns, r)))
                        .cloned()
                        .collect();
                    if have.len() == items.len() {
                        continue;
                    }
                    for i in items {
                        if let Some(r) = i.as_str().filter(|r| !resolves(columns, r)) {
                            reasons.push(format!(
                                "the board has no column `{r}`: dropped from {}.{key}",
                                verb.kind
                            ));
                        }
                    }
                    if have.is_empty() {
                        reasons.push(format!(
                            "no column of {}.{key} is on the board: {} is dropped",
                            verb.kind, verb.kind
                        ));
                        keep = false;
                        break;
                    }
                    a2.args.insert(key.to_owned(), Value::Array(have));
                }
                _ => {}
            }
        }
        if keep {
            actions.push(a2);
        }
    }
    if !rule.actions.is_empty() && actions.is_empty() {
        reasons.push("no verb is left".to_owned());
        off = true;
    }
    out.actions = actions;

    if reasons.is_empty() {
        return None;
    }
    if off {
        let off_rule = Rule {
            name: rule.name.clone(),
            description: Some(format!("Off for this board: {}.", reasons.join("; "))),
            enabled: false,
            on: Vec::new(),
            when: Vec::new(),
            if_card: Vec::new(),
            if_job: Vec::new(),
            max_per_hour: None,
            actions: Vec::new(),
            line: rule.line,
        };
        return Some((off_rule, true, reasons));
    }
    let note = format!("Mapped to this board: {}.", reasons.join("; "));
    out.description = Some(match &rule.description {
        Some(d) => format!("{d} {note}"),
        None => note,
    });
    Some((out, false, reasons))
}

/// Map the pack to the board `columns`. `own_text` and `own` are the
/// project's file (empty when it has none). A pack rule that the file
/// replaces stays as the file has it (`kept`). Each other pack rule that
/// names a column the board lacks becomes an override at the end of the
/// file. The caller checks the new text ([`rules_file::parse`]) before it
/// stores it.
pub fn migrate(
    pack: &RuleSet,
    own_text: &str,
    own: &RuleSet,
    columns: &[BoardColumn],
) -> Migration {
    let mut changes = Vec::new();
    let mut overrides = Vec::new();
    for p in &pack.rules {
        if own.rules.iter().any(|r| r.name == p.name) {
            if map_rule(p, columns).is_some() {
                changes.push(Change {
                    rule: p.name.clone(),
                    change: "kept",
                    reasons: vec![format!(
                        "{} has its own rule `{}`; it stays",
                        rules_file::FILE_NAME,
                        p.name
                    )],
                });
            }
            continue;
        }
        if let Some((rule, off, reasons)) = map_rule(p, columns) {
            changes.push(Change {
                rule: p.name.clone(),
                change: if off { "disabled" } else { "mapped" },
                reasons,
            });
            overrides.push(rule);
        }
    }
    let text = if overrides.is_empty() {
        own_text.to_owned()
    } else {
        let mut t = own_text.trim_end().to_owned();
        if !t.is_empty() {
            t.push_str("\n\n");
        }
        t.push_str(MIGRATION_HEADER);
        for r in &overrides {
            t.push('\n');
            t.push_str(&to_wcl(r));
        }
        t
    };
    Migration {
        text,
        changes,
        overrides,
    }
}

/// `rule` (a migrated `agent-park` or `agent-unpark`) with the wait column
/// `merge_queue` (TER-222), or `None` when it has it. `mq` is the name of
/// the board's column for a `data.to` or `data.from`; a `_role` path takes
/// the role `merge_queue`.
fn with_merge_queue(rule: &Rule, mq: &str) -> Option<Rule> {
    let value = |path: &str| -> String {
        if path.ends_with("_role") {
            "merge_queue".to_owned()
        } else {
            mq.to_owned()
        }
    };
    let column_path = |p: &str| {
        matches!(
            p,
            "data.to" | "data.from" | "data.to_role" | "data.from_role"
        )
    };
    let mut out = rule.clone();
    let mut changed = false;
    for m in &mut out.when {
        if m.op != MatchOp::In || !column_path(&m.path) {
            continue;
        }
        let v = Value::String(value(&m.path));
        if let Value::Array(items) = &mut m.value
            && !items.contains(&v)
        {
            items.push(v);
            changed = true;
        }
    }
    if rule.name == "agent-unpark" {
        let to = |m: &MatchRule| matches!(m.path.as_str(), "data.to" | "data.to_role");
        let has = rule
            .when
            .iter()
            .any(|m| m.op == MatchOp::Ne && to(m) && m.value == Value::String(value(&m.path)));
        let last = rule
            .when
            .iter()
            .rposition(|m| m.op == MatchOp::Ne && to(m) && m.value != Value::from("done"));
        if let (false, Some(i)) = (has, last) {
            let path = rule.when[i].path.clone();
            out.when.insert(
                i + 1,
                MatchRule {
                    value: Value::String(value(&path)),
                    path,
                    op: MatchOp::Ne,
                },
            );
            changed = true;
        }
    }
    changed.then_some(out)
}

/// TER-243: add the wait column `merge_queue` (TER-222) to the
/// `agent-park` and `agent-unpark` overrides that an older [`migrate`]
/// wrote into `text` (the project's file, `own` its rules read without the
/// column check). Without it a move from review to the merge queue wakes
/// the bound agent with a return turn. Only an override after the
/// [`MIGRATION_HEADER`] whose block is still the migration's text is
/// changed: an edited rule stays. `None`: nothing to change, or the board
/// has no `merge_queue` column (by name, else by role).
pub fn add_merge_queue_to_wait_overrides(
    text: &str,
    own: &RuleSet,
    columns: &[BoardColumn],
) -> Option<String> {
    let header = text.find(MIGRATION_HEADER)?;
    let mq = column_named_or_role(columns, "merge_queue")?;
    let mut out = text.to_owned();
    let mut changed = false;
    for r in &own.rules {
        if !matches!(r.name.as_str(), "agent-park" | "agent-unpark")
            || !r.enabled
            || !r
                .description
                .as_deref()
                .is_some_and(|d| d.contains("Mapped to this board:"))
        {
            continue;
        }
        let Some(new) = with_merge_queue(r, &mq.name) else {
            continue;
        };
        let old = to_wcl(r);
        let Some(at) = out[header..].find(&old) else {
            continue;
        };
        let at = header + at;
        out.replace_range(at..at + old.len(), &to_wcl(&new));
        changed = true;
    }
    changed.then_some(out)
}

// ----------------------------------------------------------------- lint

/// A rule names a column that the board lacks: its runs fail.
pub const LINT_MISSING_COLUMN: &str = "missing_column";
/// A rule moves cards into a column that no rule moves them out of.
pub const LINT_STUCK_COLUMN: &str = "stuck_column";

/// One finding of [`lint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// [`LINT_MISSING_COLUMN`] or [`LINT_STUCK_COLUMN`].
    pub code: &'static str,
    /// The column (the reference as the rule writes it for a missing one,
    /// the board's name for a stuck one).
    pub column: String,
    /// The rules that name the missing column, or that move cards into the
    /// stuck one.
    pub rules: Vec<String>,
    pub message: String,
}

/// Can `rule` move a card that is in `col` (a board name) to another
/// column? Only by a `card_move` whose target is on the board and is not
/// `col`, whose `from` (when given) holds `col`, in a rule whose `if_card`
/// conditions do not rule out `col`.
fn leaves(rule: &Rule, col: &str, columns: &[BoardColumn]) -> bool {
    let named = |refs: &[String]| {
        refs.iter()
            .any(|r| rules_file::resolve_column(columns, r).is_some_and(|c| c.name == col))
    };
    let ruled_out = rule
        .if_card
        .iter()
        .any(|c| (!c.column.is_empty() && !named(&c.column)) || named(&c.not_column));
    if ruled_out {
        return false;
    }
    rule.actions
        .iter()
        .filter(|a| a.verb == "card.move")
        .any(|a| {
            let to = a
                .args
                .get("to")
                .and_then(Value::as_str)
                .and_then(|r| rules_file::resolve_column(columns, r));
            let Some(to) = to else { return false };
            if to.name == col {
                return false;
            }
            match a.args.get("from") {
                None | Some(Value::Null) => true,
                Some(v) => {
                    let refs: Vec<String> = match v {
                        Value::String(s) => vec![s.clone()],
                        Value::Array(items) => items
                            .iter()
                            .filter_map(|i| i.as_str().map(str::to_owned))
                            .collect(),
                        _ => Vec::new(),
                    };
                    named(&refs)
                }
            }
        })
}

/// The problems of the rules `set` (the rules that the engine runs, the
/// pack included) on a board with `columns`. Rules that are off count for
/// nothing.
///
/// - [`LINT_MISSING_COLUMN`]: a rule names a column that the board lacks,
///   so each of its runs fails.
/// - [`LINT_STUCK_COLUMN`]: a rule moves cards into a column (a
///   `card_move`, or the new card of a `job_fix`) and no rule moves a card
///   out of it. A person must move each such card on. The column `done`
///   and the column with the role `blocked` are left out: a card stops
///   there by design.
pub fn lint(set: &RuleSet, columns: &[BoardColumn]) -> Vec<Finding> {
    let live: Vec<&Rule> = set.rules.iter().filter(|r| r.enabled).collect();
    let mut out = Vec::new();

    let mut missing: Vec<(String, Vec<String>)> = Vec::new();
    for r in &live {
        for a in r.actions.iter().filter(|a| a.verb == "group.open") {
            let key = a.args.get("key");
            let has_close = live.iter().any(|r| {
                r.actions
                    .iter()
                    .any(|close| close.verb == "group.close" && close.args.get("key") == key)
            });
            if !has_close && !a.args.contains_key("close_after") {
                out.push(Finding {
                    code: "group_without_close",
                    column: a.args.get("column").and_then(Value::as_str).unwrap_or_default().into(),
                    rules: vec![r.name.clone()],
                    message: format!("rule `{}` opens a group without a matching group_close or close_after safeguard", r.name),
                });
            }
        }
    }
    for r in &live {
        for c in rules_file::column_refs(r) {
            if resolves(columns, &c) {
                continue;
            }
            match missing.iter_mut().find(|(m, _)| *m == c) {
                Some((_, rules)) => rules.push(r.name.clone()),
                None => missing.push((c, vec![r.name.clone()])),
            }
        }
    }
    for (column, rules) in missing {
        out.push(Finding {
            code: LINT_MISSING_COLUMN,
            message: format!(
                "the board has no column `{column}`: each run of {} fails",
                rules
                    .iter()
                    .map(|r| format!("`{r}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            column,
            rules,
        });
    }

    let mut entered: Vec<(String, Vec<String>)> = Vec::new();
    for r in &live {
        for a in &r.actions {
            let target = match a.verb.as_str() {
                "card.move" => a.args.get("to").and_then(Value::as_str),
                "job.fix" => Some(a.args.get("to").and_then(Value::as_str).unwrap_or("@ready")),
                _ => None,
            };
            let Some(col) = target.and_then(|t| rules_file::resolve_column(columns, t)) else {
                continue;
            };
            if col.name == "done" || col.role.as_deref() == Some("blocked") {
                continue;
            }
            match entered.iter_mut().find(|(c, _)| *c == col.name) {
                Some((_, rules)) => {
                    if !rules.contains(&r.name) {
                        rules.push(r.name.clone());
                    }
                }
                None => entered.push((col.name.clone(), vec![r.name.clone()])),
            }
        }
    }
    for (column, rules) in entered {
        if live.iter().any(|r| leaves(r, &column, columns)) {
            continue;
        }
        out.push(Finding {
            code: LINT_STUCK_COLUMN,
            message: format!(
                "{} move cards into `{column}`, but no rule moves a card out of it: a person must move each one on",
                rules
                    .iter()
                    .map(|r| format!("`{r}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            column,
            rules,
        });
    }
    out
}

// ------------------------------------------------- one rule on or off

/// The bytes of `text` from `from` that are code: not in a string and not
/// in a comment, with the brace depth before each (relative to `from`).
fn code_bytes(text: &str, from: usize) -> Vec<(usize, u8, i32)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = from;
    let mut depth = 0i32;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push((i, c, depth));
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The rule block that starts on `line` (1-based): the offset of the line,
/// the offset just after its `{`, and the offset just after its `}`.
fn block_at(text: &str, line: usize) -> Option<(usize, usize, usize)> {
    let start = if line <= 1 {
        0
    } else {
        text.match_indices('\n').nth(line - 2).map(|(i, _)| i + 1)?
    };
    let code = code_bytes(text, start);
    let open = code.iter().find(|(_, c, _)| *c == b'{')?.0;
    let close = code.iter().rev().find(|(_, c, d)| *c == b'}' && *d == 1)?.0;
    (close > open).then_some((start, open + 1, close + 1))
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The byte range of the value of a top-level `enabled = <bool>` field of
/// the block that opens at `open` (the offset after its `{`).
fn enabled_value(text: &str, open: usize, close: usize) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let code = code_bytes(text, open - 1);
    for &(i, _, depth) in &code {
        if depth != 1 || i >= close || !b[i..].starts_with(b"enabled") {
            continue;
        }
        if i > 0 && is_ident(b[i - 1]) {
            continue;
        }
        let mut j = i + "enabled".len();
        if j < b.len() && is_ident(b[j]) {
            continue;
        }
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        if b.get(j) != Some(&b'=') {
            continue;
        }
        j += 1;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        for word in ["true", "false"] {
            let end = j + word.len();
            if b[j..].starts_with(word.as_bytes()) && (end >= b.len() || !is_ident(b[end])) {
                return Some((j, end));
            }
        }
    }
    None
}

/// The text of a `rules.wcl` with the rule `name` on (`enabled`) or off.
/// `own` is the file as parsed from `text`; `pack` the built-in pack.
///
/// - A rule of the file: its `enabled` field is set, or added after the
///   `{` of the rule. A pack override in the short form (`enabled = false`
///   with no `on` and no verb) goes when it is switched on, so the pack
///   rule runs again.
/// - A pack rule that the file does not name: off appends the short form;
///   on changes nothing.
///
/// `Ok(None)`: the rule is already so. `Err`: no such rule, or a short
/// form of a rule that is not in the pack (it has nothing to run).
pub fn set_enabled(
    text: &str,
    own: &RuleSet,
    pack: &RuleSet,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, String> {
    let in_pack = pack.rules.iter().any(|r| r.name == name);
    let Some(rule) = own.rules.iter().find(|r| r.name == name) else {
        if !in_pack {
            return Err(format!(
                "no rule `{name}` in the project's rules or the pack"
            ));
        }
        if enabled {
            return Ok(None);
        }
        let mut t = text.trim_end().to_owned();
        if !t.is_empty() {
            t.push_str("\n\n");
        }
        t.push_str(&format!("rule {} {{ enabled = false }}\n", quote(name)));
        return Ok(Some(t));
    };
    if rule.enabled == enabled {
        return Ok(None);
    }
    let (start, open, close) = block_at(text, rule.line)
        .ok_or_else(|| format!("cannot find the block of the rule `{name}` in the text"))?;
    let short = rule.on.is_empty() && rule.actions.is_empty();
    if enabled && short {
        if !in_pack {
            return Err(format!(
                "the rule `{name}` has no `on` and no verb: edit the file to give it some"
            ));
        }
        let mut end = close;
        if text[end..].starts_with('\n') {
            end += 1;
        }
        let mut t = format!("{}{}", &text[..start], &text[end..]);
        if t.trim().is_empty() {
            t.clear();
        }
        return Ok(Some(t));
    }
    let word = if enabled { "true" } else { "false" };
    if let Some((a, b)) = enabled_value(text, open, close) {
        return Ok(Some(format!("{}{word}{}", &text[..a], &text[b..])));
    }
    // No field: the rule is on by default, so this switches it off.
    let insert = if text[open..].starts_with('\n') || text[open..].starts_with("\r\n") {
        format!("\n  enabled = {word}")
    } else {
        format!(" enabled = {word} ")
    };
    Ok(Some(format!("{}{insert}{}", &text[..open], &text[open..])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules_file::{Context, default_pack, parse};

    fn cols(list: &[(&str, Option<&str>)]) -> Vec<BoardColumn> {
        list.iter()
            .map(|(n, r)| BoardColumn {
                name: (*n).to_owned(),
                role: r.map(str::to_owned),
            })
            .collect()
    }

    /// The default board (store/board.rs): no validating, no accepted.
    fn default_board() -> Vec<BoardColumn> {
        cols(&[
            ("backlog", None),
            ("ready", None),
            ("in_progress", Some("working")),
            ("review", None),
            ("blocked", Some("blocked")),
            ("done", None),
        ])
    }

    fn ctx(columns: &[BoardColumn]) -> Context {
        Context {
            columns: Some(columns.to_vec()),
            max_rules: 50,
        }
    }

    fn no_lines(set: &RuleSet) -> Vec<Rule> {
        set.rules
            .iter()
            .cloned()
            .map(|mut r| {
                r.line = 0;
                r
            })
            .collect()
    }

    #[test]
    fn the_pack_reads_back_from_its_wcl() {
        let pack = default_pack();
        assert!(!pack.rules.is_empty());
        let text: String = pack.rules.iter().map(to_wcl).collect::<Vec<_>>().join("\n");
        let back = parse(&text, "rules.wcl", &Context::default()).expect("the WCL of the pack");
        assert_eq!(no_lines(&back), no_lines(pack));
    }

    #[test]
    fn quote_escapes_and_reads_back() {
        let r = Rule {
            name: "q".into(),
            description: Some("a \"quoted\" \\ text\nline two".into()),
            enabled: true,
            on: vec!["card".into()],
            when: vec![],
            if_card: vec![CardCondition {
                column: vec!["@review".into()],
                assigned: Some(false),
                ..Default::default()
            }],
            if_job: vec![],
            max_per_hour: Some(5),
            actions: vec![Action {
                verb: "card.comment".into(),
                args: [("text".to_owned(), Value::String("say \"hi\"".into()))]
                    .into_iter()
                    .collect(),
            }],
            line: 0,
        };
        let back = parse(&to_wcl(&r), "rules.wcl", &Context::default()).expect("parses");
        assert_eq!(no_lines(&back), vec![r]);
    }

    #[test]
    fn a_full_board_needs_no_migration() {
        let board = cols(&[
            ("backlog", None),
            ("ready", None),
            ("in_progress", Some("working")),
            ("validating", Some("validating")),
            ("review", Some("review")),
            ("merge_queue", None),
            ("accepted", Some("accepted")),
            ("blocked", Some("blocked")),
            ("done", None),
        ]);
        let m = migrate(default_pack(), "", &RuleSet::default(), &board);
        assert!(m.changes.is_empty(), "{:?}", m.changes);
        assert_eq!(m.text, "");
    }

    #[test]
    fn the_migration_maps_the_pack_to_a_board_without_validating_and_accepted() {
        let board = default_board();
        let m = migrate(default_pack(), "", &RuleSet::default(), &board);
        let by = |n: &str| m.changes.iter().find(|c| c.rule == n);
        // merge-pass: the move to @accepted becomes its comment.
        let mp = by("merge-pass").expect("merge-pass changes");
        assert_eq!(mp.change, "mapped");
        let o = m.overrides.iter().find(|r| r.name == "merge-pass").unwrap();
        assert!(o.actions.iter().all(|a| a.verb != "card.move"));
        assert!(o.actions.iter().any(|a| a.verb == "card.comment"
            && a.args["text"] == Value::String("{{job.end_comment}}".into())));
        // agent-park: only review is a wait column of this board.
        let park = m.overrides.iter().find(|r| r.name == "agent-park").unwrap();
        assert_eq!(park.when[1].value, serde_json::json!(["review"]));
        let unpark = m
            .overrides
            .iter()
            .find(|r| r.name == "agent-unpark")
            .unwrap();
        let nes: Vec<&Value> = unpark.when.iter().skip(2).map(|w| &w.value).collect();
        assert_eq!(nes, vec![&Value::from("review"), &Value::from("done")]);
        // validate-pass and the failures name columns this board has.
        assert!(by("validate-pass").is_none());
        assert!(by("job-fail-red").is_none());
        // The text checks against the board, and the effective set lints
        // clean of missing columns.
        let own = parse(&m.text, "rules.wcl", &ctx(&board)).expect("the migrated text checks");
        let set = rules_file::with_pack(default_pack(), &own);
        let found = lint(&set, &board);
        assert!(
            found.iter().all(|f| f.code != LINT_MISSING_COLUMN),
            "{found:?}"
        );
        // Without the migration, merge-pass names a missing column.
        let raw = lint(default_pack(), &board);
        assert!(
            raw.iter()
                .any(|f| f.code == LINT_MISSING_COLUMN && f.column == "@accepted")
        );
        // A second migration changes nothing.
        let again = migrate(default_pack(), &m.text, &own, &board);
        assert!(again.overrides.is_empty());
        assert_eq!(again.text, m.text);
    }

    #[test]
    fn the_migration_drops_the_entire_group_lifecycle_without_anchors() {
        let board = cols(&[("ready", None), ("review", Some("review")), ("done", None)]);
        let m = migrate(default_pack(), "", &RuleSet::default(), &board);
        let own = parse(&m.text, "rules.wcl", &ctx(&board)).expect("migrated rules");
        let set = rules_file::with_pack(default_pack(), &own);
        assert!(
            set.rules
                .iter()
                .all(|r| r.actions.iter().all(|a| !a.verb.starts_with("group.")))
        );
        let queued = set.rules.iter().find(|r| r.name == "merge-queued").unwrap();
        assert!(queued.actions.iter().any(|a| a.verb == "card.label"));
        assert!(queued.actions.iter().any(|a| a.verb == "card.comment"));
    }

    #[test]
    fn the_migration_maps_names_to_roles_and_keeps_own_rules() {
        let board = cols(&[
            ("ready", None),
            ("doing", Some("working")),
            ("code_review", Some("review")),
            ("shipped", Some("accepted")),
            ("done", None),
        ]);
        let own_text =
            "rule \"merge-pass\" {\n  on = [\"job.verdict\"]\n  card_comment { text = \"x\" }\n}\n";
        let own = parse(own_text, "rules.wcl", &ctx(&board)).unwrap();
        let m = migrate(default_pack(), own_text, &own, &board);
        assert!(m.text.starts_with(own_text.trim_end()));
        let park = m.overrides.iter().find(|r| r.name == "agent-park").unwrap();
        assert_eq!(
            park.when[1].value,
            serde_json::json!(["review", "accepted"])
        );
        let kept = m.changes.iter().find(|c| c.rule == "merge-pass");
        assert!(kept.is_none(), "merge-pass resolves @accepted by role here");
        parse(&m.text, "rules.wcl", &ctx(&board)).expect("checks");
    }

    #[test]
    fn a_move_with_from_to_a_missing_column_is_dropped_not_commented() {
        let pack = parse(
            "rule \"m\" {\n  on = [\"card\"]\n  card_move { to = \"@accepted\"  from = [\"review\"]  comment = \"moved\" }\n  card_label { add = [\"x\"] }\n}\nrule \"n\" {\n  on = [\"card\"]\n  card_move { to = \"@accepted\"  comment = \"moved\" }\n}\n",
            "rules.wcl",
            &Context::default(),
        )
        .unwrap();
        let board = default_board();
        let m = migrate(&pack, "", &RuleSet::default(), &board);
        let o = |n: &str| m.overrides.iter().find(|r| r.name == n).unwrap();
        let verbs: Vec<&str> = o("m").actions.iter().map(|a| a.verb.as_str()).collect();
        assert_eq!(verbs, vec!["card.label"]);
        let reasons = &m.changes.iter().find(|c| c.rule == "m").unwrap().reasons;
        assert!(reasons.iter().any(|r| r.contains("`from`")), "{reasons:?}");
        // Without `from`, the move still becomes its comment.
        let verbs: Vec<&str> = o("n").actions.iter().map(|a| a.verb.as_str()).collect();
        assert_eq!(verbs, vec!["card.comment"]);
        parse(&m.text, "rules.wcl", &ctx(&board)).expect("checks");
    }

    #[test]
    fn a_rule_that_cannot_act_is_turned_off() {
        let board = cols(&[("ready", None), ("review", None)]);
        let m = migrate(default_pack(), "", &RuleSet::default(), &board);
        let done = m
            .changes
            .iter()
            .find(|c| c.rule == "agent-end-done")
            .unwrap();
        assert_eq!(done.change, "disabled");
        let o = m
            .overrides
            .iter()
            .find(|r| r.name == "agent-end-done")
            .unwrap();
        assert!(!o.enabled && o.on.is_empty() && o.actions.is_empty());
        let own = parse(&m.text, "rules.wcl", &ctx(&board)).expect("checks");
        let set = rules_file::with_pack(default_pack(), &own);
        assert!(
            !set.rules
                .iter()
                .find(|r| r.name == "agent-end-done")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn lint_finds_a_column_that_no_rule_leaves() {
        let board = cols(&[
            ("ready", None),
            ("validating", Some("validating")),
            ("review", None),
            ("blocked", Some("blocked")),
            ("done", None),
        ]);
        let text = r#"rule "in" {
  on = ["job"]
  card_move { to = "@validating" }
}
rule "out-elsewhere" {
  on = ["job.verdict"]
  card_move { to = "review"  from = ["ready"] }
}
rule "to-done" {
  on = ["card"]
  card_move { to = "done" }
}
rule "to-blocked" {
  on = ["card"]
  if_card { column = ["ready"] }
  card_move { to = "blocked"  comment = "x" }
}
"#;
        let set = parse(text, "rules.wcl", &ctx(&board)).unwrap();
        let found = lint(&set, &board);
        let stuck: Vec<&str> = found
            .iter()
            .filter(|f| f.code == LINT_STUCK_COLUMN)
            .map(|f| f.column.as_str())
            .collect();
        // review: out-elsewhere enters it, and `to-done` leaves any column.
        assert_eq!(stuck, Vec::<&str>::new());
        // Without to-done nothing leaves validating (`in` leaves review,
        // `out-elsewhere` leaves only ready, `to-blocked` runs only in ready).
        let text2 = text.replace(
            "card_move { to = \"done\" }",
            "card_comment { text = \"x\" }",
        );
        let set = parse(&text2, "rules.wcl", &ctx(&board)).unwrap();
        let found = lint(&set, &board);
        let stuck: Vec<&str> = found
            .iter()
            .filter(|f| f.code == LINT_STUCK_COLUMN)
            .map(|f| f.column.as_str())
            .collect();
        assert_eq!(stuck, vec!["validating"]);
        let v = found.iter().find(|f| f.column == "validating").unwrap();
        assert_eq!(v.rules, vec!["in"]);
    }

    #[test]
    fn group_open_needs_a_matching_close_or_timeout() {
        let text = r#"rule "open" {
          on = ["job"]
          group_open { key = "batch:{{job.id}}" title = "Batch" column = "review" done_when = "done" }
        }"#;
        let set = parse(text, "rules.wcl", &Context::default()).unwrap();
        assert!(
            lint(&set, &[])
                .iter()
                .any(|f| f.code == "group_without_close")
        );
        let closed = format!(
            "{text}\nrule \"close\" {{ on = [\"job\"] group_close {{ key = \"batch:{{{{job.id}}}}\" }} }}"
        );
        let set = parse(&closed, "rules.wcl", &Context::default()).unwrap();
        assert!(
            !lint(&set, &[])
                .iter()
                .any(|f| f.code == "group_without_close")
        );
        let timeout = text.replace(
            "done_when = \"done\"",
            "done_when = \"done\" close_after = \"1h\"",
        );
        let set = parse(&timeout, "rules.wcl", &Context::default()).unwrap();
        assert!(
            !lint(&set, &[])
                .iter()
                .any(|f| f.code == "group_without_close")
        );
    }

    #[test]
    fn lint_of_the_pack_on_a_full_board_is_clean() {
        let board = cols(&[
            ("backlog", None),
            ("ready", None),
            ("in_progress", Some("working")),
            ("review", Some("review")),
            ("merge_queue", Some("merge_queue")),
            ("accepted", Some("accepted")),
            ("blocked", Some("blocked")),
            ("done", None),
        ]);
        let found = lint(default_pack(), &board);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn set_enabled_edits_the_text() {
        let text = "// head\nrule \"a\" {\n  on = [\"card\"]\n  card_comment { text = \"enabled = true\" }\n}\n\nrule \"b\" { enabled = true  on = [\"card\"]  card_comment { text = \"x\" } }\n";
        let pack = parse(
            "rule \"p\" {\n  on = [\"card\"]\n  card_comment { text = \"p\" }\n}\n",
            "rules.wcl",
            &Context::default(),
        )
        .unwrap();
        let own = parse(text, "rules.wcl", &Context::default()).unwrap();
        // a: no field yet, a line after the `{`.
        let t = set_enabled(text, &own, &pack, "a", false).unwrap().unwrap();
        assert!(t.contains("rule \"a\" {\n  enabled = false\n  on"), "{t}");
        assert!(t.contains("text = \"enabled = true\""));
        let s = parse(&t, "rules.wcl", &Context::default()).unwrap();
        assert!(!s.rules[0].enabled && s.rules[1].enabled);
        // a: on again, the field flips.
        let t2 = set_enabled(&t, &s, &pack, "a", true).unwrap().unwrap();
        assert!(parse(&t2, "rules.wcl", &Context::default()).unwrap().rules[0].enabled);
        // b: one line, the field flips.
        let t3 = set_enabled(text, &own, &pack, "b", false).unwrap().unwrap();
        assert!(t3.contains("rule \"b\" { enabled = false  on"), "{t3}");
        // Already so: no change.
        assert_eq!(set_enabled(text, &own, &pack, "a", true).unwrap(), None);
        // A pack rule: off appends the short form, on removes it.
        let t4 = set_enabled(text, &own, &pack, "p", false).unwrap().unwrap();
        assert!(t4.ends_with("rule \"p\" { enabled = false }\n"), "{t4}");
        let s4 = parse(&t4, "rules.wcl", &Context::default()).unwrap();
        let t5 = set_enabled(&t4, &s4, &pack, "p", true).unwrap().unwrap();
        assert_eq!(t5.trim_end(), text.trim_end());
        assert_eq!(set_enabled(text, &own, &pack, "p", true).unwrap(), None);
        // Unknown.
        assert!(set_enabled(text, &own, &pack, "zz", false).is_err());
    }

    #[test]
    fn set_enabled_on_a_migrated_short_form() {
        let board = cols(&[("ready", None), ("review", None)]);
        let m = migrate(default_pack(), "", &RuleSet::default(), &board);
        let own = parse(&m.text, "rules.wcl", &ctx(&board)).unwrap();
        let t = set_enabled(&m.text, &own, default_pack(), "agent-end-done", true)
            .unwrap()
            .unwrap();
        let own2 = parse(&t, "rules.wcl", &Context::default()).unwrap();
        assert!(own2.rules.iter().all(|r| r.name != "agent-end-done"));
        assert_eq!(own2.rules.len(), own.rules.len() - 1);
    }

    #[test]
    fn effective_names_the_origin() {
        let own_text = "rule \"agent-park\" { enabled = false }\nrule \"mine\" {\n  on = [\"card\"]\n  card_comment { text = \"x\" }\n}\n";
        let own = parse(own_text, "rules.wcl", &Context::default()).unwrap();
        let e = effective(Some(default_pack()), &own);
        let o = |n: &str| e.iter().find(|r| r.rule.name == n).map(|r| r.origin);
        assert_eq!(o("agent-park"), Some(ORIGIN_OVERRIDE));
        assert_eq!(o("merge-pass"), Some(ORIGIN_PACK));
        assert_eq!(o("mine"), Some(ORIGIN_OWN));
        assert_eq!(e.last().unwrap().rule.name, "mine");
        let code = effective(None, &own);
        assert_eq!(code.len(), 2);
        assert!(code.iter().all(|r| r.origin == ORIGIN_OWN));
    }

    /// The overrides that `migrate` wrote before TER-222, on a board with
    /// no `validating` column: no `merge_queue` in the wait columns.
    fn old_wait_overrides() -> String {
        format!(
            "rule \"mine\" {{\n  on = [\"card\"]\n  card_comment {{ text = \"x\" }}\n}}\n\n{MIGRATION_HEADER}\n\
rule \"agent-park\" {{\n  description = \"The card waits on others: its bound agent parks. Mapped to this board: the board has no column `validating`: dropped from `data.to`.\"\n  on = [\"card\"]\n  max_per_hour = 600\n  when \"data.change\" {{ eq = \"moved\" }}\n  when \"data.to\" {{ one_of = [\"review\", \"accepted\"] }}\n  agent_park {{ reason = \"card in {{{{card.column}}}}\" }}\n}}\n\n\
rule \"agent-unpark\" {{\n  description = \"The card came back out of a wait column. Mapped to this board: the board has no column `validating`: `data.to ne validating` always holds, dropped.\"\n  on = [\"card\"]\n  max_per_hour = 600\n  when \"data.change\" {{ eq = \"moved\" }}\n  when \"data.to\" {{ ne = \"review\" }}\n  when \"data.to\" {{ ne = \"accepted\" }}\n  when \"data.to\" {{ ne = \"done\" }}\n  agent_unpark {{}}\n}}\n"
        )
    }

    fn queue_board() -> Vec<BoardColumn> {
        cols(&[
            ("backlog", None),
            ("ready", None),
            ("in_progress", Some("working")),
            ("review", Some("review")),
            ("queue", Some("merge_queue")),
            ("accepted", Some("accepted")),
            ("blocked", Some("blocked")),
            ("done", None),
        ])
    }

    #[test]
    fn old_wait_overrides_get_the_merge_queue() {
        let board = queue_board();
        let text = old_wait_overrides();
        let own = parse(&text, "rules.wcl", &ctx(&board)).expect("the old text checks");
        // The literal is what `to_wcl` writes, so the blocks are found.
        for r in own.rules.iter().filter(|r| r.name.starts_with("agent-")) {
            assert!(text.contains(&to_wcl(r)), "{}", to_wcl(r));
        }
        let new = add_merge_queue_to_wait_overrides(&text, &own, &board).expect("a change");
        let set = parse(&new, "rules.wcl", &ctx(&board)).expect("the new text checks");
        let rule = |n: &str| set.rules.iter().find(|r| r.name == n).unwrap().clone();
        assert_eq!(
            rule("agent-park").when[1].value,
            serde_json::json!(["review", "accepted", "queue"])
        );
        let nes: Vec<Value> = rule("agent-unpark")
            .when
            .iter()
            .skip(1)
            .map(|w| w.value.clone())
            .collect();
        assert_eq!(
            nes,
            vec![
                Value::from("review"),
                Value::from("accepted"),
                Value::from("queue"),
                Value::from("done")
            ]
        );
        // The own rule and the descriptions stay; a second pass changes
        // nothing.
        assert!(new.starts_with("rule \"mine\" {"));
        assert_eq!(rule("agent-park").description, own.rules[1].description);
        assert_eq!(add_merge_queue_to_wait_overrides(&new, &set, &board), None);
    }

    #[test]
    fn role_overrides_get_the_merge_queue_role() {
        let board = queue_board();
        let text = old_wait_overrides()
            .replace("\"data.to\" { one_of", "\"data.to_role\" { one_of")
            .replace(
                "\"data.to\" { ne = \"review\"",
                "\"data.to_role\" { ne = \"review\"",
            )
            .replace(
                "\"data.to\" { ne = \"accepted\"",
                "\"data.to_role\" { ne = \"accepted\"",
            );
        let own = parse(&text, "rules.wcl", &ctx(&board)).unwrap();
        let new = add_merge_queue_to_wait_overrides(&text, &own, &board).expect("a change");
        assert!(
            new.contains(
                "when \"data.to_role\" { one_of = [\"review\", \"accepted\", \"merge_queue\"] }"
            ),
            "{new}"
        );
        assert!(new.contains("when \"data.to_role\" { ne = \"accepted\" }\n  when \"data.to_role\" { ne = \"merge_queue\" }\n  when \"data.to\" { ne = \"done\" }"), "{new}");
    }

    #[test]
    fn edited_or_unmigrated_wait_overrides_stay() {
        let board = queue_board();
        let text = old_wait_overrides();
        // A board without a merge queue column.
        let plain = cols(&[("review", Some("review")), ("accepted", Some("accepted"))]);
        let own = parse(&text, "rules.wcl", &ctx(&plain)).unwrap();
        assert_eq!(add_merge_queue_to_wait_overrides(&text, &own, &plain), None);
        // An override that the admin edited after the migration.
        let edited = text.replace("max_per_hour = 600\n  when \"data.change\" { eq = \"moved\" }\n  when \"data.to\" { one_of", "max_per_hour = 60\n  when \"data.change\" { eq = \"moved\" }\n  when \"data.to\" { one_of");
        let own = parse(&edited, "rules.wcl", &ctx(&board)).unwrap();
        let new = add_merge_queue_to_wait_overrides(&edited, &own, &board).expect("unpark changes");
        assert!(
            new.contains("one_of = [\"review\", \"accepted\"] }"),
            "{new}"
        );
        assert!(new.contains("ne = \"queue\""), "{new}");
        // The same rules without the migration header are the project's own.
        let own_only = text.replace(MIGRATION_HEADER, "");
        let own = parse(&own_only, "rules.wcl", &ctx(&board)).unwrap();
        assert_eq!(
            add_merge_queue_to_wait_overrides(&own_only, &own, &board),
            None
        );
    }
}
