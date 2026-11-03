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
                        s["default"] = if s["default_on"] == true { "on" } else { "-" }.into();
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
                        ("DEFAULT", "default"),
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
            let mut skipped = Vec::new();
            match (from_dir, description) {
                (Some(dir), _) => {
                    let s = read_skill_dir(dir)?;
                    body["skill_md"] = s.skill_md.into();
                    body["files"] = s.files;
                    skipped = s.skipped;
                }
                (None, Some(d)) => body["description"] = d.clone().into(),
                (None, None) => {
                    return Err(CliError::usage(
                        "give --description (a new SKILL.md) or --from-dir",
                    ));
                }
            }
            let mut r = ctx.api.post("/api/skills", Some(body)).await?;
            if from_dir.is_some() {
                r["skipped"] = json!(skipped);
            }
            ctx.emit(&r, |c, r| {
                c.line(format!(
                    "created skill {} ({} file(s)); commit it with `terrarium admin config commit`",
                    field(r, "name"),
                    field(r, "files")
                ));
                print_skipped(c, &skipped);
            })
        }
        SkillCmd::Import { dir, update } => import(ctx, dir, *update).await,
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
                            "via": if u["default"] == true {
                                "default"
                            } else if u["all"] == true {
                                "*"
                            } else {
                                "name"
                            },
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

fn print_skipped(ctx: &Ctx, skipped: &[String]) {
    if !skipped.is_empty() {
        ctx.line(format!("  skipped: {}", skipped.join(", ")));
    }
}

/// The child dirs of `dir` that hold a `SKILL.md`, sorted by name. A
/// hidden dir is not a skill.
fn skill_dirs(dir: &Path) -> anyhow::Result<Vec<(String, std::path::PathBuf)>> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    let mut out: Vec<(String, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().to_str()?.to_owned(), e.path())))
        .filter(|(n, p)| !n.starts_with('.') && p.join(SKILL_FILE).is_file())
        .collect();
    out.sort();
    Ok(out)
}

/// One skill of `skill import`: the result row.
async fn import_one(ctx: &Ctx, name: &str, dir: &Path, update: bool) -> Value {
    let mut row = json!({"name": name, "result": "failed"});
    if let Err(e) = core::check_name(name) {
        row["error"] = e.into();
        return row;
    }
    let s = match read_skill_dir(dir) {
        Ok(s) => s,
        Err(e) => {
            row["error"] = e.to_string().into();
            return row;
        }
    };
    row["skipped"] = json!(s.skipped);
    let body = json!({"name": name, "skill_md": s.skill_md, "files": s.files});
    let mut sent = ctx.api.post("/api/skills", Some(body.clone())).await;
    let mut result = "created";
    if update
        && let Err(e) = &sent
        && e.downcast_ref::<crate::api::ServerError>()
            .is_some_and(|e| e.status == 409)
    {
        let mut put = body;
        if let Some(o) = put.as_object_mut() {
            o.remove("name");
        }
        sent = ctx.api.put(&path(name), put).await;
        result = "updated";
    }
    match sent {
        Ok(r) => {
            row["result"] = result.into();
            row["files"] = r["files"].clone();
        }
        Err(e) => {
            let server = e.downcast_ref::<crate::api::ServerError>();
            if server.is_some_and(|s| s.status == 409) {
                row["result"] = "exists".into();
                row["error"] = "the skill exists; give --update to replace it".into();
            } else {
                row["error"] = e.to_string().into();
            }
            if let Some(p) = server
                .and_then(|s| s.details.as_ref())
                .and_then(|d| d.get("problems"))
            {
                row["problems"] = p.clone();
            }
        }
    }
    row
}

/// `skill import <dir>`: each child dir with a `SKILL.md`, one at a time.
/// A failed skill does not stop the others; the exit code is 1 when one
/// failed.
async fn import(ctx: &Ctx, dir: &Path, update: bool) -> anyhow::Result<()> {
    let dirs = skill_dirs(dir)?;
    if dirs.is_empty() {
        let hint = if dir.join(SKILL_FILE).is_file() {
            "; it is one skill: use `terrarium skill new <name> --from-dir`"
        } else {
            ""
        };
        return Err(CliError::usage(format!(
            "{} has no child dir with a {SKILL_FILE}{hint}",
            dir.display()
        )));
    }
    let mut rows = Vec::new();
    for (name, path) in &dirs {
        rows.push(import_one(ctx, name, path, update).await);
    }
    let ok = |r: &&Value| r["result"] == "created" || r["result"] == "updated";
    let failed = rows.iter().filter(|r| !ok(r)).count();
    let v = json!({"imported": rows.iter().filter(ok).count(), "failed": failed, "skills": rows});
    ctx.emit(&v, |c, _| {
        for r in &rows {
            let what = match r["result"].as_str().unwrap_or_default() {
                "created" | "updated" => {
                    format!("{} ({} file(s))", field(r, "result"), field(r, "files"))
                }
                other => format!("{other}: {}", field(r, "error")),
            };
            c.line(format!("{}: {what}", field(r, "name")));
            for p in r["problems"].as_array().into_iter().flatten() {
                c.line(format!("  {}", p.as_str().unwrap_or_default()));
            }
            let skipped: Vec<String> = r["skipped"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect();
            print_skipped(c, &skipped);
        }
        c.line(format!(
            "{} imported, {failed} failed; commit them with `terrarium admin config commit`",
            rows.len() - failed
        ));
    })?;
    if failed == 0 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "{failed} of {} skill(s) failed",
            rows.len()
        ))
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

/// A skill dir read for the API body.
#[derive(Debug)]
pub struct SkillDir {
    pub skill_md: String,
    /// The other files: a text file as a string, a binary file as
    /// `{"base64": ...}`.
    pub files: Value,
    /// The paths that the import skipped (`core::read_import_dir`).
    pub skipped: Vec<String>,
}

/// The SKILL.md and the other files of a skill dir, for the API body. The
/// skips of an import (hidden paths, Python caches, `.skillignore`) and
/// the local checks (symlinks, caps) run first; the daemon checks the
/// rest.
pub fn read_skill_dir(dir: &Path) -> anyhow::Result<SkillDir> {
    let core::Import { files, skipped } = core::read_import_dir(dir)
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
    Ok(SkillDir {
        skill_md: md,
        files: serde_json::to_value(out)?,
        skipped,
    })
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
        std::fs::create_dir_all(d.path().join("__pycache__")).unwrap();
        std::fs::write(d.path().join("__pycache__/a.pyc"), b"\0").unwrap();
        let SkillDir {
            skill_md: md,
            files,
            skipped,
        } = read_skill_dir(d.path()).unwrap();
        assert!(md.starts_with("---\nname: x"));
        assert_eq!(skipped, ["__pycache__/"]);
        assert_eq!(files["notes.md"], "n");
        assert_eq!(files["assets/a.png"]["base64"], "iVBORwA=");
        assert!(files.get(SKILL_FILE).is_none());
        assert!(read_skill_dir(&d.path().join("missing")).is_err());
    }

    #[test]
    fn import_finds_the_child_dirs_with_a_skill_md() {
        let d = tempfile::tempdir().unwrap();
        for n in ["b-skill", "a-skill", ".hidden", "no-md"] {
            std::fs::create_dir_all(d.path().join(n)).unwrap();
        }
        for n in ["b-skill", "a-skill", ".hidden"] {
            std::fs::write(d.path().join(n).join(SKILL_FILE), "x").unwrap();
        }
        std::fs::write(d.path().join("loose.md"), "x").unwrap();
        let names: Vec<String> = skill_dirs(d.path())
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["a-skill", "b-skill"]);
        assert!(skill_dirs(&d.path().join("missing")).is_err());
    }
}
