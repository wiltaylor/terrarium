//! The DB of a project (PLAN §15.4): record types with a schema, and
//! records of a type. Agents and the operator keep structured data here:
//! lists of items, inventories, results, decisions.
//!
//! - Every member of the project (its agents too) reads and writes types
//!   and records. Another project's agent needs sysadmin.
//! - Each record write is checked against the schema of its type
//!   (`schema::check_data`); a ref must point at a record of the target
//!   type; a unique field has no duplicate.
//! - A schema change that existing records fail is refused, with up to 20
//!   of the failing records. Removing a field needs `confirm`.
//! - Deleting a type that has records needs `?confirm=records` and
//!   project_admin. A record that other records refer to is not deleted.
//! - Events `db.type` and `db.record`; audit rows for type changes and
//!   deletes.

pub mod query;
pub mod schema;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use self::query::{Col, QueryError, TypeInfo, View};
use self::schema::{Field, Kind, Problems, check_data, check_fields, problem_text};
use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::policy;
use crate::store::memory::name_of;
use crate::store::{Page, Paged, projects};

/// A schema change lists at most this many failing records.
const MAX_FAILURES: usize = 20;
/// A refused record delete lists at most this many referrers.
const MAX_REFERRERS: usize = 20;

// ---- shapes ----

/// A record type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbType {
    pub id: String,
    pub project: String,
    pub name: String,
    pub description: String,
    pub fields: Vec<Field>,
    pub revision: i64,
    /// The number of records of the type.
    pub records: i64,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub project: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub data: Map<String, Value>,
    pub revision: i64,
    pub created_by: String,
    pub created_by_name: String,
    pub updated_by: String,
    pub updated_by_name: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A column of a query answer: a field, or a record property (`meta`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    #[serde(flatten)]
    pub field: Field,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub meta: bool,
}

/// One value of a `group:` answer and its count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub value: Value,
    pub count: i64,
}

/// The answer of `GET /api/projects/{p}/db/query`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    #[serde(rename = "type")]
    pub type_name: String,
    pub query: String,
    /// `table`, `list`, `count`, or `group`.
    pub view: String,
    /// The columns: `fields:`, or every field of the type.
    pub fields: Vec<Column>,
    /// The page of records (none for `count:true` and `group:`). `data`
    /// has only the columns.
    pub records: Vec<Record>,
    /// The number of records that match (all pages).
    pub count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// `group:`: the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<Group>>,
    /// A label for each record ID in a ref column (or a ref group): the
    /// first string field of the record.
    pub labels: BTreeMap<String, String>,
}

// ---- rows ----

struct TypeRow {
    id: String,
    project: String,
    name: String,
    description: String,
    fields: Vec<Field>,
    revision: i64,
    created_by: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SchemaCol {
    fields: Vec<Field>,
}

fn type_row(r: &Row<'_>) -> rusqlite::Result<TypeRow> {
    let schema: String = r.get("schema")?;
    let s: SchemaCol = serde_json::from_str(&schema).unwrap_or_default();
    Ok(TypeRow {
        id: r.get("id")?,
        project: r.get("project")?,
        name: r.get("name")?,
        description: r.get("description")?,
        fields: s.fields,
        revision: r.get("revision")?,
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

impl TypeRow {
    fn info(&self) -> TypeInfo {
        TypeInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            fields: self.fields.clone(),
        }
    }
}

fn type_by_name(conn: &Connection, project: &str, name: &str) -> ApiResult<Option<TypeRow>> {
    Ok(conn
        .query_row(
            "SELECT * FROM db_types WHERE project = ?1 AND name = ?2",
            params![project, name],
            type_row,
        )
        .optional()?)
}

fn type_by_id(conn: &Connection, id: &str) -> ApiResult<Option<TypeRow>> {
    Ok(conn
        .query_row("SELECT * FROM db_types WHERE id = ?1", [id], type_row)
        .optional()?)
}

fn types_of(conn: &Connection, project: &str) -> ApiResult<Vec<TypeRow>> {
    let mut stmt = conn.prepare("SELECT * FROM db_types WHERE project = ?1 ORDER BY name")?;
    let rows = stmt
        .query_map([project], type_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn need_type(conn: &Connection, project: &str, name: &str) -> ApiResult<TypeRow> {
    type_by_name(conn, project, name)?
        .ok_or_else(|| ApiError::not_found(format!("project `{project}` has no DB type `{name}`")))
}

fn record_count(conn: &Connection, type_id: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM db_records WHERE type = ?1",
        [type_id],
        |r| r.get(0),
    )?)
}

fn render_type(conn: &Connection, row: TypeRow) -> ApiResult<DbType> {
    Ok(DbType {
        records: record_count(conn, &row.id)?,
        created_by_name: name_of(conn, &row.created_by)?,
        id: row.id,
        project: row.project,
        name: row.name,
        description: row.description,
        fields: row.fields,
        revision: row.revision,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// A record row before the names: `(id, project, type id, data, ...)`.
struct RecordRow {
    id: String,
    project: String,
    type_id: String,
    data: Map<String, Value>,
    revision: i64,
    created_by: String,
    updated_by: String,
    created_at: String,
    updated_at: String,
}

fn record_row(r: &Row<'_>) -> rusqlite::Result<RecordRow> {
    let data: String = r.get("data")?;
    Ok(RecordRow {
        id: r.get("id")?,
        project: r.get("project")?,
        type_id: r.get("type")?,
        data: serde_json::from_str(&data).unwrap_or_default(),
        revision: r.get("revision")?,
        created_by: r.get("created_by")?,
        updated_by: r.get("updated_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn record_by_id(conn: &Connection, id: &str) -> ApiResult<Option<RecordRow>> {
    Ok(conn
        .query_row("SELECT * FROM db_records WHERE id = ?1", [id], record_row)
        .optional()?)
}

/// Names of addresses, looked up once each.
#[derive(Default)]
struct Names(HashMap<String, String>);

impl Names {
    fn of(&mut self, conn: &Connection, key: &str) -> ApiResult<String> {
        if let Some(n) = self.0.get(key) {
            return Ok(n.clone());
        }
        let n = name_of(conn, key)?;
        self.0.insert(key.to_owned(), n.clone());
        Ok(n)
    }
}

fn render_record(
    conn: &Connection,
    names: &mut Names,
    type_name: &str,
    r: RecordRow,
) -> ApiResult<Record> {
    Ok(Record {
        created_by_name: names.of(conn, &r.created_by)?,
        updated_by_name: names.of(conn, &r.updated_by)?,
        id: r.id,
        project: r.project,
        type_name: type_name.to_owned(),
        data: r.data,
        revision: r.revision,
        created_by: r.created_by,
        updated_by: r.updated_by,
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}

// ---- checks ----

fn invalid_fields(message: &str, p: &Problems) -> ApiError {
    ApiError::invalid(format!("{message}: {}", problem_text(p)))
        .with_details(json!({ "fields": p }))
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26 && s.parse::<ulid::Ulid>().is_ok()
}

/// The JSON value as a SQL value, for a compare with `json_extract`.
fn sql_value(v: &Value) -> SqlValue {
    match v {
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or_default()),
        },
        other => SqlValue::Text(other.to_string()),
    }
}

/// Check the ref fields of `data`, and turn a name into the record ID:
/// a value that is not a record ID finds the one record of the target
/// type whose first string field has it.
fn resolve_refs(
    conn: &Connection,
    project: &str,
    fields: &[Field],
    data: &mut Map<String, Value>,
    problems: &mut Problems,
) -> ApiResult<()> {
    for f in fields.iter().filter(|f| f.kind == Kind::Ref) {
        let Some(Value::String(v)) = data.get(&f.name).cloned() else {
            continue;
        };
        let target_name = f.target.clone().unwrap_or_default();
        let Some(target) = type_by_name(conn, project, &target_name)? else {
            problems.insert(
                f.name.clone(),
                format!("the target type `{target_name}` is gone"),
            );
            continue;
        };
        if is_ulid(&v) {
            let ok: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM db_records WHERE id = ?1 AND type = ?2)",
                params![v, target.id],
                |r| r.get(0),
            )?;
            if !ok {
                problems.insert(
                    f.name.clone(),
                    format!("no `{target_name}` record has the ID {v}"),
                );
            }
            continue;
        }
        let Some(label) = target.info().label_field().cloned() else {
            problems.insert(
                f.name.clone(),
                format!("give the ID of a `{target_name}` record (the type has no string field to find `{v}` by)"),
            );
            continue;
        };
        let mut stmt = conn.prepare(
            "SELECT id FROM db_records WHERE type = ?1 AND json_extract(data, ?2) = ?3 LIMIT 2",
        )?;
        let ids = stmt
            .query_map(params![target.id, format!("$.{}", label.name), v], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        match ids.as_slice() {
            [one] => {
                data.insert(f.name.clone(), Value::String(one.clone()));
            }
            [] => {
                problems.insert(
                    f.name.clone(),
                    format!("no `{target_name}` record has {} `{v}`", label.name),
                );
            }
            _ => {
                problems.insert(
                    f.name.clone(),
                    format!(
                        "more than one `{target_name}` record has {} `{v}`; give the record ID",
                        label.name
                    ),
                );
            }
        }
    }
    Ok(())
}

/// Check the unique fields of `data` against the other records of the type.
fn check_unique(
    conn: &Connection,
    type_id: &str,
    fields: &[Field],
    data: &Map<String, Value>,
    id: &str,
    problems: &mut Problems,
) -> ApiResult<()> {
    for f in fields.iter().filter(|f| f.unique) {
        let Some(v) = data.get(&f.name) else {
            continue;
        };
        let other: Option<String> = conn
            .query_row(
                "SELECT id FROM db_records WHERE type = ?1 AND json_extract(data, ?2) = ?3
                 AND id != ?4 LIMIT 1",
                params![type_id, format!("$.{}", f.name), sql_value(v), id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(o) = other {
            problems.insert(
                f.name.clone(),
                format!("must be unique; record {o} has this value"),
            );
        }
    }
    Ok(())
}

/// The full check of one record's data: the schema, the refs, and the
/// unique fields. Returns the stored form.
fn check_record(
    conn: &Connection,
    project: &str,
    t: &TypeRow,
    data: &Map<String, Value>,
    defaults: bool,
    id: &str,
) -> Result<Map<String, Value>, ApiResult<Problems>> {
    let mut out = match check_data(&t.fields, data, defaults) {
        Ok(d) => d,
        Err(mut p) => {
            // Name the bad refs too, so that a form shows every problem
            // at once.
            let mut refs: Map<String, Value> = data
                .iter()
                .filter(|(k, v)| v.is_string() && !p.contains_key(*k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if let Err(e) = resolve_refs(conn, project, &t.fields, &mut refs, &mut p) {
                return Err(Err(e));
            }
            return Err(Ok(p));
        }
    };
    let mut p = Problems::new();
    if let Err(e) = resolve_refs(conn, project, &t.fields, &mut out, &mut p) {
        return Err(Err(e));
    }
    if let Err(e) = check_unique(conn, &t.id, &t.fields, &out, id, &mut p) {
        return Err(Err(e));
    }
    if p.is_empty() { Ok(out) } else { Err(Ok(p)) }
}

/// Check the ref targets of a field list: each is a type of the project,
/// or the type itself (`self_name`).
fn check_targets(
    conn: &Connection,
    project: &str,
    self_name: &str,
    fields: &[Field],
) -> ApiResult<()> {
    let mut problems = Vec::new();
    for f in fields.iter().filter(|f| f.kind == Kind::Ref) {
        let t = f.target.as_deref().unwrap_or_default();
        if t != self_name && type_by_name(conn, project, t)?.is_none() {
            problems.push(format!(
                "field `{}`: project `{project}` has no type `{t}`",
                f.name
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(schema_invalid(problems))
    }
}

fn schema_invalid(problems: Vec<String>) -> ApiError {
    ApiError::invalid(format!("the schema is not valid: {}", problems.join("; ")))
        .with_details(json!({ "problems": problems }))
}

fn check_description(d: &str) -> ApiResult<()> {
    if d.chars().count() > schema::MAX_DESCRIPTION_CHARS {
        return Err(ApiError::invalid(format!(
            "the description has more than {} characters",
            schema::MAX_DESCRIPTION_CHARS
        )));
    }
    Ok(())
}

/// Redact the free text of a field list: the descriptions.
fn redact_fields(t: &Tx<'_>, fields: Vec<Field>) -> Vec<Field> {
    fields
        .into_iter()
        .map(|mut f| {
            f.description = f.description.map(|d| t.redact_string(d));
            f
        })
        .collect()
}

fn schema_text(fields: &[Field]) -> ApiResult<String> {
    Ok(serde_json::to_string(&json!({ "fields": fields }))?)
}

// ---- events ----

fn emit_type(t: &mut Tx<'_>, row: &TypeRow, change: &str, extra: Value) -> ApiResult<()> {
    let mut data = json!({
        "change": change,
        "project": row.project,
        "type": row.name,
        "id": row.id,
        "revision": row.revision,
    });
    if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
        d.extend(e.clone());
    }
    t.emit(
        EventDraft::new(
            "db.type",
            format!("DB type {} {change} (rev {})", row.name, row.revision),
            data,
        )
        .project(row.project.clone()),
    )?;
    Ok(())
}

fn emit_record(
    t: &mut Tx<'_>,
    project: &str,
    type_name: &str,
    id: &str,
    revision: i64,
    change: &str,
) -> ApiResult<()> {
    t.emit(
        EventDraft::new(
            "db.record",
            format!("DB {type_name} record {id} {change}"),
            json!({
                "change": change,
                "project": project,
                "type": type_name,
                "id": id,
                "revision": revision,
            }),
        )
        .project(project.to_owned()),
    )?;
    Ok(())
}

// ---- types ----

/// `?limit=&cursor=` on the type list: the cursor is the last name.
pub fn list_types(
    conn: &Connection,
    actor: &Actor,
    project: &str,
    page: &Page,
) -> ApiResult<Paged<DbType>> {
    projects::get(conn, project)?;
    policy::read_db(actor, project)?;
    let limit = page.limit();
    let mut stmt = conn.prepare(
        "SELECT * FROM db_types WHERE project = ?1 AND name > ?2 ORDER BY name LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![project, page.cursor().unwrap_or(""), (limit + 1) as i64],
            type_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let items = rows
        .into_iter()
        .map(|r| render_type(conn, r))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Paged::from_rows(items, limit, |t| t.name.clone()))
}

pub fn get_type(conn: &Connection, actor: &Actor, project: &str, name: &str) -> ApiResult<DbType> {
    projects::get(conn, project)?;
    policy::read_db(actor, project)?;
    let row = need_type(conn, project, name)?;
    render_type(conn, row)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewType {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub fields: Vec<Field>,
}

pub fn create_type(t: &mut Tx<'_>, actor: &Actor, project: &str, n: NewType) -> ApiResult<DbType> {
    projects::get(t, project)?;
    policy::write_db(actor, project)?;
    if !schema::valid_type_name(&n.name) {
        return Err(ApiError::invalid(format!(
            "bad type name `{}`: use 1-63 of a-z, 0-9, `_`, `-`, starting with a letter",
            n.name
        )));
    }
    check_description(&n.description)?;
    let problems = check_fields(&n.fields);
    if !problems.is_empty() {
        return Err(schema_invalid(problems));
    }
    if type_by_name(t, project, &n.name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "project `{project}` has a DB type `{}` already",
            n.name
        )));
    }
    check_targets(t, project, &n.name, &n.fields)?;
    let fields = redact_fields(t, n.fields);
    let description = t.redact_string(n.description);
    let id = ulid::Ulid::new().to_string();
    let at = now();
    t.execute(
        "INSERT INTO db_types (id, project, name, description, schema, revision, created_by,
                               created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)",
        params![
            id,
            project,
            n.name,
            description,
            schema_text(&fields)?,
            actor.address(),
            at
        ],
    )?;
    let row = type_by_id(t, &id)?.ok_or_else(|| ApiError::internal("type vanished"))?;
    let names: Vec<&str> = row.fields.iter().map(|f| f.name.as_str()).collect();
    t.audit(
        "db.type.create",
        &format!("{project}/{}", row.name),
        Some(project),
        json!({"type_id": row.id, "fields": names}),
    )?;
    emit_type(t, &row, "created", json!({}))?;
    render_type(t, row)
}

/// A field rename of a schema change.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rename {
    pub from: String,
    pub to: String,
}

/// `PATCH /api/projects/{p}/db/types/{name}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypePatch {
    /// The revision the change starts from. Another one is `409`.
    pub revision: i64,
    #[serde(default)]
    pub description: Option<String>,
    /// The new field list, after the renames. Absent: the fields stay
    /// (with the renames).
    #[serde(default)]
    pub fields: Option<Vec<Field>>,
    /// Renames, applied first: the data of each record moves.
    #[serde(default)]
    pub rename: Vec<Rename>,
    /// Needed when a field goes away: its data goes from every record.
    #[serde(default)]
    pub confirm: bool,
}

/// One record that a schema change would break.
#[derive(Debug, Clone, Serialize)]
struct Failure {
    id: String,
    field: String,
    message: String,
}

/// Change the description or the schema of a type. The records change
/// with it: renamed data moves, removed data goes, a new field with a
/// default fills them. Then every record must pass the new schema, else
/// nothing changes.
pub fn update_type(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    name: &str,
    p: TypePatch,
) -> ApiResult<DbType> {
    projects::get(t, project)?;
    policy::write_db(actor, project)?;
    let row = need_type(t, project, name)?;
    if p.revision != row.revision {
        return Err(ApiError::conflict(format!(
            "type `{name}` is at revision {}, not your revision {}; load it again and redo the change",
            row.revision, p.revision
        ))
        .with_details(json!({"current": row.revision, "revision": p.revision})));
    }
    if let Some(d) = &p.description {
        check_description(d)?;
    }
    // The renames, on a copy of the old field list.
    let mut old = row.fields.clone();
    let mut renamed: Vec<(String, String)> = Vec::new();
    for r in &p.rename {
        if !schema::valid_field_name(&r.to) {
            return Err(ApiError::invalid(format!(
                "cannot rename `{}` to `{}`: a field name is 1-63 of a-z, 0-9, `_`, starting with a letter",
                r.from, r.to
            )));
        }
        if old.iter().any(|f| f.name == r.to) {
            return Err(ApiError::invalid(format!(
                "cannot rename `{}` to `{}`: the type has a field `{}`",
                r.from, r.to, r.to
            )));
        }
        let Some(f) = old.iter_mut().find(|f| f.name == r.from) else {
            return Err(ApiError::invalid(format!(
                "cannot rename `{}`: type `{name}` has no such field",
                r.from
            )));
        };
        f.name.clone_from(&r.to);
        renamed.push((r.from.clone(), r.to.clone()));
    }
    let new = p.fields.clone().unwrap_or_else(|| old.clone());
    let problems = check_fields(&new);
    if !problems.is_empty() {
        return Err(schema_invalid(problems));
    }
    check_targets(t, project, name, &new)?;
    let removed: Vec<String> = old
        .iter()
        .filter(|f| !new.iter().any(|n| n.name == f.name))
        .map(|f| f.name.clone())
        .collect();
    if !removed.is_empty() && !p.confirm {
        return Err(ApiError::new(
            Code::ConfirmRequired,
            format!(
                "the change removes the field(s) {} and their data from every record; send confirm: true",
                removed.iter().map(|r| format!("`{r}`")).collect::<Vec<_>>().join(", ")
            ),
        )
        .with_details(json!({"removed": removed})));
    }
    let added: Vec<&Field> = new
        .iter()
        .filter(|f| !old.iter().any(|o| o.name == f.name))
        .collect();
    let changed: Vec<&Field> = new
        .iter()
        .filter(|f| old.iter().any(|o| o.name == f.name && o != *f))
        .collect();
    // The records, with the changes.
    let mut stmt = t.prepare("SELECT * FROM db_records WHERE type = ?1 ORDER BY id")?;
    let records = stmt
        .query_map([&row.id], record_row)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for f in &added {
        if f.required && f.default.is_none() && !records.is_empty() {
            return Err(ApiError::invalid(format!(
                "the new required field `{}` needs a default for the {} records of `{name}`",
                f.name,
                records.len()
            )));
        }
    }
    let mut failures: Vec<Failure> = Vec::new();
    let mut failing = 0usize;
    let mut writes: Vec<(String, Map<String, Value>)> = Vec::new();
    // Unique values in the new schema, to find duplicates among the records.
    let mut seen: HashMap<(String, String), String> = HashMap::new();
    for rec in &records {
        let mut data = rec.data.clone();
        for (from, to) in &renamed {
            if let Some(v) = data.remove(from) {
                data.insert(to.clone(), v);
            }
        }
        for r in &removed {
            data.remove(r);
        }
        // A new field, or a field that is required now, takes its default
        // where the record has no value.
        for f in added.iter().chain(changed.iter().filter(|f| f.required)) {
            if let Some(d) = &f.default
                && data.get(&f.name).is_none_or(Value::is_null)
            {
                data.insert(f.name.clone(), d.clone());
            }
        }
        let mut problems = match check_data(&new, &data, false) {
            Ok(checked) => {
                let mut p = Problems::new();
                let mut checked = checked;
                resolve_refs(t, project, &new, &mut checked, &mut p)?;
                for f in new.iter().filter(|f| f.unique) {
                    if let Some(v) = checked.get(&f.name) {
                        let key = (f.name.clone(), v.to_string());
                        if let Some(o) = seen.get(&key) {
                            p.insert(
                                f.name.clone(),
                                format!("must be unique; record {o} has this value"),
                            );
                        } else {
                            seen.insert(key, rec.id.clone());
                        }
                    }
                }
                if p.is_empty() && checked != rec.data {
                    writes.push((rec.id.clone(), checked));
                }
                p
            }
            Err(p) => p,
        };
        if !problems.is_empty() {
            failing += 1;
            if failures.len() < MAX_FAILURES {
                let (field, message) = problems.pop_first().unwrap_or_default();
                failures.push(Failure {
                    id: rec.id.clone(),
                    field,
                    message,
                });
            }
        }
    }
    if failing > 0 {
        let first = &failures[0];
        return Err(ApiError::invalid(format!(
            "{failing} records of `{name}` do not fit the new schema (for example {}: {}: {}); change or delete them first, or change the schema",
            first.id, first.field, first.message
        ))
        .with_details(json!({"records": failures, "failing": failing})));
    }
    let at = now();
    let me = actor.address();
    for (id, data) in &writes {
        t.execute(
            "UPDATE db_records SET data = ?2, revision = revision + 1, updated_by = ?3,
                                   updated_at = ?4
             WHERE id = ?1",
            params![id, serde_json::to_string(data)?, me, at],
        )?;
    }
    let fields = redact_fields(t, new.clone());
    let description = match p.description {
        Some(d) => t.redact_string(d),
        None => row.description.clone(),
    };
    t.execute(
        "UPDATE db_types SET description = ?2, schema = ?3, revision = revision + 1,
                             updated_at = ?4
         WHERE id = ?1",
        params![row.id, description, schema_text(&fields)?, at],
    )?;
    let after = type_by_id(t, &row.id)?.ok_or_else(|| ApiError::internal("type vanished"))?;
    let names = |v: &[&Field]| v.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
    let detail = json!({
        "type_id": row.id,
        "revision": after.revision,
        "added": names(&added),
        "removed": removed,
        "renamed": renamed.iter().map(|(a, b)| json!({"from": a, "to": b})).collect::<Vec<_>>(),
        "changed": names(&changed),
        "description": row.description != after.description,
        "records_changed": writes.len(),
    });
    t.audit(
        "db.type.update",
        &format!("{project}/{name}"),
        Some(project),
        detail,
    )?;
    emit_type(
        t,
        &after,
        "updated",
        json!({"records_changed": writes.len()}),
    )?;
    render_type(t, after)
}

/// Delete a type. `confirm` is `true` for a type with no records, and
/// `records` for one with records (which also needs project_admin). A type
/// that a ref field of another type points at stays.
pub fn delete_type(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    name: &str,
    confirm: Option<&str>,
) -> ApiResult<()> {
    projects::get(t, project)?;
    policy::write_db(actor, project)?;
    let row = need_type(t, project, name)?;
    let referrers: Vec<String> = types_of(t, project)?
        .iter()
        .filter(|o| o.name != name)
        .flat_map(|o| {
            o.fields
                .iter()
                .filter(|f| f.kind == Kind::Ref && f.target.as_deref() == Some(name))
                .map(|f| format!("{}.{}", o.name, f.name))
                .collect::<Vec<_>>()
        })
        .collect();
    if !referrers.is_empty() {
        return Err(ApiError::conflict(format!(
            "type `{name}` is the target of {}; remove those fields first",
            referrers.join(", ")
        ))
        .with_details(json!({"referrers": referrers})));
    }
    let records = record_count(t, &row.id)?;
    match (records, confirm) {
        (0, Some("true" | "records")) => {}
        (0, _) => {
            return Err(ApiError::new(
                Code::ConfirmRequired,
                format!("deleting type `{name}` needs ?confirm=true"),
            ));
        }
        (n, Some("records")) => {
            policy::purge_db_type(actor, project, n)?;
        }
        (n, _) => {
            return Err(ApiError::new(
                Code::ConfirmRequired,
                format!(
                    "type `{name}` has {n} records; ?confirm=records deletes them with the type (project_admin, sysadmin, or the operator)"
                ),
            )
            .with_details(json!({"records": n})));
        }
    }
    t.execute("DELETE FROM db_records WHERE type = ?1", [&row.id])?;
    t.execute("DELETE FROM db_types WHERE id = ?1", [&row.id])?;
    t.audit(
        "db.type.delete",
        &format!("{project}/{name}"),
        Some(project),
        json!({"type_id": row.id, "revision": row.revision, "records": records}),
    )?;
    emit_type(t, &row, "deleted", json!({"records": records}))?;
    Ok(())
}

// ---- records ----

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRecord {
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub data: Map<String, Value>,
}

pub fn create_record(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    n: NewRecord,
) -> ApiResult<Record> {
    projects::get(t, project)?;
    policy::write_db(actor, project)?;
    let ty = need_type(t, project, &n.type_name)?;
    let data = redact_data(t, n.data);
    let id = ulid::Ulid::new().to_string();
    let data = match check_record(t, project, &ty, &data, true, &id) {
        Ok(d) => d,
        Err(Ok(p)) => {
            return Err(invalid_fields(
                &format!("the `{}` record is not valid", ty.name),
                &p,
            ));
        }
        Err(Err(e)) => return Err(e),
    };
    let at = now();
    let me = actor.address();
    t.execute(
        "INSERT INTO db_records (id, project, type, data, revision, created_by, updated_by,
                                 created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5, ?6, ?6)",
        params![id, project, ty.id, serde_json::to_string(&data)?, me, at],
    )?;
    emit_record(t, project, &ty.name, &id, 1, "created")?;
    let row = record_by_id(t, &id)?.ok_or_else(|| ApiError::internal("record vanished"))?;
    render_record(t, &mut Names::default(), &ty.name, row)
}

/// Redact the values of record data: every string is free text,
/// whatever the field name.
fn redact_data(t: &Tx<'_>, data: Map<String, Value>) -> Map<String, Value> {
    match t.redact_values(Value::Object(data)) {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Find a record of `project` by ID; another project's record is not found.
fn find_record(conn: &Connection, project: &str, id: &str) -> ApiResult<(RecordRow, TypeRow)> {
    let missing = || ApiError::not_found(format!("project `{project}` has no DB record `{id}`"));
    let r = record_by_id(conn, id)?.ok_or_else(missing)?;
    if r.project != project {
        return Err(missing());
    }
    let ty = type_by_id(conn, &r.type_id)?.ok_or_else(missing)?;
    Ok((r, ty))
}

pub fn get_record(conn: &Connection, actor: &Actor, project: &str, id: &str) -> ApiResult<Record> {
    projects::get(conn, project)?;
    policy::read_db(actor, project)?;
    let (r, ty) = find_record(conn, project, id)?;
    render_record(conn, &mut Names::default(), &ty.name, r)
}

/// `PATCH /api/projects/{p}/db/records/{id}`: merge `data` into the
/// record; `null` removes a field.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordPatch {
    #[serde(default)]
    pub data: Map<String, Value>,
    /// The revision the change starts from. Another one is `409`. Absent:
    /// no check.
    #[serde(default)]
    pub revision: Option<i64>,
}

pub fn update_record(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    id: &str,
    p: RecordPatch,
) -> ApiResult<Record> {
    projects::get(t, project)?;
    policy::write_db(actor, project)?;
    let (row, ty) = find_record(t, project, id)?;
    if let Some(rev) = p.revision
        && rev != row.revision
    {
        let current = render_record(t, &mut Names::default(), &ty.name, row_copy(&row))?;
        return Err(ApiError::conflict(format!(
            "record {id} is at revision {}, not your revision {rev}; someone changed it; load it again",
            row.revision
        ))
        .with_details(json!({"current": row.revision, "revision": rev, "record": current})));
    }
    let mut data = row.data.clone();
    for (k, v) in redact_data(t, p.data) {
        if v.is_null() {
            data.remove(&k);
        } else {
            data.insert(k, v);
        }
    }
    let data = match check_record(t, project, &ty, &data, false, id) {
        Ok(d) => d,
        Err(Ok(p)) => {
            return Err(invalid_fields(
                &format!("the `{}` record is not valid", ty.name),
                &p,
            ));
        }
        Err(Err(e)) => return Err(e),
    };
    if data == row.data {
        return render_record(t, &mut Names::default(), &ty.name, row);
    }
    t.execute(
        "UPDATE db_records SET data = ?2, revision = revision + 1, updated_by = ?3,
                               updated_at = ?4
         WHERE id = ?1",
        params![id, serde_json::to_string(&data)?, actor.address(), now()],
    )?;
    emit_record(t, project, &ty.name, id, row.revision + 1, "updated")?;
    let row = record_by_id(t, id)?.ok_or_else(|| ApiError::internal("record vanished"))?;
    render_record(t, &mut Names::default(), &ty.name, row)
}

fn row_copy(r: &RecordRow) -> RecordRow {
    RecordRow {
        id: r.id.clone(),
        project: r.project.clone(),
        type_id: r.type_id.clone(),
        data: r.data.clone(),
        revision: r.revision,
        created_by: r.created_by.clone(),
        updated_by: r.updated_by.clone(),
        created_at: r.created_at.clone(),
        updated_at: r.updated_at.clone(),
    }
}

/// Delete a record. A record that a ref field of another record points
/// at stays: the answer lists up to 20 of those records.
pub fn delete_record(t: &mut Tx<'_>, actor: &Actor, project: &str, id: &str) -> ApiResult<()> {
    projects::get(t, project)?;
    policy::write_db(actor, project)?;
    let (row, ty) = find_record(t, project, id)?;
    let mut referrers = Vec::new();
    let mut total = 0i64;
    for o in types_of(t, project)? {
        for f in o
            .fields
            .iter()
            .filter(|f| f.kind == Kind::Ref && f.target.as_deref() == Some(ty.name.as_str()))
        {
            let path = format!("$.{}", f.name);
            let n: i64 = t.query_row(
                "SELECT COUNT(*) FROM db_records WHERE type = ?1 AND json_extract(data, ?2) = ?3
                 AND id != ?3",
                params![o.id, path, id],
                |r| r.get(0),
            )?;
            total += n;
            if n > 0 && referrers.len() < MAX_REFERRERS {
                let mut stmt = t.prepare(
                    "SELECT id FROM db_records WHERE type = ?1 AND json_extract(data, ?2) = ?3
                     AND id != ?3 ORDER BY id LIMIT ?4",
                )?;
                let ids = stmt
                    .query_map(
                        params![o.id, path, id, (MAX_REFERRERS - referrers.len()) as i64],
                        |r| r.get::<_, String>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                for i in ids {
                    referrers.push(json!({"id": i, "type": o.name, "field": f.name}));
                }
            }
        }
    }
    if total > 0 {
        return Err(ApiError::conflict(format!(
            "{total} records refer to record {id}; change or delete them first"
        ))
        .with_details(json!({"referrers": referrers, "total": total})));
    }
    t.execute("DELETE FROM db_records WHERE id = ?1", [id])?;
    t.audit(
        "db.record.delete",
        &format!("{project}/{}/{id}", ty.name),
        Some(project),
        json!({"type": ty.name, "id": id, "revision": row.revision}),
    )?;
    emit_record(t, project, &ty.name, id, row.revision, "deleted")?;
    Ok(())
}

// ---- query ----

fn query_error(e: &QueryError) -> ApiError {
    ApiError::bad_request(format!("bad query: {e}"))
        .with_details(json!({"position": e.position, "message": e.message}))
}

/// The JSON of a group value from SQL: a bool field gives `true`/`false`.
fn group_value(c: &Col, v: SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) if c.kind() == Kind::Bool => Value::Bool(i != 0),
        SqlValue::Integer(i) => json!(i),
        SqlValue::Real(x) => json!(x),
        SqlValue::Text(s) => Value::String(s),
        SqlValue::Blob(_) => Value::Null,
    }
}

/// Run a query of `project`. `scope`: the type when the query names none
/// (`?type=`). `cursor`: the offset of the page (the `next_cursor` of the
/// last page).
pub fn run_query(
    conn: &Connection,
    actor: &Actor,
    project: &str,
    q: &str,
    scope: Option<&str>,
    cursor: Option<&str>,
) -> ApiResult<QueryResult> {
    projects::get(conn, project)?;
    policy::read_db(actor, project)?;
    let parsed = query::parse(q).map_err(|e| query_error(&e))?;
    let type_name = match (&parsed.type_name, scope) {
        (Some(n), Some(s)) if n.name != s => {
            return Err(query_error(&QueryError {
                position: n.pos,
                message: format!("the query names type `{}`, the request `{s}`", n.name),
            }));
        }
        (Some(n), _) => n.clone(),
        (None, Some(s)) => query::Named {
            name: s.to_owned(),
            pos: 0,
        },
        (None, None) => {
            return Err(query_error(&QueryError {
                position: 0,
                message: "the query needs type:<name>".into(),
            }));
        }
    };
    let Some(ty) = type_by_name(conn, project, &type_name.name)? else {
        let names: Vec<String> = types_of(conn, project)?
            .into_iter()
            .map(|t| t.name)
            .collect();
        return Err(query_error(&QueryError {
            position: type_name.pos,
            message: format!(
                "project `{project}` has no DB type `{}`; its types: {}",
                type_name.name,
                if names.is_empty() {
                    "none".to_owned()
                } else {
                    names.join(", ")
                }
            ),
        }));
    };
    let info = ty.info();
    let lookup = |n: &str| {
        type_by_name(conn, project, n)
            .ok()
            .flatten()
            .map(|t| t.info())
    };
    let plan = query::compile(&info, &parsed, &lookup).map_err(|e| query_error(&e))?;
    let offset: usize = match cursor.filter(|c| !c.is_empty()) {
        Some(c) => c
            .parse()
            .map_err(|_| ApiError::bad_request(format!("bad cursor `{c}`")))?,
        None => 0,
    };
    let base_params: Vec<SqlValue> = vec![
        SqlValue::Text(project.to_owned()),
        SqlValue::Text(ty.id.clone()),
    ];
    let where_sql = format!("WHERE r.project = ? AND r.type = ?{}", plan.filter.text);
    let mut where_params = base_params.clone();
    where_params.extend(plan.filter.params.iter().cloned());
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM db_records r {where_sql}"),
        params_from_iter(where_params.iter()),
        |r| r.get(0),
    )?;
    let columns: Vec<Column> = plan
        .columns
        .iter()
        .map(|c| Column {
            field: c.field(),
            meta: matches!(c, Col::Meta(_)),
        })
        .collect();
    let mut out = QueryResult {
        type_name: ty.name.clone(),
        query: q.to_owned(),
        view: plan.view.as_str().to_owned(),
        fields: columns,
        records: Vec::new(),
        count,
        next_cursor: None,
        group: None,
        groups: None,
        labels: BTreeMap::new(),
    };
    let mut ref_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(g) = &plan.group {
        let (select, join) = query::group_parts(g);
        let order = query::group_order(g);
        let sql = format!(
            "SELECT {} AS g, COUNT(*) AS n FROM db_records r{} {where_sql} GROUP BY g ORDER BY {} LIMIT ?",
            select.text, join.text, order.text
        );
        let mut ps = select.params.clone();
        ps.extend(join.params.iter().cloned());
        ps.extend(where_params.iter().cloned());
        ps.extend(order.params.iter().cloned());
        ps.push(SqlValue::Integer(plan.limit as i64));
        let mut stmt = conn.prepare(&sql)?;
        let groups = stmt
            .query_map(params_from_iter(ps.iter()), |r| {
                Ok((r.get::<_, SqlValue>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(v, n)| Group {
                value: group_value(g, v),
                count: n,
            })
            .collect::<Vec<_>>();
        if let Col::Field(f) = g
            && f.kind == Kind::Ref
        {
            let set = ref_ids
                .entry(f.target.clone().unwrap_or_default())
                .or_default();
            for gr in &groups {
                if let Some(s) = gr.value.as_str() {
                    set.insert(s.to_owned());
                }
            }
        }
        out.group = Some(g.name().to_owned());
        out.groups = Some(groups);
    } else if !plan.count && plan.view != View::Count {
        let sql = format!(
            "SELECT r.* FROM db_records r {where_sql} ORDER BY {} LIMIT ? OFFSET ?",
            plan.order.text
        );
        let mut ps = where_params.clone();
        ps.extend(plan.order.params.iter().cloned());
        ps.push(SqlValue::Integer(plan.limit as i64 + 1));
        ps.push(SqlValue::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(params_from_iter(ps.iter()), record_row)?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > plan.limit {
            rows.truncate(plan.limit);
            out.next_cursor = Some((offset + plan.limit).to_string());
        }
        let keep: Vec<&Field> = plan
            .columns
            .iter()
            .filter_map(|c| match c {
                Col::Field(f) => Some(f),
                Col::Meta(_) => None,
            })
            .collect();
        let mut names = Names::default();
        for mut r in rows {
            r.data.retain(|k, _| keep.iter().any(|f| &f.name == k));
            for f in keep.iter().filter(|f| f.kind == Kind::Ref) {
                if let Some(Value::String(v)) = r.data.get(&f.name) {
                    ref_ids
                        .entry(f.target.clone().unwrap_or_default())
                        .or_default()
                        .insert(v.clone());
                }
            }
            out.records
                .push(render_record(conn, &mut names, &ty.name, r)?);
        }
    }
    for (target, ids) in ref_ids {
        let Some(tt) = type_by_name(conn, project, &target)? else {
            continue;
        };
        let Some(label) = tt.info().label_field().cloned() else {
            continue;
        };
        let mut stmt = conn
            .prepare("SELECT json_extract(data, ?2) FROM db_records WHERE id = ?1 AND type = ?3")?;
        for id in ids {
            let l: Option<Option<String>> = stmt
                .query_row(params![id, format!("$.{}", label.name), tt.id], |r| {
                    r.get(0)
                })
                .optional()?;
            if let Some(Some(l)) = l {
                out.labels.insert(id, l);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
