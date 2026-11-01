//! `db`: the project DB (docs/api.md §27, PLAN §15.4): record types with a
//! schema, records, and queries.

use serde_json::{Map, Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::{DbCmd, DbFormat, DbRecCmd, DbTypeCmd};
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::{Table, field, short_time};
use crate::transport::encode;

/// The field kinds. `enum`, `list`, and `ref` take an argument in `()`.
const KINDS: &[&str] = &[
    "string", "text", "number", "integer", "bool", "date", "datetime", "enum", "list", "ref",
    "json",
];

/// Parse a `--field` spec: `name:kind[(arg)][!][=default]`, for example
/// `severity:enum(low,medium,high)!=low`. Gives the field as JSON.
pub fn parse_field(spec: &str) -> Result<Value, String> {
    let usage = || {
        format!(
            "bad field `{spec}`: use name:kind[!][=default], for example title:string! or \
             severity:enum(low,high)!=low or tags:list(string) or service:ref(service)"
        )
    };
    let (name, rest) = spec.split_once(':').ok_or_else(usage)?;
    let name = name.trim();
    if name.is_empty() {
        return Err(usage());
    }
    let kind_len = rest
        .find(|c: char| !c.is_ascii_lowercase())
        .unwrap_or(rest.len());
    let kind = &rest[..kind_len];
    if !KINDS.contains(&kind) {
        return Err(format!(
            "bad field `{spec}`: `{kind}` is not a kind; the kinds: {}",
            KINDS.join(", ")
        ));
    }
    let mut rest = &rest[kind_len..];
    let mut arg = None;
    if let Some(r) = rest.strip_prefix('(') {
        let end = r
            .find(')')
            .ok_or_else(|| format!("bad field `{spec}`: `(` has no `)`"))?;
        arg = Some(r[..end].to_owned());
        rest = &r[end + 1..];
    }
    let required = rest.starts_with('!');
    if required {
        rest = &rest[1..];
    }
    let default = match rest.strip_prefix('=') {
        Some(d) => Some(d),
        None if rest.is_empty() => None,
        None => return Err(usage()),
    };
    let mut f = json!({"name": name, "kind": kind, "required": required});
    match (kind, arg) {
        ("enum", Some(a)) => {
            let values: Vec<String> = a
                .split(',')
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .collect();
            f["values"] = json!(values);
        }
        ("list", Some(a)) => f["of"] = json!(a.trim()),
        ("list", None) => f["of"] = json!("string"),
        ("ref", Some(a)) => f["ref"] = json!(a.trim()),
        ("enum" | "ref", None) => {
            return Err(format!(
                "bad field `{spec}`: {kind} needs its argument, as {kind}(...)"
            ));
        }
        (_, Some(_)) => {
            return Err(format!("bad field `{spec}`: {kind} takes no `(...)`"));
        }
        (_, None) => {}
    }
    if let Some(d) = default {
        f["default"] = value_for(&f, d)?;
    }
    Ok(f)
}

/// A text value as the JSON value of the field's kind. A list is `a,b,c`
/// or a JSON list; `json` is JSON; a number that does not parse stays
/// text, so that the daemon names the problem.
pub fn value_for(field: &Value, raw: &str) -> Result<Value, String> {
    let kind = field["kind"].as_str().unwrap_or("string");
    let scalar = |k: &str, s: &str| -> Value {
        match k {
            "integer" => s
                .trim()
                .parse::<i64>()
                .map_or_else(|_| Value::String(s.to_owned()), Value::from),
            "number" => s
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(|n| serde_json::Number::from_f64(n).map(Value::Number))
                .map_or_else(
                    || Value::String(s.to_owned()),
                    |v| {
                        // `3` stays an integer in the JSON.
                        s.trim().parse::<i64>().map_or(v, Value::from)
                    },
                ),
            "bool" => match s.trim() {
                "true" | "yes" | "1" => Value::Bool(true),
                "false" | "no" | "0" => Value::Bool(false),
                _ => Value::String(s.to_owned()),
            },
            _ => Value::String(s.to_owned()),
        }
    };
    Ok(match kind {
        "json" => serde_json::from_str(raw)
            .map_err(|e| format!("`{}` takes JSON: {e}", field["name"].as_str().unwrap_or("")))?,
        "list" => {
            let of = field["of"].as_str().unwrap_or("string");
            if raw.trim_start().starts_with('[') {
                serde_json::from_str(raw).map_err(|e| {
                    format!(
                        "`{}` takes a,b,c or a JSON list: {e}",
                        field["name"].as_str().unwrap_or("")
                    )
                })?
            } else if raw.trim().is_empty() {
                json!([])
            } else {
                Value::Array(raw.split(',').map(|x| scalar(of, x.trim())).collect())
            }
        }
        k => scalar(k, raw),
    })
}

/// Split `field=value` at the first `=`.
pub fn split_set(s: &str) -> Result<(&str, &str), String> {
    s.split_once('=')
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.trim(), v))
        .ok_or_else(|| format!("bad --set `{s}`: use field=value"))
}

fn types_path(p: &str) -> String {
    format!("{}/db/types", project_path(p))
}

fn type_path(p: &str, name: &str) -> String {
    format!("{}/db/types/{}", project_path(p), encode(name))
}

fn record_path(p: &str, id: &str) -> String {
    format!("{}/db/records/{}", project_path(p), encode(id))
}

/// The field definitions of a type, by name.
fn field_map(t: &Value) -> Map<String, Value> {
    as_items(&t["fields"])
        .iter()
        .filter_map(|f| Some((f["name"].as_str()?.to_owned(), f.clone())))
        .collect()
}

/// The data of `--set`, `--unset`, and `--json-data`, in the kinds of the
/// type's fields.
fn data_of(
    ctx: &Ctx,
    t: &Value,
    sets: &[String],
    unsets: &[String],
    json_data: Option<&str>,
) -> anyhow::Result<Map<String, Value>> {
    let mut data = match json_data {
        Some(j) => match serde_json::from_str::<Value>(&ctx.text(j)?) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return Err(CliError::usage("--json-data takes a JSON object")),
            Err(e) => return Err(CliError::usage(format!("--json-data is not JSON: {e}"))),
        },
        None => Map::new(),
    };
    let fields = field_map(t);
    for s in sets {
        let (k, v) = split_set(s).map_err(CliError::usage)?;
        let v = if v == "-" {
            ctx.text("-")?
        } else {
            v.to_owned()
        };
        // An unknown field goes as text: the daemon names it.
        let f = fields
            .get(k)
            .cloned()
            .unwrap_or_else(|| json!({"name": k, "kind": "string"}));
        data.insert(k.to_owned(), value_for(&f, &v).map_err(CliError::usage)?);
    }
    for u in unsets {
        data.insert(u.clone(), Value::Null);
    }
    Ok(data)
}

/// A field as one line: `title  string  required  default: x`.
fn field_line(f: &Value) -> Vec<String> {
    let kind = f["kind"].as_str().unwrap_or("-");
    let kind = match kind {
        "enum" => format!("enum({})", field(f, "values")),
        "list" => format!("list({})", field(f, "of")),
        "ref" => format!("ref({})", field(f, "ref")),
        k => k.to_owned(),
    };
    let mut flags = Vec::new();
    if f["required"].as_bool() == Some(true) {
        flags.push("required".to_owned());
    }
    if f["unique"].as_bool() == Some(true) {
        flags.push("unique".to_owned());
    }
    vec![
        field(f, "name"),
        kind,
        if flags.is_empty() {
            "-".into()
        } else {
            flags.join(",")
        },
        f.get("default")
            .filter(|d| !d.is_null())
            .map_or_else(|| "-".into(), cell),
        field(f, "description"),
    ]
}

fn print_type(ctx: &Ctx, t: &Value) {
    ctx.line(format!(
        "{}  rev {}  {} records",
        field(t, "name"),
        field(t, "revision"),
        field(t, "records")
    ));
    if let Some(d) = t["description"].as_str().filter(|d| !d.is_empty()) {
        ctx.line(d);
    }
    let mut table = Table::new(&["FIELD", "KIND", "FLAGS", "DEFAULT", "DESCRIPTION"]);
    for f in as_items(&t["fields"]) {
        table.row(field_line(f));
    }
    ctx.out.write_str(&table.render(ctx.out.width()));
}

/// A value as one cell: text as it is, a list joined with `,`, other JSON
/// as JSON.
fn cell(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) if s.is_empty() => "-".into(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(cell).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

fn print_record(ctx: &Ctx, r: &Value) {
    ctx.line(format!(
        "{} {}  rev {}  by {}, {}",
        field(r, "type"),
        field(r, "id"),
        field(r, "revision"),
        field(r, "updated_by_name"),
        short_time(&field(r, "updated_at"))
    ));
    if let Some(m) = r["data"].as_object() {
        let width = m.keys().map(String::len).max().unwrap_or(0);
        for (k, v) in m {
            let text = cell(v);
            if text.contains('\n') {
                ctx.line(format!("{k}:"));
                for l in text.lines() {
                    ctx.line(format!("  {l}"));
                }
            } else {
                ctx.line(format!("{k:<width$}  {text}"));
            }
        }
    }
}

/// The value of a column of a query answer: a property for a `meta`
/// column, else the data; a ref shows its label.
fn column_value(answer: &Value, rec: &Value, col: &Value) -> String {
    let name = col["name"].as_str().unwrap_or_default();
    if col["meta"].as_bool() == Some(true) {
        let key = match name {
            "created" => "created_at",
            "updated" => "updated_at",
            other => other,
        };
        let v = field(rec, key);
        return if key.ends_with("_at") {
            short_time(&v)
        } else {
            v
        };
    }
    let v = &rec["data"][name];
    if col["kind"] == "ref"
        && let Some(id) = v.as_str()
        && let Some(l) = answer["labels"][id].as_str()
    {
        return l.to_owned();
    }
    let text = cell(v);
    // A table cell is one line.
    match text.lines().next() {
        Some(first) if text.contains('\n') => format!("{first} …"),
        _ => text,
    }
}

/// RFC 4180: quote a cell with a comma, a quote, or a line break.
pub fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// Print a query answer as a table or CSV.
pub fn print_answer(ctx: &Ctx, v: &Value, format: DbFormat) {
    let csv = format == DbFormat::Csv;
    let row_out = |cells: Vec<String>| {
        cells
            .iter()
            .map(|c| csv_cell(c))
            .collect::<Vec<_>>()
            .join(",")
    };
    if let Some(groups) = v["groups"].as_array() {
        let name = field(v, "group");
        let label = |g: &Value| match g["value"].as_str().and_then(|s| v["labels"][s].as_str()) {
            Some(l) => l.to_owned(),
            None => cell(&g["value"]),
        };
        if csv {
            ctx.line(row_out(vec![name, "count".into()]));
            for g in groups {
                ctx.line(row_out(vec![label(g), field(g, "count")]));
            }
            return;
        }
        let head = name.to_uppercase();
        let mut t = Table::new(&[head.as_str(), "COUNT"]);
        for g in groups {
            t.row(vec![label(g), field(g, "count")]);
        }
        ctx.out.write_str(&t.render(ctx.out.width()));
        return;
    }
    if v["view"] == "count" {
        ctx.line(field(v, "count"));
        return;
    }
    let cols = as_items(&v["fields"]);
    let records = as_items(&v["records"]);
    let mut headers = vec!["ID".to_owned()];
    headers.extend(cols.iter().map(|c| field(c, "name").to_uppercase()));
    if csv {
        let mut h = vec!["id".to_owned()];
        h.extend(cols.iter().map(|c| field(c, "name")));
        ctx.line(row_out(h));
        for r in records {
            let mut cells = vec![field(r, "id")];
            for c in cols {
                let name = c["name"].as_str().unwrap_or_default();
                // CSV keeps the whole text and the ref ID.
                cells.push(if c["meta"].as_bool() == Some(true) {
                    column_value(v, r, c)
                } else {
                    match &r["data"][name] {
                        Value::Null => String::new(),
                        x => cell(x),
                    }
                });
            }
            ctx.line(row_out(cells));
        }
        return;
    }
    if records.is_empty() {
        ctx.line(format!("no {} records", field(v, "type")));
        return;
    }
    let hs: Vec<&str> = headers.iter().map(String::as_str).collect();
    let mut t = Table::new(&hs);
    for r in records {
        let mut cells = vec![field(r, "id")];
        cells.extend(cols.iter().map(|c| column_value(v, r, c)));
        t.row(cells);
    }
    ctx.out.write_str(&t.render(ctx.out.width()));
    if let Some(next) = v["next_cursor"].as_str() {
        ctx.line(format!(
            "{} of {}; the next page: --cursor {next}",
            records.len(),
            field(v, "count")
        ));
    }
}

async fn run_query(
    ctx: &Ctx,
    p: &str,
    q: &str,
    format: DbFormat,
    cursor: Option<&String>,
) -> anyhow::Result<()> {
    let v = ctx
        .api
        .get(
            &format!("{}/db/query", project_path(p)),
            query([("q", Some(q.to_owned())), ("cursor", cursor.cloned())]),
        )
        .await?;
    if ctx.json || format == DbFormat::Json {
        ctx.line(serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    print_answer(ctx, &v, format);
    Ok(())
}

/// The field list of `--from-file`: a list, or an object with `fields`.
fn fields_from(ctx: &Ctx, arg: &str) -> anyhow::Result<(Vec<Value>, Option<String>)> {
    let text = if arg == "-" {
        ctx.text("-")?
    } else {
        std::fs::read_to_string(arg)
            .map_err(|e| CliError::usage(format!("cannot read {arg}: {e}")))?
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(a)) => Ok((a, None)),
        Ok(Value::Object(o)) => {
            let fields = o
                .get("fields")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| CliError::usage(format!("{arg} has no `fields` list")))?;
            let d = o
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Ok((fields, d))
        }
        Ok(_) => Err(CliError::usage(format!("{arg}: give a list of fields"))),
        Err(e) => Err(CliError::usage(format!("{arg} is not JSON: {e}"))),
    }
}

fn mark_unique(fields: &mut [Value], unique: &[String]) -> anyhow::Result<()> {
    for u in unique {
        let f = fields
            .iter_mut()
            .find(|f| f["name"] == u.as_str())
            .ok_or_else(|| CliError::usage(format!("--unique {u}: no such field")))?;
        f["unique"] = Value::Bool(true);
    }
    Ok(())
}

pub async fn db(ctx: &Ctx, cmd: &DbCmd) -> anyhow::Result<()> {
    let p = ctx.project_or(None).await?;
    match cmd {
        DbCmd::Query {
            query: q,
            format,
            cursor,
        } => run_query(ctx, &p, &q.join(" "), *format, cursor.as_ref()).await,
        DbCmd::Type { command } => db_type(ctx, &p, command).await,
        DbCmd::Rec { command } => db_rec(ctx, &p, command).await,
    }
}

async fn db_type(ctx: &Ctx, p: &str, cmd: &DbTypeCmd) -> anyhow::Result<()> {
    match cmd {
        DbTypeCmd::Ls => {
            let v = Value::Array(ctx.api.list(&types_path(p), vec![], None).await?);
            ctx.emit(&v, |c, v| {
                let rows: Vec<Value> = as_items(v)
                    .iter()
                    .map(|t| {
                        let mut t = t.clone();
                        t["field_count"] = json!(as_items(&t["fields"]).len());
                        t
                    })
                    .collect();
                print_table(
                    c,
                    &rows,
                    &[
                        ("TYPE", "name"),
                        ("RECORDS", "records"),
                        ("FIELDS", "field_count"),
                        ("REV", "revision"),
                        ("DESCRIPTION", "description"),
                        ("UPDATED", "updated_at"),
                    ],
                    "no types; make one with `terrarium db type new`",
                );
            })
        }
        DbTypeCmd::Show { name } => {
            let v = ctx.api.get(&type_path(p, name), vec![]).await?;
            ctx.emit(&v, print_type)
        }
        DbTypeCmd::New {
            name,
            description,
            fields,
            unique,
            from_file,
        } => {
            let (mut list, file_desc) = match from_file {
                Some(f) => fields_from(ctx, f)?,
                None => (
                    fields
                        .iter()
                        .map(|f| parse_field(f).map_err(CliError::usage))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                    None,
                ),
            };
            if list.is_empty() {
                return Err(CliError::usage("give at least one --field, or --from-file"));
            }
            mark_unique(&mut list, unique)?;
            let mut b = json!({"name": name, "fields": list});
            if let Some(d) = description.clone().or(file_desc) {
                b["description"] = json!(d);
            }
            let v = ctx.api.post(&types_path(p), Some(b)).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "created type {} with {} fields",
                    field(v, "name"),
                    as_items(&v["fields"]).len()
                ));
            })
        }
        DbTypeCmd::Edit {
            name,
            description,
            fields,
            unique,
            renames,
            removes,
            from_file,
            confirm,
        } => {
            let cur = ctx.api.get(&type_path(p, name), vec![]).await?;
            let mut list: Vec<Value> = as_items(&cur["fields"]).to_vec();
            let mut rename = Vec::new();
            for r in renames {
                let (from, to) = r
                    .split_once(':')
                    .filter(|(a, b)| !a.is_empty() && !b.is_empty())
                    .ok_or_else(|| CliError::usage(format!("bad --rename `{r}`: use old:new")))?;
                rename.push(json!({"from": from, "to": to}));
                for f in &mut list {
                    if f["name"] == from {
                        f["name"] = json!(to);
                    }
                }
            }
            let mut file_desc = None;
            if let Some(f) = from_file {
                let (l, d) = fields_from(ctx, f)?;
                list = l;
                file_desc = d;
            }
            for spec in fields {
                let f = parse_field(spec).map_err(CliError::usage)?;
                match list.iter_mut().find(|x| x["name"] == f["name"]) {
                    Some(x) => *x = f,
                    None => list.push(f),
                }
            }
            for r in removes {
                let before = list.len();
                list.retain(|f| f["name"] != r.as_str());
                if list.len() == before {
                    return Err(CliError::usage(format!("--remove {r}: no such field")));
                }
            }
            mark_unique(&mut list, unique)?;
            let mut b = json!({"revision": cur["revision"], "fields": list});
            if !rename.is_empty() {
                b["rename"] = Value::Array(rename);
            }
            if let Some(d) = description.clone().or(file_desc) {
                b["description"] = json!(d);
            }
            if *confirm {
                b["confirm"] = json!(true);
            }
            let v = ctx.api.patch(&type_path(p, name), b).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "changed type {}; now rev {}",
                    field(v, "name"),
                    field(v, "revision")
                ));
            })
        }
        DbTypeCmd::Rm { name, records } => {
            let confirm = if *records { "records" } else { "true" };
            ctx.api
                .delete(
                    &type_path(p, name),
                    query([("confirm", Some(confirm.to_owned()))]),
                )
                .await?;
            ctx.emit(&json!({"deleted": name}), |c, _| {
                c.line(format!("deleted type {name}"));
            })
        }
    }
}

async fn db_rec(ctx: &Ctx, p: &str, cmd: &DbRecCmd) -> anyhow::Result<()> {
    match cmd {
        DbRecCmd::New {
            type_name,
            sets,
            json_data,
        } => {
            let t = ctx.api.get(&type_path(p, type_name), vec![]).await?;
            let data = data_of(ctx, &t, sets, &[], json_data.as_deref())?;
            let v = ctx
                .api
                .post(
                    &format!("{}/db/records", project_path(p)),
                    Some(json!({"type": type_name, "data": data})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("created {} {}", field(v, "type"), field(v, "id")));
            })
        }
        DbRecCmd::Get { id } => {
            let v = ctx.api.get(&record_path(p, id), vec![]).await?;
            ctx.emit(&v, print_record)
        }
        DbRecCmd::Set {
            id,
            sets,
            unsets,
            json_data,
            revision,
        } => {
            if sets.is_empty() && unsets.is_empty() && json_data.is_none() {
                return Err(CliError::usage("give --set, --unset, or --json-data"));
            }
            let cur = ctx.api.get(&record_path(p, id), vec![]).await?;
            let tname = cur["type"].as_str().unwrap_or_default().to_owned();
            let t = ctx.api.get(&type_path(p, &tname), vec![]).await?;
            let data = data_of(ctx, &t, sets, unsets, json_data.as_deref())?;
            let mut b = json!({"data": data});
            if let Some(r) = revision {
                b["revision"] = json!(r);
            }
            let v = ctx.api.patch(&record_path(p, id), b).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "changed {} {}; now rev {}",
                    field(v, "type"),
                    field(v, "id"),
                    field(v, "revision")
                ));
            })
        }
        DbRecCmd::Rm { id } => {
            ctx.api.delete(&record_path(p, id), vec![]).await?;
            ctx.emit(&json!({"deleted": id}), |c, _| {
                c.line(format!("deleted record {id}"));
            })
        }
        DbRecCmd::Ls {
            type_name,
            query: q,
            format,
            cursor,
        } => {
            let mut text = format!("type:{type_name}");
            for w in q {
                text.push(' ');
                text.push_str(w);
            }
            run_query(ctx, p, &text, *format, cursor.as_ref()).await
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn field_specs() {
        assert_eq!(
            parse_field("title:string!").unwrap(),
            json!({"name": "title", "kind": "string", "required": true})
        );
        assert_eq!(
            parse_field("severity:enum(low, medium,high)!=low").unwrap(),
            json!({"name": "severity", "kind": "enum", "required": true,
                   "values": ["low", "medium", "high"], "default": "low"})
        );
        assert_eq!(
            parse_field("tags:list(string)").unwrap(),
            json!({"name": "tags", "kind": "list", "required": false, "of": "string"})
        );
        assert_eq!(
            parse_field("sizes:list(integer)=1,2").unwrap()["default"],
            json!([1, 2])
        );
        assert_eq!(
            parse_field("service:ref(service)").unwrap()["ref"],
            "service"
        );
        assert_eq!(parse_field("port:integer=8080").unwrap()["default"], 8080);
        assert_eq!(parse_field("ok:bool=true").unwrap()["default"], true);
        assert_eq!(parse_field("x:number=2.5").unwrap()["default"], 2.5);
        // A default may hold `=`, `:`, and spaces (the shell quoted them).
        assert_eq!(
            parse_field("note:string=a=b: c").unwrap()["default"],
            "a=b: c"
        );
        assert_eq!(
            parse_field("meta:json={\"a\":1}").unwrap()["default"],
            json!({"a": 1})
        );
        for bad in [
            "title",
            ":string",
            "title:strin",
            "title:enum",
            "title:ref",
            "title:string(x)",
            "title:enum(a,b",
            "title:string?",
            "meta:json={",
        ] {
            assert!(parse_field(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn set_values_follow_the_kind() {
        let f = |k: &str| json!({"name": "x", "kind": k, "of": "integer"});
        assert_eq!(value_for(&f("integer"), "42").unwrap(), 42);
        assert_eq!(value_for(&f("integer"), "lots").unwrap(), "lots");
        assert_eq!(value_for(&f("number"), "3").unwrap(), 3);
        assert_eq!(value_for(&f("number"), "2.5").unwrap(), 2.5);
        assert_eq!(value_for(&f("bool"), "yes").unwrap(), true);
        assert_eq!(value_for(&f("list"), "1, 2,3").unwrap(), json!([1, 2, 3]));
        assert_eq!(value_for(&f("list"), "[4]").unwrap(), json!([4]));
        assert_eq!(value_for(&f("list"), "").unwrap(), json!([]));
        assert_eq!(
            value_for(
                &json!({"name": "t", "kind": "list", "of": "string"}),
                "a b,c"
            )
            .unwrap(),
            json!(["a b", "c"])
        );
        assert_eq!(
            value_for(&f("string"), "say \"hi\", ok").unwrap(),
            "say \"hi\", ok"
        );
        assert_eq!(
            value_for(&f("json"), "[1,{\"a\":null}]").unwrap(),
            json!([1, {"a": null}])
        );
        assert!(value_for(&f("json"), "{").is_err());
        assert_eq!(split_set("title=a = b").unwrap(), ("title", "a = b"));
        assert_eq!(split_set("empty=").unwrap(), ("empty", ""));
        assert!(split_set("=x").is_err());
        assert!(split_set("nothing").is_err());
    }

    #[test]
    fn csv_quotes() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("two\nlines"), "\"two\nlines\"");
    }
}
