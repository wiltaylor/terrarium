//! `fleet`: fleet files (`docs/fleet.md`). The file content goes to the
//! daemon in the body, so these commands work in both modes.

use anyhow::Context;
use serde_json::{Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::FleetCmd;
use crate::ctx::Ctx;
use crate::output::{field, short_time};
use crate::transport::encode;

const OBJECT_COLS: &[(&str, &str)] = &[("KIND", "kind"), ("NAME", "key"), ("PROJECT", "project")];

const FLEET_COLS: &[(&str, &str)] = &[
    ("NAME", "name"),
    ("OBJECTS", "objects"),
    ("PROJECTS", "projects"),
    ("ORIGIN", "origin"),
    ("APPLIED", "applied_at"),
    ("BY", "applied_by"),
];

pub async fn fleet(ctx: &Ctx, cmd: &FleetCmd) -> anyhow::Result<()> {
    match cmd {
        FleetCmd::Validate { file, offline } => {
            let content = read(ctx, file)?;
            let v = if *offline {
                offline_validation(&content, file)
            } else {
                ctx.api
                    .post("/api/fleet/validate", Some(request(&content, file)))
                    .await?
            };
            ctx.emit(&v, print_validation)?;
            fail_on_errors(&v)
        }
        FleetCmd::Plan { file, prune } => {
            let content = read(ctx, file)?;
            let mut body = request(&content, file);
            body["prune"] = json!(prune);
            let v = ctx.api.post("/api/fleet/plan", Some(body)).await?;
            ctx.emit(&v, |c, v| print_plan(c, v, *prune, false))?;
            fail_on_errors(&v)
        }
        FleetCmd::Apply {
            file,
            prune,
            confirm,
            start,
        } => {
            let content = read(ctx, file)?;
            let mut body = request(&content, file);
            body["prune"] = json!(prune);
            body["confirm"] = json!(confirm);
            body["start"] = json!(start);
            let v = ctx.api.post("/api/fleet/apply", Some(body)).await?;
            ctx.emit(&v, |c, v| print_plan(c, v, *prune, true))
        }
        FleetCmd::Ls => {
            let v = ctx.api.get("/api/fleet", vec![]).await?;
            ctx.emit(&v, |c, v| {
                let items: Vec<Value> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| {
                        let mut r = r.clone();
                        r["projects"] = json!(join(&r["projects"]));
                        r
                    })
                    .collect();
                print_table(c, &items, FLEET_COLS, "no fleets");
            })
        }
        FleetCmd::Show { name } => {
            let v = ctx
                .api
                .get(&format!("/api/fleet/{}", encode(name)), vec![])
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("fleet:       {}", field(v, "name")));
                if v["description"].is_string() {
                    c.line(format!("description: {}", field(v, "description")));
                }
                c.line(format!("origin:      {}", field(v, "origin")));
                c.line(format!(
                    "applied:     {} by {}",
                    short_time(&field(v, "applied_at")),
                    field(v, "applied_by")
                ));
                c.line(format!("projects:    {}", join(&v["projects"])));
                c.line("");
                print_plan(c, &v["plan"], false, false);
            })
        }
        FleetCmd::Export {
            project,
            name,
            output,
        } => {
            let q = query([("project", Some(project.clone())), ("name", name.clone())]);
            let v = ctx.api.get("/api/fleet/export", q).await?;
            let content = v["content"].as_str().unwrap_or_default();
            match output {
                Some(path) => {
                    std::fs::write(path, content)
                        .with_context(|| format!("cannot write {}", path.display()))?;
                    ctx.emit(&v, |c, v| {
                        c.line(format!(
                            "fleet {} written to {}",
                            field(v, "fleet"),
                            path.display()
                        ));
                    })
                }
                None if ctx.json => ctx.emit(&v, |_, _| {}),
                None => {
                    ctx.out.write_str(content);
                    Ok(())
                }
            }
        }
    }
}

/// The file text: `-` reads stdin. Each `system_prompt_file` goes into
/// the text as `system_prompt`, relative to the file (stdin: the current
/// directory).
fn read(ctx: &Ctx, file: &str) -> anyhow::Result<String> {
    let (text, base) = if file == "-" {
        (ctx.text("-")?, std::path::PathBuf::from("."))
    } else {
        let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {file}"))?;
        let base = std::path::Path::new(file)
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        (text, base)
    };
    terrarium_core::fleet::inline_prompt_files(&text, &base).map_err(CliError::usage)
}

fn request(content: &str, file: &str) -> Value {
    let mut body = json!({"content": content});
    if file != "-" {
        body["path"] = json!(file);
    }
    body
}

/// `fleet validate --offline`: the checks that need no daemon.
fn offline_validation(content: &str, file: &str) -> Value {
    let fallback = (file != "-")
        .then(|| terrarium_core::fleet::name_from_path(std::path::Path::new(file)))
        .flatten();
    let parsed = terrarium_core::fleet::parse(content, fallback.as_deref());
    let (errors, warnings): (Vec<_>, Vec<_>) =
        parsed.issues.into_iter().partition(|i| i.is_error());
    json!({
        "fleet": parsed.fleet.as_ref().map(|f| f.name.clone()),
        "valid": errors.is_empty(),
        "errors": errors,
        "warnings": warnings,
        "objects": parsed.fleet.as_ref().map(|f| f.objects()).unwrap_or_default(),
        "references": parsed.fleet.as_ref().map(|f| f.references()).unwrap_or_default(),
    })
}

/// Exit 2 when the daemon found errors in the file.
fn fail_on_errors(v: &Value) -> anyhow::Result<()> {
    let n = v["errors"].as_array().map_or(0, Vec::len);
    if v["valid"] == false || n > 0 {
        return Err(CliError::usage(format!("the fleet file has {n} error(s)")));
    }
    Ok(())
}

fn join(v: &Value) -> String {
    let items: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned))
        .collect();
    if items.is_empty() {
        "-".into()
    } else {
        items.join(",")
    }
}

fn print_issues(ctx: &Ctx, v: &Value) {
    for (key, level) in [("errors", "error"), ("warnings", "warning")] {
        for i in v[key].as_array().into_iter().flatten() {
            let at = i["at"].as_str().unwrap_or_default();
            let msg = i["message"].as_str().unwrap_or_default();
            if at.is_empty() {
                ctx.line(format!("{level}: {msg}"));
            } else {
                ctx.line(format!("{level}: {at}: {msg}"));
            }
        }
    }
}

fn print_validation(ctx: &Ctx, v: &Value) {
    let objects: Vec<Value> = v["objects"].as_array().cloned().unwrap_or_default();
    print_table(ctx, &objects, OBJECT_COLS, "no objects");
    let refs = &v["references"];
    for (key, what) in [
        ("subscriptions", "subscription"),
        ("targets", "target"),
        ("prompts", "prompt template"),
    ] {
        for (name, users) in refs[key].as_object().into_iter().flatten() {
            ctx.line(format!(
                "uses {what} {name} (outside the file): {}",
                join(users)
            ));
        }
    }
    print_issues(ctx, v);
    let errors = v["errors"].as_array().map_or(0, Vec::len);
    let warnings = v["warnings"].as_array().map_or(0, Vec::len);
    let name = v["fleet"].as_str().unwrap_or("-");
    if errors == 0 {
        ctx.line(format!(
            "fleet {name} is valid: {} object(s), {warnings} warning(s)",
            objects.len()
        ));
    } else {
        ctx.line(format!(
            "fleet {name} is not valid: {errors} error(s), {warnings} warning(s)"
        ));
    }
}

/// A short JSON form of a changed value.
fn value(v: &Value) -> String {
    match v {
        Value::Null => "(none)".into(),
        other => other.to_string(),
    }
}

/// The plan as a list of objects with a mark, as `terraform plan` shows
/// it: `+` create, `~` update, `-` delete or release, `=` unchanged, `!`
/// conflict. `applied` prints the result of an apply.
pub fn print_plan(ctx: &Ctx, v: &Value, prune: bool, applied: bool) {
    if v["valid"] == false {
        print_issues(ctx, v);
        ctx.line(format!(
            "fleet {} is not valid; nothing is planned",
            v["fleet"].as_str().filter(|s| !s.is_empty()).unwrap_or("-")
        ));
        return;
    }
    let s = &v["summary"];
    let n = |k: &str| s[k].as_u64().unwrap_or(0);
    let mut head = format!(
        "fleet {}: {} to create, {} to update, {} to delete, {} unchanged",
        field(v, "fleet"),
        n("create"),
        n("update"),
        n("delete") + n("release"),
        n("unchanged")
    );
    if n("conflict") > 0 {
        head.push_str(&format!(", {} conflict(s)", n("conflict")));
    }
    if n("drift") > 0 {
        head.push_str(&format!(", {} drifted", n("drift")));
    }
    ctx.line(head);
    let entries = v["entries"].as_array().cloned().unwrap_or_default();
    if !entries.is_empty() {
        ctx.line("");
    }
    let width = entries
        .iter()
        .map(|e| e["key"].as_str().map_or(0, str::len))
        .max()
        .unwrap_or(0);
    let mut held_back = 0;
    for e in &entries {
        let action = e["action"].as_str().unwrap_or_default();
        let mark = match action {
            "create" => "+",
            "update" => "~",
            "delete" | "release" => "-",
            "conflict" => "!",
            _ => "=",
        };
        let mut notes = Vec::new();
        if action == "release" {
            notes.push("release: stays as an API object".to_owned());
        }
        if e["adopt"] == true {
            notes.push("adopt".to_owned());
        }
        if let Some(d) = e["drift"].as_array().filter(|d| !d.is_empty()) {
            notes.push(format!("drift: {}", join(&Value::Array(d.clone()))));
        }
        if matches!(action, "delete" | "release") && !prune {
            held_back += 1;
            notes.push("kept: needs --prune".to_owned());
        } else if action == "delete" && e["live"] == true {
            notes.push("live session: needs --confirm".to_owned());
        }
        if let Some(r) = e["reason"].as_str() {
            notes.push(if action == "conflict" {
                format!("conflict: {r}")
            } else {
                r.to_owned()
            });
        }
        if applied && let Some(r) = e["result"].as_str() {
            notes.push(r.to_owned());
        }
        let mut line = format!(
            "  {mark} {:<12} {:<width$}",
            e["kind"].as_str().unwrap_or_default(),
            e["key"].as_str().unwrap_or_default()
        );
        if !notes.is_empty() {
            line.push_str("  ");
            line.push_str(&notes.join("; "));
        }
        ctx.line(line.trim_end());
        if matches!(action, "create" | "update") {
            for c in e["changes"].as_array().into_iter().flatten() {
                ctx.line(format!(
                    "      {}: {} -> {}",
                    field(c, "field"),
                    value(&c["from"]),
                    value(&c["to"])
                ));
            }
        }
    }
    print_issues(ctx, v);
    if held_back > 0 {
        ctx.line(format!(
            "{held_back} object(s) are no longer in the file; add --prune to remove them"
        ));
    }
    if applied {
        ctx.line("apply complete");
        for c in v["checkout"].as_array().into_iter().flatten() {
            if c["error"].is_null() {
                ctx.line(format!(
                    "cloned {} into {}",
                    field(c, "repo"),
                    field(c, "agent")
                ));
            }
        }
        for a in v["started"].as_array().into_iter().flatten() {
            ctx.line(format!("started {}", a.as_str().unwrap_or_default()));
        }
        for f in v["failures"].as_array().into_iter().flatten() {
            ctx.line(format!("failed: {}", f.as_str().unwrap_or_default()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use clap::Parser;
    use serde_json::{Value, json};

    use crate::api::exit_code_of;
    use crate::cli::Cli;
    use crate::ctx::Ctx;
    use crate::transport::mock::MockTransport;
    use crate::transport::{ApiRequest, Method};

    const FILE: &str = "name = \"t\"\n[[projects]]\nname = \"p\"\n";

    async fn run(
        args: &[&str],
        replies: Vec<(u16, Value)>,
    ) -> (anyhow::Result<()>, String, Vec<ApiRequest>) {
        let cli =
            Cli::try_parse_from(std::iter::once("terrarium").chain(args.iter().copied())).unwrap();
        let mock = MockTransport::new();
        for (s, b) in replies {
            mock.reply(s, b);
        }
        let mut ctx = Ctx::for_test(Arc::new(mock.clone()), None).with_stdin(FILE);
        ctx.json = cli.json;
        let result = crate::commands::run(&ctx, &cli.command).await;
        (result, ctx.out.captured(), mock.requests())
    }

    fn plan_reply() -> Value {
        json!({
            "fleet": "terrarium",
            "valid": true,
            "errors": [],
            "warnings": [{"level": "warning", "at": "prompts", "message": "cannot check prompt template `x`"}],
            "summary": {"create": 1, "update": 1, "unchanged": 1, "delete": 1, "release": 0, "conflict": 1, "drift": 1},
            "entries": [
                {"kind": "project", "key": "terrarium", "project": "terrarium", "action": "unchanged"},
                {"kind": "agent", "key": "terrarium/new", "project": "terrarium", "action": "create",
                 "changes": [{"field": "harness", "from": null, "to": "claude"}]},
                {"kind": "agent", "key": "terrarium/lead", "project": "terrarium", "action": "update",
                 "drift": ["prompt"],
                 "changes": [{"field": "prompt", "from": "default", "to": "project-lead"},
                             {"field": "model", "from": null, "to": "opus"}]},
                {"kind": "agent", "key": "terrarium/old", "project": "terrarium", "action": "delete", "live": true},
                {"kind": "target", "key": "box", "action": "conflict", "reason": "declared in targets/box.toml; the definition file wins"}
            ]
        })
    }

    #[tokio::test]
    async fn plan_prints_marks_changes_and_notes() {
        let (r, out, reqs) = run(&["fleet", "plan", "-"], vec![(200, plan_reply())]).await;
        r.unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, Method::Post);
        assert_eq!(reqs[0].path, "/api/fleet/plan");
        assert_eq!(reqs[0].body, Some(json!({"content": FILE, "prune": false})));
        let want = "\
fleet terrarium: 1 to create, 1 to update, 1 to delete, 1 unchanged, 1 conflict(s), 1 drifted

  = project      terrarium
  + agent        terrarium/new
      harness: (none) -> \"claude\"
  ~ agent        terrarium/lead  drift: prompt
      prompt: \"default\" -> \"project-lead\"
      model: (none) -> \"opus\"
  - agent        terrarium/old   kept: needs --prune
  ! target       box             conflict: declared in targets/box.toml; the definition file wins
warning: prompts: cannot check prompt template `x`
1 object(s) are no longer in the file; add --prune to remove them
";
        assert_eq!(out, want);

        let (r, out, reqs) = run(
            &["fleet", "plan", "-", "--prune"],
            vec![(200, plan_reply())],
        )
        .await;
        r.unwrap();
        assert_eq!(reqs[0].body.as_ref().unwrap()["prune"], true);
        assert!(
            out.contains("  - agent        terrarium/old   live session: needs --confirm\n"),
            "{out}"
        );
        assert!(!out.contains("add --prune"), "{out}");
    }

    #[tokio::test]
    async fn plan_prints_project_create_fields_including_delivery() {
        let reply = json!({
            "fleet": "t", "valid": true,
            "summary": {"create": 1},
            "entries": [{
                "kind": "project", "key": "p", "action": "create",
                "changes": [
                    {"field": "description", "from": null, "to": "Project p"},
                    {"field": "delivery", "from": null,
                     "to": {"recipe": "sh -c 'just build && just test'", "target": "build-box"}}
                ]
            }]
        });
        let (r, out, _) = run(&["fleet", "plan", "-"], vec![(200, reply.clone())]).await;
        r.unwrap();
        assert_eq!(
            out,
            "\
fleet t: 1 to create, 0 to update, 0 to delete, 0 unchanged

  + project      p
      description: (none) -> \"Project p\"
      delivery: (none) -> {\"recipe\":\"sh -c 'just build && just test'\",\"target\":\"build-box\"}
"
        );

        let (r, out, _) = run(
            &["--json", "fleet", "plan", "-"],
            vec![(200, reply.clone())],
        )
        .await;
        r.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), reply);
    }

    #[tokio::test]
    async fn an_invalid_plan_prints_the_errors_and_exits_2() {
        let reply = json!({
            "fleet": "t", "valid": false, "entries": [], "summary": {}, "warnings": [],
            "errors": [{"level": "error", "at": "projects.p.agents.a", "message": "the agent has no `harness`"}]
        });
        let (r, out, _) = run(&["fleet", "plan", "-"], vec![(200, reply)]).await;
        assert_eq!(exit_code_of(&r.unwrap_err()), 2);
        assert_eq!(
            out,
            "error: projects.p.agents.a: the agent has no `harness`\nfleet t is not valid; nothing is planned\n"
        );
    }

    #[tokio::test]
    async fn apply_sends_the_flags_and_prints_results() {
        let mut reply = plan_reply();
        reply["entries"][3]["result"] = json!("done");
        reply["started"] = json!(["terrarium/new"]);
        reply["failures"] = json!(["clone of x into terrarium/new: no credential"]);
        let (r, out, reqs) = run(
            &["fleet", "apply", "-", "--prune", "--confirm", "--start"],
            vec![(200, reply)],
        )
        .await;
        r.unwrap();
        assert_eq!(reqs[0].path, "/api/fleet/apply");
        assert_eq!(
            reqs[0].body,
            Some(json!({"content": FILE, "prune": true, "confirm": true, "start": true}))
        );
        assert!(
            out.contains("  - agent        terrarium/old   live session: needs --confirm; done\n"),
            "{out}"
        );
        assert!(out.ends_with(
            "apply complete\nstarted terrarium/new\nfailed: clone of x into terrarium/new: no credential\n"
        ), "{out}");
    }

    #[tokio::test]
    async fn validate_lists_objects_and_fails_on_errors() {
        let ok = json!({
            "fleet": "t", "valid": true, "errors": [], "warnings": [],
            "objects": [{"kind": "project", "key": "p", "project": "p"},
                        {"kind": "agent", "key": "p/a", "project": "p"}],
            "references": {"subscriptions": {"max": ["p/a"]}, "targets": {}, "prompts": {}}
        });
        let (r, out, reqs) = run(&["fleet", "validate", "-"], vec![(200, ok)]).await;
        r.unwrap();
        assert_eq!(reqs[0].path, "/api/fleet/validate");
        assert!(out.contains("KIND"), "{out}");
        assert!(
            out.contains("uses subscription max (outside the file): p/a\n"),
            "{out}"
        );
        assert!(
            out.ends_with("fleet t is valid: 2 object(s), 0 warning(s)\n"),
            "{out}"
        );

        let bad = json!({"fleet": "t", "valid": false, "objects": [], "references": {}, "warnings": [],
                         "errors": [{"level": "error", "at": "", "message": "bad TOML"}]});
        let (r, out, _) = run(&["fleet", "validate", "-"], vec![(200, bad)]).await;
        assert_eq!(exit_code_of(&r.unwrap_err()), 2);
        assert!(out.contains("error: bad TOML\n"), "{out}");

        // Offline: no request at all.
        let (r, out, reqs) = run(&["fleet", "validate", "-", "--offline"], vec![]).await;
        r.unwrap();
        assert!(reqs.is_empty());
        assert!(
            out.ends_with("fleet t is valid: 1 object(s), 0 warning(s)\n"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn ls_show_and_export() {
        let rows = json!([{"name": "terrarium", "objects": 6, "projects": ["terrarium", "ops"],
                           "origin": "api", "applied_at": "2026-10-24T10:00:00.000Z", "applied_by": "operator"}]);
        let (r, out, reqs) = run(&["fleet", "ls"], vec![(200, rows)]).await;
        r.unwrap();
        assert_eq!(reqs[0].path, "/api/fleet");
        assert!(out.contains("terrarium,ops"), "{out}");

        let mut shown = json!({"name": "terrarium", "origin": "file:fleets/t.toml", "projects": ["terrarium"],
                               "applied_at": "2026-10-24T10:00:00.000Z", "applied_by": "system"});
        shown["plan"] = plan_reply();
        let (r, out, reqs) = run(&["fleet", "show", "terrarium"], vec![(200, shown)]).await;
        r.unwrap();
        assert_eq!(reqs[0].path, "/api/fleet/terrarium");
        assert!(
            out.starts_with("fleet:       terrarium\norigin:      file:fleets/t.toml\n"),
            "{out}"
        );
        assert!(
            out.contains("~ agent        terrarium/lead  drift: prompt"),
            "{out}"
        );

        let (r, out, reqs) = run(
            &["fleet", "export", "hand", "--name", "h"],
            vec![(
                200,
                json!({"fleet": "h", "project": "hand", "content": "version = 1\nname = \"h\"\n"}),
            )],
        )
        .await;
        r.unwrap();
        assert_eq!(reqs[0].path, "/api/fleet/export");
        assert_eq!(
            reqs[0].query,
            [
                ("project".to_owned(), "hand".to_owned()),
                ("name".to_owned(), "h".to_owned())
            ]
        );
        assert_eq!(out, "version = 1\nname = \"h\"\n");

        // `admin fleet` is the same command.
        let (r, _, reqs) = run(&["admin", "fleet", "ls"], vec![(200, json!([]))]).await;
        r.unwrap();
        assert_eq!(reqs[0].path, "/api/fleet");
    }
}
