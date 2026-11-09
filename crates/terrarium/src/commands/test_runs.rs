//! `test`: test runs (PLAN §15.5, docs/api.md §29). `push` uploads a
//! test result file; the daemon finds the format and parses it.

use anyhow::Context;
use serde_json::{Value, json};

use super::print_table;
use crate::api::{CliError, query};
use crate::cli::TestCmd;
use crate::commands::artifacts::base64;
use crate::commands::projects::project_path;
use crate::commands::system::as_items;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

/// The largest file the CLI sends (the daemon's limit).
pub const MAX_FILE: u64 = 20 * 1024 * 1024;

fn run_path(id: &str) -> String {
    format!("/api/test-runs/{}", encode(id))
}

fn n(v: &Value, key: &str) -> i64 {
    v["counts"][key].as_i64().unwrap_or(0)
}

/// `120 passed, 3 failed, 1 errored, 2 skipped` (the zero counts after
/// `passed` stay out).
pub fn counts_text(v: &Value) -> String {
    let mut parts = vec![format!("{} passed", n(v, "passed"))];
    for k in ["failed", "errored", "skipped"] {
        if n(v, k) > 0 {
            parts.push(format!("{} {k}", n(v, k)));
        }
    }
    parts.join(", ")
}

/// `5.3s`, `120ms`, `2m 5s`.
pub fn duration_text(ms: Option<i64>) -> String {
    match ms {
        None => "-".into(),
        Some(ms) if ms < 1000 => format!("{ms}ms"),
        Some(ms) if ms < 60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        Some(ms) => format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

/// The dashboard link: the daemon URL and the page path, when the CLI
/// talks to the daemon over HTTP; else the path.
fn link(ctx: &Ctx, path: &str) -> String {
    let base = ctx.api.describe();
    if base.starts_with("http://") || base.starts_with("https://") {
        format!("{}{path}", base.trim_end_matches('/'))
    } else {
        path.to_owned()
    }
}

fn mark(status: &str) -> &'static str {
    match status {
        "passed" => "ok",
        "failed" => "FAILED",
        "errored" => "ERROR",
        "skipped" => "skip",
        _ => "-",
    }
}

/// The body of `POST /api/projects/{p}/test-runs`.
#[allow(clippy::too_many_arguments)]
pub fn push_body(
    path: &std::path::Path,
    format: &str,
    name: Option<&str>,
    cards: &[String],
    labels: &[String],
    commit: Option<&str>,
    branch: Option<&str>,
) -> anyhow::Result<Value> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if meta.is_dir() {
        return Err(CliError::usage(format!(
            "{} is a folder; give a test result file",
            path.display()
        )));
    }
    if meta.len() > MAX_FILE {
        return Err(CliError::usage(format!(
            "{} is {} bytes; test result files are capped at 20 MiB",
            path.display(),
            meta.len()
        )));
    }
    let data = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "results".into());
    let mut b = json!({"file_name": file_name, "format": format});
    // Text goes as text; anything else (UTF-16 TRX) as base64.
    match String::from_utf8(data) {
        Ok(text) => b["content"] = json!(text),
        Err(e) => b["data_base64"] = json!(base64(e.as_bytes())),
    }
    if let Some(n) = name {
        b["name"] = json!(n);
    }
    if !cards.is_empty() {
        b["cards"] = json!(cards);
    }
    if !labels.is_empty() {
        b["labels"] = json!(labels);
    }
    if let Some(c) = commit {
        b["commit"] = json!(c);
    }
    if let Some(br) = branch {
        b["branch"] = json!(br);
    }
    Ok(b)
}

fn summary_line(v: &Value) -> String {
    let tool = v["tool"]
        .as_str()
        .map(|t| format!(", {t}"))
        .unwrap_or_default();
    format!(
        "{} {} ({}{tool}): {} in {}",
        field(v, "id"),
        field(v, "name"),
        field(v, "format"),
        counts_text(v),
        duration_text(v["duration_ms"].as_i64())
    )
}

fn print_links(ctx: &Ctx, v: &Value) {
    let cards = as_items(&v["cards"]);
    let labels: Vec<String> = as_items(&v["labels"])
        .iter()
        .map(|x| x.as_str().unwrap_or("-").to_owned())
        .collect();
    if !cards.is_empty() || !labels.is_empty() {
        let cards: Vec<String> = cards
            .iter()
            .map(|c| c.as_str().unwrap_or("-").to_owned())
            .collect();
        ctx.line(format!(
            "linked: cards {}  labels {}",
            if cards.is_empty() {
                "-".into()
            } else {
                cards.join(", ")
            },
            if labels.is_empty() {
                "-".into()
            } else {
                labels.join(", ")
            }
        ));
    }
}

fn print_case(ctx: &Ctx, c: &Value) {
    let class = c["classname"]
        .as_str()
        .map(|k| format!("{k} :: "))
        .unwrap_or_default();
    ctx.line(format!(
        "  [{}] {} / {class}{}  ({})",
        mark(c["status"].as_str().unwrap_or_default()),
        field(c, "suite"),
        field(c, "name"),
        duration_text(c["duration_ms"].as_i64())
    ));
    if let Some(m) = c["message"].as_str() {
        for l in m.lines().take(6) {
            ctx.line(format!("      {l}"));
        }
    }
}

fn print_diffs(ctx: &Ctx, title: &str, v: &Value, key: &str) {
    let n = v["counts"][key].as_i64().unwrap_or(0);
    if n == 0 {
        return;
    }
    ctx.line(format!("{title} ({n}):"));
    for d in as_items(&v[key]) {
        let class = d["classname"]
            .as_str()
            .map(|k| format!("{k} :: "))
            .unwrap_or_default();
        ctx.line(format!(
            "  {} / {class}{}  ({} -> {})",
            field(d, "suite"),
            field(d, "name"),
            field(d, "base_status"),
            field(d, "status")
        ));
        if key == "newly_failing"
            && let Some(m) = d["message"].as_str().and_then(|m| m.lines().next())
        {
            ctx.line(format!("      {m}"));
        }
    }
    let shown = as_items(&v[key]).len() as i64;
    if shown < n {
        ctx.line(format!("  ... and {} more", n - shown));
    }
}

pub async fn test(ctx: &Ctx, cmd: &TestCmd) -> anyhow::Result<()> {
    match cmd {
        TestCmd::Push {
            file,
            format,
            name,
            cards,
            labels,
            commit,
            branch,
            fail_on_failures,
        } => {
            let p = ctx.project_or(None).await?;
            let body = push_body(
                file,
                format,
                name.as_deref(),
                cards,
                labels,
                commit.as_deref(),
                branch.as_deref(),
            )?;
            let v = ctx
                .api
                .post(&format!("{}/test-runs", project_path(&p)), Some(body))
                .await?;
            let url = link(ctx, v["url"].as_str().unwrap_or_default());
            ctx.emit(&v, |c, v| {
                c.line(format!("pushed {}", summary_line(v)));
                print_links(c, v);
                for note in as_items(&v["notes"]) {
                    c.line(format!("note: {}", note.as_str().unwrap_or_default()));
                }
                c.line(format!("dashboard: {url}"));
            })?;
            let failing = n(&v, "failed") + n(&v, "errored");
            if *fail_on_failures && failing > 0 {
                return Err(CliError::TestsFailed(failing).into());
            }
            Ok(())
        }
        TestCmd::Ls {
            card,
            label,
            name,
            failed,
            limit,
            all,
        } => {
            let p = ctx.project_or(None).await?;
            let q = query([
                ("card", card.clone()),
                ("label", label.clone()),
                ("name", name.clone()),
                ("status", failed.then(|| "failed".to_owned())),
            ]);
            let items = ctx
                .api
                .list(
                    &format!("{}/test-runs", project_path(&p)),
                    q,
                    (!*all).then_some(*limit),
                )
                .await?;
            let rows: Vec<Value> = items
                .iter()
                .map(|v| {
                    let mut r = v.clone();
                    r["result"] = json!(counts_text(v));
                    r["took"] = json!(duration_text(v["duration_ms"].as_i64()));
                    r
                })
                .collect();
            let v = Value::Array(items);
            ctx.emit(&v, |c, _| {
                print_table(
                    c,
                    &rows,
                    &[
                        ("ID", "id"),
                        ("NAME", "name"),
                        ("STATUS", "status"),
                        ("RESULT", "result"),
                        ("TIME", "took"),
                        ("FORMAT", "format"),
                        ("COMMIT", "commit"),
                        ("CARDS", "cards"),
                        ("CREATED", "created_at"),
                    ],
                    "no test runs",
                );
            })
        }
        TestCmd::Show { id, failed } => {
            let mut q = vec![("limit".to_owned(), "5000".to_owned())];
            if *failed {
                q.push(("status".into(), "failed".into()));
            }
            let v = ctx.api.get(&run_path(id), q).await?;
            let url = link(ctx, v["url"].as_str().unwrap_or_default());
            ctx.emit(&v, |c, v| {
                c.line(format!("{}  [{}]", summary_line(v), field(v, "status")));
                c.line(format!(
                    "commit: {}  branch: {}  file: {}  by {} at {}",
                    field(v, "commit"),
                    field(v, "branch"),
                    field(v, "file_name"),
                    field(v, "created_by_name"),
                    field(v, "created_at")
                ));
                print_links(c, v);
                for note in as_items(&v["notes"]) {
                    c.line(format!("note: {}", note.as_str().unwrap_or_default()));
                }
                c.line("suites:");
                for s in as_items(&v["suites"]) {
                    c.line(format!(
                        "  {}: {} ({})",
                        field(s, "name"),
                        counts_text(s),
                        duration_text(s["duration_ms"].as_i64())
                    ));
                }
                let cases = as_items(&v["cases"]);
                // Without --failed, the list shows the failing cases only
                // when there are some; else every case.
                let show: Vec<&Value> = if *failed {
                    cases.iter().collect()
                } else {
                    let bad: Vec<&Value> = cases
                        .iter()
                        .filter(|k| matches!(k["status"].as_str(), Some("failed" | "errored")))
                        .collect();
                    if bad.is_empty() {
                        cases.iter().collect()
                    } else {
                        bad
                    }
                };
                if !show.is_empty() {
                    c.line("cases:");
                }
                for k in &show {
                    print_case(c, k);
                }
                let total = v["cases_total"].as_i64().unwrap_or(0);
                if (cases.len() as i64) < total {
                    c.line(format!(
                        "({} of {total} cases; see the dashboard or --json)",
                        cases.len()
                    ));
                }
                c.line(format!("dashboard: {url}"));
            })
        }
        TestCmd::Compare { id, with } => {
            let v = ctx
                .api
                .get(
                    &format!("{}/compare", run_path(id)),
                    vec![("with".into(), with.clone())],
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!("run:  {}", summary_line(&v["run"])));
                if v["base"].is_null() {
                    c.line(format!(
                        "base: none (no earlier run named `{}`)",
                        field(&v["run"], "name")
                    ));
                    return;
                }
                c.line(format!("base: {}", summary_line(&v["base"])));
                print_diffs(c, "newly failing", v, "newly_failing");
                print_diffs(c, "fixed", v, "fixed");
                print_diffs(c, "still failing", v, "still_failing");
                print_diffs(c, "new tests", v, "new_tests");
                print_diffs(c, "removed tests", v, "removed_tests");
                let all = &v["counts"];
                if all.as_object().is_some_and(|m| m.values().all(|x| x == 0)) {
                    c.line("no change in the results");
                }
            })
        }
        TestCmd::Link { id, cards, labels } | TestCmd::Unlink { id, cards, labels } => {
            if cards.is_empty() && labels.is_empty() {
                return Err(CliError::usage("give --card or --label"));
            }
            let add = matches!(cmd, TestCmd::Link { .. });
            let body = if add {
                json!({"add_cards": cards, "add_labels": labels})
            } else {
                json!({"remove_cards": cards, "remove_labels": labels})
            };
            let v = ctx
                .api
                .patch(&format!("{}/links", run_path(id)), body)
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "{} {}",
                    if add { "linked" } else { "unlinked" },
                    field(v, "id")
                ));
                print_links(c, v);
            })
        }
        TestCmd::Rm { id } => {
            ctx.api.delete(&run_path(id), vec![]).await?;
            ctx.emit(&json!({"deleted": id}), |c, _| {
                c.line(format!("deleted {id}"));
            })
        }
    }
}
