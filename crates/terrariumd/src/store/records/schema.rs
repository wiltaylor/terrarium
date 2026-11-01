//! The schema of a record type (PLAN §15.4): the field list, its checks,
//! and the check of a record's data against it.
//!
//! The checks here need no database. The store checks the parts that do:
//! that a `ref` points at a record of the target type, and `unique`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// At most this many fields in a type.
pub const MAX_FIELDS: usize = 100;
/// At most this many bytes in the JSON of one record's data.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
/// A `string` value has at most this many characters.
pub const MAX_STRING_CHARS: usize = 1000;
/// A `text` value, and the JSON of a `json` value, has at most this many bytes.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// A `list` value has at most this many items.
pub const MAX_LIST_ITEMS: usize = 1000;
/// An `enum` field has at most this many values.
pub const MAX_ENUM_VALUES: usize = 200;
/// A description has at most this many characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1000;

/// Names that no field may have: the record's own properties, and the
/// words of the query language that take `:`.
pub const RESERVED: &[&str] = &[
    "id",
    "type",
    "created",
    "updated",
    "created_by",
    "updated_by",
    "revision",
    "sort",
    "limit",
    "fields",
    "count",
    "group",
    "view",
];

/// The kind of a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// One line of text, at most 1000 characters.
    String,
    /// Long text (Markdown), at most 64 KiB.
    Text,
    Number,
    Integer,
    Bool,
    /// `YYYY-MM-DD`.
    Date,
    /// RFC 3339; stored in UTC with milliseconds.
    Datetime,
    /// One of `values`; the order of `values` is the sort order.
    Enum,
    /// A list of `of` items.
    List,
    /// The ID of a record of the type `ref`, in the same project.
    Ref,
    /// Any JSON, at most 64 KiB.
    Json,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Text => "text",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Bool => "bool",
            Self::Date => "date",
            Self::Datetime => "datetime",
            Self::Enum => "enum",
            Self::List => "list",
            Self::Ref => "ref",
            Self::Json => "json",
        }
    }

    /// The kinds that may be `unique`.
    fn may_be_unique(self) -> bool {
        matches!(
            self,
            Self::String
                | Self::Number
                | Self::Integer
                | Self::Date
                | Self::Datetime
                | Self::Enum
                | Self::Ref
        )
    }
}

/// The kind of the items of a `list` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    String,
    Integer,
    Number,
}

impl ItemKind {
    fn kind(self) -> Kind {
        match self {
            Self::String => Kind::String,
            Self::Integer => Kind::Integer,
            Self::Number => Kind::Number,
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One field of a type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub required: bool,
    /// The value of a record that does not give one, at its create. A
    /// field added to a type with records fills them with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `enum`: the values, in sort order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    /// `list`: the kind of the items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<ItemKind>,
    /// `ref`: the name of the target type.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// No two records of the type have the same value.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique: bool,
}

impl Field {
    /// A field with only a name and a kind.
    pub fn new(name: &str, kind: Kind) -> Self {
        Self {
            name: name.to_owned(),
            kind,
            required: false,
            default: None,
            description: None,
            values: None,
            of: None,
            target: None,
            unique: false,
        }
    }

    /// The position of `value` in `values` (the sort order of an enum).
    pub fn enum_index(&self, value: &str) -> Option<usize> {
        self.values.as_ref()?.iter().position(|v| v == value)
    }
}

/// True for a valid type name: `[a-z][a-z0-9_-]{0,62}`.
pub fn valid_type_name(name: &str) -> bool {
    let mut c = name.chars();
    matches!(c.next(), Some(f) if f.is_ascii_lowercase())
        && name.len() <= 63
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// True for a valid field name: `[a-z][a-z0-9_]{0,62}`.
pub fn valid_field_name(name: &str) -> bool {
    let mut c = name.chars();
    matches!(c.next(), Some(f) if f.is_ascii_lowercase())
        && name.len() <= 63
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The problems of a field list. Empty when it is good. The check of the
/// `ref` targets (that the type exists) is the store's.
pub fn check_fields(fields: &[Field]) -> Vec<String> {
    let mut out = Vec::new();
    if fields.is_empty() {
        out.push("a type needs at least one field".to_owned());
    }
    if fields.len() > MAX_FIELDS {
        out.push(format!(
            "a type has at most {MAX_FIELDS} fields, not {}",
            fields.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for f in fields {
        let n = &f.name;
        let mut bad = |m: String| out.push(format!("field `{n}`: {m}"));
        if !valid_field_name(n) {
            bad("a field name is 1-63 of a-z, 0-9, `_`, starting with a letter".into());
        } else if RESERVED.contains(&n.as_str()) {
            bad(format!(
                "the name is reserved (reserved: {})",
                RESERVED.join(", ")
            ));
        }
        if !seen.insert(n.as_str()) {
            bad("two fields have this name".into());
        }
        if f.description
            .as_ref()
            .is_some_and(|d| d.chars().count() > MAX_DESCRIPTION_CHARS)
        {
            bad(format!(
                "the description has more than {MAX_DESCRIPTION_CHARS} characters"
            ));
        }
        match (&f.kind, &f.values) {
            (Kind::Enum, None) => bad("an enum field needs `values`".into()),
            (Kind::Enum, Some(v)) => {
                if v.is_empty() || v.len() > MAX_ENUM_VALUES {
                    bad(format!("an enum has 1 to {MAX_ENUM_VALUES} values"));
                }
                let mut s = BTreeSet::new();
                for x in v {
                    if x.trim().is_empty() || x.chars().count() > 100 || x.trim() != x {
                        bad(format!(
                            "the enum value `{x}` is not valid: 1-100 characters, no space at the ends"
                        ));
                    }
                    if !s.insert(x) {
                        bad(format!("the enum value `{x}` is there twice"));
                    }
                }
            }
            (_, Some(_)) => bad("only an enum field has `values`".into()),
            _ => {}
        }
        match (&f.kind, &f.of) {
            (Kind::List, None) => {
                bad("a list field needs `of` (string, integer, or number)".into())
            }
            (Kind::List, Some(_)) | (_, None) => {}
            (_, Some(_)) => bad("only a list field has `of`".into()),
        }
        match (&f.kind, &f.target) {
            (Kind::Ref, None) => bad("a ref field needs `ref`, the name of the target type".into()),
            (Kind::Ref, Some(t)) if !valid_type_name(t) => {
                bad(format!("`{t}` is not a valid type name"));
            }
            (Kind::Ref, Some(_)) | (_, None) => {}
            (_, Some(_)) => bad("only a ref field has `ref`".into()),
        }
        if f.unique && !f.kind.may_be_unique() {
            bad(format!("a {} field cannot be unique", f.kind.as_str()));
        }
        if let Some(d) = &f.default {
            if f.kind == Kind::Ref {
                bad("a ref field has no default".into());
            } else if let Err(m) = check_value(f, d) {
                bad(format!("the default is not valid: {m}"));
            } else if f.unique {
                bad("a unique field has no default".into());
            }
        }
    }
    out
}

/// True when a `string` or `text` value counts as missing: empty after a
/// trim.
fn blank(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s.trim().is_empty())
}

/// Check one value against its field, and give it in its stored form: a
/// `datetime` in UTC with milliseconds. A `ref` gives the value as it is;
/// the store looks the record up.
pub fn check_value(f: &Field, v: &Value) -> Result<Value, String> {
    check_kind(f.kind, f, v)
}

fn check_kind(kind: Kind, f: &Field, v: &Value) -> Result<Value, String> {
    let want = |what: &str| Err(format!("must be {what}"));
    match kind {
        Kind::String => match v.as_str() {
            Some(s) if s.chars().count() > MAX_STRING_CHARS => Err(format!(
                "has more than {MAX_STRING_CHARS} characters (use a text field for long text)"
            )),
            Some(s) if s.contains('\n') => {
                Err("is one line (use a text field for more lines)".into())
            }
            Some(_) => Ok(v.clone()),
            None => want("a string"),
        },
        Kind::Text => match v.as_str() {
            Some(s) if s.len() > MAX_TEXT_BYTES => {
                Err(format!("has more than {} KiB", MAX_TEXT_BYTES / 1024))
            }
            Some(_) => Ok(v.clone()),
            None => want("a string"),
        },
        Kind::Number => match v.as_f64() {
            Some(n) if n.is_finite() && v.is_number() => Ok(v.clone()),
            _ => want("a number"),
        },
        Kind::Integer => match v.as_i64() {
            Some(_) => Ok(v.clone()),
            None => want("an integer"),
        },
        Kind::Bool => match v {
            Value::Bool(_) => Ok(v.clone()),
            _ => want("true or false"),
        },
        Kind::Date => match v.as_str().and_then(parse_date) {
            Some(d) => Ok(Value::String(d)),
            None => want("a date YYYY-MM-DD"),
        },
        Kind::Datetime => match v.as_str().and_then(parse_datetime) {
            Some(d) => Ok(Value::String(d)),
            None => want("an RFC 3339 time, for example 2026-10-01T09:30:00Z"),
        },
        Kind::Enum => {
            let values = f.values.as_deref().unwrap_or_default();
            match v.as_str() {
                Some(s) if values.iter().any(|x| x == s) => Ok(v.clone()),
                _ => Err(format!("must be one of: {}", values.join(", "))),
            }
        }
        Kind::List => {
            let Some(items) = v.as_array() else {
                return want("a list");
            };
            if items.len() > MAX_LIST_ITEMS {
                return Err(format!("has more than {MAX_LIST_ITEMS} items"));
            }
            let item = f.of.unwrap_or(ItemKind::String).kind();
            items
                .iter()
                .enumerate()
                .map(|(i, x)| check_kind(item, f, x).map_err(|m| format!("item {}: {m}", i + 1)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        Kind::Ref => match v.as_str() {
            Some(s) if !s.trim().is_empty() && s.len() <= 200 => Ok(Value::String(s.trim().into())),
            _ => want("the ID of a record (or the name of one)"),
        },
        Kind::Json => {
            if serde_json::to_string(v).map_or(0, |s| s.len()) > MAX_TEXT_BYTES {
                Err(format!("has more than {} KiB", MAX_TEXT_BYTES / 1024))
            } else {
                Ok(v.clone())
            }
        }
    }
}

/// A `YYYY-MM-DD` date, as it is.
pub fn parse_date(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 10 && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok())
        .then(|| s.to_owned())
}

/// An RFC 3339 time, in UTC with milliseconds (the stored form, so that
/// the text order is the time order).
pub fn parse_datetime(s: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| {
            d.to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
}

/// The problems of one record, by field name. The key `*` is a problem of
/// the whole record.
pub type Problems = BTreeMap<String, String>;

/// Check `data` against `fields` and give it in its stored form. A null
/// value is the same as no value. `defaults`: fill a missing field with
/// its default (a create). An unknown field, a missing required field, and
/// a value of the wrong kind are problems.
pub fn check_data(
    fields: &[Field],
    data: &Map<String, Value>,
    defaults: bool,
) -> Result<Map<String, Value>, Problems> {
    let mut problems = Problems::new();
    let mut out = Map::new();
    for k in data.keys() {
        if !fields.iter().any(|f| &f.name == k) {
            let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
            problems.insert(
                k.clone(),
                format!("is not a field of this type (fields: {})", names.join(", ")),
            );
        }
    }
    for f in fields {
        let given = data.get(&f.name).filter(|v| !v.is_null());
        let value = match (given, &f.default) {
            (Some(v), _) => Some(v),
            (None, Some(d)) if defaults => Some(d),
            (None, _) => None,
        };
        match value {
            Some(v) if f.required && blank(v) => {
                problems.insert(f.name.clone(), "is required".into());
            }
            Some(v) => match check_value(f, v) {
                Ok(v) => {
                    out.insert(f.name.clone(), v);
                }
                Err(m) => {
                    problems.insert(f.name.clone(), m);
                }
            },
            None if f.required => {
                problems.insert(f.name.clone(), "is required".into());
            }
            None => {}
        }
    }
    if problems.is_empty() {
        let size = serde_json::to_string(&out).map_or(0, |s| s.len());
        if size > MAX_RECORD_BYTES {
            problems.insert(
                "*".into(),
                format!(
                    "the record has {size} bytes; the limit is {} KiB",
                    MAX_RECORD_BYTES / 1024
                ),
            );
        }
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

/// One line for each problem: `title: is required`.
pub fn problem_text(p: &Problems) -> String {
    p.iter()
        .map(|(k, m)| {
            if k == "*" {
                m.clone()
            } else {
                format!("{k}: {m}")
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(v: Value) -> Vec<Field> {
        serde_json::from_value(v).unwrap()
    }

    fn bug() -> Vec<Field> {
        fields(json!([
            {"name": "title", "kind": "string", "required": true},
            {"name": "severity", "kind": "enum", "required": true, "values": ["low", "medium", "high"], "default": "low"},
            {"name": "notes", "kind": "text"},
            {"name": "points", "kind": "integer"},
            {"name": "score", "kind": "number"},
            {"name": "open", "kind": "bool"},
            {"name": "reported", "kind": "date"},
            {"name": "seen_at", "kind": "datetime"},
            {"name": "tags", "kind": "list", "of": "string"},
            {"name": "sizes", "kind": "list", "of": "integer"},
            {"name": "service", "kind": "ref", "ref": "service"},
            {"name": "extra", "kind": "json"}
        ]))
    }

    fn data(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn good_data_of_every_kind() {
        let d = check_data(
            &bug(),
            &data(json!({
                "title": "Login fails", "severity": "high", "notes": "# Steps\n\n1. x",
                "points": 3, "score": 2.5, "open": true, "reported": "2026-10-01",
                "seen_at": "2026-10-01T10:00:00+10:00", "tags": ["auth", "ui"],
                "sizes": [1, 2], "service": "auth", "extra": {"a": [1, {"b": null}]}
            })),
            true,
        )
        .unwrap();
        assert_eq!(d["seen_at"], "2026-10-01T00:00:00.000Z");
        assert_eq!(d["tags"], json!(["auth", "ui"]));
        assert_eq!(d["score"], 2.5);
    }

    #[test]
    fn defaults_and_required() {
        let d = check_data(&bug(), &data(json!({"title": "x"})), true).unwrap();
        assert_eq!(d["severity"], "low");
        // No default on an update.
        let p = check_data(&bug(), &data(json!({"title": "x"})), false).unwrap_err();
        assert_eq!(p["severity"], "is required");
        // A blank string is missing; null is missing.
        let p = check_data(&bug(), &data(json!({"title": "  ", "notes": null})), true).unwrap_err();
        assert_eq!(p["title"], "is required");
        assert!(!p.contains_key("notes"));
        let p = check_data(&bug(), &data(json!({})), true).unwrap_err();
        assert_eq!(p["title"], "is required");
    }

    #[test]
    fn wrong_kinds_and_unknown_fields() {
        let p = check_data(
            &bug(),
            &data(json!({
                "title": 3, "severity": "urgent", "notes": 1, "points": 2.5, "score": "x",
                "open": "yes", "reported": "2026-13-01", "seen_at": "yesterday",
                "tags": "auth", "sizes": [1, "2"], "service": 7, "colour": "red"
            })),
            true,
        )
        .unwrap_err();
        assert_eq!(p["title"], "must be a string");
        assert_eq!(p["severity"], "must be one of: low, medium, high");
        assert_eq!(p["notes"], "must be a string");
        assert_eq!(p["points"], "must be an integer");
        assert_eq!(p["score"], "must be a number");
        assert_eq!(p["open"], "must be true or false");
        assert_eq!(p["reported"], "must be a date YYYY-MM-DD");
        assert!(p["seen_at"].starts_with("must be an RFC 3339 time"));
        assert_eq!(p["tags"], "must be a list");
        assert_eq!(p["sizes"], "item 2: must be an integer");
        assert!(p["service"].starts_with("must be the ID"));
        assert!(p["colour"].starts_with("is not a field of this type"));
        assert!(problem_text(&p).contains("colour: is not a field"));
    }

    #[test]
    fn limits() {
        let f = bug();
        let long = "x".repeat(MAX_STRING_CHARS + 1);
        let p = check_data(&f, &data(json!({"title": long})), true).unwrap_err();
        assert!(p["title"].contains("more than 1000 characters"));
        let p = check_data(&f, &data(json!({"title": "a\nb"})), true).unwrap_err();
        assert!(p["title"].contains("one line"));
        let big = "y".repeat(MAX_TEXT_BYTES + 1);
        let p = check_data(&f, &data(json!({"title": "t", "notes": big})), true).unwrap_err();
        assert!(p["notes"].contains("64 KiB"));
        let p = check_data(
            &f,
            &data(json!({"title": "t", "sizes": vec![1; MAX_LIST_ITEMS + 1]})),
            true,
        )
        .unwrap_err();
        assert!(p["sizes"].contains("more than 1000 items"));
        // Two fields of 40 KiB each: each is good, the record is too big.
        let half = "z".repeat(40 * 1024);
        let p = check_data(
            &f,
            &data(json!({"title": "t", "notes": half, "extra": half})),
            true,
        )
        .unwrap_err();
        assert!(p["*"].contains("the limit is 64 KiB"));
    }

    #[test]
    fn field_list_checks() {
        assert!(check_fields(&bug()).is_empty());
        assert_eq!(check_fields(&[]), ["a type needs at least one field"]);
        let many: Vec<Field> = (0..=MAX_FIELDS)
            .map(|i| Field::new(&format!("f{i}"), Kind::String))
            .collect();
        assert!(check_fields(&many)[0].contains("at most 100 fields"));
        let bad = fields(json!([
            {"name": "Title", "kind": "string"},
            {"name": "id", "kind": "string"},
            {"name": "a", "kind": "string"},
            {"name": "a", "kind": "string"},
            {"name": "e", "kind": "enum"},
            {"name": "e2", "kind": "enum", "values": ["x", "x", " y"]},
            {"name": "s", "kind": "string", "values": ["x"]},
            {"name": "l", "kind": "list"},
            {"name": "r", "kind": "ref"},
            {"name": "r2", "kind": "ref", "ref": "Bad Type"},
            {"name": "j", "kind": "json", "unique": true},
            {"name": "d", "kind": "integer", "default": "x"},
            {"name": "rd", "kind": "ref", "ref": "t", "default": "x"},
            {"name": "o", "kind": "string", "of": "string"}
        ]));
        let p = check_fields(&bad).join("\n");
        for want in [
            "field `Title`: a field name is 1-63",
            "field `id`: the name is reserved",
            "field `a`: two fields have this name",
            "field `e`: an enum field needs `values`",
            "field `e2`: the enum value `x` is there twice",
            "field `e2`: the enum value ` y` is not valid",
            "field `s`: only an enum field has `values`",
            "field `l`: a list field needs `of`",
            "field `r`: a ref field needs `ref`",
            "field `r2`: `Bad Type` is not a valid type name",
            "field `j`: a json field cannot be unique",
            "field `d`: the default is not valid: must be an integer",
            "field `rd`: a ref field has no default",
            "field `o`: only a list field has `of`",
        ] {
            assert!(p.contains(want), "{want} not in\n{p}");
        }
        // An unknown key in a field is a bad body.
        assert!(
            serde_json::from_value::<Field>(json!({"name": "a", "kind": "string", "x": 1}))
                .is_err()
        );
    }

    #[test]
    fn names() {
        assert!(valid_type_name("bug"));
        assert!(valid_type_name("load-test_2"));
        assert!(!valid_type_name("2bug"));
        assert!(!valid_type_name("Bug"));
        assert!(!valid_type_name(""));
        assert!(!valid_type_name(&"a".repeat(64)));
        assert!(valid_field_name("run_at"));
        assert!(!valid_field_name("run-at"));
        assert!(!valid_field_name("a\"b"));
    }
}
