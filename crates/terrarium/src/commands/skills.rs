//! `skill`: the skill library (PLAN §8.6, docs/api.md §26).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use anyhow::Context as _;
use base64::Engine as _;
use serde_json::{Value, json};
use terrarium_core::skills::{self as core, SKILL_FILE};

use super::{print_object, print_table};
use crate::api::CliError;
use crate::cli::SkillCmd;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

fn path(name: &str) -> String {
    format!("/api/skills/{}", encode(name))
}

pub async fn skill(ctx: &Ctx, cmd: &SkillCmd) -> anyhow::Result<()> {
    match cmd {
        SkillCmd::Ls => {
            let v = ctx.api.get("/api/skills", vec![]).await?;
            ctx.emit(&v, |c, v| {
                let items: Vec<Value> = v
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|mut s| {
                        let n = s["errors"].as_array().map_or(0, Vec::len);
                        s["status"] = if n == 0 {
                            "ok".into()
                        } else {
                            format!("{n} problem(s)").into()
                        };
                        s
                    })
                    .collect();
                print_table(
                    c,
                    &items,
                    &[
                        ("NAME", "name"),
                        ("FILES", "files"),
                        ("STATUS", "status"),
                        ("DESCRIPTION", "description"),
                    ],
                    "no skills",
                );
            })
        }
        SkillCmd::Show { name } => {
            let v = ctx.api.get(&path(name), vec![]).await?;
            ctx.emit(&v, print_one)
        }
        SkillCmd::New {
            name,
            description,
            from_dir,
        } => {
            let mut body = json!({"name": name});
            match (from_dir, description) {
                (Some(dir), _) => {
                    let (md, files) = read_skill_dir(dir)?;
                    body["skill_md"] = md.into();
                    body["files"] = files;
                }
                (None, Some(d)) => body["description"] = d.clone().into(),
                (None, None) => {
                    return Err(CliError::usage(
                        "give --description (a new SKILL.md) or --from-dir",
                    ));
                }
            }
            let r = ctx.api.post("/api/skills", Some(body)).await?;
            ctx.emit(&r, |c, r| {
                c.line(format!(
                    "created skill {} ({} file(s)); commit it with `terrarium admin config commit`",
                    field(r, "name"),
                    field(r, "files")
                ));
            })
        }
        SkillCmd::Edit { name, file } => {
            let md = if file.as_os_str() == "-" {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .context("cannot read stdin")?;
                s
            } else {
                std::fs::read_to_string(file)
                    .with_context(|| format!("cannot read {}", file.display()))?
            };
            let r = ctx.api.put(&path(name), json!({"skill_md": md})).await?;
            ctx.emit(&r, |c, r| {
                c.line(format!(
                    "updated skill {}; agents get it at their next start",
                    field(r, "name")
                ));
            })
        }
        SkillCmd::Rm { name } => {
            let r = ctx.api.delete(&path(name), vec![]).await?;
            ctx.emit(&r, |c, _| c.line(format!("deleted skill {name}")))
        }
        SkillCmd::Validate { name } => {
            let items = match name {
                Some(n) => vec![ctx.api.get(&path(n), vec![]).await?],
                None => ctx
                    .api
                    .get("/api/skills", vec![])
                    .await?
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            };
            let bad: Vec<&Value> = items
                .iter()
                .filter(|s| s["errors"].as_array().is_some_and(|e| !e.is_empty()))
                .collect();
            let v = json!({"checked": items.len(), "invalid": bad});
            ctx.emit(&v, |c, _| {
                for s in &bad {
                    c.line(format!("{}:", field(s, "name")));
                    for e in s["errors"].as_array().into_iter().flatten() {
                        c.line(format!("  {}", e.as_str().unwrap_or_default()));
                    }
                }
                if bad.is_empty() {
                    c.line(format!("{} skill(s): no problems", items.len()));
                }
            })?;
            if bad.is_empty() {
                Ok(())
            } else {
                Err(CliError::usage(format!(
                    "{} skill(s) with problems; an agent does not get them",
                    bad.len()
                )))
            }
        }
        SkillCmd::Usage { name } => {
            let v = ctx
                .api
                .get(&format!("{}/usage", path(name)), vec![])
                .await?;
            ctx.emit(&v, |c, v| {
                let mut rows = Vec::new();
                for (kind, key) in [("agent", "agents"), ("template", "templates")] {
                    for u in v[key].as_array().into_iter().flatten() {
                        rows.push(json!({
                            "kind": kind,
                            "name": u["name"],
                            "via": if u["all"] == true { "*" } else { "name" },
                        }));
                    }
                }
                print_table(
                    c,
                    &rows,
                    &[("KIND", "kind"), ("NAME", "name"), ("VIA", "via")],
                    "no agent or template names this skill",
                );
            })
        }
    }
}

fn print_one(ctx: &Ctx, v: &Value) {
    let mut head = v.clone();
    let md = head
        .as_object_mut()
        .and_then(|o| {
            o.remove("file_list");
            o.remove("skill_md")
        })
        .and_then(|t| t.as_str().map(str::to_owned))
        .unwrap_or_default();
    print_object(ctx, &head);
    let files: Vec<Value> = v["file_list"].as_array().cloned().unwrap_or_default();
    if !files.is_empty() {
        ctx.line("files:");
        for f in &files {
            ctx.line(format!(
                "  {} ({} bytes)",
                field(f, "path"),
                field(f, "size")
            ));
        }
    }
    ctx.line(format!("{SKILL_FILE}:"));
    ctx.out.write_str(&md);
    if !md.ends_with('\n') {
        ctx.line("");
    }
}

/// The SKILL.md and the other files of a skill dir, for the API body: a
/// text file as a string, a binary file as `{"base64": ...}`. The local
/// checks (symlinks, caps) run first; the daemon checks the rest.
pub fn read_skill_dir(dir: &Path) -> anyhow::Result<(String, Value)> {
    let files = core::read_dir(dir)
        .map_err(|p| CliError::usage(format!("{}: {}", dir.display(), p.join("; "))))?;
    let md = files
        .get(SKILL_FILE)
        .ok_or_else(|| CliError::usage(format!("{} has no {SKILL_FILE}", dir.display())))?;
    let md = String::from_utf8(md.clone())
        .map_err(|_| CliError::usage(format!("{SKILL_FILE} is not UTF-8 text")))?;
    let mut out = BTreeMap::new();
    for (p, b) in files.iter().filter(|(p, _)| p.as_str() != SKILL_FILE) {
        let v = match std::str::from_utf8(b) {
            Ok(t) if !b.contains(&0) => Value::String(t.to_owned()),
            _ => json!({"base64": base64::engine::general_purpose::STANDARD.encode(b)}),
        };
        out.insert(p.clone(), v);
    }
    Ok((md, serde_json::to_value(out)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skill_dir_becomes_a_body() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("assets")).unwrap();
        std::fs::write(
            d.path().join(SKILL_FILE),
            "---\nname: x\ndescription: d\n---\n",
        )
        .unwrap();
        std::fs::write(d.path().join("assets/a.png"), b"\x89PNG\0").unwrap();
        std::fs::write(d.path().join("notes.md"), "n").unwrap();
        let (md, files) = read_skill_dir(d.path()).unwrap();
        assert!(md.starts_with("---\nname: x"));
        assert_eq!(files["notes.md"], "n");
        assert_eq!(files["assets/a.png"]["base64"], "iVBORwA=");
        assert!(files.get(SKILL_FILE).is_none());
        assert!(read_skill_dir(&d.path().join("missing")).is_err());
    }
}
