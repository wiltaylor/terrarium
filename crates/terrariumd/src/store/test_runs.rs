//! Test runs (PLAN §15.5, docs/api.md §29): uploaded test result files,
//! parsed into suites and cases ([`crate::test_results`]), linked to cards
//! and versions.
//!
//! - Members of the project and its agents upload and read runs. Another
//!   project's agent needs sysadmin.
//! - A link to a card follows the card's edit rights (its assignee or
//!   creator, or an admin; not archived; a done card only for an admin). A
//!   link to a version follows the version's edit rights (its creator or
//!   an admin, in draft or after a rejection). Unlinking needs the same.
//! - The creator, a project admin, sysadmin, or the operator deletes a
//!   run; a delete by another actor is audited.
//! - Known secret values are redacted in the file before the parse, so
//!   the cases and the stored original file never hold them.
//! - Events `test_run` {change, project, id, name, counts, cards,
//!   versions}. Nothing moves a card because of a result.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;
use crate::store::memory::name_of;
use crate::store::{Paged, board, projects, versions};
use crate::test_results::{self, Counts, Format, ParseError, ParsedRun, Status, clean_line};

/// The default number of cases on a page of a run.
pub const CASES_DEFAULT: usize = 500;
/// The most cases on a page of a run.
pub const CASES_MAX: usize = 5000;
/// The most items in each list of a comparison.
pub const COMPARE_MAX: usize = 1000;

// ---- shapes ----

/// A version that a run links to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionRef {
    pub id: String,
    pub name: String,
}

/// A run without its cases: the list item and the answer of a write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestRunSummary {
    pub id: String,
    pub project: String,
    pub name: String,
    pub format: String,
    pub tool: Option<String>,
    /// `passed`, `failed`, or `empty`.
    pub status: String,
    pub counts: Counts,
    pub duration_ms: Option<i64>,
    pub started_at: Option<String>,
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub file_name: String,
    pub raw_size: i64,
    pub notes: Vec<String>,
    pub cards: Vec<String>,
    pub versions: Vec<VersionRef>,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
    /// The dashboard page of the run.
    pub url: String,
}

/// A suite of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuiteView {
    pub name: String,
    pub duration_ms: Option<i64>,
    pub counts: Counts,
}

/// A case of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseView {
    pub seq: i64,
    pub suite: String,
    pub name: String,
    pub classname: Option<String>,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub status: String,
    pub duration_ms: Option<i64>,
    pub message: Option<String>,
    pub trace: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

/// `GET /api/test-runs/{id}`: the run, its suites, and a page of cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestRunDetail {
    #[serde(flatten)]
    pub run: TestRunSummary,
    pub suites: Vec<SuiteView>,
    pub cases: Vec<CaseView>,
    /// The number of cases that match the filter (all pages).
    pub cases_total: i64,
    pub next_cursor: Option<String>,
}

/// The latest run of a card, for the chip on the board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardTests {
    pub id: String,
    pub name: String,
    pub status: String,
    pub counts: Counts,
    pub created_at: String,
}

/// One case in a comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseDiff {
    pub suite: String,
    pub classname: Option<String>,
    pub name: String,
    /// The status in the run; `None` for a removed test.
    pub status: Option<String>,
    /// The status in the base run; `None` for a new test.
    pub base_status: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareCounts {
    pub newly_failing: i64,
    pub fixed: i64,
    pub still_failing: i64,
    pub new_tests: i64,
    pub removed_tests: i64,
}

/// `GET /api/test-runs/{id}/compare`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compare {
    pub run: TestRunSummary,
    pub base: Option<TestRunSummary>,
    pub counts: CompareCounts,
    pub newly_failing: Vec<CaseDiff>,
    pub fixed: Vec<CaseDiff>,
    pub still_failing: Vec<CaseDiff>,
    pub new_tests: Vec<CaseDiff>,
    pub removed_tests: Vec<CaseDiff>,
}

// ---- the upload ----

/// The fields of an upload besides the file.
#[derive(Debug, Clone, Default)]
pub struct NewRun {
    pub name: Option<String>,
    pub file_name: Option<String>,
    pub cards: Vec<String>,
    pub versions: Vec<String>,
    pub commit: Option<String>,
    pub branch: Option<String>,
}

/// A parsed upload, ready to store: made outside the database lock.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub parsed: ParsedRun,
    pub raw_gz: Vec<u8>,
    pub raw_size: usize,
}

pub fn parse_error(e: ParseError) -> ApiError {
    match e {
        ParseError::TooLarge { size, max } => ApiError::new(
            Code::TooLarge,
            format!("the file is {size} bytes; the limit is {max} bytes"),
        )
        .with_details(json!({"size": size, "max_bytes": max})),
        ParseError::Invalid(m) => ApiError::invalid(m),
    }
}

/// Redact the known secret values in the file, parse it, and gzip it.
/// CPU work: run it off the async threads.
pub fn prepare(
    bytes: &[u8],
    format: Option<Format>,
    redactor: &terrarium_core::redact::Redactor,
    view: &terrarium_core::redact::View,
) -> ApiResult<Prepared> {
    if bytes.len() > test_results::MAX_BYTES {
        return Err(parse_error(ParseError::TooLarge {
            size: bytes.len(),
            max: test_results::MAX_BYTES,
        }));
    }
    let data = redactor.redact_bytes_as(bytes, view);
    let parsed = test_results::parse(&data, format).map_err(parse_error)?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&data)
        .and_then(|_| gz.finish())
        .map(|raw_gz| Prepared {
            parsed,
            raw_gz,
            raw_size: data.len(),
        })
        .map_err(|e| ApiError::internal(format!("cannot compress the file: {e}")))
}

// ---- rows ----

struct RunRow {
    pk: i64,
    id: String,
    project: String,
    name: String,
    format: String,
    tool: Option<String>,
    counts: Counts,
    duration_ms: Option<i64>,
    started_at: Option<String>,
    commit: Option<String>,
    branch: Option<String>,
    file_name: String,
    raw_size: i64,
    notes: Vec<String>,
    created_by: String,
    created_at: String,
}

const RUN_COLS: &str = "pk, id, project, name, format, tool, passed, failed, errored, skipped, \
     total, duration_ms, started_at, commit_sha, branch, file_name, raw_size, notes, created_by, \
     created_at";

fn run_row(r: &Row<'_>) -> rusqlite::Result<RunRow> {
    let notes: String = r.get("notes")?;
    Ok(RunRow {
        pk: r.get("pk")?,
        id: r.get("id")?,
        project: r.get("project")?,
        name: r.get("name")?,
        format: r.get("format")?,
        tool: r.get("tool")?,
        counts: Counts {
            passed: r.get("passed")?,
            failed: r.get("failed")?,
            errored: r.get("errored")?,
            skipped: r.get("skipped")?,
            total: r.get("total")?,
        },
        duration_ms: r.get("duration_ms")?,
        started_at: r.get("started_at")?,
        commit: r.get("commit_sha")?,
        branch: r.get("branch")?,
        file_name: r.get("file_name")?,
        raw_size: r.get("raw_size")?,
        notes: serde_json::from_str(&notes).unwrap_or_default(),
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
    })
}

fn by_id(conn: &Connection, id: &str) -> ApiResult<Option<RunRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {RUN_COLS} FROM test_runs WHERE id = ?1"),
            [id.trim()],
            run_row,
        )
        .optional()?)
}

/// The run with this ID that the actor may read.
fn find_for(conn: &Connection, actor: &Actor, id: &str) -> ApiResult<RunRow> {
    let row = by_id(conn, id)?.ok_or_else(|| ApiError::not_found(format!("no test run `{id}`")))?;
    policy::read_project(actor, &row.project)?;
    Ok(row)
}

pub fn page_path(project: &str, id: &str) -> String {
    format!("/p/{project}/board/tests/{id}")
}

fn render(conn: &Connection, row: RunRow) -> ApiResult<TestRunSummary> {
    let mut stmt =
        conn.prepare("SELECT card FROM test_run_cards WHERE run = ?1 ORDER BY created_at, card")?;
    let cards = stmt
        .query_map([row.pk], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT v.id, v.name FROM test_run_versions l JOIN versions v ON v.id = l.version
         WHERE l.run = ?1 ORDER BY l.created_at, v.name",
    )?;
    let versions = stmt
        .query_map([row.pk], |r| {
            Ok(VersionRef {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TestRunSummary {
        status: row.counts.status().to_owned(),
        created_by_name: name_of(conn, &row.created_by)?,
        url: page_path(&row.project, &row.id),
        id: row.id,
        project: row.project,
        name: row.name,
        format: row.format,
        tool: row.tool,
        counts: row.counts,
        duration_ms: row.duration_ms,
        started_at: row.started_at,
        commit: row.commit,
        branch: row.branch,
        file_name: row.file_name,
        raw_size: row.raw_size,
        notes: row.notes,
        cards,
        versions,
        created_by: row.created_by,
        created_at: row.created_at,
    })
}

// ---- checks ----

/// One line of 1 to `max` characters.
fn check_line(what: &str, v: &str, max: usize) -> ApiResult<String> {
    let s = clean_line(v);
    if s.is_empty() || s.chars().count() > max {
        return Err(ApiError::invalid(format!(
            "bad {what} `{v}`: use one line of 1 to {max} characters"
        )));
    }
    Ok(s)
}

fn opt_line(what: &str, v: Option<&str>, max: usize) -> ApiResult<Option<String>> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => check_line(what, s, max).map(Some),
        None => Ok(None),
    }
}

/// The run name: given, else from the file, else the tool, else the file
/// name without its extension.
fn run_name(n: &NewRun, p: &ParsedRun, file_name: &str) -> ApiResult<String> {
    if let Some(name) = n.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return check_line("test run name", name, 200);
    }
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(s, _)| s)
        .to_owned();
    let pick = p
        .suggested_name
        .clone()
        .or_else(|| p.tool.clone())
        .unwrap_or(stem);
    let pick: String = clean_line(&pick).chars().take(200).collect();
    Ok(if pick.is_empty() {
        "tests".into()
    } else {
        pick
    })
}

fn default_file_name(f: Format) -> &'static str {
    match f {
        Format::Junit => "junit.xml",
        Format::Trx => "results.trx",
        Format::Ctrf => "ctrf.json",
        Format::LibtestJson => "libtest.json",
        Format::VitestJson => "vitest.json",
        Format::JestJson => "jest.json",
        Format::Tap => "results.tap",
    }
}

fn file_name_of(n: &NewRun, f: Format) -> ApiResult<String> {
    let given = n
        .file_name
        .as_deref()
        .map(|s| s.rsplit(['/', '\\']).next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty() && *s != "." && *s != "..");
    match given {
        Some(s) => check_line("file name", s, 255),
        None => Ok(default_file_name(f).to_owned()),
    }
}

// ---- events ----

fn emit(t: &mut Tx<'_>, s: &TestRunSummary, change: &str) -> ApiResult<()> {
    let c = &s.counts;
    let text = format!(
        "test run {} {change}: {} passed, {} failed, {} errored, {} skipped",
        s.name, c.passed, c.failed, c.errored, c.skipped
    );
    t.emit(
        EventDraft::new(
            "test_run",
            text,
            json!({
                "change": change,
                "project": s.project,
                "id": s.id,
                "name": s.name,
                "counts": s.counts,
                "cards": s.cards,
                "versions": s.versions.iter().map(|v| v.name.clone()).collect::<Vec<_>>(),
            }),
        )
        .project(s.project.clone()),
    )?;
    Ok(())
}

// ---- writes ----

fn link_card(t: &mut Tx<'_>, actor: &Actor, run: &RunRow, card: &str) -> ApiResult<()> {
    let id = board::check_test_link(t, actor, card, &run.project)?;
    t.execute(
        "INSERT OR IGNORE INTO test_run_cards (run, card, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![run.pk, id, actor.address(), now()],
    )?;
    Ok(())
}

fn unlink_card(t: &mut Tx<'_>, actor: &Actor, run: &RunRow, card: &str) -> ApiResult<()> {
    let id = board::check_test_link(t, actor, card, &run.project)?;
    t.execute(
        "DELETE FROM test_run_cards WHERE run = ?1 AND card = ?2",
        params![run.pk, id],
    )?;
    Ok(())
}

fn link_version(t: &mut Tx<'_>, actor: &Actor, run: &RunRow, key: &str) -> ApiResult<()> {
    let v = versions::check_test_link(t, actor, key, &run.project)?;
    t.execute(
        "INSERT OR IGNORE INTO test_run_versions (run, version, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![run.pk, v, actor.address(), now()],
    )?;
    Ok(())
}

fn unlink_version(t: &mut Tx<'_>, actor: &Actor, run: &RunRow, key: &str) -> ApiResult<()> {
    let v = versions::check_test_link(t, actor, key, &run.project)?;
    t.execute(
        "DELETE FROM test_run_versions WHERE run = ?1 AND version = ?2",
        params![run.pk, v],
    )?;
    Ok(())
}

/// Store a parsed upload. A link that the actor may not make stores
/// nothing.
pub fn create(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    n: &NewRun,
    prep: Prepared,
) -> ApiResult<TestRunSummary> {
    projects::get(t, project)?;
    policy::write_test_runs(actor, project)?;
    let p = prep.parsed;
    let file_name = file_name_of(n, p.format)?;
    let name = run_name(n, &p, &file_name)?;
    let commit = opt_line("commit", n.commit.as_deref(), 200)?;
    let branch = opt_line("branch", n.branch.as_deref(), 200)?;
    let counts = p.counts();
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO test_runs (id, project, name, format, tool, passed, failed, errored, skipped,
                                total, duration_ms, started_at, commit_sha, branch, file_name,
                                raw_size, raw_gz, notes, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20)",
        params![
            id,
            project,
            name,
            p.format.as_str(),
            p.tool,
            counts.passed,
            counts.failed,
            counts.errored,
            counts.skipped,
            counts.total,
            p.duration_ms,
            p.started_at,
            commit,
            branch,
            file_name,
            prep.raw_size as i64,
            prep.raw_gz,
            serde_json::to_string(&p.notes)?,
            actor.address(),
            now()
        ],
    )?;
    let pk = t.last_insert_rowid();
    {
        let mut suite_stmt = t.prepare(
            "INSERT INTO test_suites (run, idx, name, duration_ms, passed, failed, errored,
                                      skipped, total)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut case_stmt = t.prepare(
            "INSERT INTO test_cases (run, seq, suite, name, classname, file, line, status,
                                     duration_ms, message, trace, stdout, stderr)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )?;
        let mut seq = 0i64;
        for (i, s) in p.suites.iter().enumerate() {
            let c = s.counts();
            suite_stmt.execute(params![
                pk,
                i as i64,
                s.name,
                s.duration_ms,
                c.passed,
                c.failed,
                c.errored,
                c.skipped,
                c.total
            ])?;
            for k in &s.cases {
                case_stmt.execute(params![
                    pk,
                    seq,
                    i as i64,
                    k.name,
                    k.classname,
                    k.file,
                    k.line,
                    k.status.as_str(),
                    k.duration_ms,
                    k.message,
                    k.trace,
                    k.stdout,
                    k.stderr
                ])?;
                seq += 1;
            }
        }
    }
    let row = by_id(t, &id)?.ok_or_else(|| ApiError::internal("the new test run is gone"))?;
    for c in &n.cards {
        link_card(t, actor, &row, c)?;
    }
    for v in &n.versions {
        link_version(t, actor, &row, v)?;
    }
    let s = render(t, row)?;
    emit(t, &s, "created")?;
    Ok(s)
}

/// `PATCH /api/test-runs/{id}/links`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinksPatch {
    #[serde(default)]
    pub add_cards: Vec<String>,
    #[serde(default)]
    pub remove_cards: Vec<String>,
    #[serde(default)]
    pub add_versions: Vec<String>,
    #[serde(default)]
    pub remove_versions: Vec<String>,
}

pub fn update_links(
    t: &mut Tx<'_>,
    actor: &Actor,
    id: &str,
    p: &LinksPatch,
) -> ApiResult<TestRunSummary> {
    let row = find_for(t, actor, id)?;
    policy::write_test_runs(actor, &row.project)?;
    let total =
        p.add_cards.len() + p.remove_cards.len() + p.add_versions.len() + p.remove_versions.len();
    if total == 0 {
        return Err(ApiError::invalid(
            "give add_cards, remove_cards, add_versions, or remove_versions",
        ));
    }
    for c in &p.add_cards {
        link_card(t, actor, &row, c)?;
    }
    for c in &p.remove_cards {
        unlink_card(t, actor, &row, c)?;
    }
    for v in &p.add_versions {
        link_version(t, actor, &row, v)?;
    }
    for v in &p.remove_versions {
        unlink_version(t, actor, &row, v)?;
    }
    let s = render(t, row)?;
    let change = if p.add_cards.is_empty() && p.add_versions.is_empty() {
        "unlinked"
    } else {
        "linked"
    };
    emit(t, &s, change)?;
    Ok(s)
}

pub fn delete(t: &mut Tx<'_>, actor: &Actor, id: &str) -> ApiResult<()> {
    let row = find_for(t, actor, id)?;
    policy::delete_test_run(actor, &row.project, &row.created_by)?;
    let s = render(
        t,
        by_id(t, &row.id)?.ok_or_else(|| ApiError::not_found("gone"))?,
    )?;
    t.execute("DELETE FROM test_runs WHERE pk = ?1", [row.pk])?;
    if actor.address() != row.created_by {
        t.audit(
            "test_run.delete",
            &format!("{}/{}", row.project, row.id),
            Some(&row.project),
            json!({"name": row.name, "counts": row.counts, "created_by": row.created_by}),
        )?;
    }
    emit(t, &s, "deleted")?;
    Ok(())
}

// ---- reads ----

/// `GET /api/projects/{p}/test-runs` filters.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub card: Option<String>,
    pub version: Option<String>,
    pub name: Option<String>,
    /// `passed`, `failed`, or `empty`.
    pub status: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// Runs of a project, newest first. Cursor: the internal order key.
pub fn list(
    conn: &Connection,
    actor: &Actor,
    project: &str,
    q: &ListQuery,
) -> ApiResult<Paged<TestRunSummary>> {
    projects::get(conn, project)?;
    policy::read_project(actor, project)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let mut sql = format!("SELECT {RUN_COLS} FROM test_runs r WHERE r.project = ?");
    let mut args: Vec<rusqlite::types::Value> = vec![project.to_owned().into()];
    if let Some(c) = q.card.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        sql.push_str(" AND r.pk IN (SELECT run FROM test_run_cards WHERE card = ?)");
        args.push(board::normalise_id(c).into());
    }
    if let Some(v) = q
        .version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let row = versions::find(conn, v, Some(project))?;
        if row.project != project {
            return Err(ApiError::not_found(format!(
                "project `{project}` has no version `{v}`"
            )));
        }
        sql.push_str(" AND r.pk IN (SELECT run FROM test_run_versions WHERE version = ?)");
        args.push(row.id.into());
    }
    if let Some(n) = q.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        sql.push_str(" AND r.name = ?");
        args.push(n.to_owned().into());
    }
    match q.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => {}
        Some("failed") => sql.push_str(" AND r.failed + r.errored > 0"),
        Some("passed") => sql.push_str(" AND r.failed + r.errored = 0 AND r.total > 0"),
        Some("empty") => sql.push_str(" AND r.total = 0"),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "bad status `{other}`: use passed, failed, or empty"
            )));
        }
    }
    if let Some(c) = q.cursor.as_deref().filter(|c| !c.is_empty()) {
        let before: i64 = c
            .parse()
            .map_err(|_| ApiError::bad_request(format!("bad cursor `{c}`")))?;
        sql.push_str(" AND r.pk < ?");
        args.push(before.into());
    }
    sql.push_str(" ORDER BY r.pk DESC LIMIT ?");
    args.push(((limit + 1) as i64).into());
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args), run_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut items = Vec::new();
    let mut keys = Vec::new();
    for r in rows {
        keys.push(r.pk);
        items.push(render(conn, r)?);
    }
    let more = items.len() > limit;
    items.truncate(limit);
    let next = if more {
        keys.get(limit - 1).map(|k| k.to_string())
    } else {
        None
    };
    Ok(Paged { items, next })
}

/// `GET /api/test-runs/{id}` filters.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseQuery {
    /// `failed` (failed or errored), `passed`, `errored`, or `skipped`.
    pub status: Option<String>,
    /// Text in the name, the classname, or the suite; any case.
    pub q: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

fn suites_of(conn: &Connection, pk: i64) -> ApiResult<Vec<SuiteView>> {
    let mut stmt = conn.prepare(
        "SELECT name, duration_ms, passed, failed, errored, skipped, total FROM test_suites
         WHERE run = ?1 ORDER BY idx",
    )?;
    let rows = stmt
        .query_map([pk], |r| {
            Ok(SuiteView {
                name: r.get(0)?,
                duration_ms: r.get(1)?,
                counts: Counts {
                    passed: r.get(2)?,
                    failed: r.get(3)?,
                    errored: r.get(4)?,
                    skipped: r.get(5)?,
                    total: r.get(6)?,
                },
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn case_view(r: &Row<'_>) -> rusqlite::Result<CaseView> {
    Ok(CaseView {
        seq: r.get("seq")?,
        suite: r.get("suite_name")?,
        name: r.get("name")?,
        classname: r.get("classname")?,
        file: r.get("file")?,
        line: r.get("line")?,
        status: r.get("status")?,
        duration_ms: r.get("duration_ms")?,
        message: r.get("message")?,
        trace: r.get("trace")?,
        stdout: r.get("stdout")?,
        stderr: r.get("stderr")?,
    })
}

/// `%`, `_`, and `\` as literals in a LIKE pattern with `ESCAPE '\'`.
fn like_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn get(conn: &Connection, actor: &Actor, id: &str, q: &CaseQuery) -> ApiResult<TestRunDetail> {
    let row = find_for(conn, actor, id)?;
    let pk = row.pk;
    let limit = q.limit.unwrap_or(CASES_DEFAULT).clamp(1, CASES_MAX);
    let mut filter = String::from(" WHERE c.run = ?");
    let mut args: Vec<rusqlite::types::Value> = vec![pk.into()];
    match q.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => {}
        Some("failed") => filter.push_str(" AND c.status IN ('failed', 'errored')"),
        Some(s) if Status::from_name(s).is_some() => {
            filter.push_str(" AND c.status = ?");
            args.push(s.to_owned().into());
        }
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "bad status `{other}`: use failed, passed, errored, or skipped"
            )));
        }
    }
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        filter.push_str(
            " AND (lower(c.name) LIKE ? ESCAPE '\\' OR lower(coalesce(c.classname, '')) LIKE ? \
             ESCAPE '\\' OR lower(s.name) LIKE ? ESCAPE '\\')",
        );
        let pat = format!("%{}%", like_escape(&text.to_lowercase()));
        for _ in 0..3 {
            args.push(pat.clone().into());
        }
    }
    let from = "FROM test_cases c JOIN test_suites s ON s.run = c.run AND s.idx = c.suite";
    let cases_total: i64 = conn.query_row(
        &format!("SELECT count(*) {from}{filter}"),
        params_from_iter(args.iter()),
        |r| r.get(0),
    )?;
    let mut page_filter = filter.clone();
    let mut page_args = args.clone();
    if let Some(c) = q.cursor.as_deref().filter(|c| !c.is_empty()) {
        let after: i64 = c
            .parse()
            .map_err(|_| ApiError::bad_request(format!("bad cursor `{c}`")))?;
        page_filter.push_str(" AND c.seq > ?");
        page_args.push(after.into());
    }
    page_args.push(((limit + 1) as i64).into());
    let mut stmt = conn.prepare(&format!(
        "SELECT c.*, s.name AS suite_name {from}{page_filter} ORDER BY c.seq LIMIT ?"
    ))?;
    let mut cases = stmt
        .query_map(params_from_iter(page_args), case_view)?
        .collect::<Result<Vec<_>, _>>()?;
    let more = cases.len() > limit;
    cases.truncate(limit);
    let next_cursor = if more {
        cases.last().map(|c| c.seq.to_string())
    } else {
        None
    };
    Ok(TestRunDetail {
        suites: suites_of(conn, pk)?,
        run: render(conn, row)?,
        cases,
        cases_total,
        next_cursor,
    })
}

/// The original file: its name, its media type, and its bytes.
pub fn raw(conn: &Connection, actor: &Actor, id: &str) -> ApiResult<(String, String, Vec<u8>)> {
    let row = find_for(conn, actor, id)?;
    let gz: Vec<u8> = conn.query_row(
        "SELECT raw_gz FROM test_runs WHERE pk = ?1",
        [row.pk],
        |r| r.get(0),
    )?;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice())
        .read_to_end(&mut out)
        .map_err(|e| ApiError::internal(format!("cannot read the stored file: {e}")))?;
    let ctype = Format::parse_name(&row.format)
        .ok()
        .flatten()
        .map_or("application/octet-stream", Format::content_type);
    Ok((row.file_name, ctype.to_owned(), out))
}

/// The latest run linked to a card, for the board chip.
pub fn card_chip(conn: &Connection, card: &str) -> ApiResult<Option<CardTests>> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {RUN_COLS} FROM test_runs WHERE pk = (
                   SELECT max(run) FROM test_run_cards WHERE card = ?1)"
            ),
            [card],
            run_row,
        )
        .optional()?;
    Ok(row.map(|r| CardTests {
        status: r.counts.status().to_owned(),
        id: r.id,
        name: r.name,
        counts: r.counts,
        created_at: r.created_at,
    }))
}

// ---- compare ----

/// The match key of a case: suite, classname, name, and the number of
/// cases before it with the same key (a test that runs twice).
type Key = (String, Option<String>, String, usize);

struct Entry {
    status: Status,
    message: Option<String>,
}

fn case_map(conn: &Connection, pk: i64) -> ApiResult<(Vec<Key>, HashMap<Key, Entry>)> {
    let mut stmt = conn.prepare(
        "SELECT s.name, c.classname, c.name, c.status, c.message FROM test_cases c
           JOIN test_suites s ON s.run = c.run AND s.idx = c.suite
         WHERE c.run = ?1 ORDER BY c.seq",
    )?;
    let rows = stmt
        .query_map([pk], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen: HashMap<(String, Option<String>, String), usize> = HashMap::new();
    let mut order = Vec::with_capacity(rows.len());
    let mut map = HashMap::with_capacity(rows.len());
    for (suite, class, name, status, message) in rows {
        let n = seen
            .entry((suite.clone(), class.clone(), name.clone()))
            .or_insert(0);
        let key = (suite, class, name, *n);
        *n += 1;
        order.push(key.clone());
        map.insert(
            key,
            Entry {
                status: Status::from_name(&status).unwrap_or(Status::Errored),
                message,
            },
        );
    }
    Ok((order, map))
}

fn diff(key: &Key, now: Option<&Entry>, base: Option<&Entry>) -> CaseDiff {
    CaseDiff {
        suite: key.0.clone(),
        classname: key.1.clone(),
        name: key.2.clone(),
        status: now.map(|e| e.status.as_str().to_owned()),
        base_status: base.map(|e| e.status.as_str().to_owned()),
        message: now.or(base).and_then(|e| e.message.clone()),
    }
}

/// Compare a run with another run of the project: `with` is a run ID, or
/// `previous` (the default): the newest older run with the same name.
///
/// - newly failing: failed or errored now, passed or skipped in the base;
/// - fixed: failed or errored in the base, passed now;
/// - still failing: failed or errored in both;
/// - new tests: only in the run; removed tests: only in the base.
pub fn compare(
    conn: &Connection,
    actor: &Actor,
    id: &str,
    with: Option<&str>,
) -> ApiResult<Compare> {
    let row = find_for(conn, actor, id)?;
    let base = match with
        .map(str::trim)
        .filter(|w| !w.is_empty() && *w != "previous")
    {
        Some(w) => {
            let b = find_for(conn, actor, w)?;
            if b.project != row.project {
                return Err(ApiError::invalid(
                    "compare a test run with a run of the same project",
                ));
            }
            Some(b)
        }
        None => conn
            .query_row(
                &format!(
                    "SELECT {RUN_COLS} FROM test_runs WHERE project = ?1 AND name = ?2 AND pk < ?3
                     ORDER BY pk DESC LIMIT 1"
                ),
                params![row.project, row.name, row.pk],
                run_row,
            )
            .optional()?,
    };
    let mut out = Compare {
        run: render(
            conn,
            by_id(conn, &row.id)?.ok_or_else(|| ApiError::not_found("gone"))?,
        )?,
        base: None,
        counts: CompareCounts::default(),
        newly_failing: Vec::new(),
        fixed: Vec::new(),
        still_failing: Vec::new(),
        new_tests: Vec::new(),
        removed_tests: Vec::new(),
    };
    let Some(base) = base else {
        return Ok(out);
    };
    let (order, now_map) = case_map(conn, row.pk)?;
    let (base_order, base_map) = case_map(conn, base.pk)?;
    let push = |list: &mut Vec<CaseDiff>, n: &mut i64, d: CaseDiff| {
        *n += 1;
        if list.len() < COMPARE_MAX {
            list.push(d);
        }
    };
    for key in &order {
        let now_e = now_map.get(key);
        let base_e = base_map.get(key);
        let (Some(n), b) = (now_e, base_e) else {
            continue;
        };
        match b {
            None => push(
                &mut out.new_tests,
                &mut out.counts.new_tests,
                diff(key, now_e, None),
            ),
            Some(b) => match (n.status.failing(), b.status.failing()) {
                (true, true) => push(
                    &mut out.still_failing,
                    &mut out.counts.still_failing,
                    diff(key, now_e, base_e),
                ),
                (true, false) => push(
                    &mut out.newly_failing,
                    &mut out.counts.newly_failing,
                    diff(key, now_e, base_e),
                ),
                (false, true) if n.status == Status::Passed => push(
                    &mut out.fixed,
                    &mut out.counts.fixed,
                    diff(key, now_e, base_e),
                ),
                _ => {}
            },
        }
    }
    let now_keys: HashSet<&Key> = order.iter().collect();
    for key in &base_order {
        if !now_keys.contains(key) {
            push(
                &mut out.removed_tests,
                &mut out.counts.removed_tests,
                diff(key, None, base_map.get(key)),
            );
        }
    }
    out.base = Some(render(conn, base)?);
    Ok(out)
}
