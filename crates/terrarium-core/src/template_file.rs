//! Agent template files (PLAN §5b, `docs/api.md` §21.5): one WCL file for
//! each template in the config repo.
//!
//! - `templates/<name>.wcl`: a global template.
//! - `templates/<project>/<name>.wcl`: a template of `<project>`.
//!
//! The file name is the template name. The file holds the description
//! and the spec ([`crate::agent_template::Spec`]) as top-level fields, and
//! `policy` as a block:
//!
//! ```wcl
//! description  = "Board worker on Claude Code"
//! harness      = "claude"
//! subscription = "anthropic-max-20x"
//! persona      = "worker"
//! restart      = "on-failure"
//!
//! policy {
//!   mail_to = ["lead"]
//! }
//! ```
//!
//! [`parse`] checks a file against the schema ([`schema`]: a `@document`
//! type and a `@block("policy")` type, given to WCL in the environment, so
//! a file has no `import`), then makes the agent definition from the spec
//! ([`crate::agent_template::to_def`]). Each error names the file, the
//! line, and the column. [`render`] writes a spec as a file: a header
//! comment, the fields in a fixed order, a multi-line `system_prompt` as a
//! raw heredoc.

use serde_json::{Map, Value};
use wcl_lang::ast::{BuiltinType, TypeRef};
use wcl_lang::{DecoratorBuilder, Document, Environment, TypeBuilder, TypeFieldBuilder};

use crate::agent_template::{self, Spec};

/// The dir of the template files in the config repo.
pub const DIR: &str = "templates";
/// The extension of a template file.
pub const EXTENSION: &str = "wcl";

/// The type of a field of the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    List,
    Bool,
}

/// The top-level fields, in the order that [`render`] writes them.
/// `system_prompt` comes last: it is the long one. `policy` is a block.
const FIELDS: &[(&str, Kind)] = &[
    ("description", Kind::Text),
    ("harness", Kind::Text),
    ("subscription", Kind::Text),
    ("subscription_pool", Kind::List),
    ("model", Kind::Text),
    ("prompt", Kind::Text),
    ("persona", Kind::Text),
    ("roles", Kind::List),
    ("restart", Kind::Text),
    ("idle_stop", Kind::Text),
    ("compact_on_idle", Kind::Bool),
    ("compact_max_age", Kind::Text),
    ("card_reminder", Kind::Bool),
    ("skills", Kind::List),
    ("skills_off", Kind::List),
    ("default_skills", Kind::Bool),
    ("new_session_on_prompt_change", Kind::Bool),
    ("system_prompt", Kind::Text),
];

/// The fields of the `policy` block, in order.
const POLICY_FIELDS: &[&str] = agent_template::POLICY_KEYS;

/// The block kind of the policy.
const POLICY: &str = "policy";

fn type_ref(kind: Kind) -> TypeRef {
    match kind {
        Kind::Text => TypeRef::Builtin(BuiltinType::Utf8),
        Kind::Bool => TypeRef::Builtin(BuiltinType::Bool),
        Kind::List => TypeRef::List(Box::new(TypeRef::Builtin(BuiltinType::Utf8))),
    }
}

fn wcl_type(kind: Kind) -> &'static str {
    match kind {
        Kind::Text => "utf8",
        Kind::Bool => "bool",
        Kind::List => "list<utf8>",
    }
}

/// The schema as an environment: the file needs no `import`.
fn environment() -> Environment {
    let mut env = Environment::new();
    let mut policy = TypeBuilder::new(["TemplatePolicy"]).decorator(
        DecoratorBuilder::new(["block"]).positional(wcl_lang::Value::Utf8(POLICY.into())),
    );
    for f in POLICY_FIELDS {
        policy = policy.field(TypeFieldBuilder::new(*f, type_ref(Kind::List)).optional(true));
    }
    env.add_type(policy.build());
    let mut doc =
        TypeBuilder::new(["AgentTemplate"]).decorator(DecoratorBuilder::new(["document"]));
    for (name, kind) in FIELDS {
        doc = doc.field(TypeFieldBuilder::new(*name, type_ref(*kind)).optional(true));
    }
    doc = doc.field(
        TypeFieldBuilder::new(
            POLICY,
            TypeRef::Named {
                path: vec!["TemplatePolicy".into()],
                args: Vec::new(),
            },
        )
        .optional(true)
        .decorator(
            DecoratorBuilder::new(["child"]).positional(wcl_lang::Value::Utf8(POLICY.into())),
        ),
    );
    env.add_type(doc.build());
    env
}

/// The schema as WCL text, for the docs. [`parse`] uses the same types.
pub fn schema() -> String {
    let mut s = String::from("@document\ntype AgentTemplate {\n");
    for (name, kind) in FIELDS {
        s.push_str(&format!("  {name}: {}?\n", wcl_type(*kind)));
    }
    s.push_str("  @child(\"policy\") policy: TemplatePolicy?\n}\n\n@block(\"policy\")\ntype TemplatePolicy {\n");
    for f in POLICY_FIELDS {
        s.push_str(&format!("  {f}: list<utf8>?\n"));
    }
    s.push_str("}\n");
    s
}

/// The path of a template file in the config repo:
/// `templates/<name>.wcl` or `templates/<project>/<name>.wcl`.
pub fn rel_path(project: Option<&str>, name: &str) -> String {
    match project {
        Some(p) => format!("{DIR}/{p}/{name}.{EXTENSION}"),
        None => format!("{DIR}/{name}.{EXTENSION}"),
    }
}

/// The scope and the name of a path in the config repo, when it is a
/// template file path. The names are not checked here.
pub fn key_of_rel(rel: &str) -> Option<(Option<String>, String)> {
    let rest = rel.strip_prefix(DIR)?.strip_prefix('/')?;
    let stem = rest.strip_suffix(EXTENSION)?.strip_suffix('.')?;
    match stem.split('/').collect::<Vec<_>>()[..] {
        [name] if !name.is_empty() => Some((None, name.to_owned())),
        [project, name] if !project.is_empty() && !name.is_empty() => {
            Some((Some(project.to_owned()), name.to_owned()))
        }
        _ => None,
    }
}

/// A template file, read.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub description: Option<String>,
    /// The spec as the file gives it (not in the normal form).
    pub spec: Spec,
}

/// One problem of a file, at a position (1-based; 0 when the problem has
/// no position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// The problems of a file. The text names each one as
/// `<file>:<line>:<column>: <message>`, one for each line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    pub file: String,
    pub problems: Vec<Problem>,
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, p) in self.problems.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            if p.line == 0 {
                write!(f, "{}: {}", self.file, p.message)?;
            } else {
                write!(f, "{}:{}:{}: {}", self.file, p.line, p.column, p.message)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for FileError {}

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

/// A WCL value as JSON: a text, a bool, or a list of texts.
fn json_of(v: &wcl_lang::Value, kind: Kind) -> Result<Value, String> {
    use wcl_lang::Value as W;
    let text = |v: &W| match v {
        W::Utf8(s) | W::Ascii(s) => Ok(s.clone()),
        other => Err(format!("expected a text, not `{other:?}`")),
    };
    match (kind, v) {
        (_, W::None) => Ok(Value::Null),
        (Kind::Bool, W::Bool(b)) => Ok(Value::Bool(*b)),
        (Kind::Text, v) => text(v).map(Value::String),
        (Kind::List, W::List(items)) => items
            .iter()
            .map(|i| text(i).map(Value::String))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        (k, other) => Err(format!("expected {}, not `{other:?}`", wcl_type(k))),
    }
}

/// Read a template file. `file` names the file in the errors. The spec is
/// checked as an agent definition: a missing `harness` or a bad value is
/// an error too.
pub fn parse(text: &str, file: &str) -> Result<Parsed, FileError> {
    let fail = |problems: Vec<Problem>| FileError {
        file: file.to_owned(),
        problems,
    };
    let doc = match Document::open_with(text, file, &environment()) {
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
    let mut spec = Map::new();
    let mut description = None;
    // The offset of each field, for the errors of the definition check.
    let mut at: Vec<(String, usize)> = Vec::new();
    for f in doc.fields() {
        let Some((name, kind)) = FIELDS.iter().find(|(n, _)| *n == f.name()) else {
            // The schema check reported it.
            continue;
        };
        at.push(((*name).to_owned(), f.span().start));
        let v = match f.value() {
            Ok(v) => v,
            // A schema violation is in the list already.
            Err(e) if label_offset(e).is_some() && !problems.is_empty() => continue,
            Err(e) => {
                problems.push(problem_at(text, Some(f.span().start), e.to_string()));
                continue;
            }
        };
        match json_of(v, *kind) {
            Ok(Value::Null) => {}
            Ok(Value::String(s)) if *name == "description" => description = Some(s),
            Ok(j) => {
                spec.insert((*name).to_owned(), j);
            }
            Err(e) => problems.push(problem_at(
                text,
                Some(f.span().start),
                format!("field '{name}': {e}"),
            )),
        }
    }
    let mut policies = doc.blocks().filter(|b| b.kind() == POLICY);
    if let Some(b) = policies.next() {
        at.push((POLICY.to_owned(), b.span().start));
        let mut policy = Map::new();
        for f in b.fields() {
            if !POLICY_FIELDS.contains(&f.name()) {
                continue;
            }
            at.push((format!("policy.{}", f.name()), f.span().start));
            match f.value() {
                Ok(v) => match json_of(v, Kind::List) {
                    Ok(Value::Null) => {}
                    Ok(j) => {
                        policy.insert(f.name().to_owned(), j);
                    }
                    Err(e) => problems.push(problem_at(
                        text,
                        Some(f.span().start),
                        format!("field 'policy.{}': {e}", f.name()),
                    )),
                },
                Err(e) if label_offset(e).is_some() && !problems.is_empty() => {}
                Err(e) => problems.push(problem_at(text, Some(f.span().start), e.to_string())),
            }
        }
        spec.insert(POLICY.into(), Value::Object(policy));
    }
    for extra in policies {
        problems.push(problem_at(
            text,
            Some(extra.span().start),
            "a template has one `policy` block",
        ));
    }
    if !problems.is_empty() {
        return Err(fail(problems));
    }
    if let Err(e) = agent_template::to_def(&spec, "x", "x") {
        // Find the field that the check refuses: the spec with only that
        // field (and a harness) fails the same way.
        let harness = spec
            .get("harness")
            .cloned()
            .unwrap_or_else(|| Value::String("pi".into()));
        let culprit = at.iter().find(|(key, _)| {
            let mut one = Map::new();
            one.insert("harness".into(), harness.clone());
            match key.split_once('.') {
                Some((_, pk)) => {
                    if let Some(v) = spec.get(POLICY).and_then(|p| p.get(pk)) {
                        let mut p = Map::new();
                        p.insert(pk.to_owned(), v.clone());
                        one.insert(POLICY.into(), Value::Object(p));
                    }
                }
                None => {
                    if let Some(v) = spec.get(key.as_str()) {
                        one.insert(key.clone(), v.clone());
                    }
                }
            }
            agent_template::to_def(&one, "x", "x").is_err()
        });
        let message = if spec.contains_key("harness") {
            e
        } else {
            "missing field 'harness' (claude, codex, copilot, or pi)".to_owned()
        };
        return Err(fail(vec![match culprit {
            Some((key, offset)) => problem_at(text, Some(*offset), format!("{key}: {message}")),
            None => problem_at(text, None, message),
        }]));
    }
    Ok(Parsed { description, spec })
}

/// A WCL string literal of `s`: the five escapes of a plain string.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The literal of a JSON value of a spec field.
fn literal(key: &str, v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(quote(s)),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Array(items) => {
            let parts = items
                .iter()
                .map(|i| match i {
                    Value::String(s) => Ok(quote(s)),
                    other => Err(format!("`{key}` holds `{other}`, not a text")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", parts.join(", ")))
        }
        other => Err(format!(
            "`{key}` holds `{other}`, which a template file cannot hold"
        )),
    }
}

/// `text` as a raw heredoc, indented by two spaces, when the heredoc
/// reads back as the same text: a multi-line text with a final newline,
/// no `\r`, no line of blanks only, and no indent on every line.
fn heredoc(key: &str, text: &str) -> Option<String> {
    let body = text.strip_suffix('\n')?;
    if !body.contains('\n') && body.len() < 80
        || text.contains('\r')
        || body.lines().any(|l| !l.is_empty() && l.trim().is_empty())
        || body
            .lines()
            .filter(|l| !l.is_empty())
            .all(|l| l.starts_with(char::is_whitespace))
    {
        return None;
    }
    let tag = (0..)
        .map(|i| match i {
            0 => "PROMPT".to_owned(),
            i => format!("PROMPT{i}"),
        })
        .find(|t| !body.lines().any(|l| l.trim() == t))?;
    let mut out = format!("{key} = <<'{tag}'\n");
    for l in body.lines() {
        if !l.is_empty() {
            out.push_str("  ");
            out.push_str(l);
        }
        out.push('\n');
    }
    out.push_str(&format!("  {tag}\n"));
    Some(out)
}

/// Write a template as a file. The spec should be in its normal form
/// (`agent_template::normalize`), so that one template has one text. The
/// file reads back ([`parse`]) as `description` and `spec`.
pub fn render(
    name: &str,
    project: Option<&str>,
    description: Option<&str>,
    spec: &Spec,
) -> Result<String, String> {
    if let Some(k) = spec
        .keys()
        .find(|k| *k != POLICY && !FIELDS.iter().any(|(n, _)| n == k))
    {
        return Err(format!("unknown key `{k}`"));
    }
    let scope = match project {
        Some(p) => format!("project `{p}`"),
        None => "global".to_owned(),
    };
    let mut out = format!(
        "// Terrarium agent template: {name} ({scope}).\n\
         // The file name is the template name. Edit this file by hand, or with\n\
         // `terrarium template edit` or the dashboard: they write the whole file\n\
         // again, without your comments. The fields: docs/api.md §21.5.\n\n"
    );
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut long = None;
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        fields.push(("description".into(), quote(d)));
    }
    for (key, _) in FIELDS.iter().filter(|(n, _)| *n != "description") {
        let Some(v) = spec.get(*key).filter(|v| !v.is_null()) else {
            continue;
        };
        if *key == "system_prompt"
            && let Some(text) = v.as_str()
            && let Some(h) = heredoc(key, text)
        {
            long = Some(h);
            continue;
        }
        fields.push(((*key).to_owned(), literal(key, v)?));
    }
    let width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in &fields {
        out.push_str(&format!("{k:<width$} = {v}\n"));
    }
    if let Some(h) = long {
        out.push('\n');
        out.push_str(&h);
    }
    match spec.get(POLICY) {
        None | Some(Value::Null) => {}
        Some(Value::Object(p)) => {
            if let Some(k) = p.keys().find(|k| !POLICY_FIELDS.contains(&k.as_str())) {
                return Err(format!("unknown key `policy.{k}`"));
            }
            out.push_str("\npolicy {\n");
            let width = POLICY_FIELDS
                .iter()
                .filter(|k| p.contains_key(**k))
                .map(|k| k.len())
                .max()
                .unwrap_or(0);
            for k in POLICY_FIELDS {
                if let Some(v) = p.get(*k).filter(|v| !v.is_null()) {
                    out.push_str(&format!("  {k:<width$} = {}\n", literal(k, v)?));
                }
            }
            out.push_str("}\n");
        }
        Some(other) => return Err(format!("`policy` holds `{other}`, not a table")),
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn spec(v: Value) -> Spec {
        match v {
            Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    fn round_trip(s: &Spec, description: Option<&str>) -> String {
        let text = render("worker", Some("terra"), description, s).unwrap();
        let back = parse(&text, "worker.wcl").unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(&back.spec, s, "{text}");
        assert_eq!(back.description.as_deref(), description, "{text}");
        text
    }

    #[test]
    fn every_field_goes_to_the_file_and_back() {
        let full = spec(json!({
            "harness": "claude",
            "subscription": "anthropic-max-20x",
            "subscription_pool": ["anthropic-max-20x", "anthropic-pro"],
            "model": "sonnet",
            "prompt": "worker",
            "persona": "worker",
            "roles": ["project_admin"],
            "restart": "on-failure",
            "idle_stop": "5m",
            "compact_on_idle": true,
            "compact_max_age": "6h",
            "skills": ["fern-check", "*"],
            "skills_off": ["terrarium-db"],
            "default_skills": false,
            "new_session_on_prompt_change": false,
            "system_prompt": "You are a worker.\n\n  Indented line with ${not_a_slot} and \\d.\nPROMPT\nLast line.\n",
            "policy": {"targets": ["box"], "mail_to": ["lead", "*"], "cross_project": ["ops/*"]},
        }));
        let text = round_trip(&full, Some("A \"quoted\" worker\nwith two lines"));
        assert!(text.starts_with("// Terrarium agent template: worker (project `terra`).\n"));
        // The prompt is a raw heredoc, with a tag that no line holds.
        assert!(text.contains("system_prompt = <<'PROMPT1'\n"), "{text}");
        assert!(
            text.contains("\npolicy {\n  targets       = [\"box\"]\n"),
            "{text}"
        );
        // The order is fixed.
        let harness = text.find("harness").unwrap();
        let model = text.find("model ").unwrap();
        let policy = text.find("policy {").unwrap();
        assert!(harness < model && model < policy, "{text}");
        // The same spec gives the same text.
        assert_eq!(
            text,
            render(
                "worker",
                Some("terra"),
                Some("A \"quoted\" worker\nwith two lines"),
                &full
            )
            .unwrap()
        );
    }

    #[test]
    fn texts_that_a_heredoc_cannot_hold_are_quoted() {
        for prompt in [
            "One line.",
            "Two lines,\nno final newline",
            "  every line\n  has an indent\n",
            "a line of blanks\n   \nin it\n",
            "a CR\r\nline\n",
            "\ttab first\nsecond\n",
        ] {
            let s = spec(json!({"harness": "codex", "system_prompt": prompt}));
            round_trip(&s, None);
        }
        let s = spec(json!({"harness": "pi", "policy": {}}));
        let text = round_trip(&s, None);
        assert!(text.contains("policy {\n}\n"), "{text}");
        round_trip(&spec(json!({"harness": "pi"})), None);
    }

    #[test]
    fn schema_errors_name_the_file_and_the_line() {
        let text = "harness = \"claude\"\nroles = \"x\"\nmodle = \"m\"\npolicy {\n  targetz = []\n}\nfoo { }\n";
        let e = parse(text, "templates/w.wcl").unwrap_err().to_string();
        assert!(
            e.contains("templates/w.wcl:2:1: field 'roles' declared as list<utf8>"),
            "{e}"
        );
        assert!(
            e.contains("templates/w.wcl:3:1: top-level field 'modle' is not declared"),
            "{e}"
        );
        assert!(
            e.contains("templates/w.wcl:5:3: field 'targetz' is not declared"),
            "{e}"
        );
        assert!(e.contains("templates/w.wcl:7:1: block kind 'foo'"), "{e}");

        // A syntax error.
        let e = parse("harness = \"claude\nroles = [\n", "t.wcl").unwrap_err();
        assert_eq!(e.problems[0].line, 1, "{e}");
        assert!(e.to_string().starts_with("t.wcl:1:"), "{e}");
        // A duplicate field, and two policy blocks.
        let e = parse("harness = \"a\"\nharness = \"b\"\n", "t.wcl").unwrap_err();
        assert!(
            e.to_string().contains("t.wcl:2:1: duplicate declaration"),
            "{e}"
        );
        let e = parse("harness = \"pi\"\npolicy {\n}\npolicy {\n}\n", "t.wcl").unwrap_err();
        assert!(
            e.to_string()
                .contains("t.wcl:4:1: a template has one `policy` block"),
            "{e}"
        );
    }

    #[test]
    fn definition_errors_point_at_the_field() {
        let e = parse("model = \"m\"\n", "t.wcl").unwrap_err().to_string();
        assert!(e.contains("missing field 'harness'"), "{e}");
        let e = parse("harness = \"emacs\"\n", "t.wcl")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("t.wcl:1:1: harness: "), "{e}");
        let e = parse("harness = \"pi\"\n\nrestart = \"sometimes\"\n", "t.wcl")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("t.wcl:3:1: restart: "), "{e}");
        let e = parse("harness = \"pi\"\nidle_stop = \"soon\"\n", "t.wcl")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("t.wcl:2:1: idle_stop: "), "{e}");
    }

    #[test]
    fn a_hand_written_file_may_use_wcl() {
        let text = r#"
// A comment.
let team = "lead"
let version = "k2"
harness = "pi"
model   = $"kimi-${version}"
policy {
  mail_to = [team]
}
"#;
        let p = parse(text, "t.wcl").unwrap();
        assert_eq!(
            Value::Object(p.spec),
            json!({"harness": "pi", "model": "kimi-k2", "policy": {"mail_to": ["lead"]}})
        );
    }

    #[test]
    fn paths() {
        assert_eq!(rel_path(None, "w"), "templates/w.wcl");
        assert_eq!(rel_path(Some("p"), "w"), "templates/p/w.wcl");
        assert_eq!(key_of_rel("templates/w.wcl"), Some((None, "w".into())));
        assert_eq!(
            key_of_rel("templates/p/w.wcl"),
            Some((Some("p".into()), "w".into()))
        );
        for bad in [
            "templates/a/b/c.wcl",
            "templates/w.toml",
            "prompts/w.wcl",
            "templates/.wcl",
        ] {
            assert_eq!(key_of_rel(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_schema_text_is_valid_wcl() {
        let s = schema();
        let doc = Document::open(&s, "schema.wcl").unwrap();
        assert!(doc.schema_errors().is_empty());
        for (name, _) in FIELDS {
            assert!(s.contains(&format!("  {name}: ")), "{name}");
        }
    }
}
