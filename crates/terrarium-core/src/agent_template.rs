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
    "subscription_pool",
    "model",
    "prompt",
    "system_prompt",
    "persona",
    "roles",
    "policy",
    "restart",
    "idle_stop",
    "compact_on_idle",
    "compact_max_age",
    "card_reminder",
    "skills",
    "skills_off",
    "default_skills",
    "new_session_on_prompt_change",
    TOOLS_KEY,
    GRANTS_KEY,
];

/// A key of a template spec only, not of an agent definition: the tool
/// proxies that an agent made from the template gets (GitHub #21, design
/// Q4). Only a sysadmin or the operator sets it; the daemon grants the
/// tools at the create.
pub const TOOLS_KEY: &str = "tools";

/// The tools of a spec: a sorted list of tool names with no copy. A value
/// that is not a list of valid tool names is an error.
pub fn tools_of(spec: &Spec) -> Result<Vec<String>, String> {
    let Some(v) = spec.get(TOOLS_KEY) else {
        return Ok(Vec::new());
    };
    let Value::Array(list) = v else {
        return Err("`tools` must be a list of tool names".into());
    };
    let mut out = Vec::new();
    for x in list {
        let n = x
            .as_str()
            .ok_or_else(|| "`tools` must be a list of tool names".to_owned())?;
        crate::tool::check_name(n).map_err(|e| format!("tools: {e}"))?;
        out.push(n.to_owned());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// A key of a template spec only, not of an agent definition: the secret
/// grants that an agent made from the template gets (TER-88), each
/// `<name>` or `<scope>/<name>` with an optional level (`:use`, `:run`,
/// `:run=<target>,<target>`, `:read`). The daemon checks each one against
/// the user of the template at the spawn or the create
/// (`terrariumd::spawn_rights`).
pub const GRANTS_KEY: &str = "grants";

/// At most this many `grants` on one template.
pub const MAX_GRANTS: usize = 32;

/// The grants of a spec: the entries trimmed, with no copy, in their
/// order. A value that is not a list of strings, an empty entry, or more
/// than [`MAX_GRANTS`] entries is an error. The daemon checks the syntax
/// of each entry.
pub fn grants_of(spec: &Spec) -> Result<Vec<String>, String> {
    let Some(v) = spec.get(GRANTS_KEY) else {
        return Ok(Vec::new());
    };
    let Value::Array(list) = v else {
        return Err("`grants` must be a list of secret grants".into());
    };
    let mut out: Vec<String> = Vec::new();
    for x in list {
        let g = x
            .as_str()
            .ok_or_else(|| "`grants` must be a list of secret grants".to_owned())?
            .trim();
        if g.is_empty() {
            return Err("an entry of `grants` is empty".into());
        }
        if !out.iter().any(|o| o == g) {
            out.push(g.to_owned());
        }
    }
    if out.len() > MAX_GRANTS {
        return Err(format!(
            "a template has at most {MAX_GRANTS} grants; it has {}",
            out.len()
        ));
    }
    Ok(out)
}

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
    tools_of(spec)?;
    grants_of(spec)?;
    let mut m = spec.clone();
    // Template keys, not agent fields.
    m.remove(TOOLS_KEY);
    m.remove(GRANTS_KEY);
    m.insert("name".into(), Value::String(name.to_owned()));
    m.insert("project".into(), Value::String(project.to_owned()));
    let mut def: AgentDef = serde_json::from_value(Value::Object(m)).map_err(|e| e.to_string())?;
    def.system_prompt = crate::config::clean_system_prompt(def.system_prompt)?;
    def.persona = crate::config::clean_persona(def.persona)?;
    def.skills = crate::skills::clean_list(&def.skills)?;
    def.skills_off = crate::skills::clean_off_list(&def.skills_off)?;
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
    // A template carries no spawn rights (PLAN §18, "Ephemeral agents"):
    // they are set on each agent.
    if let Some(Value::Object(p)) = m.get_mut("policy") {
        p.remove("spawn");
    }
    m
}

/// Check a spec and write it in its normal form: each default filled in,
/// no empty optional field. Two specs that make the same agent have the
/// same normal form.
pub fn normalize(spec: &Spec) -> Result<Spec, String> {
    let mut n = spec_of(&to_def(spec, "x", "x")?);
    let tools = tools_of(spec)?;
    if !tools.is_empty() {
        n.insert(TOOLS_KEY.into(), Value::from(tools));
    }
    let grants = grants_of(spec)?;
    if !grants.is_empty() {
        n.insert(GRANTS_KEY.into(), Value::from(grants));
    }
    Ok(n)
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
    m.insert("subscription_pool".into(), to(&def.subscription_pool));
    m.insert("model".into(), to(&def.model));
    m.insert("prompt".into(), to(&def.prompt));
    m.insert("system_prompt".into(), to(&def.system_prompt));
    m.insert("persona".into(), to(&def.persona));
    m.insert("roles".into(), to(&def.roles));
    // Each policy field (TER-193: the export of an agent of a template
    // dropped `spawn`, `calendar_write`, and `thread_open`). The pattern
    // names every field, so a new one does not build until it is here.
    let crate::config::Policy {
        targets,
        mail_to,
        cross_project,
        spawn,
        calendar_write,
        thread_open,
    } = &def.policy;
    let mut p = Map::new();
    p.insert("targets".into(), to(targets));
    p.insert("mail_to".into(), to(mail_to));
    p.insert("cross_project".into(), to(cross_project));
    p.insert("spawn".into(), to(spawn));
    p.insert("calendar_write".into(), Value::Bool(*calendar_write));
    p.insert("thread_open".into(), Value::Bool(*thread_open));
    m.insert("policy".into(), Value::Object(p));
    m.insert("restart".into(), to(&def.restart));
    m.insert("idle_stop".into(), to(&def.idle_stop));
    m.insert("compact_on_idle".into(), to(&def.compact_on_idle));
    m.insert("compact_max_age".into(), to(&def.compact_max_age));
    m.insert("card_reminder".into(), to(&def.card_reminder));
    m.insert("skills".into(), to(&def.skills));
    m.insert("skills_off".into(), to(&def.skills_off));
    m.insert("default_skills".into(), Value::Bool(def.default_skills));
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
            // The template sets a policy key (`spawn`) that the agent does
            // not have: a file has no `null` to drop it.
            if diff.values().any(Value::is_null) {
                return None;
            }
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

    /// PLAN §8.6: a create copies `skills_off` and `default_skills` from
    /// the template; the body wins over each.
    #[test]
    fn skill_settings_copy_from_the_template() {
        let t = spec(json!({"harness": "claude", "skills": ["a"],
                            "skills_off": ["terrarium-db"], "default_skills": false}));
        let def = to_def(&merge(&t, &Spec::new()), "w", "p").unwrap();
        assert_eq!(def.skills, ["a"]);
        assert_eq!(def.skills_off, ["terrarium-db"]);
        assert!(!def.default_skills);
        let body = spec(json!({"default_skills": true, "skills_off": null}));
        let def = to_def(&merge(&t, &body), "w", "p").unwrap();
        assert!(def.default_skills);
        assert!(def.skills_off.is_empty());
        // The normal form writes `default_skills` only when it is false.
        let n = normalize(&t).unwrap();
        assert_eq!(n.get("default_skills"), Some(&json!(false)));
        let n = normalize(&spec(json!({"harness": "claude"}))).unwrap();
        assert!(!n.contains_key("default_skills") && !n.contains_key("skills_off"));
        let e = normalize(&spec(json!({"harness": "claude", "skills_off": ["*"]}))).unwrap_err();
        assert!(e.contains("default_skills"), "{e}");
    }

    /// `tools` is a key of the template, not of the agent: it stays in the
    /// normal form, sorted and with no copy, and never reaches the agent
    /// definition. A bad list is refused.
    #[test]
    fn tools_stay_on_the_template() {
        let s = spec(json!({"harness": "claude", "tools": ["tracker-read", "deploy", "deploy"]}));
        assert_eq!(tools_of(&s).unwrap(), ["deploy", "tracker-read"]);
        let n = normalize(&s).unwrap();
        assert_eq!(n["tools"], json!(["deploy", "tracker-read"]));
        let def = to_def(&s, "w", "p").unwrap();
        assert!(!spec_of(&def).contains_key("tools"));
        assert!(
            !normalize(&spec(json!({"harness": "claude", "tools": []})))
                .unwrap()
                .contains_key("tools")
        );
        for bad in [
            json!({"harness": "claude", "tools": "deploy"}),
            json!({"harness": "claude", "tools": [1]}),
            json!({"harness": "claude", "tools": ["Bad_Name"]}),
        ] {
            assert!(to_def(&spec(bad.clone()), "w", "p").is_err(), "{bad}");
        }
    }

    /// `grants` is a key of the template, not of the agent (TER-88): it
    /// stays in the normal form, trimmed and with no copy, in its order,
    /// and never reaches the agent definition. A bad list is refused.
    #[test]
    fn grants_stay_on_the_template() {
        let s = spec(json!({"harness": "claude",
                            "grants": ["gh:run=box", " p/k:read ", "gh:run=box"]}));
        assert_eq!(grants_of(&s).unwrap(), ["gh:run=box", "p/k:read"]);
        let n = normalize(&s).unwrap();
        assert_eq!(n["grants"], json!(["gh:run=box", "p/k:read"]));
        let def = to_def(&s, "w", "p").unwrap();
        assert!(!spec_of(&def).contains_key("grants"));
        assert!(
            !normalize(&spec(json!({"harness": "claude", "grants": []})))
                .unwrap()
                .contains_key("grants")
        );
        let many: Vec<String> = (0..=MAX_GRANTS).map(|i| format!("s{i}")).collect();
        for bad in [
            json!({"harness": "claude", "grants": "gh"}),
            json!({"harness": "claude", "grants": [1]}),
            json!({"harness": "claude", "grants": [" "]}),
            json!({"harness": "claude", "grants": many}),
        ] {
            assert!(to_def(&spec(bad.clone()), "w", "p").is_err(), "{bad}");
        }
        // A create body puts its own list over the template's, or clears it.
        let m = merge(&s, &spec(json!({"grants": null})));
        assert!(grants_of(&m).unwrap().is_empty());
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
                               "system_prompt": "x".repeat(crate::config::system_prompt_max() + 1)}));
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

    /// TER-193: the overrides carry each policy field (`spawn` with its
    /// bound fields, `calendar_write`, `thread_open`), so an agent of a
    /// template keeps its spawn rights through an export.
    #[test]
    fn overrides_carry_the_spawn_policy_and_rights() {
        let t = spec(json!({"harness": "claude", "roles": ["project_admin"],
                            "policy": {"targets": ["a"]}}));
        let mut def = to_def(&t, "lead", "p").unwrap();
        def.policy.spawn = Some(Box::new(crate::config::SpawnPolicy {
            templates: vec!["card-impl".into()],
            max_concurrent: 4,
            bound: true,
            max_bound_lifetime: Some("3d".into()),
            ..Default::default()
        }));
        def.policy.thread_open = true;
        def.policy.calendar_write = true;
        let o = overrides_for(&def, &t).unwrap();
        assert_eq!(
            Value::Object(o.clone()),
            json!({"policy": {
                "spawn": {"templates": ["card-impl"], "max_concurrent": 4,
                          "bound": true, "max_bound_lifetime": "3d"},
                "calendar_write": true, "thread_open": true}})
        );
        // As a fleet entry reads it: the template, then the entry's keys
        // (a template itself has no spawn rights, so not `to_def`).
        let mut m = merge(&t, &o);
        m.insert("name".into(), json!("lead"));
        m.insert("project".into(), json!("p"));
        let back: AgentDef = serde_json::from_value(Value::Object(m)).unwrap();
        assert_eq!(back, def);
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
