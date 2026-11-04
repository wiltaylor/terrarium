//! `template` (and `admin template`): docs/api.md §21.

use anyhow::Context as _;
use serde_json::{Map, Value, json};
use terrarium_core::{agent_template, template_file};

use super::{
    persona_value, print_object, print_object_with_text, print_table, set_opt, system_prompt_value,
};
use crate::api::{CliError, query};
use crate::cli::{TemplateCmd, TemplateSpecArgs};
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

/// `/api/templates/{key}` for a ULID, `project/name`, `global:name`, or a
/// name.
fn path(key: &str) -> String {
    format!("/api/templates/{}", encode(key))
}

/// `project/name`, or `global:name` for a global template.
fn key_of(t: &Value) -> String {
    match t["project"].as_str() {
        Some(p) => format!("{p}/{}", field(t, "name")),
        None => format!("{}{}", agent_template::GLOBAL_PREFIX, field(t, "name")),
    }
}

pub async fn template(ctx: &Ctx, cmd: &TemplateCmd) -> anyhow::Result<()> {
    match cmd {
        TemplateCmd::Ls { global } => {
            let q = if *global {
                query([("scope", Some("global".to_owned()))])
            } else {
                query([("project", ctx.project.clone())])
            };
            let v = ctx.api.get("/api/templates", q).await?;
            ctx.emit(&v, print_list)
        }
        TemplateCmd::Show { template, wcl } => {
            if *wcl {
                let v = ctx
                    .api
                    .get(&format!("{}/wcl", path(template)), vec![])
                    .await?;
                return ctx.emit(&v, |c, v| {
                    c.line(
                        v["text"]
                            .as_str()
                            .unwrap_or_default()
                            .trim_end_matches('\n'),
                    );
                });
            }
            let v = ctx.api.get(&path(template), vec![]).await?;
            ctx.emit(&v, print_one)
        }
        TemplateCmd::New(a) => {
            let (spec, from_file) = spec_of(ctx, &a.spec, Map::new())?;
            if !spec.contains_key("harness") {
                return Err(CliError::usage(
                    "give --harness (claude, codex, copilot, or pi), or --from-file",
                ));
            }
            let name = match (&a.name, &a.spec.from_file) {
                (Some(n), _) => n.clone(),
                (None, Some(p)) if is_wcl(p) => p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                _ => {
                    return Err(CliError::usage(
                        "give the template name (or --from-file <name>.wcl)",
                    ));
                }
            };
            let mut body = json!({"name": name, "spec": spec});
            if a.global {
                body["project"] = Value::Null;
            } else if let Some(p) = &ctx.project {
                body["project"] = p.clone().into();
            }
            set_opt(
                &mut body,
                "description",
                a.description.clone().or(from_file),
            );
            let r = ctx.api.post("/api/templates", Some(body)).await?;
            ctx.emit(&r, |c, r| {
                c.line(format!("created template {}", key_of(r)));
            })
        }
        TemplateCmd::Edit(a) => {
            let mut body = json!({});
            set_opt(&mut body, "name", a.rename.clone());
            set_opt(&mut body, "description", a.description.clone());
            if has_spec_flags(&a.spec) {
                // The flags change the fields they name; `--from-file`
                // gives the whole spec.
                let base = if a.spec.from_file.is_some() {
                    Map::new()
                } else {
                    let now = ctx.api.get(&path(&a.template), vec![]).await?;
                    now["spec"].as_object().cloned().unwrap_or_default()
                };
                let (spec, _) = spec_of(ctx, &a.spec, base)?;
                body["spec"] = Value::Object(spec);
            }
            if body.as_object().is_some_and(Map::is_empty) {
                return Err(CliError::usage("give at least one field to change"));
            }
            let r = ctx.api.patch(&path(&a.template), body).await?;
            ctx.emit(&r, |c, r| {
                c.line(format!(
                    "updated template {} (revision {})",
                    key_of(r),
                    field(r, "revision")
                ));
            })
        }
        TemplateCmd::Rm { template } => {
            ctx.api.delete(&path(template), vec![]).await?;
            ctx.emit(&json!({"deleted": template}), |c, _| {
                c.line(format!("deleted template {template}"));
            })
        }
        TemplateCmd::FromAgent {
            agent,
            name,
            global,
            description,
        } => {
            let mut body = json!({"name": name});
            if *global {
                body["project"] = Value::Null;
            } else if let Some(p) = &ctx.project {
                body["project"] = p.clone().into();
            }
            set_opt(&mut body, "description", description.clone());
            let r = ctx
                .api
                .post(
                    &format!("/api/templates/from-agent/{}", encode(agent)),
                    Some(body),
                )
                .await?;
            ctx.emit(&r, |c, r| {
                c.line(format!("created template {} from {agent}", key_of(r)));
            })
        }
    }
}

/// A WCL template file: the extension `.wcl`.
fn is_wcl(p: &std::path::Path) -> bool {
    p.extension().is_some_and(|e| e == template_file::EXTENSION)
}

/// The spec and the description of a template file: WCL
/// (`template_file::parse`, the format of the config repo), or TOML for
/// any other extension.
pub(crate) fn read_file(
    p: &std::path::Path,
) -> anyhow::Result<(Map<String, Value>, Option<String>)> {
    let text =
        std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
    if is_wcl(p) {
        let parsed = template_file::parse(&text, &p.display().to_string())
            .map_err(|e| CliError::usage(e.to_string()))?;
        return Ok((parsed.spec, parsed.description));
    }
    let dir = p.parent().unwrap_or(std::path::Path::new(""));
    agent_template::spec_from_toml_in(&text, dir)
        .map_err(|e| CliError::usage(format!("{}: {e}", p.display())))
}

fn has_spec_flags(a: &TemplateSpecArgs) -> bool {
    a.from_file.is_some()
        || a.harness.is_some()
        || a.subscription.is_some()
        || a.model.is_some()
        || a.prompt.is_some()
        || a.system_prompt.system_prompt.is_some()
        || a.system_prompt.system_prompt_file.is_some()
        || a.system_prompt.no_system_prompt
        || a.system_prompt.persona.is_some()
        || a.restart.is_some()
        || a.idle_stop.is_some()
        || a.compact_on_idle.is_some()
        || a.compact_max_age.is_some()
        || !a.roles.is_empty()
        || !a.targets.is_empty()
        || !a.mail_to.is_empty()
        || !a.cross_project.is_empty()
        || !a.skills.is_empty()
        || a.no_skills
        || !a.skills_off.is_empty()
        || a.skills_off_clear
        || a.no_default_skills
        || a.default_skills
}

/// The spec: `base` (or the file), then the flags over it. The second
/// value is the file's `description`, if any.
/// `--no-system-prompt` sends `null`, which removes the key.
fn spec_of(
    ctx: &Ctx,
    a: &TemplateSpecArgs,
    base: Map<String, Value>,
) -> anyhow::Result<(Map<String, Value>, Option<String>)> {
    let (base, description) = match &a.from_file {
        Some(p) => read_file(p)?,
        None => (base, None),
    };
    let mut flags = Map::new();
    for (k, v) in [
        ("harness", &a.harness),
        ("subscription", &a.subscription),
        ("model", &a.model),
        ("prompt", &a.prompt),
        ("restart", &a.restart),
        ("idle_stop", &a.idle_stop),
        ("compact_max_age", &a.compact_max_age),
    ] {
        if let Some(v) = v {
            flags.insert(k.into(), v.clone().into());
        }
    }
    if let Some(v) = &a.compact_on_idle {
        flags.insert(
            "compact_on_idle".into(),
            super::agents::switch(v, "--compact-on-idle")?.into(),
        );
    }
    if let Some(v) = system_prompt_value(ctx, &a.system_prompt)? {
        flags.insert("system_prompt".into(), v);
    }
    if let Some(v) = persona_value(&a.system_prompt) {
        flags.insert("persona".into(), v);
    }
    if let Some(v) = super::replace_list("--role", &a.roles)? {
        flags.insert("roles".into(), v);
    }
    if a.no_skills {
        // `null` removes the key: no skills.
        flags.insert("skills".into(), Value::Null);
    } else if !a.skills.is_empty() {
        flags.insert("skills".into(), json!(a.skills));
    }
    if a.skills_off_clear {
        // `null` removes the key: no skill is off.
        flags.insert("skills_off".into(), Value::Null);
    } else if !a.skills_off.is_empty() {
        flags.insert("skills_off".into(), json!(a.skills_off));
    }
    if a.no_default_skills {
        flags.insert("default_skills".into(), false.into());
    } else if a.default_skills {
        // `null` removes the key: the default, true.
        flags.insert("default_skills".into(), Value::Null);
    }
    let mut policy = Map::new();
    for (k, flag, v) in [
        ("targets", "--target", &a.targets),
        ("mail_to", "--mail-to", &a.mail_to),
        ("cross_project", "--cross-project", &a.cross_project),
    ] {
        if let Some(v) = super::replace_list(flag, v)? {
            policy.insert(k.into(), v);
        }
    }
    if !policy.is_empty() {
        flags.insert("policy".into(), Value::Object(policy));
    }
    Ok((agent_template::merge(&base, &flags), description))
}

/// The SUBSCRIPTION cell: the name, `-` for the default account of the
/// harness, and `(missing)` when the subscription does not exist.
fn subscription_cell(t: &Value) -> String {
    let name = t["spec"]["subscription"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("-");
    if t["subscription_missing"].is_string() {
        format!("{name} (missing)")
    } else {
        name.to_owned()
    }
}

fn print_list(ctx: &Ctx, v: &Value) {
    let items = as_items(v);
    let rows: Vec<Value> = items
        .iter()
        .map(|t| {
            json!({
                "key": key_of(t),
                "harness": t["spec"]["harness"],
                "subscription": subscription_cell(t),
                "prompt": t["spec"]["prompt"],
                "revision": t["revision"],
                "source": t["source"],
                "description": t["description"],
            })
        })
        .collect();
    print_table(
        ctx,
        &rows,
        &[
            ("TEMPLATE", "key"),
            ("HARNESS", "harness"),
            ("SUBSCRIPTION", "subscription"),
            ("PROMPT", "prompt"),
            ("REV", "revision"),
            ("SOURCE", "source"),
            ("DESCRIPTION", "description"),
        ],
        "no templates",
    );
    // A template whose subscription does not exist makes agents that do
    // not start (issue #14): say why under the table.
    for t in items {
        if let Some(why) = t["subscription_missing"].as_str() {
            ctx.line(format!("{}: {why}", key_of(t)));
        }
    }
}

fn print_one(ctx: &Ctx, v: &Value) {
    let mut head = super::with_effective_skills_text(v);
    if let Some(o) = head.as_object_mut() {
        o.remove("spec");
        o.insert("key".into(), key_of(v).into());
    }
    print_object(ctx, &head);
    ctx.line("spec:");
    print_object_with_text(ctx, &v["spec"], "system_prompt");
}
