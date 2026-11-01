//! The query language of the DB (PLAN §15.4): one parser for the API, the
//! CLI, the dashboard, and the wiki `db` blocks (they all call the API).
//!
//! A query is terms separated by spaces:
//!
//! - `type:<name>`: the type (the API can give it instead);
//! - `field=value`, `!=`, `>`, `>=`, `<`, `<=`: comparisons (numbers,
//!   dates, times, and enums in their declared order);
//! - `field~text`: contains, without case (a list: has the item);
//! - `field:null`, `field:!null`: the field has no value, or has one;
//! - `sort:field`, `sort:-field` (more than one, or `sort:a,-b`);
//! - `limit:N` (1 to 500, default 50), `fields:a,b,c` (the columns);
//! - `count:true` (only the count), `group:field` (a count for each value);
//! - `view:table|list|count|group` (for the wiki block; the API gives it
//!   back);
//! - a value in double quotes can have spaces: `title~"login page"`.
//!
//! The compiler makes SQL with bound parameters only. A field name comes
//! from the schema (an allowlist); no text of the query goes into the SQL
//! text. An error has the position (a character offset) of the term.

use rusqlite::types::Value as SqlValue;

use super::schema::{Field, Kind, parse_date, parse_datetime};

/// The default `limit:`.
pub const DEFAULT_LIMIT: usize = 50;
/// The largest `limit:`.
pub const MAX_LIMIT: usize = 500;

/// A parse or compile error at a character offset of the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub position: usize,
    pub message: String,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "position {}: {}", self.position, self.message)
    }
}

fn err<T>(position: usize, message: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError {
        position,
        message: message.into(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Contains,
    Null,
    NotNull,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Contains => "~",
            Self::Null => ":null",
            Self::NotNull => ":!null",
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Contains | Self::Null | Self::NotNull => "",
        }
    }
}

/// A name and the position of its first character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub pos: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub field: Named,
    pub op: Op,
    pub value: String,
    pub value_pos: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    pub field: Named,
    pub desc: bool,
}

/// The `view:` of a wiki block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Table,
    List,
    Count,
    Group,
}

impl View {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::List => "list",
            Self::Count => "count",
            Self::Group => "group",
        }
    }
}

/// A parsed query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub type_name: Option<Named>,
    pub filters: Vec<Filter>,
    pub sort: Vec<SortKey>,
    pub limit: Option<usize>,
    pub fields: Option<Vec<Named>>,
    pub count: bool,
    pub group: Option<Named>,
    pub view: Option<View>,
}

/// One term: `key`, operator, value, and their positions.
struct Term {
    key: String,
    key_pos: usize,
    op: Op,
    op_pos: usize,
    value: String,
    value_pos: usize,
    /// The value had a quoted part (it is a literal: `"null"` is text).
    quoted: bool,
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Split the query into terms.
fn terms(q: &str) -> Result<Vec<Term>, QueryError> {
    let s: Vec<char> = q.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < s.len() {
        if s[i].is_whitespace() {
            i += 1;
            continue;
        }
        let key_pos = i;
        while i < s.len() && is_key_char(s[i]) {
            i += 1;
        }
        let key: String = s[key_pos..i].iter().collect();
        if key.is_empty() {
            return err(
                i,
                format!(
                    "expected a field name at `{}`; a term is field=value, field~text, field:null, type:<name>, sort:field",
                    s[i]
                ),
            );
        }
        let op_pos = i;
        let two: String = s[i..s.len().min(i + 2)].iter().collect();
        let op = match two.as_str() {
            "!=" => Some((Op::Ne, 2)),
            ">=" => Some((Op::Ge, 2)),
            "<=" => Some((Op::Le, 2)),
            _ => match s.get(i) {
                Some('=') => Some((Op::Eq, 1)),
                Some('>') => Some((Op::Gt, 1)),
                Some('<') => Some((Op::Lt, 1)),
                Some('~') => Some((Op::Contains, 1)),
                // `:` is a directive or a null check; the meaning comes later.
                Some(':') => Some((Op::Null, 1)),
                _ => None,
            },
        };
        let Some((op, n)) = op else {
            return err(
                op_pos,
                format!("expected an operator after `{key}`: one of = != > >= < <= ~ :"),
            );
        };
        i += n;
        let value_pos = i;
        let mut value = String::new();
        let mut quoted = false;
        while i < s.len() && !s[i].is_whitespace() {
            if s[i] == '"' {
                let open = i;
                quoted = true;
                i += 1;
                loop {
                    match s.get(i) {
                        None => return err(open, "this quote has no closing quote"),
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some('\\') if matches!(s.get(i + 1), Some('"' | '\\')) => {
                            value.push(s[i + 1]);
                            i += 2;
                        }
                        Some(c) => {
                            value.push(*c);
                            i += 1;
                        }
                    }
                }
            } else {
                value.push(s[i]);
                i += 1;
            }
        }
        if value.is_empty() && !quoted {
            return err(
                value_pos,
                format!(
                    "`{key}{}` needs a value (an empty text is \"\")",
                    if op == Op::Null { ":" } else { op.as_str() }
                ),
            );
        }
        out.push(Term {
            key,
            key_pos,
            op,
            op_pos,
            value,
            value_pos,
            quoted,
        });
    }
    Ok(out)
}

/// Split a `,` list of a directive, with the position of each part.
fn parts(t: &Term) -> Vec<Named> {
    let mut out = Vec::new();
    let mut off = 0;
    for p in t.value.split(',') {
        let lead = p.chars().take_while(|c| c.is_whitespace()).count();
        out.push(Named {
            name: p.trim().to_owned(),
            // A quoted value has no exact offsets; the start of the value is near.
            pos: if t.quoted {
                t.value_pos
            } else {
                t.value_pos + off + lead
            },
        });
        off += p.chars().count() + 1;
    }
    out
}

/// Parse a query.
pub fn parse(q: &str) -> Result<Query, QueryError> {
    let mut out = Query::default();
    for t in terms(q)? {
        let colon = t.op == Op::Null;
        let directive = |name: &str| colon && t.key == name;
        if t.key == "type" {
            if !colon {
                return err(t.op_pos, "write the type as type:<name>");
            }
            if out.type_name.is_some() {
                return err(t.key_pos, "a query has one type:");
            }
            out.type_name = Some(Named {
                name: t.value.clone(),
                pos: t.value_pos,
            });
        } else if directive("sort") {
            for p in parts(&t) {
                let (desc, name, pos) = match p.name.strip_prefix('-') {
                    Some(n) => (true, n.to_owned(), p.pos + 1),
                    None => match p.name.strip_prefix('+') {
                        Some(n) => (false, n.to_owned(), p.pos + 1),
                        None => (false, p.name.clone(), p.pos),
                    },
                };
                if name.is_empty() {
                    return err(
                        p.pos,
                        "sort: needs a field name, as sort:field or sort:-field",
                    );
                }
                out.sort.push(SortKey {
                    field: Named { name, pos },
                    desc,
                });
            }
        } else if directive("limit") {
            match t.value.parse::<usize>() {
                Ok(n) if (1..=MAX_LIMIT).contains(&n) => out.limit = Some(n),
                _ => {
                    return err(
                        t.value_pos,
                        format!("limit: takes a number from 1 to {MAX_LIMIT}"),
                    );
                }
            }
        } else if directive("fields") {
            let p = parts(&t);
            if let Some(e) = p.iter().find(|p| p.name.is_empty()) {
                return err(e.pos, "fields: takes field names separated by commas");
            }
            out.fields = Some(p);
        } else if directive("count") {
            out.count = match t.value.as_str() {
                "true" => true,
                "false" => false,
                _ => return err(t.value_pos, "count: takes true or false"),
            };
        } else if directive("group") {
            out.group = Some(Named {
                name: t.value.clone(),
                pos: t.value_pos,
            });
        } else if directive("view") {
            out.view = Some(match t.value.as_str() {
                "table" => View::Table,
                "list" => View::List,
                "count" => View::Count,
                "group" => View::Group,
                _ => return err(t.value_pos, "view: takes table, list, count, or group"),
            });
        } else if colon {
            let op = match (t.value.as_str(), t.quoted) {
                ("null", false) => Op::Null,
                ("!null", false) => Op::NotNull,
                _ => {
                    return err(
                        t.op_pos,
                        format!(
                            "`{}:` takes null or !null; to compare, write {}=value",
                            t.key, t.key
                        ),
                    );
                }
            };
            out.filters.push(Filter {
                field: Named {
                    name: t.key,
                    pos: t.key_pos,
                },
                op,
                value: String::new(),
                value_pos: t.value_pos,
            });
        } else {
            out.filters.push(Filter {
                field: Named {
                    name: t.key,
                    pos: t.key_pos,
                },
                op: t.op,
                value: t.value,
                value_pos: t.value_pos,
            });
        }
    }
    Ok(out)
}

// ---- compile ----

/// A record property that a query can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Meta {
    Id,
    Created,
    Updated,
    CreatedBy,
    UpdatedBy,
    Revision,
}

impl Meta {
    fn from_name(n: &str) -> Option<Self> {
        Some(match n {
            "id" => Self::Id,
            "created" => Self::Created,
            "updated" => Self::Updated,
            "created_by" => Self::CreatedBy,
            "updated_by" => Self::UpdatedBy,
            "revision" => Self::Revision,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Created => "created",
            Self::Updated => "updated",
            Self::CreatedBy => "created_by",
            Self::UpdatedBy => "updated_by",
            Self::Revision => "revision",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::Id => "r.id",
            Self::Created => "r.created_at",
            Self::Updated => "r.updated_at",
            Self::CreatedBy => "r.created_by",
            Self::UpdatedBy => "r.updated_by",
            Self::Revision => "r.revision",
        }
    }

    /// The kind the comparisons use.
    pub fn kind(self) -> Kind {
        match self {
            Self::Created | Self::Updated => Kind::Datetime,
            Self::Revision => Kind::Integer,
            Self::Id | Self::CreatedBy | Self::UpdatedBy => Kind::String,
        }
    }
}

/// A column of a query: a record property or a field of the type.
#[derive(Debug, Clone, PartialEq)]
pub enum Col {
    Meta(Meta),
    Field(Field),
}

impl Col {
    pub fn name(&self) -> &str {
        match self {
            Self::Meta(m) => m.name(),
            Self::Field(f) => &f.name,
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Self::Meta(m) => m.kind(),
            Self::Field(f) => f.kind,
        }
    }

    /// The field definition for the answer: a property gets a plain
    /// field of its kind.
    pub fn field(&self) -> Field {
        match self {
            Self::Meta(m) => Field::new(m.name(), m.kind()),
            Self::Field(f) => f.clone(),
        }
    }
}

/// The type of a query, as the store gives it.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeInfo {
    pub id: String,
    pub name: String,
    pub fields: Vec<Field>,
}

impl TypeInfo {
    /// The field whose value names a record: the first `string` field.
    pub fn label_field(&self) -> Option<&Field> {
        self.fields.iter().find(|f| f.kind == Kind::String)
    }
}

/// SQL text and its parameters, in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sql {
    pub text: String,
    pub params: Vec<SqlValue>,
}

impl Sql {
    fn push(&mut self, s: &str) {
        self.text.push_str(s);
    }
    fn param(&mut self, v: SqlValue) {
        self.text.push('?');
        self.params.push(v);
    }
    fn append(&mut self, o: Sql) {
        self.text.push_str(&o.text);
        self.params.extend(o.params);
    }
}

/// The JSON path of a field: `$.name`. The name is from the schema (it
/// matched `[a-z][a-z0-9_]*`), and it goes in as a parameter all the same.
fn path(f: &Field) -> SqlValue {
    SqlValue::Text(format!("$.{}", f.name))
}

/// The SQL value of a column.
fn expr(c: &Col) -> Sql {
    let mut s = Sql::default();
    match c {
        Col::Meta(m) => s.push(m.column()),
        Col::Field(f) => {
            s.push("json_extract(r.data, ");
            s.param(path(f));
            s.push(")");
        }
    }
    s
}

/// The sort value of a column: the index of an enum value, else the value.
fn order_expr(c: &Col) -> Sql {
    match c {
        Col::Field(f) if f.kind == Kind::Enum => {
            let mut s = Sql::default();
            s.push("(CASE ");
            s.append(expr(c));
            for (i, v) in f.values.iter().flatten().enumerate() {
                s.push(" WHEN ");
                s.param(SqlValue::Text(v.clone()));
                s.push(&format!(" THEN {i}"));
            }
            s.push(" END)");
            s
        }
        _ => expr(c),
    }
}

/// A compiled query.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The conditions after `WHERE r.project = ? AND r.type = ?`: empty,
    /// or ` AND ...`.
    pub filter: Sql,
    /// The `ORDER BY` list.
    pub order: Sql,
    /// The columns of the answer.
    pub columns: Vec<Col>,
    pub limit: usize,
    pub count: bool,
    /// `group:`: the column.
    pub group: Option<Col>,
    pub view: View,
}

/// Find a column by name.
fn resolve(t: &TypeInfo, n: &Named) -> Result<Col, QueryError> {
    if let Some(m) = Meta::from_name(&n.name) {
        return Ok(Col::Meta(m));
    }
    match t.fields.iter().find(|f| f.name == n.name) {
        Some(f) => Ok(Col::Field(f.clone())),
        None => {
            let names: Vec<&str> = t.fields.iter().map(|f| f.name.as_str()).collect();
            err(
                n.pos,
                format!(
                    "type `{}` has no field `{}`; its fields: {} (and id, created, updated, created_by, updated_by, revision)",
                    t.name,
                    n.name,
                    names.join(", ")
                ),
            )
        }
    }
}

/// `today`, `now`, or `-7d` / `-12h` / `-30m` back from now.
fn relative(v: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let now = chrono::Utc::now();
    match v {
        "now" | "today" => return Some(now),
        _ => {}
    }
    let rest = v.strip_prefix('-')?;
    let (num, unit) = rest.split_at(rest.len().checked_sub(1)?);
    let n: i64 = num.parse().ok()?;
    let d = match unit {
        "d" => chrono::Duration::days(n),
        "h" => chrono::Duration::hours(n),
        "m" => chrono::Duration::minutes(n),
        "w" => chrono::Duration::weeks(n),
        _ => return None,
    };
    now.checked_sub_signed(d)
}

/// A query value as the SQL value to compare with a column of `kind`.
fn literal(kind: Kind, c: &Col, f: &Filter) -> Result<SqlValue, QueryError> {
    let v = f.value.as_str();
    let name = c.name();
    let bad = |what: &str| err(f.value_pos, format!("`{name}` is {what}; `{v}` is not"));
    match kind {
        Kind::String | Kind::Text | Kind::Ref => Ok(SqlValue::Text(v.to_owned())),
        Kind::Integer | Kind::Number => match (v.parse::<i64>(), v.parse::<f64>()) {
            (Ok(i), _) => Ok(SqlValue::Integer(i)),
            (_, Ok(x)) if x.is_finite() => Ok(SqlValue::Real(x)),
            _ => bad("a number"),
        },
        Kind::Bool => match v {
            "true" => Ok(SqlValue::Integer(1)),
            "false" => Ok(SqlValue::Integer(0)),
            _ => bad("true or false"),
        },
        Kind::Date => match parse_date(v) {
            Some(d) => Ok(SqlValue::Text(d)),
            None => match relative(v) {
                Some(t) => Ok(SqlValue::Text(t.format("%Y-%m-%d").to_string())),
                None => bad("a date (YYYY-MM-DD, today, or -7d)"),
            },
        },
        Kind::Datetime => {
            if let Some(d) = parse_datetime(v) {
                return Ok(SqlValue::Text(d));
            }
            if let Some(d) = parse_date(v) {
                return Ok(SqlValue::Text(format!("{d}T00:00:00.000Z")));
            }
            match relative(v) {
                Some(t) => Ok(SqlValue::Text(
                    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                )),
                None => bad("a time (RFC 3339, YYYY-MM-DD, now, or -2h)"),
            }
        }
        Kind::Enum => {
            let Col::Field(field) = c else {
                return bad("an enum");
            };
            match field.enum_index(v) {
                Some(_) => Ok(SqlValue::Text(v.to_owned())),
                None => err(
                    f.value_pos,
                    format!(
                        "`{v}` is not a value of `{name}`; its values: {}",
                        field.values.as_deref().unwrap_or_default().join(", ")
                    ),
                ),
            }
        }
        Kind::List | Kind::Json => bad("not a value to compare"),
    }
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26 && s.parse::<ulid::Ulid>().is_ok()
}

/// One condition, as ` AND (...)`.
fn condition(
    t: &TypeInfo,
    f: &Filter,
    lookup: &dyn Fn(&str) -> Option<TypeInfo>,
) -> Result<Sql, QueryError> {
    let c = resolve(t, &f.field)?;
    let name = c.name().to_owned();
    let kind = c.kind();
    let mut s = Sql::default();
    s.push(" AND (");
    let refuse = |why: &str| err(f.field.pos, format!("`{name}{}`: {why}", f.op.as_str()));
    match f.op {
        Op::Null | Op::NotNull => {
            let Col::Field(field) = &c else {
                return refuse("a record always has this property");
            };
            s.push("json_type(r.data, ");
            s.param(path(field));
            s.push(if f.op == Op::Null {
                ") IS NULL"
            } else {
                ") IS NOT NULL"
            });
        }
        Op::Contains => match &c {
            Col::Field(field) if field.kind == Kind::List => {
                let item = match field.of {
                    Some(super::schema::ItemKind::Integer) => Kind::Integer,
                    Some(super::schema::ItemKind::Number) => Kind::Number,
                    _ => Kind::String,
                };
                list_has(&mut s, &c, field, item, f, false)?;
            }
            _ if matches!(kind, Kind::String | Kind::Text) => {
                s.push("instr(lower(");
                s.append(expr(&c));
                s.push("), lower(");
                s.param(SqlValue::Text(f.value.clone()));
                s.push(")) > 0");
            }
            _ => return refuse("`~` (contains) is for string, text, and list fields"),
        },
        op => match &c {
            Col::Field(field) if field.kind == Kind::List => {
                let item = match field.of {
                    Some(super::schema::ItemKind::Integer) => Kind::Integer,
                    Some(super::schema::ItemKind::Number) => Kind::Number,
                    _ => Kind::String,
                };
                match op {
                    Op::Eq => list_has(&mut s, &c, field, item, f, false)?,
                    Op::Ne => list_has(&mut s, &c, field, item, f, true)?,
                    _ => return refuse("a list takes = or ~ (has) and != (has not)"),
                }
            }
            Col::Field(field) if field.kind == Kind::Json => {
                return refuse("a json field takes only :null and :!null");
            }
            Col::Field(field) if field.kind == Kind::Ref => {
                if !matches!(op, Op::Eq | Op::Ne) {
                    return refuse("a ref field takes = and !=");
                }
                if op == Op::Ne {
                    s.push("json_type(r.data, ");
                    s.param(path(field));
                    s.push(") IS NULL OR ");
                }
                s.append(expr(&c));
                s.push(if op == Op::Eq { " IN (" } else { " NOT IN (" });
                if is_ulid(&f.value) {
                    s.param(SqlValue::Text(f.value.clone()));
                } else {
                    // A name: the records of the target type whose label
                    // field has it.
                    let target_name = field.target.clone().unwrap_or_default();
                    let Some(target) = lookup(&target_name) else {
                        return err(
                            f.field.pos,
                            format!("the target type `{target_name}` of `{name}` is gone"),
                        );
                    };
                    let Some(label) = target.label_field() else {
                        return err(
                            f.value_pos,
                            format!(
                                "type `{target_name}` has no string field to find `{}` by; give a record ID",
                                f.value
                            ),
                        );
                    };
                    s.push("SELECT x.id FROM db_records x WHERE x.type = ");
                    s.param(SqlValue::Text(target.id.clone()));
                    s.push(" AND json_extract(x.data, ");
                    s.param(path(label));
                    s.push(") = ");
                    s.param(SqlValue::Text(f.value.clone()));
                }
                s.push(")");
            }
            _ => {
                if kind == Kind::Bool && !matches!(op, Op::Eq | Op::Ne) {
                    return refuse("a bool field takes = and !=");
                }
                if matches!(c, Col::Meta(Meta::Id | Meta::CreatedBy | Meta::UpdatedBy))
                    && !matches!(op, Op::Eq | Op::Ne)
                {
                    return refuse("this property takes = and !=");
                }
                let value = literal(kind, &c, f)?;
                let ordered_enum = kind == Kind::Enum && !matches!(op, Op::Eq | Op::Ne);
                if op == Op::Ne {
                    // A record with no value is "not equal" too.
                    s.append(expr(&c));
                    s.push(" IS NULL OR ");
                }
                if ordered_enum {
                    let Col::Field(field) = &c else {
                        return refuse("not an enum");
                    };
                    let idx = field.enum_index(&f.value).unwrap_or(0);
                    s.append(order_expr(&c));
                    s.push(&format!(" {} ", op.sql()));
                    s.param(SqlValue::Integer(i64::try_from(idx).unwrap_or(0)));
                } else {
                    s.append(expr(&c));
                    s.push(&format!(" {} ", op.sql()));
                    s.param(value);
                }
            }
        },
    }
    s.push(")");
    Ok(s)
}

/// `EXISTS` (or `NOT EXISTS`) an item of the list equal to the value. A
/// text item matches without case.
fn list_has(
    s: &mut Sql,
    c: &Col,
    field: &Field,
    item: Kind,
    f: &Filter,
    not: bool,
) -> Result<(), QueryError> {
    let value = literal(item, c, f)?;
    s.push(if not { "NOT EXISTS" } else { "EXISTS" });
    s.push(" (SELECT 1 FROM json_each(r.data, ");
    s.param(path(field));
    if item == Kind::String {
        s.push(") WHERE lower(value) = lower(");
        s.param(value);
        s.push("))");
    } else {
        s.push(") WHERE value = ");
        s.param(value);
        s.push(")");
    }
    Ok(())
}

/// Compile a parsed query for the type `t`. `lookup` finds another type of
/// the project by name (the target of a ref field).
pub fn compile(
    t: &TypeInfo,
    q: &Query,
    lookup: &dyn Fn(&str) -> Option<TypeInfo>,
) -> Result<Plan, QueryError> {
    let mut filter = Sql::default();
    for f in &q.filters {
        filter.append(condition(t, f, lookup)?);
    }
    let mut order = Sql::default();
    for k in &q.sort {
        let c = resolve(t, &k.field)?;
        if matches!(c.kind(), Kind::List | Kind::Json | Kind::Text) {
            return err(
                k.field.pos,
                format!(
                    "cannot sort by `{}`, a {} field",
                    c.name(),
                    c.kind().as_str()
                ),
            );
        }
        let dir = if k.desc { "DESC" } else { "ASC" };
        // A record with no value goes last in both directions.
        order.push("(");
        order.append(expr(&c));
        order.push(" IS NULL), ");
        order.append(order_expr(&c));
        order.push(&format!(" {dir}, "));
    }
    order.push(if q.sort.is_empty() {
        "r.id DESC"
    } else {
        "r.id ASC"
    });
    let columns = match &q.fields {
        Some(list) => list
            .iter()
            .map(|n| resolve(t, n))
            .collect::<Result<Vec<_>, _>>()?,
        None => t.fields.iter().cloned().map(Col::Field).collect(),
    };
    let group = match &q.group {
        Some(n) => {
            let c = resolve(t, n)?;
            if matches!(c.kind(), Kind::Json | Kind::Text) {
                return err(
                    n.pos,
                    format!(
                        "cannot group by `{}`, a {} field",
                        c.name(),
                        c.kind().as_str()
                    ),
                );
            }
            Some(c)
        }
        None => None,
    };
    let view = q.view.unwrap_or(if group.is_some() {
        View::Group
    } else if q.count {
        View::Count
    } else {
        View::Table
    });
    if view == View::Group && group.is_none() {
        return err(0, "view:group needs group:<field>");
    }
    Ok(Plan {
        filter,
        order,
        columns,
        limit: q.limit.unwrap_or(DEFAULT_LIMIT),
        count: q.count || view == View::Count,
        group,
        view,
    })
}

/// The `SELECT` of a group query, after `FROM db_records r`: the value as
/// `g`, and the join of a list. Returns `(select, join)`.
pub fn group_parts(c: &Col) -> (Sql, Sql) {
    match c {
        Col::Field(f) if f.kind == Kind::List => {
            let mut j = Sql::default();
            j.push(", json_each(r.data, ");
            j.param(path(f));
            j.push(") AS j");
            let mut s = Sql::default();
            s.push("j.value");
            (s, j)
        }
        _ => (expr(c), Sql::default()),
    }
}

/// The order of the groups: an enum in its declared order, a bool by
/// value, else the largest count first.
pub fn group_order(c: &Col) -> Sql {
    match c {
        Col::Field(f) if f.kind == Kind::Enum => {
            let mut s = Sql::default();
            s.push("(g IS NULL), (CASE g");
            for (i, v) in f.values.iter().flatten().enumerate() {
                s.push(" WHEN ");
                s.param(SqlValue::Text(v.clone()));
                s.push(&format!(" THEN {i}"));
            }
            s.push(" END)");
            s
        }
        Col::Field(f) if f.kind == Kind::Bool => {
            let mut s = Sql::default();
            s.push("g DESC");
            s
        }
        _ => {
            let mut s = Sql::default();
            s.push("n DESC, g");
            s
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ty() -> TypeInfo {
        TypeInfo {
            id: "T1".into(),
            name: "bug".into(),
            fields: serde_json::from_value(json!([
                {"name": "title", "kind": "string", "required": true},
                {"name": "severity", "kind": "enum", "values": ["low", "medium", "high", "critical"]},
                {"name": "points", "kind": "integer"},
                {"name": "score", "kind": "number"},
                {"name": "open", "kind": "bool"},
                {"name": "reported", "kind": "date"},
                {"name": "seen_at", "kind": "datetime"},
                {"name": "tags", "kind": "list", "of": "string"},
                {"name": "service", "kind": "ref", "ref": "service"},
                {"name": "notes", "kind": "text"},
                {"name": "extra", "kind": "json"}
            ]))
            .unwrap(),
        }
    }

    fn service() -> TypeInfo {
        TypeInfo {
            id: "T2".into(),
            name: "service".into(),
            fields: vec![Field::new("name", Kind::String)],
        }
    }

    fn lookup(n: &str) -> Option<TypeInfo> {
        (n == "service").then(service)
    }

    fn plan(q: &str) -> Result<Plan, QueryError> {
        compile(&ty(), &parse(q)?, &lookup)
    }

    fn e(q: &str) -> QueryError {
        match parse(q).and_then(|p| compile(&ty(), &p, &lookup)) {
            Err(e) => e,
            Ok(p) => panic!("{q}: no error, got {p:?}"),
        }
    }

    #[test]
    fn parses_every_term() {
        let q = parse(
            r#"type:bug title~"login page" severity>=high points<3 open=true tags~ui extra:null notes:!null sort:-severity,title sort:points limit:10 fields:title,severity count:false view:list"#,
        )
        .unwrap();
        assert_eq!(q.type_name.as_ref().unwrap().name, "bug");
        assert_eq!(q.filters.len(), 7);
        assert_eq!(q.filters[0].value, "login page");
        assert_eq!(q.filters[0].op, Op::Contains);
        assert_eq!(q.filters[1].op, Op::Ge);
        assert_eq!(q.filters[2].op, Op::Lt);
        assert_eq!(q.filters[5].op, Op::Null);
        assert_eq!(q.filters[6].op, Op::NotNull);
        assert_eq!(
            q.sort
                .iter()
                .map(|k| (k.field.name.as_str(), k.desc))
                .collect::<Vec<_>>(),
            [("severity", true), ("title", false), ("points", false)]
        );
        assert_eq!(q.sort[1].field.pos, 108);
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.fields.as_ref().unwrap().len(), 2);
        assert_eq!(q.view, Some(View::List));
        // Escapes in quotes, and a quoted part inside a value.
        let q = parse(r#"title="say \"hi\" \\ there" notes=a"b c"d"#).unwrap();
        assert_eq!(q.filters[0].value, r#"say "hi" \ there"#);
        assert_eq!(q.filters[1].value, "ab cd");
        // A quoted "null" is text, not a null check.
        assert_eq!(
            e(r#"title:"null""#).message,
            "`title:` takes null or !null; to compare, write title=value"
        );
        assert_eq!(parse("").unwrap(), Query::default());
    }

    #[test]
    fn errors_have_positions() {
        let x = e("type:bug sevrity=high");
        assert_eq!(x.position, 9);
        assert!(x.message.starts_with("type `bug` has no field `sevrity`"));
        let x = e("type:bug title");
        assert_eq!(x.position, 14);
        assert!(x.message.contains("expected an operator after `title`"));
        let x = e(r#"title="open"#);
        assert_eq!(x.position, 6);
        assert_eq!(x.message, "this quote has no closing quote");
        assert_eq!(e("title= x").position, 6);
        assert_eq!(e("=x").position, 0);
        assert_eq!(e("limit:0").position, 6);
        assert_eq!(e("limit:501").position, 6);
        assert_eq!(e("limit:x").message, "limit: takes a number from 1 to 500");
        assert_eq!(e("count:yes").position, 6);
        assert_eq!(e("view:pie").position, 5);
        assert_eq!(e("type=bug").position, 4);
        assert_eq!(e("type:bug type:x").position, 9);
        let x = e("points=many");
        assert_eq!(
            (x.position, x.message.as_str()),
            (7, "`points` is a number; `many` is not")
        );
        let x = e("severity=urgent");
        assert!(
            x.message
                .contains("its values: low, medium, high, critical")
        );
        assert_eq!(
            e("open>true").message,
            "`open>`: a bool field takes = and !="
        );
        assert_eq!(e("reported>soon").position, 9);
        assert!(
            e("points~3")
                .message
                .contains("`~` (contains) is for string")
        );
        assert_eq!(e("sort:-nope").position, 6);
        assert!(e("sort:tags").message.contains("cannot sort by `tags`"));
        assert_eq!(e("fields:title,,x").position, 13);
        assert!(e("group:notes").message.contains("cannot group by `notes`"));
        assert!(e("extra=1").message.contains("only :null"));
        assert!(
            e("created:null")
                .message
                .contains("always has this property")
        );
        assert!(e("view:group").message.contains("needs group:"));
        assert!(e("service>x").message.contains("takes = and !="));
        assert!(e("tags>x").message.contains("a list takes"));
    }

    #[test]
    fn compiles_to_bound_parameters() {
        let p = plan("title~Login severity>=high open=true points!=3").unwrap();
        assert_eq!(
            p.filter.text,
            " AND (instr(lower(json_extract(r.data, ?)), lower(?)) > 0)\
             \x20AND ((CASE json_extract(r.data, ?) WHEN ? THEN 0 WHEN ? THEN 1 WHEN ? THEN 2 WHEN ? THEN 3 END) >= ?)\
             \x20AND (json_extract(r.data, ?) = ?)\
             \x20AND (json_extract(r.data, ?) IS NULL OR json_extract(r.data, ?) != ?)"
        );
        assert_eq!(p.filter.params[0], SqlValue::Text("$.title".into()));
        assert_eq!(p.filter.params[1], SqlValue::Text("Login".into()));
        // The index of `high`.
        assert_eq!(p.filter.params[7], SqlValue::Integer(2));
        assert_eq!(p.filter.params[9], SqlValue::Integer(1));
        assert_eq!(p.limit, DEFAULT_LIMIT);
        assert_eq!(p.order.text, "r.id DESC");
        assert_eq!(p.view, View::Table);
        assert_eq!(p.columns.len(), ty().fields.len());
    }

    #[test]
    fn dates_lists_refs_meta() {
        let p = plan("reported>=2026-10-01 seen_at<2026-10-02 created>-7d").unwrap();
        assert_eq!(p.filter.params[1], SqlValue::Text("2026-10-01".into()));
        assert_eq!(
            p.filter.params[3],
            SqlValue::Text("2026-10-02T00:00:00.000Z".into())
        );
        assert!(p.filter.text.contains("r.created_at > ?"));
        let p = plan("tags~ui tags!=wip").unwrap();
        assert!(
            p.filter.text.contains(
                "EXISTS (SELECT 1 FROM json_each(r.data, ?) WHERE lower(value) = lower(?))"
            )
        );
        assert!(p.filter.text.contains("NOT EXISTS"));
        let p = plan("service=auth").unwrap();
        assert!(p.filter.text.contains(
            "IN (SELECT x.id FROM db_records x WHERE x.type = ? AND json_extract(x.data, ?) = ?)"
        ));
        assert_eq!(p.filter.params[1], SqlValue::Text("T2".into()));
        let p = plan("service=01J00000000000000000000000").unwrap();
        assert!(p.filter.text.contains("IN (?)"));
        let p = plan(
            "revision>1 id=01J00000000000000000000000 sort:-created,severity fields:title,created",
        )
        .unwrap();
        assert!(p.filter.text.contains("r.revision > ?"));
        assert!(
            p.order
                .text
                .starts_with("(r.created_at IS NULL), r.created_at DESC, (json_extract")
        );
        assert!(p.order.text.ends_with("END) ASC, r.id ASC"));
        assert_eq!(p.columns[1], Col::Meta(Meta::Created));
        let p = plan("group:severity").unwrap();
        assert_eq!(p.view, View::Group);
        let p = plan("count:true").unwrap();
        assert!(p.count);
        assert_eq!(p.view, View::Count);
        let p = plan("view:count").unwrap();
        assert!(p.count);
    }

    #[test]
    fn hostile_text_never_reaches_the_sql_text() {
        for q in [
            r#"title="x') OR 1=1 --""#,
            "title=x'--",
            r#"title~"'; DROP TABLE db_records; --""#,
            r#"tags~"a' OR '1'='1""#,
            r#"service="x') UNION SELECT data FROM db_records --""#,
        ] {
            let p = plan(q).unwrap();
            assert!(!p.filter.text.contains('\''), "{q}: {}", p.filter.text);
            assert!(!p.filter.text.contains("--"), "{q}");
            assert!(!p.filter.text.contains("DROP"), "{q}");
        }
        // A field name with SQL in it is not a field.
        for q in [
            "title'=x",
            r#"ti"tle=x"#,
            "a)=1",
            "x--=1",
            "sort:title)",
            "fields:title,x'",
            r#"group:"a') --""#,
        ] {
            let r = parse(q).and_then(|p| compile(&ty(), &p, &lookup));
            assert!(r.is_err(), "{q} compiled");
        }
    }
}
