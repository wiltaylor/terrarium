//! Agent templates (PLAN §5b, `docs/api.md` §21): a named set of agent
//! settings that a create copies.
//!
//! A template's `spec` is an [`AgentDef`] without `name` and `project`.
//! It is a JSON object, not a struct of its own, so the template and the
//! agent definition share one schema: [`to_def`] checks a spec by making
//! the agent definition from it.
//!
//! A create with a template copies the values: the agent keeps no live
//! link. [`merge`] puts the fields of the create body over the spec:
//!
//! - A field in the body wins over the same field of the spec.
//! - `policy` merges one level deeper: each of `targets`, `mail_to`, and
//!   `cross_project` in the body wins over the same key of the spec. A
//!   list is not merged; the body's list replaces the spec's list.
//! - `null` removes the field, so the agent gets the default (for
//!   example `"model": null` clears the template's model).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::AgentDef;

/// A template spec: the fields of an agent definition without `name` and
/// `project`.
pub type Spec = Map<String, Value>;

/// The keys of a spec: the agent definition keys without `name` and
/// `project`.
pub const SPEC_KEYS: &[&str] = &[
    "harness",
    "subscription",
    "model",
    "prompt",
    "system_prompt",
    "roles",
    "policy",
    "restart",
    "new_session_on_prompt_change",
];

/// The keys of `policy`.
pub const POLICY_KEYS: &[&str] = &["targets", "mail_to", "cross_project"];

/// The prefix that forces the global template: `global:worker`.
pub const GLOBAL_PREFIX: &str = "global:";

/// A template in a project file (`[[templates]]`) or a fleet file
/// (`[[templates]]` for a global one, `[[projects.templates]]` for a
/// project one). The spec keys sit next to `name` and `description`, as
/// in an agent entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(flatten)]
    pub spec: Spec,
}

/// Which template a template name picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// `global:<name>`: the global template only.
    Global(&'a str),
    /// `<name>`: the template of the agent's project, else the global one.
    Nearest(&'a str),
}

/// Read a template name as a create gives it.
pub fn lookup(key: &str) -> Lookup<'_> {
    match key.trim().strip_prefix(GLOBAL_PREFIX) {
        Some(name) => Lookup::Global(name.trim()),
        None => Lookup::Nearest(key.trim()),
    }
}

/// Refuse the keys that a spec cannot have.
pub fn check_keys(spec: &Spec) -> Result<(), String> {
    for k in spec.keys() {
        match k.as_str() {
            "name" | "project" => {
                return Err(format!(
                    "a template has no `{k}`; the create of each agent gives it"
                ));
            }
            "template" => return Err("a template cannot name another template".into()),
            k if SPEC_KEYS.contains(&k) => {}
            k => {
                return Err(format!(
                    "unknown key `{k}`; known keys: {}",
                    SPEC_KEYS.join(", ")
                ));
            }
        }
    }
    if let Some(Value::Object(p)) = spec.get("policy")
        && let Some(k) = p.keys().find(|k| !POLICY_KEYS.contains(&k.as_str()))
    {
        return Err(format!(
            "unknown key `policy.{k}`; known keys: {}",
            POLICY_KEYS.join(", ")
        ));
    }
    Ok(())
}

/// The agent definition of `spec` for the agent `name` in `project`. The
/// error is the reason that the spec is not a valid definition, for
/// example a missing `harness`.
pub fn to_def(spec: &Spec, name: &str, project: &str) -> Result<AgentDef, String> {
    check_keys(spec)?;
    let mut m = spec.clone();
    m.insert("name".into(), Value::String(name.to_owned()));
    m.insert("project".into(), Value::String(project.to_owned()));
    let mut def: AgentDef = serde_json::from_value(Value::Object(m)).map_err(|e| e.to_string())?;
    def.system_prompt = crate::config::clean_system_prompt(def.system_prompt)?;
    Ok(def)
}

/// The spec of an agent definition: its fields without `name` and
/// `project`, as the definition writes them (no empty optional field).
pub fn spec_of(def: &AgentDef) -> Spec {
    let mut m = match serde_json::to_value(def) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    m.remove("name");
    m.remove("project");
    m
}

/// Check a spec and write it in its normal form: each default filled in,
/// no empty optional field. Two specs that make the same agent have the
/// same normal form.
pub fn normalize(spec: &Spec) -> Result<Spec, String> {
    Ok(spec_of(&to_def(spec, "x", "x")?))
}

/// Read a TOML agent definition (an `agents/<name>.toml`, or a template
/// table) as a spec: `name` and `project` go, and `description` comes
/// back on its own.
pub fn spec_from_toml(text: &str) -> Result<(Spec, Option<String>), String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.message().trim().to_owned())?;
    Ok(spec_from_table(table))
}

/// [`spec_from_toml`] for a file in `dir`: a `system_prompt_file` in it
/// is read, relative to `dir`, into `system_prompt`.
pub fn spec_from_toml_in(
    text: &str,
    dir: &std::path::Path,
) -> Result<(Spec, Option<String>), String> {
    let mut table: toml::Table = toml::from_str(text).map_err(|e| e.message().trim().to_owned())?;
    crate::config::inline_system_prompt_file(&mut table, dir)?;
    Ok(spec_from_table(table))
}

fn spec_from_table(table: toml::Table) -> (Spec, Option<String>) {
    let mut m = match serde_json::to_value(table) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    m.remove("name");
    m.remove("project");
    let description = m
        .remove("description")
        .and_then(|d| d.as_str().map(str::to_owned));
    (m, description)
}

/// Put `over` over `base` (the rules are in the module docs).
pub fn merge(base: &Spec, over: &Spec) -> Spec {
    let mut out = base.clone();
    for (k, v) in over {
        match (k.as_str(), v, out.get_mut(k)) {
            (_, Value::Null, _) => {
                out.remove(k);
            }
            ("policy", Value::Object(p), Some(Value::Object(have))) => {
                for (pk, pv) in p {
                    if pv.is_null() {
                        have.remove(pk);
                    } else {
                        have.insert(pk.clone(), pv.clone());
                    }
                }
            }
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// Every field of a definition, with `null` for an absent optional field
/// and the default lists and flags written out.
fn full(def: &AgentDef) -> Spec {
    fn to<T: Serialize>(v: &T) -> Value {
        serde_json::to_value(v).unwrap_or(Value::Null)
    }
    let mut m = Map::new();
    m.insert("harness".into(), to(&def.harness));
    m.insert("subscription".into(), to(&def.subscription));
    m.insert("model".into(), to(&def.model));
    m.insert("prompt".into(), to(&def.prompt));
    m.insert("system_prompt".into(), to(&def.system_prompt));
    m.insert("roles".into(), to(&def.roles));
    let mut p = Map::new();
    p.insert("targets".into(), to(&def.policy.targets));
    p.insert("mail_to".into(), to(&def.policy.mail_to));
    p.insert("cross_project".into(), to(&def.policy.cross_project));
    m.insert("policy".into(), Value::Object(p));
    m.insert("restart".into(), to(&def.restart));
    m.insert(
        "new_session_on_prompt_change".into(),
        Value::Bool(def.new_session_on_prompt_change),
    );
    m
}

/// The fields of `def` that differ from the template `spec`: the keys an
/// agent entry with `template` must set to make `def`. `None` when no
/// entry can make it: the template sets `subscription`, `model`,
/// `prompt`, or `system_prompt`, and `def` does not (a file has no
/// `null`).
pub fn overrides_for(def: &AgentDef, spec: &Spec) -> Option<Spec> {
    let base = full(&to_def(spec, &def.name, &def.project).ok()?);
    let want = full(def);
    let mut out = Map::new();
    for (k, v) in &want {
        let had = base.get(k).unwrap_or(&Value::Null);
        if v == had {
            continue;
        }
        if v.is_null() {
            return None;
        }
        if let (Value::Object(w), Value::Object(h)) = (v, had) {
            let diff: Map<String, Value> = w
                .iter()
                .filter(|(pk, pv)| h.get(pk.as_str()) != Some(pv))
                .map(|(pk, pv)| (pk.clone(), pv.clone()))
                .collect();
            out.insert(k.clone(), Value::Object(diff));
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::{Policy, RestartPolicy};
    use crate::harness::HarnessId;
    use crate::role::Role;

    fn spec(v: Value) -> Spec {
        match v {
            Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn a_spec_is_an_agent_definition_without_name_and_project() {
        let s = spec(
            json!({"harness": "pi", "model": "m", "roles": ["project_admin"],
                            "policy": {"targets": ["box"]}}),
        );
        let def = to_def(&s, "w1", "terra").unwrap();
        assert_eq!(def.name, "w1");
        assert_eq!(def.project, "terra");
        assert_eq!(def.harness, HarnessId::Pi);
        assert_eq!(def.roles, [Role::ProjectAdmin]);
        assert_eq!(def.policy.mail_to, ["*"]);
        // The normal form fills the defaults and drops the empty fields.
        let n = normalize(&s).unwrap();
        assert_eq!(
            Value::Object(n),
            json!({"harness": "pi", "model": "m", "roles": ["project_admin"],
                   "policy": {"targets": ["box"], "mail_to": ["*"]}, "restart": "on-failure"})
        );
        for (bad, why) in [
            (json!({"harness": "pi", "name": "x"}), "has no `name`"),
            (json!({"harness": "pi", "project": "x"}), "has no `project`"),
            (
                json!({"harness": "pi", "template": "t"}),
                "another template",
            ),
            (json!({"harness": "pi", "env": {}}), "unknown key `env`"),
            (json!({"harness": "pi", "policy": {"x": 1}}), "policy.x"),
            (json!({"model": "m"}), "harness"),
            (json!({"harness": "emacs"}), "emacs"),
        ] {
            let e = normalize(&spec(bad.clone())).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn merge_is_field_by_field_and_policy_key_by_key() {
        let base = spec(
            json!({"harness": "claude", "model": "m1", "prompt": "worker",
                               "roles": ["project_admin"],
                               "policy": {"targets": ["a"], "mail_to": ["lead"]}}),
        );
        let over = spec(json!({"model": "m2", "prompt": null, "roles": [],
                               "policy": {"targets": ["b", "c"]}}));
        let m = merge(&base, &over);
        assert_eq!(
            Value::Object(m),
            json!({"harness": "claude", "model": "m2", "roles": [],
                   "policy": {"targets": ["b", "c"], "mail_to": ["lead"]}})
        );
        // A policy in the body with no policy in the spec goes in whole.
        let m = merge(
            &spec(json!({"harness": "pi"})),
            &spec(json!({"policy": {"mail_to": []}})),
        );
        assert_eq!(m["policy"], json!({"mail_to": []}));
        // A null policy key goes back to its default.
        let m = merge(&base, &spec(json!({"policy": {"mail_to": null}})));
        let def = to_def(&m, "w", "p").unwrap();
        assert_eq!(def.policy.mail_to, ["*"]);
        assert_eq!(def.policy.targets, ["a"]);
    }

    #[test]
    fn a_definition_file_reads_as_a_spec() {
        let (s, d) = spec_from_toml(
            "name = \"lead\"\nproject = \"p\"\ndescription = \"x\"\nharness = \"pi\"\n",
        )
        .unwrap();
        assert_eq!(Value::Object(s), json!({"harness": "pi"}));
        assert_eq!(d.as_deref(), Some("x"));
        assert!(spec_from_toml("harness = ").is_err());
    }

    #[test]
    fn system_prompt_is_a_spec_key() {
        let t = spec(json!({"harness": "codex", "system_prompt": "Base."}));
        let def = to_def(&t, "w", "p").unwrap();
        assert_eq!(def.system_prompt.as_deref(), Some("Base."));
        // An empty text is no system prompt; the normal form drops it.
        let e = spec(json!({"harness": "codex", "system_prompt": " "}));
        assert_eq!(
            Value::Object(normalize(&e).unwrap()),
            json!({"harness": "codex", "restart": "on-failure", "policy": {"targets": [], "mail_to": ["*"]}})
        );
        // A body `null` clears the template's text.
        let m = merge(&t, &spec(json!({"system_prompt": null})));
        assert_eq!(to_def(&m, "w", "p").unwrap().system_prompt, None);
        // An agent without the template's text: no entry can say that.
        let mut other = def.clone();
        other.system_prompt = None;
        assert_eq!(overrides_for(&other, &t), None);
        other.system_prompt = Some("Own.".into());
        assert_eq!(
            Value::Object(overrides_for(&other, &t).unwrap()),
            json!({"system_prompt": "Own."})
        );
        let long = spec(json!({"harness": "codex",
                               "system_prompt": "x".repeat(crate::config::SYSTEM_PROMPT_MAX + 1)}));
        assert!(normalize(&long).is_err());
    }

    #[test]
    fn global_prefix() {
        assert_eq!(lookup("worker"), Lookup::Nearest("worker"));
        assert_eq!(lookup("global:worker"), Lookup::Global("worker"));
    }

    #[test]
    fn overrides_make_the_definition_again() {
        let t = spec(json!({"harness": "claude", "prompt": "worker",
                            "policy": {"targets": ["a"], "cross_project": ["ops/*"]}}));
        let mut def = to_def(&t, "w1", "p").unwrap();
        assert_eq!(overrides_for(&def, &t).unwrap(), Map::new());
        def.model = Some("m".into());
        def.policy.cross_project.clear();
        def.restart = RestartPolicy::Never;
        def.new_session_on_prompt_change = false;
        let o = overrides_for(&def, &t).unwrap();
        assert_eq!(
            Value::Object(o.clone()),
            json!({"model": "m", "policy": {"cross_project": []}, "restart": "never",
                   "new_session_on_prompt_change": false})
        );
        assert_eq!(to_def(&merge(&t, &o), "w1", "p").unwrap(), def);
        // The template has a prompt and the agent has none: no entry can
        // say that.
        def.prompt = None;
        assert_eq!(overrides_for(&def, &t), None);
        assert_eq!(
            def.policy,
            Policy {
                cross_project: vec![],
                ..def.policy.clone()
            }
        );
    }

    #[test]
    fn a_template_def_reads_flat_from_toml() {
        let text = r#"
name = "worker"
description = "A board worker"
harness = "claude"
prompt = "worker"
[policy]
targets = ["box"]
"#;
        let d: TemplateDef = toml::from_str(text).unwrap();
        assert_eq!(d.name, "worker");
        assert_eq!(d.description.as_deref(), Some("A board worker"));
        assert_eq!(d.spec["harness"], "claude");
        assert_eq!(d.spec["policy"], json!({"targets": ["box"]}));
        let again: TemplateDef = toml::from_str(&toml::to_string(&d).unwrap()).unwrap();
        assert_eq!(d, again);
    }
}
