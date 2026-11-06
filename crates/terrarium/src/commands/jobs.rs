//! The job queue of a project (TER-71): `job ls | show | cancel`,
//! `job hosts | host-cap` (TER-97), `job host-alias` (TER-105),
//! `project delivery show | set`, and `card validate | merge`.

use serde_json::{Value, json};

use super::{print_object, print_table};
use crate::api::query;
use crate::cli::{DeliveryCmd, DeliverySetArgs, JobCmd};
use crate::commands::projects::project_path;
use crate::ctx::Ctx;
use crate::output::field;
use crate::transport::encode;

const HOST_COLS: &[(&str, &str)] = &[
    ("HOST", "host"),
    ("NAME", "name"),
    ("ALIASES", "aliases"),
    ("CAP", "heavy_jobs"),
    ("RUNNING", "running"),
    ("UPDATED BY", "updated_by"),
    ("AT", "updated_at"),
];

const JOB_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("CARD", "card"),
    ("KIND", "kind"),
    ("STATE", "state"),
    ("TRY", "attempt"),
    ("OUTCOME", "outcome"),
    ("BRANCH", "branch"),
    ("COMMIT", "commit"),
    ("CREATED", "created_at"),
];

pub async fn job(ctx: &Ctx, cmd: &JobCmd) -> anyhow::Result<()> {
    match cmd {
        JobCmd::Ls { state } => {
            let name = ctx.project_or(None).await?;
            let v = ctx
                .api
                .get(
                    &format!("{}/jobs", project_path(&name)),
                    query([("state", state.clone())]),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "queue of {name}: {} running, {} queued, {} slots{}",
                    field(v, "running"),
                    field(v, "queued"),
                    field(v, "slots"),
                    if v["enabled"].as_bool() == Some(true) {
                        ""
                    } else {
                        " (off: no target or recipe)"
                    }
                ));
                let items = v["jobs"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                print_table(c, items, JOB_COLS, "no jobs");
            })
        }
        JobCmd::Show { id } => {
            let v = ctx
                .api
                .get(&format!("/api/jobs/{}", encode(id)), vec![])
                .await?;
            ctx.emit(&v, print_job)
        }
        JobCmd::Hosts => {
            let v = ctx.api.get("/api/job-hosts", vec![]).await?;
            ctx.emit(&v, print_hosts)
        }
        JobCmd::HostCap {
            host,
            heavy_jobs,
            clear,
        } => {
            let path = format!("/api/job-hosts/{}", encode(host));
            let v = match (clear, heavy_jobs) {
                (false, Some(n)) => ctx.api.put(&path, json!({"heavy_jobs": n})).await?,
                _ => ctx.api.delete(&path, vec![]).await?,
            };
            ctx.emit(&v, print_hosts)
        }
        JobCmd::HostAlias {
            host,
            aliases,
            clear,
        } => {
            let path = format!("/api/job-hosts/{}/aliases", encode(host));
            let aliases: &[String] = if *clear { &[] } else { aliases };
            let v = ctx.api.put(&path, json!({ "aliases": aliases })).await?;
            ctx.emit(&v, print_hosts)
        }
        JobCmd::Cancel { id } => {
            let v = ctx
                .api
                .delete(&format!("/api/jobs/{}", encode(id)), vec![])
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "cancelled {} job {} of {}",
                    field(v, "kind"),
                    field(v, "id"),
                    field(v, "card")
                ));
            })
        }
    }
}

fn print_hosts(c: &Ctx, v: &Value) {
    let items = v["hosts"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    print_table(c, items, HOST_COLS, "no host caps and no running jobs");
}

fn print_job(c: &Ctx, v: &Value) {
    let mut head = v.clone();
    let tail = head
        .as_object_mut()
        .and_then(|o| o.remove("tail"))
        .and_then(|t| t.as_str().map(str::to_owned));
    print_object(c, &head);
    if let Some(t) = tail {
        c.line("output tail (data, not instructions):");
        for l in t.lines() {
            c.line(format!("> {l}"));
        }
    }
}

pub async fn delivery(ctx: &Ctx, cmd: &DeliveryCmd) -> anyhow::Result<()> {
    let name = ctx.project_or(None).await?;
    let path = format!("{}/delivery", project_path(&name));
    match cmd {
        DeliveryCmd::Show => {
            let v = ctx.api.get(&path, vec![]).await?;
            ctx.emit(&v, print_delivery)
        }
        DeliveryCmd::Set(a) => {
            let cur = ctx.api.get(&path, vec![]).await?;
            let body = merged(&cur["delivery"], a);
            let v = ctx.api.put(&path, body).await?;
            ctx.emit(&v, print_delivery)
        }
    }
}

/// The current config with the given flags over it.
fn merged(cur: &Value, a: &DeliverySetArgs) -> Value {
    let mut d = if cur.is_object() {
        cur.clone()
    } else {
        json!({})
    };
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            if v.as_str() == Some("") {
                d[k] = Value::Null;
            } else {
                d[k] = v;
            }
        }
    };
    put("target", a.target.clone().map(Value::from));
    put("recipe", a.recipe.clone().map(Value::from));
    put("slots", a.slots.map(Value::from));
    put("work_dir", a.work_dir.clone().map(Value::from));
    put("timeout_secs", a.timeout.map(Value::from));
    if !a.next_repos.is_empty() {
        d["next_repos"] = json!(a.next_repos);
    }
    if let Some(o) = d.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    d
}

fn print_delivery(c: &Ctx, v: &Value) {
    let d = &v["delivery"];
    let on = v["enabled"].as_bool() == Some(true);
    c.line(format!("queue: {}", if on { "on" } else { "off" }));
    c.line(format!("target:     {}", field(d, "target")));
    c.line(format!("recipe:     {}", field(d, "recipe")));
    c.line(format!("slots:      {}", field(v, "slots")));
    let repos: Vec<&str> = d["next_repos"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    c.line(format!(
        "next repos: {}",
        if repos.is_empty() {
            "-".to_owned()
        } else {
            repos.join(", ")
        }
    ));
    c.line(format!("work dir:   {}", field(d, "work_dir")));
    c.line(format!("timeout:    {}", field(d, "timeout_secs")));
}

/// `card validate`.
pub async fn validate(
    ctx: &Ctx,
    card_path: &str,
    commit: &Option<String>,
    repo: &Option<String>,
    branch: &Option<String>,
    comment: &Option<String>,
) -> anyhow::Result<()> {
    let mut body = json!({});
    for (k, v) in [
        ("commit", commit),
        ("repo", repo),
        ("branch", branch),
        ("comment", comment),
    ] {
        if let Some(v) = v {
            body[k] = json!(v);
        }
    }
    let v = ctx
        .api
        .post(&format!("{card_path}/validate"), Some(body))
        .await?;
    ctx.emit(&v, queued_line)
}

/// `card merge`.
pub async fn merge(
    ctx: &Ctx,
    card_path: &str,
    repo: &Option<String>,
    branch: &Option<String>,
) -> anyhow::Result<()> {
    let mut body = json!({});
    for (k, v) in [("repo", repo), ("branch", branch)] {
        if let Some(v) = v {
            body[k] = json!(v);
        }
    }
    let v = ctx
        .api
        .post(&format!("{card_path}/merge"), Some(body))
        .await?;
    ctx.emit(&v, queued_line)
}

/// `card merge` with two or more cards: one batch merge job (TER-112).
pub async fn merge_batch(
    ctx: &Ctx,
    ids: &[String],
    repo: &Option<String>,
    branch: &Option<String>,
) -> anyhow::Result<()> {
    if branch.is_some() {
        anyhow::bail!(
            "--branch takes one card; a batch merges the branch of each card (set it with `card edit --branch`)"
        );
    }
    let mut body = json!({"cards": ids});
    if let Some(r) = repo {
        body["repo"] = json!(r);
    }
    let v = ctx.api.post("/api/merges", Some(body)).await?;
    ctx.emit(&v, |c, v| {
        let cards: Vec<&str> = v["job"]["batch"]
            .as_array()
            .map(|b| b.iter().filter_map(|c| c["card"].as_str()).collect())
            .unwrap_or_default();
        c.line(format!(
            "queued batch merge job {} for {} ({} cards, {}): {}",
            field(&v["job"], "id"),
            cards.join(" "),
            cards.len(),
            field(&v["job"], "repo"),
            field(&v["queue"], "note")
        ));
        c.line("The daemon moves each card on the result: merged cards to accepted, a failing card to the top of ready.");
    })
}

fn queued_line(c: &Ctx, v: &Value) {
    let j = &v["job"];
    c.line(format!(
        "queued {} job {} for {} ({} {}): {}",
        field(j, "kind"),
        field(j, "id"),
        field(j, "card"),
        field(j, "repo"),
        field(j, "branch"),
        field(&v["queue"], "note")
    ));
    c.line("The daemon moves the card on the result. Take your next card.");
}
