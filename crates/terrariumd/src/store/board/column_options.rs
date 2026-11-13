//! The options of a board column (TER-1112, kanban rework C1a): the move
//! checks that were fixed in the daemon, and the settings of the column's
//! queue. `GET` and `PUT /api/projects/{name}/board/columns/{column}/options`.
//!
//! The options also hold the structured rules of the column (TER-1120,
//! C1b): [`terrarium_core::board_rules`]. The engine runs them for a
//! project with `board_flow = columns` (TER-1113,
//! [`crate::rules_engine::columns`]).
//!
//! A column that a rule names by name (TER-1159, decided on TER-1113): a
//! rename of the column rewrites the name in every rule of the board
//! ([`rename_in_rules`]); a removal is refused while a rule of another
//! column names it ([`check_removal`]). A reference `@x` to a role stays.
//!
//! The options belong to the column ID: a rename keeps them, and a role
//! change does not move them. The queue settings are stored only; the
//! queue itself is C3 (TER-1112 lead decision Q4). Own-card permissions,
//! the GitHub hold, and the loop guards of the rules engine stay fixed.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::board_rules::{self, ColumnRule};
use terrarium_core::rules_file::{self, BoardColumn, Context};

use super::{BLOCKED, ColumnRow, DONE, WORKING, column, column_rows};
use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;

/// The most cards of a batch, and the longest batch wait (30 days).
pub const QUEUE_CARDS_MAX: i64 = 1000;
pub const QUEUE_WAIT_MAX_SECONDS: i64 = 30 * 24 * 3600;

/// The move checks of a column. Today's behaviour is the default
/// ([`ColumnChecks::default_for`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnChecks {
    /// A move into the column needs a comment (422 `comment_required`).
    pub require_comment_on_enter: bool,
    /// A move into the column needs every dependency accepted or done
    /// (409 `dependencies_unmet`).
    pub require_dependencies_met: bool,
    /// A card in the column is final for members: only a project admin or
    /// the operator moves, edits, or assigns it; nobody claims it or gives
    /// it a new ETA; only an admin makes a card in it.
    pub terminal_for_members: bool,
    /// A card that enters the column loses its ETA.
    pub clear_eta_on_enter: bool,
}

impl ColumnChecks {
    /// Today's checks of a new column: met dependencies for the `working`
    /// column, a comment for the `blocked` column, final for members in
    /// `done`, and the ETA cleared on every move.
    pub fn default_for(name: &str, role: Option<&str>) -> Self {
        Self {
            require_comment_on_enter: role == Some(BLOCKED),
            require_dependencies_met: role == Some(WORKING),
            terminal_for_members: name == DONE,
            clear_eta_on_enter: true,
        }
    }
}

/// When a card leaves the queue of a column (C3 implements it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueRelease {
    /// When the column has a free WIP place.
    Wip,
    /// In batches of `batch_size` cards, or after `max_wait_seconds`.
    Batch,
    /// One job at a time.
    Job,
}

impl QueueRelease {
    fn as_str(self) -> &'static str {
        match self {
            Self::Wip => "wip",
            Self::Batch => "batch",
            Self::Job => "job",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "wip" => Ok(Self::Wip),
            "batch" => Ok(Self::Batch),
            "job" => Ok(Self::Job),
            other => Err(rusqlite::Error::InvalidColumnType(
                0,
                format!("queue_release `{other}`"),
                rusqlite::types::Type::Text,
            )),
        }
    }
}

/// The queue settings of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnQueue {
    pub release: QueueRelease,
    /// `batch` only: the cards that release a batch.
    #[serde(default)]
    pub batch_size: Option<i64>,
    /// `batch` only: the seconds the oldest card waits before a smaller
    /// batch is released.
    #[serde(default)]
    pub max_wait_seconds: Option<i64>,
    /// `batch` only, optional: the most cards of one batch (at least
    /// `batch_size`).
    #[serde(default)]
    pub max_batch_cards: Option<i64>,
}

/// The options of one column, as the API returns them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnOptions {
    pub project: String,
    pub column: String,
    /// Goes up by one on each write. A `PUT` names it as `base_version`.
    pub version: i64,
    pub checks: ColumnChecks,
    pub queue: ColumnQueue,
    /// The rules of the column, in order, each with its `id` (TER-1120).
    pub rules: Vec<ColumnRule>,
}

/// The body of a `PUT`: the whole options, and the version that was read.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionsPut {
    pub base_version: i64,
    pub checks: ColumnChecks,
    pub queue: ColumnQueue,
    /// The whole rule list of the column, in order (TER-1120). A rule with
    /// an `id` replaces that rule; a rule without one is new; a rule that
    /// is not in the list is removed. Absent: the rules stay as they are.
    /// JSON values, so that each problem names its path
    /// ([`board_rules::read_rules`]).
    #[serde(default)]
    pub rules: Option<Vec<Value>>,
    /// The `can_edit` of a `GET` (TER-1116), so that a client may send back
    /// what it read. Ignored.
    #[serde(default, rename = "can_edit")]
    pub _can_edit: Option<bool>,
}

fn check_queue(q: &ColumnQueue) -> ApiResult<()> {
    let bad = |path: &str, msg: String| {
        Err(
            ApiError::invalid(format!("queue.{path}: {msg}")).with_details(json!({
                "path": format!("queue.{path}")
            })),
        )
    };
    let batch = [
        ("batch_size", q.batch_size),
        ("max_wait_seconds", q.max_wait_seconds),
        ("max_batch_cards", q.max_batch_cards),
    ];
    if q.release != QueueRelease::Batch {
        if let Some((name, _)) = batch.iter().find(|(_, v)| v.is_some()) {
            return bad(
                name,
                format!(
                    "only the release `batch` takes it; this queue releases by `{}`",
                    q.release.as_str()
                ),
            );
        }
        return Ok(());
    }
    for (name, value, max) in [
        ("batch_size", q.batch_size, QUEUE_CARDS_MAX),
        (
            "max_wait_seconds",
            q.max_wait_seconds,
            QUEUE_WAIT_MAX_SECONDS,
        ),
    ] {
        match value {
            None => return bad(name, "the release `batch` needs it".to_owned()),
            Some(v) if !(1..=max).contains(&v) => {
                return bad(name, format!("must be from 1 to {max}"));
            }
            Some(_) => {}
        }
    }
    if let Some(cap) = q.max_batch_cards {
        let size = q.batch_size.unwrap_or(1);
        if !(size..=QUEUE_CARDS_MAX).contains(&cap) {
            return bad(
                "max_batch_cards",
                format!("must be from batch_size ({size}) to {QUEUE_CARDS_MAX}"),
            );
        }
    }
    Ok(())
}

fn read(conn: &Connection, project: &str, col: &ColumnRow) -> ApiResult<ColumnOptions> {
    let (version, release, batch_size, max_wait_seconds, max_batch_cards) = conn
        .query_row(
            "SELECT options_version, queue_release, queue_batch_size, queue_max_wait_seconds,
                    queue_max_batch_cards
             FROM board_columns WHERE id = ?1",
            [col.id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    QueueRelease::parse(&r.get::<_, String>(1)?)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no column `{}`", col.name)))?;
    Ok(ColumnOptions {
        project: project.to_owned(),
        column: col.name.clone(),
        version,
        checks: col.checks,
        queue: ColumnQueue {
            release,
            batch_size,
            max_wait_seconds,
            max_batch_cards,
        },
        rules: read_rules(conn, col.id)?,
    })
}

/// The rules of the column `column_id`, in order.
fn read_rules(conn: &Connection, column_id: i64) -> ApiResult<Vec<ColumnRule>> {
    let mut q = conn.prepare(
        "SELECT id, name, definition_json FROM board_column_rules
         WHERE column_id = ?1 ORDER BY position",
    )?;
    let rows = q
        .query_map([column_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(id, name, def)| {
            let mut v: Value = serde_json::from_str(&def)
                .map_err(|e| ApiError::internal(format!("column rule {id}: {e}")))?;
            v["id"] = id.clone().into();
            v["name"] = name.into();
            serde_json::from_value(v)
                .map_err(|e| ApiError::internal(format!("column rule {id}: {e}")))
        })
        .collect()
}

/// A `422 invalid` for the problems of a rule list: the first one in the
/// message and `details.path`, all of them in `details.problems`.
fn rule_problems(problems: Vec<board_rules::Problem>) -> ApiError {
    let first = problems
        .first()
        .map(ToString::to_string)
        .unwrap_or_default();
    let more = match problems.len() {
        0 | 1 => String::new(),
        n => format!(" (and {} more)", n - 1),
    };
    ApiError::invalid(format!("{first}{more}")).with_details(json!({
        "path": problems.first().map(|p| p.path.clone()),
        "problems": problems,
    }))
}

/// Read and check the rule list of a `PUT` for the column `col`: the rule
/// model ([`board_rules::read_rules`], with the columns of the board), the
/// IDs (each one a rule of this column, once), and the cap of the project.
/// The rules come back with the IDs that they keep; a new rule has none.
fn check_rules(
    t: &Tx<'_>,
    project: &str,
    col: &ColumnRow,
    before: &[ColumnRule],
    values: &[Value],
) -> ApiResult<Vec<ColumnRule>> {
    let columns = column_rows(t, project)?
        .into_iter()
        .map(|c| BoardColumn {
            name: c.name,
            role: c.role,
        })
        .collect();
    let ctx = Context {
        columns: Some(columns),
        max_rules: board_rules::MAX_RULES_PER_COLUMN,
    };
    let rules = board_rules::read_rules(&ctx, values).map_err(rule_problems)?;
    let mut problems = Vec::new();
    for (i, r) in rules.iter().enumerate() {
        let Some(id) = &r.id else { continue };
        if !before.iter().any(|b| b.id.as_ref() == Some(id)) {
            problems.push(board_rules::Problem {
                path: format!("rules[{i}].id"),
                message: format!(
                    "column `{}` has no rule `{id}`; leave out the id for a new rule",
                    col.name
                ),
            });
        } else if rules[..i].iter().any(|o| o.id.as_ref() == Some(id)) {
            problems.push(board_rules::Problem {
                path: format!("rules[{i}].id"),
                message: format!("the rule `{id}` twice"),
            });
        }
    }
    if !problems.is_empty() {
        return Err(rule_problems(problems));
    }
    let others: i64 = t.query_row(
        "SELECT count(*) FROM board_column_rules r JOIN board_columns c ON c.id = r.column_id
         WHERE c.project = ?1 AND c.id != ?2",
        params![project, col.id],
        |r| r.get(0),
    )?;
    let total = others as usize + rules.len();
    if total > board_rules::MAX_RULES_PER_PROJECT {
        return Err(rule_problems(vec![board_rules::Problem {
            path: "rules".into(),
            message: format!(
                "the board of `{project}` would have {total} column rules; a project has at \
                 most {}",
                board_rules::MAX_RULES_PER_PROJECT
            ),
        }]));
    }
    Ok(rules)
}

/// Replace the rules of the column `column_id` with `rules`, in order. A
/// rule without an ID gets a new one. A rule that did not change keeps its
/// times.
fn write_rules(
    t: &Tx<'_>,
    column_id: i64,
    before: &[ColumnRule],
    rules: &[ColumnRule],
) -> ApiResult<()> {
    let times: HashMap<String, (String, String)> = {
        let mut q = t.prepare(
            "SELECT id, created_at, updated_at FROM board_column_rules WHERE column_id = ?1",
        )?;
        q.query_map([column_id], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<Result<_, _>>()?
    };
    t.execute(
        "DELETE FROM board_column_rules WHERE column_id = ?1",
        [column_id],
    )?;
    let at = now();
    for (position, r) in rules.iter().enumerate() {
        let id =
            r.id.clone()
                .unwrap_or_else(|| ulid::Ulid::new().to_string());
        let same = before.iter().any(|b| b == r);
        let (created, updated) = match times.get(&id) {
            Some((c, u)) if same => (c.clone(), u.clone()),
            Some((c, _)) => (c.clone(), at.clone()),
            None => (at.clone(), at.clone()),
        };
        let mut def = serde_json::to_value(r)
            .map_err(|e| ApiError::internal(format!("column rule {id}: {e}")))?;
        if let Some(o) = def.as_object_mut() {
            o.remove("id");
            o.remove("name");
        }
        t.execute(
            "INSERT INTO board_column_rules
               (id, column_id, name, position, model, definition_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                column_id,
                r.name,
                position as i64,
                board_rules::MODEL_VERSION,
                def.to_string(),
                created,
                updated
            ],
        )?;
    }
    Ok(())
}

/// The options of column `name` (a role alias names the column with that
/// role). The caller checks the read right of the project.
pub fn get_options(conn: &Connection, project: &str, name: &str) -> ApiResult<ColumnOptions> {
    let col = column(conn, project, name)?;
    read(conn, project, &col)
}

/// Replace the options of column `name`. Only the operator and a
/// `project_admin` of the project ([`policy::edit_column_options`]). A
/// `base_version` that is not the current version is 409 `stale_base`, with
/// nothing written. A bad rule list is `422 invalid` with the paths, and
/// nothing is written. A write that changes nothing writes nothing and
/// keeps the version. A change writes one audit row `board.column_options`
/// and one `board` event.
pub fn put_options(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    name: &str,
    put: &OptionsPut,
) -> ApiResult<ColumnOptions> {
    policy::read_project(actor, project)?;
    policy::edit_column_options(actor, project)?;
    let col = column(t, project, name)?;
    let before = read(t, project, &col)?;
    if put.base_version != before.version {
        return Err(ApiError::new(
            Code::StaleBase,
            format!(
                "the options of column `{}` changed: base_version {} is not the current version \
                 {}; read them again",
                col.name, put.base_version, before.version
            ),
        )
        .with_details(json!({"column": col.name, "version": before.version})));
    }
    check_queue(&put.queue)?;
    let rules = match &put.rules {
        Some(values) => Some(check_rules(t, project, &col, &before.rules, values)?),
        None => None,
    };
    let same_rules = rules.as_ref().is_none_or(|r| *r == before.rules);
    if before.checks == put.checks && before.queue == put.queue && same_rules {
        return Ok(before);
    }
    let (c, q) = (&put.checks, &put.queue);
    let n = t.execute(
        "UPDATE board_columns SET require_comment_on_enter = ?3, require_dependencies_met = ?4,
            terminal_for_members = ?5, clear_eta_on_enter = ?6, queue_release = ?7,
            queue_batch_size = ?8, queue_max_wait_seconds = ?9, queue_max_batch_cards = ?10,
            options_version = options_version + 1
         WHERE id = ?1 AND options_version = ?2",
        params![
            col.id,
            put.base_version,
            c.require_comment_on_enter,
            c.require_dependencies_met,
            c.terminal_for_members,
            c.clear_eta_on_enter,
            q.release.as_str(),
            q.batch_size,
            q.max_wait_seconds,
            q.max_batch_cards
        ],
    )?;
    if n != 1 {
        return Err(ApiError::internal(format!(
            "the options of column `{}` did not update",
            col.name
        )));
    }
    if let Some(rules) = rules.filter(|_| !same_rules) {
        write_rules(t, col.id, &before.rules, &rules)?;
    }
    let col = column(t, project, &col.name)?;
    let after = read(t, project, &col)?;
    record(t, project, &before, &after)?;
    Ok(after)
}

/// The audit row `board.column_options` and the `board` event of one
/// change of the options of a column.
fn record(
    t: &mut Tx<'_>,
    project: &str,
    before: &ColumnOptions,
    after: &ColumnOptions,
) -> ApiResult<()> {
    t.audit(
        "board.column_options",
        project,
        Some(project),
        json!({"column": after.column, "version": after.version,
               "before": {"checks": before.checks, "queue": before.queue, "rules": before.rules},
               "after": {"checks": after.checks, "queue": after.queue, "rules": after.rules}}),
    )?;
    t.emit(
        EventDraft::new(
            "board",
            format!("{project}: options of column `{}` changed", after.column),
            json!({"column": after.column, "options_version": after.version,
                   "checks": after.checks, "queue": after.queue,
                   "rule_count": after.rules.len()}),
        )
        .project(project),
    )?;
    Ok(())
}

/// The rules of column `col`, which the legacy board PATCH
/// (`super::set_columns`) removes (TER-1120). No rules: `None`. With rules,
/// the removal needs the options right ([`policy::edit_column_options`]):
/// the board PATCH right alone (a `sysadmin`) gets `403`, and the caller's
/// transaction writes nothing. Else the rules for the `board.columns` audit
/// row: `{column, before, after}`, with `after` empty. The rows go with the
/// column (`ON DELETE CASCADE`).
pub(super) fn drop_rules(
    t: &Tx<'_>,
    actor: &Actor,
    project: &str,
    col: &ColumnRow,
) -> ApiResult<Option<Value>> {
    let rules = read_rules(t, col.id)?;
    if rules.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
    if let Err(e) = policy::edit_column_options(actor, project) {
        return Err(ApiError::forbidden(format!(
            "this board change removes column `{}` and its {} rule(s): {}; {}",
            col.name,
            rules.len(),
            names.join(", "),
            e.message
        ))
        .with_details(json!({"column": col.name, "rules": names})));
    }
    Ok(Some(
        json!({"column": col.name, "before": rules, "after": []}),
    ))
}

/// The role checks of the legacy board PATCH (`super::set_columns`): set the
/// checks of column `old`, now named `name`, to `checks`. A change needs the
/// options right ([`policy::edit_column_options`]): the board PATCH right
/// alone (a `sysadmin`) gets `403`, and the caller's transaction writes
/// nothing. A change raises the version and is recorded as a `PUT` is.
pub(super) fn carry_checks(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    old: &ColumnRow,
    name: &str,
    checks: ColumnChecks,
) -> ApiResult<()> {
    if checks == old.checks {
        return Ok(());
    }
    let changed: Vec<&str> = [
        (
            "require_comment_on_enter",
            old.checks.require_comment_on_enter != checks.require_comment_on_enter,
        ),
        (
            "require_dependencies_met",
            old.checks.require_dependencies_met != checks.require_dependencies_met,
        ),
    ]
    .into_iter()
    .filter_map(|(k, c)| c.then_some(k))
    .collect();
    if let Err(e) = policy::edit_column_options(actor, project) {
        return Err(ApiError::forbidden(format!(
            "this board change gives column `{name}` a new role, and with it a new {}; {}",
            changed.join(" and "),
            e.message
        ))
        .with_details(json!({"column": name, "options": changed})));
    }
    let mut col = old.clone();
    col.name = name.to_owned();
    let before = read(t, project, &col)?;
    t.execute(
        "UPDATE board_columns SET require_comment_on_enter = ?2,
            require_dependencies_met = ?3, options_version = options_version + 1
         WHERE id = ?1",
        params![
            col.id,
            checks.require_comment_on_enter,
            checks.require_dependencies_met
        ],
    )?;
    col.checks = checks;
    let after = read(t, project, &col)?;
    record(t, project, &before, &after)
}

/// Every column rule of `project`: (rule ID, column ID, column name, rule).
fn project_rules(
    conn: &Connection,
    project: &str,
) -> ApiResult<Vec<(String, i64, String, ColumnRule)>> {
    let mut q = conn.prepare(
        "SELECT r.id, r.column_id, c.name, r.name, r.definition_json
           FROM board_column_rules r JOIN board_columns c ON c.id = r.column_id
          WHERE c.project = ?1 ORDER BY c.position, r.position",
    )?;
    let rows = q
        .query_map([project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(id, col_id, col, name, def)| {
            let mut v: Value = serde_json::from_str(&def)
                .map_err(|e| ApiError::internal(format!("column rule {id}: {e}")))?;
            v["id"] = id.clone().into();
            v["name"] = name.into();
            let rule: ColumnRule = serde_json::from_value(v)
                .map_err(|e| ApiError::internal(format!("column rule {id}: {e}")))?;
            Ok((id, col_id, col, rule))
        })
        .collect()
}

/// The legacy board PATCH (`super::set_columns`) removes the columns
/// `removed` (TER-1159): refused with `409` while a rule of a column that
/// stays names one of them. A reference `@x` where a column of the board has
/// the role `x` is a role, not a name: it does not hold the column. Nothing
/// is written.
pub(super) fn check_removal(
    t: &Tx<'_>,
    project: &str,
    current: &[ColumnRow],
    removed: &[&ColumnRow],
) -> ApiResult<()> {
    if removed.is_empty() {
        return Ok(());
    }
    let gone: Vec<&str> = removed.iter().map(|c| c.name.as_str()).collect();
    let gone_ids: Vec<i64> = removed.iter().map(|c| c.id).collect();
    let is_role = |x: &str| current.iter().any(|c| c.role.as_deref() == Some(x));
    let mut users: Vec<Value> = Vec::new();
    let mut named: Vec<&str> = Vec::new();
    for (_, col_id, col, rule) in project_rules(t, project)? {
        if gone_ids.contains(&col_id) {
            continue;
        }
        let legacy = crate::rules_engine::columns::engine_rule(&col, &rule);
        for r in rules_file::column_refs(&legacy) {
            let name = match r.strip_prefix('@') {
                Some(x) if is_role(x) => continue,
                Some(x) => x,
                None => r.as_str(),
            };
            if let Some(g) = gone.iter().find(|g| **g == name) {
                if !named.contains(g) {
                    named.push(*g);
                }
                users.push(json!({"column": col, "rule": rule.name, "names": g}));
            }
        }
    }
    if users.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = users
        .iter()
        .map(|u| {
            format!(
                "{}/{}",
                u["column"].as_str().unwrap_or_default(),
                u["rule"].as_str().unwrap_or_default()
            )
        })
        .collect();
    Err(ApiError::conflict(format!(
        "column `{}` is named by the column rule(s) {}; change those rules first",
        named.join("`, `"),
        list.join(", ")
    ))
    .with_details(json!({"reason": "column_in_use", "columns": named, "rules": users})))
}

/// The legacy board PATCH (`super::set_columns`) renamed columns
/// (`renames`: old name to new name, all at once) (TER-1159): rewrite the
/// names in every column rule of the board, in the same write. The roles
/// are those of the board after the change. Each column whose rules
/// changed gets a new options version. Returns the rules changed, for the
/// audit row of the PATCH: `{column, rule, before, after}`.
pub(super) fn rename_in_rules(
    t: &Tx<'_>,
    project: &str,
    renames: &HashMap<String, String>,
) -> ApiResult<Vec<Value>> {
    if renames.iter().all(|(a, b)| a == b) {
        return Ok(Vec::new());
    }
    let roles: Vec<String> = column_rows(t, project)?
        .into_iter()
        .filter_map(|c| c.role)
        .collect();
    let at = now();
    let mut out = Vec::new();
    let mut bumped: Vec<i64> = Vec::new();
    for (id, col_id, col, mut rule) in project_rules(t, project)? {
        let before = rule.clone();
        if !rules_file::rename_columns(&mut rule.if_card, &mut rule.actions, renames, &roles) {
            continue;
        }
        let mut def = serde_json::to_value(&rule)
            .map_err(|e| ApiError::internal(format!("column rule {id}: {e}")))?;
        if let Some(o) = def.as_object_mut() {
            o.remove("id");
            o.remove("name");
        }
        t.execute(
            "UPDATE board_column_rules SET definition_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, def.to_string(), at],
        )?;
        if !bumped.contains(&col_id) {
            t.execute(
                "UPDATE board_columns SET options_version = options_version + 1 WHERE id = ?1",
                [col_id],
            )?;
            bumped.push(col_id);
        }
        out.push(json!({"column": col, "rule": rule.name, "before": before, "after": rule}));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn queue(release: QueueRelease, b: Option<i64>, w: Option<i64>, m: Option<i64>) -> ColumnQueue {
        ColumnQueue {
            release,
            batch_size: b,
            max_wait_seconds: w,
            max_batch_cards: m,
        }
    }

    #[test]
    fn queue_settings_are_checked() {
        use QueueRelease::*;
        assert!(check_queue(&queue(Wip, None, None, None)).is_ok());
        assert!(check_queue(&queue(Job, None, None, None)).is_ok());
        assert!(check_queue(&queue(Batch, Some(4), Some(1800), Some(8))).is_ok());
        assert!(check_queue(&queue(Batch, Some(4), Some(1800), None)).is_ok());
        let path = |q| {
            check_queue(&q).unwrap_err().details.unwrap()["path"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(path(queue(Wip, Some(4), None, None)), "queue.batch_size");
        assert_eq!(
            path(queue(Job, None, None, Some(2))),
            "queue.max_batch_cards"
        );
        assert_eq!(path(queue(Batch, None, Some(60), None)), "queue.batch_size");
        assert_eq!(
            path(queue(Batch, Some(4), None, None)),
            "queue.max_wait_seconds"
        );
        assert_eq!(
            path(queue(Batch, Some(0), Some(60), None)),
            "queue.batch_size"
        );
        assert_eq!(
            path(queue(
                Batch,
                Some(4),
                Some(QUEUE_WAIT_MAX_SECONDS + 1),
                None
            )),
            "queue.max_wait_seconds"
        );
        assert_eq!(
            path(queue(Batch, Some(4), Some(60), Some(3))),
            "queue.max_batch_cards"
        );
    }

    #[test]
    fn defaults_are_todays_checks() {
        let d = ColumnChecks::default_for;
        assert!(d("in_progress", Some(WORKING)).require_dependencies_met);
        assert!(d("blocked", Some(BLOCKED)).require_comment_on_enter);
        assert!(d(DONE, None).terminal_for_members);
        let review = d("review", Some("review"));
        assert!(!review.require_comment_on_enter && !review.require_dependencies_met);
        assert!(!review.terminal_for_members && review.clear_eta_on_enter);
    }
}
