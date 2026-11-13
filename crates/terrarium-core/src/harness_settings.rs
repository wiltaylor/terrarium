//! The user settings of a harness (PLAN §8.2): one file for each harness
//! in the config repo, `<config repo>/harness/<harness>/<file>`.
//!
//! The adapters build the harness settings in three layers. A later layer
//! wins over an earlier one:
//!
//! 1. The user file: the packaged default, as the operator edited it.
//! 2. The per-agent options that the agent definition gives (`model`, the
//!    prompt).
//! 3. The keys that Terrarium must force: the yolo flags, the hooks, the
//!    sandbox and session paths, the quiet-start keys, telemetry off. Each
//!    adapter names them, with the reason, in its `flags.rs` or `launch.rs`.
//!
//! The JSON harnesses (Claude Code, Copilot, pi) merge with
//! [`deep_merge`]. Codex takes a TOML file, which this module reads into
//! the same JSON tree; its adapter turns the tree into `-c key=value`
//! overrides.
//!
//! A file that does not parse stops the agent start. The error names the
//! file and the line: the daemon never falls back to no settings.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::harness::HarnessId;

/// The directory of the harness files in the config repo.
pub const DIR: &str = "harness";

/// The file name of the settings of `harness`.
pub fn file_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Codex => "config.toml",
        // Grok: not read until its adapter (TER-1192) names its file.
        HarnessId::Claude | HarnessId::Copilot | HarnessId::Pi | HarnessId::Grok => "settings.json",
    }
}

/// `<root>/harness/<harness>/<file>`.
pub fn path_in(root: &Path, harness: HarnessId) -> PathBuf {
    root.join(DIR)
        .join(harness.to_string())
        .join(file_name(harness))
}

/// Read the user settings of `harness` from `path`. A missing file is
/// `Ok(None)`. A file that does not parse, or whose top level is not an
/// object (a table), is an error that names the file and the line.
pub fn load(path: &Path, harness: HarnessId) -> Result<Option<Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse(&text, harness)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Parse the text of a settings file. The error has the line (and the
/// column for JSON), and not the file name.
pub fn parse(text: &str, harness: HarnessId) -> Result<Value, String> {
    let value = match harness {
        HarnessId::Codex => {
            let table: toml::Table = toml::from_str(text).map_err(|e| {
                let line = e
                    .span()
                    .map(|s| line_of(text, s.start))
                    .map_or_else(String::new, |l| format!("line {l}: "));
                format!("{line}{}", e.message())
            })?;
            toml_to_json(toml::Value::Table(table))
        }
        _ => {
            if text.trim().is_empty() {
                return Ok(Value::Object(Map::new()));
            }
            serde_json::from_str::<Value>(text)
                .map_err(|e| format!("line {} column {}: {e}", e.line(), e.column()))?
        }
    };
    if !value.is_object() {
        return Err("line 1: the top level must be an object".into());
    }
    Ok(value)
}

/// The line (from 1) of the byte `offset` in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()
        .iter()
        .take(offset.min(text.len()))
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// A TOML value as JSON. A date or time becomes its TOML text.
pub fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

/// Lay `overlay` over `base`. Two objects merge key by key, at every
/// depth. Any other value of `overlay` (an array too) replaces the value
/// in `base`. A `null` in `overlay` removes the key.
pub fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                if v.is_null() {
                    b.remove(k);
                } else if let Some(have) = b.get_mut(k) {
                    deep_merge(have, v);
                } else {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_names_and_paths() {
        assert_eq!(file_name(HarnessId::Claude), "settings.json");
        assert_eq!(file_name(HarnessId::Codex), "config.toml");
        assert_eq!(
            path_in(Path::new("/c"), HarnessId::Copilot),
            Path::new("/c/harness/copilot/settings.json")
        );
    }

    #[test]
    fn deep_merge_merges_objects_and_replaces_the_rest() {
        let mut base = json!({"a": {"x": 1, "y": [1, 2]}, "b": true, "c": 1});
        deep_merge(
            &mut base,
            &json!({"a": {"y": [3], "z": "n"}, "b": false, "c": null}),
        );
        assert_eq!(base, json!({"a": {"x": 1, "y": [3], "z": "n"}, "b": false}));
    }

    #[test]
    fn a_broken_json_file_names_the_line() {
        let e = parse("{\n  \"a\": 1,\n  \"b\": \n}\n", HarnessId::Claude).unwrap_err();
        assert!(e.starts_with("line 4 column 1"), "{e}");
        let e = parse("[1]", HarnessId::Pi).unwrap_err();
        assert!(e.contains("must be an object"), "{e}");
        assert_eq!(parse(" \n", HarnessId::Pi).unwrap(), json!({}));
    }

    #[test]
    fn a_broken_toml_file_names_the_line() {
        let e = parse("a = 1\n\nb = = 2\n", HarnessId::Codex).unwrap_err();
        assert!(e.starts_with("line 3: "), "{e}");
        let v = parse(
            "model = \"m\"\n[tui]\nalt = true\n[t]\nwhen = 1979-05-27\n",
            HarnessId::Codex,
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"model": "m", "tui": {"alt": true}, "t": {"when": "1979-05-27"}})
        );
    }

    #[test]
    fn load_names_the_file_and_a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = path_in(dir.path(), HarnessId::Claude);
        assert_eq!(load(&p, HarnessId::Claude).unwrap(), None);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{\"a\": }").unwrap();
        let e = load(&p, HarnessId::Claude).unwrap_err();
        assert!(e.contains("harness/claude/settings.json: line 1"), "{e}");
    }
}
