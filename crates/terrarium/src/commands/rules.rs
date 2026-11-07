//! `rule` (docs/api.md §19.8, §19.9, §19.12, TER-171): the board rules of
//! a project. `ls` and `show` read the rules that run (the built-in pack
//! with a project of `board_flow = rules`, and the project's `rules.wcl`),
//! `check` lints them (or a file), `test` plans them on events of the log
//! without a write, `log` lists their runs, `enable` and `disable` switch
//! one rule, and `migrate` maps the pack to the board and switches the
//! project to rules.

use serde_json::{Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::RuleCmd;
use crate::ctx::Ctx;
use crate::output::{field, short_time};
use crate::transport::encode;

fn base(project: &str) -> String {
    format!("/api/projects/{}/rules", encode(project))
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn yes_no(v: &Value) -> &'static str {
    if *v == true { "yes" } else { "no" }
}

/// The verbs of a rule (`card.move, card.label`).
fn verbs(r: &Value) -> String {
    r["actions"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| field(x, "verb"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// `job.verdict (2 when)`.
fn on_text(r: &Value) -> String {
    let mut t = strings(&r["on"]).join(",");
    let n = r["when"].as_array().map_or(0, Vec::len);
    if n > 0 {
        t.push_str(&format!(" ({n} when)"));
    }
    t
}

const LS_COLS: &[(&str, &str)] = &[
    ("RULE", "name"),
    ("FROM", "origin"),
    ("ON", "enabled_text"),
    ("EVENTS", "on_text"),
    ("VERBS", "verbs"),
];

fn flow_line(v: &Value) -> String {
    let flow = field(v, "board_flow");
    let note = if flow == "rules" {
        "the built-in pack runs, with the project's rules.wcl"
    } else {
        "the daemon runs the card flow; only the project's rules.wcl runs"
    };
    format!("project {}: board_flow {flow} ({note})", field(v, "project"))
}

fn print_ls(ctx: &Ctx, v: &Value) {
    ctx.line(flow_line(v));
    let rows: Vec<Value> = v["rules"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            json!({
                "name": r["name"],
                "origin": r["origin"],
                "enabled_text": yes_no(&r["enabled"]),
                "on_text": on_text(r),
                "verbs": verbs(r),
            })
        })
        .collect();
    print_table(ctx, &rows, LS_COLS, "no rules");
}

const RUN_COLS: &[(&str, &str)] = &[
    ("RUN", "id"),
    ("CREATED", "created"),
    ("RULE", "rule"),
    ("EVENT", "event_seq"),
    ("CARD", "card"),
    ("STATE", "state"),
    ("STEP", "step"),
    ("REASON", "reason"),
];

fn run_rows(runs: &Value) -> Vec<Value> {
    runs.as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let mut r = r.clone();
            r["created"] = json!(short_time(&field(&r, "created_at")));
            r
        })
        .collect()
}

fn print_findings(ctx: &Ctx, findings: &Value) {
    let list = findings.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        ctx.line("lint: no findings");
        return;
    }
    for f in &list {
        ctx.line(format!(
            "lint: {} {}: {}",
            field(f, "code"),
            field(f, "column"),
            field(f, "message")
        ));
    }
}

/// The text of `FILE`, or of stdin for `-`.
fn read_text(ctx: &Ctx, file: &str) -> anyhow::Result<String> {
    if file == "-" {
        return ctx.text("-");
    }
    std::fs::read_to_string(file)
        .map_err(|e| CliError::usage(format!("cannot read `{file}`: {e}")))
}

/// Exit 1 without a message (the output said why).
fn exit_one() -> anyhow::Error {
    CliError::RemoteExit(1).into()
}

pub async fn rule(ctx: &Ctx, cmd: &RuleCmd) -> anyhow::Result<()> {
    let project = ctx.project_or(None).await?;
    let base = base(&project);
    match cmd {
        RuleCmd::Ls => {
            let v = ctx
                .api
                .get(&format!("{base}/effective"), Vec::new())
                .await?;
            ctx.emit(&v, print_ls)
        }
        RuleCmd::Show { rule } => {
            let v = ctx
                .api
                .get(&format!("{base}/effective"), Vec::new())
                .await?;
            let Some(r) = v["rules"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["name"] == rule.as_str())
                .cloned()
            else {
                return Err(CliError::usage(format!(
                    "project `{project}` runs no rule `{rule}`: see `terrarium rule ls`"
                )));
            };
            let runs = ctx
                .api
                .get(
                    &format!("{base}/runs"),
                    query([("rule", Some(rule.clone())), ("limit", Some("5".into()))]),
                )
                .await?;
            let out = json!({"rule": r, "runs": runs["runs"]});
            ctx.emit(&out, |c, o| {
                let r = &o["rule"];
                c.line(format!(
                    "rule {}  ({}, enabled {})",
                    field(r, "name"),
                    field(r, "origin"),
                    yes_no(&r["enabled"])
                ));
                if let Some(d) = r["description"].as_str() {
                    c.line(format!("description: {d}"));
                }
                c.line("");
                c.out.write_str(r["wcl"].as_str().unwrap_or_default());
                c.line("");
                c.line("last runs:");
                print_table(c, &run_rows(&o["runs"]), RUN_COLS, "no runs");
            })
        }
        RuleCmd::Check { file } => {
            let body = match file {
                Some(f) => json!({"text": read_text(ctx, f)?}),
                None => json!({}),
            };
            let v = ctx.api.post(&format!("{base}/lint"), Some(body)).await?;
            ctx.emit(&v, |c, v| {
                if file.is_some() {
                    c.line("check: the file is good");
                }
                print_findings(c, &v["findings"]);
            })?;
            if v["findings"].as_array().is_some_and(|f| !f.is_empty()) {
                return Err(exit_one());
            }
            Ok(())
        }
        RuleCmd::Test {
            seq,
            from,
            to,
            file,
        } => {
            let mut body = match (seq, from) {
                (Some(s), None) => json!({"seq": s}),
                (None, Some(f)) => json!({"from": f}),
                _ => {
                    return Err(CliError::usage(
                        "give --seq <n>, or --from <n> (and --to <n>)",
                    ));
                }
            };
            if let Some(t) = to {
                if seq.is_some() {
                    return Err(CliError::usage("--to goes with --from, not --seq"));
                }
                body["to"] = json!(t);
            }
            if let Some(f) = file {
                body["text"] = json!(read_text(ctx, f)?);
            }
            let v = ctx.api.post(&format!("{base}/dry-run"), Some(body)).await?;
            ctx.emit(&v, |c, v| {
                let plans = v["plans"].as_array().cloned().unwrap_or_default();
                c.line(format!(
                    "{} events, {} planned runs (the {} rules; nothing was written)",
                    field(v, "events"),
                    plans.len(),
                    field(v, "source")
                ));
                let rows: Vec<Value> = plans
                    .iter()
                    .map(|p| {
                        let what = match p["outcome"].as_str() {
                            Some("run") => p["actions"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .map(|x| field(x, "verb"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default(),
                            _ => field(p, "reason"),
                        };
                        json!({
                            "event": format!("{} {}", field(p, "event"), field(p, "kind")),
                            "rule": p["rule"],
                            "card": p["card"],
                            "outcome": p["outcome"],
                            "what": what,
                        })
                    })
                    .collect();
                print_table(
                    c,
                    &rows,
                    &[
                        ("EVENT", "event"),
                        ("RULE", "rule"),
                        ("CARD", "card"),
                        ("OUTCOME", "outcome"),
                        ("ACTIONS OR REASON", "what"),
                    ],
                    "no rule would run",
                );
            })
        }
        RuleCmd::Log { rule, limit } => {
            let v = ctx
                .api
                .get(
                    &format!("{base}/runs"),
                    query([("rule", rule.clone()), ("limit", Some(limit.to_string()))]),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                print_table(c, &run_rows(&v["runs"]), RUN_COLS, "no runs");
            })
        }
        RuleCmd::Enable { rule } | RuleCmd::Disable { rule } => {
            let on = matches!(cmd, RuleCmd::Enable { .. });
            let verb = if on { "enable" } else { "disable" };
            let v = ctx
                .api
                .post(
                    &format!("{base}/{}/{verb}", encode(rule)),
                    Some(json!({})),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                let state = if on { "on" } else { "off" };
                if v["changed"] == true {
                    c.line(format!("rule {rule}: {state} (rules.wcl changed)"));
                } else {
                    c.line(format!("rule {rule}: {state} already"));
                }
            })
        }
        RuleCmd::Migrate { apply, text } => {
            let v = ctx
                .api
                .post(&format!("{base}/migrate"), Some(json!({"apply": apply})))
                .await?;
            ctx.emit(&v, |c, v| {
                let changes = v["changes"].as_array().cloned().unwrap_or_default();
                if changes.is_empty() {
                    c.line("the pack fits the board: no rule needs a change");
                }
                for ch in &changes {
                    c.line(format!(
                        "{} {}: {}",
                        field(ch, "change"),
                        field(ch, "rule"),
                        strings(&ch["reasons"]).join("; ")
                    ));
                }
                print_findings(c, &v["findings"]);
                if *text {
                    c.line("--- rules.wcl ---");
                    let t = v["text"].as_str().unwrap_or_default();
                    c.out.write_str(t);
                    if !t.is_empty() && !t.ends_with('\n') {
                        c.line("");
                    }
                }
                if v["applied"] == true {
                    c.line(format!(
                        "applied: project {} runs board_flow rules{}",
                        field(v, "project"),
                        if v["changed"] == true {
                            " (rules.wcl changed)"
                        } else {
                            ""
                        }
                    ));
                } else {
                    c.line(
                        "not applied: run with --apply to store rules.wcl and switch the project to board_flow rules",
                    );
                }
            })
        }
    }
}
