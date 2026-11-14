//! The conversion of every board to column rules at the upgrade, and the
//! standard board of a new project (TER-1117, kanban rework C5, D5;
//! docs/api.md §19.14).
//!
//! - [`plan`] reads what a board runs today: the built-in pack mapped to
//!   the board ([`rules_tools::migrate`]) with its `rules.wcl`, for a
//!   `code` board without the rules that the coded flow never ran
//!   ([`CODE_FLOW_SKIPS`]) and with the validate rules when the board has a
//!   validating column; and the two dispatcher settings. The rules go
//!   through [`board_convert::convert`]. It writes nothing: it is the dry
//!   run and the report.
//! - [`apply`] writes one plan in one transaction: `board_flow = columns`
//!   first (the column-only verbs need it, rev-1214b), then the rules of
//!   each column after the rules it has, the batch queue, and the row of
//!   `board_conversions` with what was there before. A `columns` board is
//!   left as it is, so a second run writes nothing.
//! - [`revert`] takes one project back from its row: the written rules and
//!   the queue go, and the flow is the old one again, from now.
//! - [`upgrade`] runs [`apply`] for every project at the load of the
//!   config, when the switch is on ([`crate::db::Tx::board_columns`]), and
//!   mails the report to the leads of each converted project.
//! - [`standard_board`] gives a new project its rules: a `rules` board with
//!   the merge queue rules, converted.
//!
//! The legacy settings and `rules.wcl` stay stored: C2b (TER-1172) removes
//! them with the pack and the coded flow.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use terrarium_core::board_convert::{self, BatchQueue, Column, Conversion, Dispatchers, Entry};
use terrarium_core::config::BoardFlow;
use terrarium_core::rules_file::{self, BoardColumn, Context, Rule, RuleSet};
use terrarium_core::rules_tools;

use crate::actor::Actor;
use crate::db::{Db, Tx};
use crate::error::{ApiError, ApiResult};
use crate::store::board::column_options::{
    self, ColumnQueue, OptionsPut, QueueRelease, get_options, put_options,
};
use crate::store::{mail, projects};

/// The pack rules that a `code` board did not run (docs/api.md §19.11):
/// the triage mails and the automatic cut. Those boards keep their
/// schedules for both.
pub const CODE_FLOW_SKIPS: &[&str] = &[
    "triage-new-backlog",
    "triage-moved-backlog",
    "triage-hourly",
    "version-cut",
];

/// What [`plan`] found for one project.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub project: String,
    /// The flow of the board now.
    pub board_flow: &'static str,
    pub card_dispatcher: Option<String>,
    pub review_dispatcher: Option<String>,
    /// The SHA-256 of the stored `rules.wcl`, when there is one.
    pub rules_hash: Option<String>,
    /// The new rules of each column, in board order.
    pub columns: Vec<PlanColumn>,
    pub batch: Option<BatchQueue>,
    pub report: Vec<Entry>,
    /// Problems of the read (a `rules.wcl` that does not parse): the
    /// conversion goes on without those rules.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanColumn {
    pub column: String,
    pub rules: Vec<terrarium_core::board_rules::ColumnRule>,
}

impl Plan {
    pub fn rule_count(&self) -> usize {
        self.columns.iter().map(|c| c.rules.len()).sum()
    }
}

/// The answer of [`apply`] and [`revert`].
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub project: String,
    /// `converted`, `skipped` (already `columns`), or `reverted`.
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The rules written or removed.
    pub rules: usize,
    /// The converted rules that a column had by name already: kept.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
}

/// The columns of `project` for the conversion, in board order.
fn columns(conn: &Connection, project: &str) -> ApiResult<Vec<Column>> {
    let mut st = conn.prepare(
        "SELECT name, role, wip_limit, queue_release FROM board_columns
          WHERE project = ?1 ORDER BY position",
    )?;
    let rows = st
        .query_map([project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(name, role, wip, release)| Column {
            name,
            role,
            queues: wip.is_some() || release.as_deref().is_some_and(|r| r != "wip"),
        })
        .collect())
}

fn parse(text: &str, file: &str) -> Result<RuleSet, String> {
    let ctx = Context {
        columns: None,
        max_rules: usize::MAX,
        columns_flow: false,
    };
    rules_file::parse(text, file, &ctx).map_err(|e| e.to_string())
}

/// The rules that a board with `flow` runs, with their origins: the pack
/// mapped to `board`, with `own`.
fn running(flow: BoardFlow, own_text: &str, own: &RuleSet, board: &[BoardColumn]) -> Vec<(Rule, &'static str)> {
    let pack = rules_file::default_pack();
    let mig = rules_tools::migrate(pack, own_text, own, board);
    let mut with = own.clone();
    with.rules.extend(mig.overrides);
    rules_tools::effective(Some(pack), &with)
        .into_iter()
        .filter(|e| {
            !(flow == BoardFlow::Code
                && e.origin != rules_tools::ORIGIN_OWN
                && CODE_FLOW_SKIPS.contains(&e.rule.name.as_str()))
        })
        .map(|e| (e.rule, e.origin))
        .collect()
}

fn has_validating(board: &[BoardColumn]) -> bool {
    board
        .iter()
        .any(|c| c.role.as_deref() == Some("validating") || c.name == "validating")
}

/// Convert the board of `project` as it is now: the plan, nothing written.
pub fn plan(conn: &Connection, project: &str) -> ApiResult<Plan> {
    let p = projects::get(conn, project)?;
    let stored = crate::store::rules::get(conn, project)?;
    let mut problems = Vec::new();
    let mut own = if stored.exists {
        match parse(&stored.text, rules_file::FILE_NAME) {
            Ok(set) => set,
            Err(e) => {
                problems.push(format!(
                    "{} does not parse; its rules are not converted: {}",
                    rules_file::FILE_NAME,
                    e.lines().next().unwrap_or_default()
                ));
                RuleSet::default()
            }
        }
    } else {
        RuleSet::default()
    };
    let own_text = if problems.is_empty() { stored.text.clone() } else { String::new() };
    let cols = columns(conn, project)?;
    let board: Vec<BoardColumn> = cols
        .iter()
        .map(|c| BoardColumn {
            name: c.name.clone(),
            role: c.role.clone(),
        })
        .collect();
    // The coded flow moves a card to validating when its validate job is
    // queued: on the column rules that is the opt-in validate rules.
    if p.board_flow == BoardFlow::Code && has_validating(&board) {
        let v = parse(rules_file::VALIDATE_RULES, "rules.validate.wcl").map_err(ApiError::internal)?;
        for r in v.rules {
            if !own.rules.iter().any(|o| o.name == r.name) {
                own.rules.push(r);
            }
        }
    }
    let rules = running(p.board_flow, &own_text, &own, &board);
    let d = Dispatchers {
        card: p.card_dispatcher.clone(),
        review: p.review_dispatcher.clone(),
    };
    Ok(from_conversion(
        &p,
        stored.hash,
        board_convert::convert(&rules, &cols, &d),
        problems,
    ))
}

fn from_conversion(
    p: &projects::Project,
    rules_hash: Option<String>,
    c: Conversion,
    problems: Vec<String>,
) -> Plan {
    Plan {
        project: p.name.clone(),
        board_flow: p.board_flow.as_str(),
        card_dispatcher: p.card_dispatcher.clone(),
        review_dispatcher: p.review_dispatcher.clone(),
        rules_hash,
        columns: c
            .columns
            .into_iter()
            .map(|(column, rules)| PlanColumn { column, rules })
            .collect(),
        batch: c.batch,
        report: c.report,
        problems,
    }
}

/// Write `plan` (of a board that is not `columns`): the flow, the rules,
/// the batch queue. Returns the IDs of the rules written, the queue that
/// the batch column had, and the converted rules kept out by name.
fn write(t: &mut Tx<'_>, plan: &Plan) -> ApiResult<(Vec<String>, Option<Value>, Vec<String>)> {
    // The flow first: `review.start` is refused on another board.
    projects::set_columns_flow(t, &plan.project)?;
    let actor = Actor::Operator;
    let mut ids = Vec::new();
    let mut kept = Vec::new();
    let mut queue_before = None;
    for pc in &plan.columns {
        let before = get_options(t, &plan.project, &pc.column)?;
        let mut values: Vec<Value> = before
            .rules
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let mut new_names = Vec::new();
        for r in &pc.rules {
            if before.rules.iter().any(|b| b.name == r.name) {
                kept.push(format!("{}/{}", pc.column, r.name));
                continue;
            }
            new_names.push(r.name.clone());
            values.push(serde_json::to_value(r).map_err(|e| ApiError::internal(e.to_string()))?);
        }
        let mut queue = before.queue;
        if let Some(b) = plan.batch.as_ref().filter(|b| b.column == pc.column) {
            queue_before = Some(json!({"column": pc.column, "queue": before.queue}));
            queue = ColumnQueue {
                release: QueueRelease::Batch,
                batch_size: Some(b.batch_size),
                max_wait_seconds: Some(b.max_wait_seconds),
                max_batch_cards: Some(b.max_batch_cards),
            };
        }
        let after = put_options(
            t,
            &actor,
            &plan.project,
            &pc.column,
            &OptionsPut {
                base_version: before.version,
                checks: before.checks,
                queue,
                rules: Some(values),
                _can_edit: None,
            },
        )
        .map_err(|e| {
            let detail = e.details.as_ref().map(ToString::to_string).unwrap_or_default();
            ApiError::internal(format!(
                "column `{}`: {} {detail}",
                pc.column, e.message
            ))
        })?;
        ids.extend(
            after
                .rules
                .iter()
                .filter(|r| new_names.contains(&r.name))
                .filter_map(|r| r.id.clone()),
        );
    }
    Ok((ids, queue_before, kept))
}

/// Convert the board of `project` (D5). A `columns` board is skipped. One
/// transaction: a failure writes nothing, and the board stays as it was.
pub fn apply(t: &mut Tx<'_>, project: &str) -> ApiResult<Outcome> {
    let p = projects::get(t, project)?;
    if p.board_flow == BoardFlow::Columns {
        return Ok(Outcome {
            project: project.to_owned(),
            result: "skipped",
            reason: Some("the board runs on column rules already".into()),
            rules: 0,
            kept: Vec::new(),
            plan: None,
        });
    }
    let plan = plan(t, project)?;
    if plan.rule_count() > terrarium_core::board_rules::MAX_RULES_PER_PROJECT {
        return Err(ApiError::conflict(format!(
            "project `{project}`: the conversion gives {} column rules, more than the {} of a \
             project; the board is not converted",
            plan.rule_count(),
            terrarium_core::board_rules::MAX_RULES_PER_PROJECT
        )));
    }
    let (ids, queue_before, kept) = write(t, &plan)?;
    t.execute(
        "INSERT OR REPLACE INTO board_conversions (project, converted_at, previous_flow,
             previous_flow_at, card_dispatcher, review_dispatcher, rules_hash, rule_ids,
             queue_before, report)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            project,
            crate::db::now(),
            p.board_flow.as_str(),
            p.board_flow_at,
            p.card_dispatcher,
            p.review_dispatcher,
            plan.rules_hash,
            json!(ids).to_string(),
            queue_before.map(|q| q.to_string()),
            json!({"report": plan.report, "problems": plan.problems, "kept": kept,
                   "batch": plan.batch}).to_string()
        ],
    )?;
    t.audit(
        "board.convert",
        project,
        Some(project),
        json!({"previous_flow": p.board_flow.as_str(), "rules": ids.len(), "kept": kept,
               "batch": plan.batch, "card_dispatcher": p.card_dispatcher,
               "review_dispatcher": p.review_dispatcher,
               "not_converted": plan.report.iter().filter(|e| e.result == "not_converted")
                   .map(|e| e.rule.clone()).collect::<Vec<_>>()}),
    )?;
    Ok(Outcome {
        project: project.to_owned(),
        result: "converted",
        reason: None,
        rules: ids.len(),
        kept,
        plan: Some(plan),
    })
}

/// The stored conversion of `project`: `{converted_at, previous_flow,
/// card_dispatcher, review_dispatcher, rules_hash, rule_ids, report}`.
pub fn record(conn: &Connection, project: &str) -> ApiResult<Option<Value>> {
    Ok(conn
        .query_row(
            "SELECT converted_at, previous_flow, previous_flow_at, card_dispatcher,
                    review_dispatcher, rules_hash, rule_ids, queue_before, report
               FROM board_conversions WHERE project = ?1",
            [project],
            |r| {
                let js = |s: Option<String>| {
                    s.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                        .unwrap_or(Value::Null)
                };
                Ok(json!({
                    "project": project,
                    "converted_at": r.get::<_, String>(0)?,
                    "previous_flow": r.get::<_, Option<String>>(1)?,
                    "previous_flow_at": r.get::<_, Option<String>>(2)?,
                    "card_dispatcher": r.get::<_, Option<String>>(3)?,
                    "review_dispatcher": r.get::<_, Option<String>>(4)?,
                    "rules_hash": r.get::<_, Option<String>>(5)?,
                    "rule_ids": js(r.get(6)?),
                    "queue_before": js(r.get(7)?),
                    "report": js(r.get(8)?),
                }))
            },
        )
        .optional()?)
}

/// Take `project` back to the flow it had before its conversion: the
/// rules that the conversion wrote go (an edited rule too: it has the
/// same ID), the batch column gets its old queue, and the flow is the old
/// one from now (`board_flow_at`), so the old flow does not run the events
/// of the columns time. The dispatcher settings and `rules.wcl` were not
/// changed by the conversion. Run it while the queues are idle.
pub fn revert(t: &mut Tx<'_>, project: &str) -> ApiResult<Outcome> {
    let Some(rec) = record(t, project)? else {
        return Err(ApiError::not_found(format!(
            "project `{project}` has no board conversion to revert"
        )));
    };
    let ids: Vec<String> = rec["rule_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let queue_before = &rec["queue_before"];
    let actor = Actor::Operator;
    let mut removed = 0;
    let cols = columns(t, project)?;
    for c in &cols {
        let before = get_options(t, project, &c.name)?;
        let keep: Vec<&terrarium_core::board_rules::ColumnRule> = before
            .rules
            .iter()
            .filter(|r| !r.id.as_ref().is_some_and(|id| ids.contains(id)))
            .collect();
        let mut queue = before.queue;
        if queue_before["column"].as_str() == Some(c.name.as_str())
            && let Ok(q) = serde_json::from_value::<ColumnQueue>(queue_before["queue"].clone())
        {
            queue = q;
        }
        if keep.len() == before.rules.len() && queue == before.queue {
            continue;
        }
        removed += before.rules.len() - keep.len();
        let values: Vec<Value> = keep
            .iter()
            .map(|r| serde_json::to_value(r))
            .collect::<Result<_, _>>()
            .map_err(|e| ApiError::internal(e.to_string()))?;
        put_options(
            t,
            &actor,
            project,
            &c.name,
            &OptionsPut {
                base_version: before.version,
                checks: before.checks,
                queue,
                rules: Some(values),
                _can_edit: None,
            },
        )?;
    }
    let flow = BoardFlow::parse(rec["previous_flow"].as_str());
    let stored = (flow != BoardFlow::Code).then_some(flow.as_str());
    t.execute(
        "UPDATE projects SET board_flow = ?2, board_flow_at = ?3 WHERE name = ?1",
        params![project, stored, crate::db::now()],
    )?;
    t.execute("DELETE FROM board_conversions WHERE project = ?1", [project])?;
    t.audit(
        "board.convert_revert",
        project,
        Some(project),
        json!({"board_flow": flow.as_str(), "rules_removed": removed}),
    )?;
    Ok(Outcome {
        project: project.to_owned(),
        result: "reverted",
        reason: None,
        rules: removed,
        kept: Vec::new(),
        plan: None,
    })
}

/// The rules of a new project's standard board: the pack of a `rules`
/// board and the merge queue rules, converted to its columns, with no
/// dispatcher (the project has no lead yet). Called in the transaction
/// that makes the project, after its board.
pub fn standard_board(t: &mut Tx<'_>, project: &str) -> ApiResult<()> {
    let p = projects::get(t, project)?;
    let cols = columns(t, project)?;
    let board: Vec<BoardColumn> = cols
        .iter()
        .map(|c| BoardColumn {
            name: c.name.clone(),
            role: c.role.clone(),
        })
        .collect();
    let own = parse(rules_file::MERGE_QUEUE_RULES, "rules.merge_queue.wcl")
        .map_err(ApiError::internal)?;
    let rules = running(BoardFlow::Rules, rules_file::MERGE_QUEUE_RULES, &own, &board);
    let conv = board_convert::convert(&rules, &cols, &Dispatchers::default());
    let plan = from_conversion(&p, None, conv, Vec::new());
    let (ids, _, _) = write(t, &plan)?;
    t.audit(
        "board.standard",
        project,
        Some(project),
        json!({"rules": ids.len(), "batch": plan.batch}),
    )?;
    Ok(())
}

/// The report mail of one converted project, for its leads.
pub fn report_text(o: &Outcome) -> String {
    let Some(plan) = &o.plan else {
        return String::new();
    };
    let mut s = format!(
        "The board of project `{}` now runs on column rules (kanban rework, TER-1117). It ran \
         `board_flow = {}` before.\n\n{} rules were written to the columns. Each column's cog \
         shows its rules.\n\n| Rule | Result | Trigger | Columns | Note |\n|---|---|---|---|---|\n",
        o.project,
        plan.board_flow,
        o.rules
    );
    for e in &plan.report {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            e.rule,
            e.result,
            e.trigger.unwrap_or("-"),
            if e.columns.is_empty() { "-".to_owned() } else { e.columns.join(", ") },
            if e.note.is_empty() { "-" } else { e.note.as_str() }
        ));
    }
    if let Some(b) = &plan.batch {
        s.push_str(&format!(
            "\nThe column `{}` releases batches: {} cards, or after {} s, at most {}.\n",
            b.column, b.batch_size, b.max_wait_seconds, b.max_batch_cards
        ));
    }
    if !o.kept.is_empty() {
        s.push_str(&format!(
            "\nThese columns had a rule of the same name already; it stays and the converted one \
             was not written: {}.\n",
            o.kept.join(", ")
        ));
    }
    for p in &plan.problems {
        s.push_str(&format!("\nProblem: {p}\n"));
    }
    if plan.card_dispatcher.is_some() || plan.review_dispatcher.is_some() {
        s.push_str(
            "\nThe `card_dispatcher` and `review_dispatcher` settings have no effect any more: \
             their rules are on the ready and review columns.\n",
        );
    }
    s
}

/// Convert every project's board (the upgrade, D5). Each project in its
/// own write: a failure leaves that board as it was and is one line of
/// the load report. A converted project's leads get the report by mail.
pub async fn upgrade(db: &Db) -> Vec<String> {
    let names = match db
        .read(|c| {
            let mut st = c.prepare("SELECT name FROM projects ORDER BY name")?;
            let v = st
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
    {
        Ok(v) => v,
        Err(e) => return vec![format!("board conversion: read: {}", e.message)],
    };
    let mut errors = Vec::new();
    for name in names {
        let project = name.clone();
        let r = db
            .write("system", move |t| {
                if !t.board_columns {
                    return Ok(None);
                }
                let o = apply(t, &project)?;
                if o.result != "converted" {
                    return Ok(None);
                }
                let body = report_text(&o);
                let subject = format!("{project}: the board runs on column rules");
                for to in crate::owner::leads(t, &project, "")? {
                    mail::send(
                        t,
                        mail::Outgoing {
                            from: &mail::Address::System,
                            to: &to,
                            subject: &subject,
                            body: &body,
                            thread: None,
                            in_reply_to: None,
                        },
                    )?;
                }
                Ok(Some(o.rules))
            })
            .await;
        match r {
            Ok(Some(n)) => tracing::info!(project = %name, rules = n, "board converted to column rules"),
            Ok(None) => {}
            Err(e) => errors.push(format!(
                "board of project `{name}`: not converted: {}",
                e.message
            )),
        }
    }
    errors
}

/// The options of every column of `project`, for a test or a reader.
pub fn options_of(conn: &Connection, project: &str) -> ApiResult<Vec<column_options::ColumnOptions>> {
    columns(conn, project)?
        .iter()
        .map(|c| get_options(conn, project, &c.name))
        .collect()
}
