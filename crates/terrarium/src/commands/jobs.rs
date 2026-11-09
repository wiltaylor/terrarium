//! The job queue of a project (TER-71): `job ls | show | cancel`,
//! `job stats` (TER-135),
//! `job hosts | host-cap` (TER-97), `job host-alias` (TER-105),
//! `project delivery show | set`, and `card validate | merge`.

use serde_json::{Value, json};

use super::{print_object, print_table};
use crate::api::{CliError, query};
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
        JobCmd::Stats { since } => {
            let name = ctx.project_or(None).await?;
            let since = since
                .as_deref()
                .map(|s| super::system::parse_since(s, chrono::Local::now()))
                .transpose()?;
            let v = ctx
                .api
                .get(
                    &format!("{}/jobs/stats", project_path(&name)),
                    query([("since", since)]),
                )
                .await?;
            ctx.emit(&v, print_stats)
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

/// The most failed targets that `job stats` prints.
const STATS_TARGETS: usize = 10;

/// `passed/ended (rate%)` of a first-attempt block.
fn rate(v: &Value) -> String {
    let pct = v["pass_rate"]
        .as_f64()
        .map_or_else(|| "-".to_owned(), |r| format!("{:.0}%", r * 100.0));
    format!("{pct} ({}/{})", field(v, "passed"), field(v, "ended"))
}

fn print_stats(c: &Ctx, v: &Value) {
    c.line(format!(
        "jobs of {} since {}: {}",
        field(v, "project"),
        field(v, "since"),
        field(v, "jobs")
    ));
    c.line(format!(
        "first-attempt pass rate: {}",
        rate(&v["first_attempt"])
    ));
    c.line(format!(
        "  validate {}, merge {}",
        rate(&v["validate"]),
        rate(&v["merge"])
    ));
    c.line(format!(
        "reruns passed: {} (a failure the rerun did not repeat)",
        rate(&v["reruns"])
    ));
    c.line(format!(
        "conflicts {}, could not run {}, cancelled {}, open {}, split halves {}",
        field(v, "conflicts"),
        field(v, "errors"),
        field(v, "cancelled"),
        field(v, "open"),
        field(v, "splits")
    ));
    let steps = v["failed_steps"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !steps.is_empty() {
        let list: Vec<String> = steps
            .iter()
            .map(|s| format!("{} {}", field(s, "step"), field(s, "failed")))
            .collect();
        c.line(format!("failed steps: {}", list.join(", ")));
    }
    let targets = v["failed_targets"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if targets.len() > STATS_TARGETS {
        c.line(format!(
            "failed test targets (top {STATS_TARGETS} of {}, --json for all):",
            targets.len()
        ));
    } else {
        c.line("failed test targets:");
    }
    if v["untargeted_test_fails"].as_u64().unwrap_or(0) > 0 {
        c.line(format!(
            "test-step fails with no target in the tail (clipped): {}",
            field(v, "untargeted_test_fails")
        ));
    }
    print_table(
        c,
        &targets[..targets.len().min(STATS_TARGETS)],
        &[
            ("TARGET", "target"),
            ("FAILED", "failed"),
            ("FIRST TRY", "first_attempt"),
        ],
        "no failed test targets",
    );
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
            let body = merged(&cur["delivery"], a)?;
            let v = ctx.api.put(&path, body).await?;
            ctx.emit(&v, print_delivery)
        }
    }
}

/// The current config with the given flags over it.
fn merged(cur: &Value, a: &DeliverySetArgs) -> anyhow::Result<Value> {
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
    if a.clear_merge_secrets || !a.merge_secrets.is_empty() {
        let mut refs = serde_json::Map::new();
        for entry in &a.merge_secrets {
            let (name, reference) = entry
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--merge-secret needs NAME=@secret:name[.field]"))?;
            if name.is_empty() || !reference.starts_with("@secret:") {
                anyhow::bail!("--merge-secret needs NAME=@secret:name[.field]");
            }
            refs.insert(name.to_owned(), Value::from(reference));
        }
        d["merge_secrets"] = Value::Object(refs);
    }
    merged_docker_ci(&mut d, a)?;
    if let Some(o) = d.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    Ok(d)
}

/// The six fields of the daemon's `docker_ci` object, with the flag that
/// sets each. The object is all-or-nothing: the API needs every field, so
/// a flag left out keeps the stored value, and the first set needs all
/// six (docs/api.md 35.1).
const DOCKER_CI_FIELDS: [(&str, &str); 6] = [
    ("destination", "--docker-ci-destination"),
    ("port", "--docker-ci-port"),
    ("known_hosts", "--docker-ci-known-hosts"),
    ("reset_destination", "--docker-ci-reset-destination"),
    ("reset_port", "--docker-ci-reset-port"),
    ("reset_known_hosts", "--docker-ci-reset-known-hosts"),
];

/// Lay the `--docker-ci-*` flags over `d`, or remove the object with
/// `--clear-docker-ci`. With no flag the object stays as it is.
fn merged_docker_ci(d: &mut Value, a: &DeliverySetArgs) -> anyhow::Result<()> {
    if a.clear_docker_ci {
        if let Some(o) = d.as_object_mut() {
            o.remove("docker_ci");
        }
        return Ok(());
    }
    let given = [
        a.docker_ci_destination.clone().map(Value::from),
        a.docker_ci_port.map(Value::from),
        a.docker_ci_known_hosts.clone().map(Value::from),
        a.docker_ci_reset_destination.clone().map(Value::from),
        a.docker_ci_reset_port.map(Value::from),
        a.docker_ci_reset_known_hosts.clone().map(Value::from),
    ];
    if given.iter().all(Option::is_none) {
        return Ok(());
    }
    let mut docker = if d["docker_ci"].is_object() {
        d["docker_ci"].clone()
    } else {
        json!({})
    };
    for ((field, _), v) in DOCKER_CI_FIELDS.iter().zip(given) {
        if let Some(v) = v {
            docker[field] = v;
        }
    }
    let missing: Vec<&str> = DOCKER_CI_FIELDS
        .iter()
        .filter(|(f, _)| docker[f].is_null())
        .map(|(_, flag)| *flag)
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "docker_ci needs all six values, also set {}",
            missing.join(", ")
        );
    }
    d["docker_ci"] = docker;
    Ok(())
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
    let docker = &d["docker_ci"];
    if docker.is_object() {
        c.line(format!(
            "docker ci:  {}:{} (reset {}:{})",
            field(docker, "destination"),
            field(docker, "port"),
            field(docker, "reset_destination"),
            field(docker, "reset_port")
        ));
        c.line(format!("docker pin: {}", field(docker, "known_hosts")));
        c.line(format!(
            "reset pin:  {}",
            field(docker, "reset_known_hosts")
        ));
    } else {
        c.line("docker ci:  -");
    }
}

/// `card validate`.
pub async fn validate(
    ctx: &Ctx,
    card_path: &str,
    commit: &Option<String>,
    repo: &Option<String>,
    branch: &Option<String>,
    comment: &Option<String>,
    eta: &Option<String>,
) -> anyhow::Result<()> {
    let eta = planned(eta)?;
    let mut body = json!({});
    for (k, v) in [
        ("commit", commit),
        ("repo", repo),
        ("branch", branch),
        ("comment", comment),
        ("eta", &eta),
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
    eta: &Option<String>,
) -> anyhow::Result<()> {
    let eta = planned(eta)?;
    let mut body = json!({});
    for (k, v) in [("repo", repo), ("branch", branch), ("eta", &eta)] {
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
    eta: &Option<String>,
) -> anyhow::Result<()> {
    let eta = planned(eta)?;
    if branch.is_some() {
        anyhow::bail!(
            "--branch takes one card; a batch merges the branch of each card (set it with `card edit --branch`)"
        );
    }
    let mut body = json!({"cards": ids});
    if let Some(r) = repo {
        body["repo"] = json!(r);
    }
    if let Some(e) = eta {
        body["eta"] = json!(e);
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

/// Check the planned duration of a job here, for a usage error before the
/// call (TER-155 D2): a duration such as `2h`, not a time, at most the ETA
/// limit. The daemon checks it again.
fn planned(eta: &Option<String>) -> anyhow::Result<Option<String>> {
    let Some(v) = eta else {
        return Ok(None);
    };
    let d = terrarium_core::eta::parse_duration(v).map_err(|e| {
        CliError::usage(format!(
            "--eta: {e}; a job takes a duration from its start, not a time"
        ))
    })?;
    if d > chrono::Duration::days(terrarium_core::eta::MAX_DAYS) {
        return Err(CliError::usage(format!(
            "--eta `{v}`: at most {} days",
            terrarium_core::eta::MAX_DAYS
        )));
    }
    Ok(Some(v.trim().to_owned()))
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[derive(Parser)]
    struct SetArgs {
        #[command(flatten)]
        delivery: super::DeliverySetArgs,
    }

    fn parse_set(flags: &[&str]) -> Result<SetArgs, clap::Error> {
        SetArgs::try_parse_from(std::iter::once("test").chain(flags.iter().copied()))
    }

    /// A complete `docker_ci` object as the daemon stores it, with
    /// placeholder values (never a real pin).
    fn docker_ci() -> serde_json::Value {
        serde_json::json!({
            "destination": "root@vm.example", "port": 2250,
            "known_hosts": "vm.example ssh-ed25519 AAAAplace-holder-pin\n",
            "reset_destination": "reset@reset.example", "reset_port": 22,
            "reset_known_hosts": "reset.example ssh-ed25519 AAAAplace-holder-reset\n",
        })
    }

    #[test]
    fn docker_ci_flags_set_keep_and_clear() {
        let args = parse_set(&[
            "--docker-ci-destination",
            "root@vm.example",
            "--docker-ci-port",
            "2250",
            "--docker-ci-known-hosts",
            "vm.example ssh-ed25519 AAAAplace-holder-pin\n",
            "--docker-ci-reset-destination",
            "reset@reset.example",
            "--docker-ci-reset-port",
            "22",
            "--docker-ci-reset-known-hosts",
            "reset.example ssh-ed25519 AAAAplace-holder-reset\n",
        ])
        .unwrap();
        let cur = serde_json::json!({"target": "p-box", "recipe": "just ci::validate"});
        let value = super::merged(&cur, &args.delivery).unwrap();
        assert_eq!(value["docker_ci"], docker_ci());
        // The other fields stay untouched.
        assert_eq!(value["target"], "p-box");

        // A flag left out keeps the stored value, the given one replaces it.
        let args = parse_set(&[
            "--docker-ci-port",
            "2251",
            "--docker-ci-reset-known-hosts",
            "reset.example ssh-ed25519 AAAAnew\n",
        ])
        .unwrap();
        let cur = serde_json::json!({"docker_ci": docker_ci()});
        let value = super::merged(&cur, &args.delivery).unwrap();
        let mut want = docker_ci();
        want["port"] = serde_json::json!(2251);
        want["reset_known_hosts"] = serde_json::json!("reset.example ssh-ed25519 AAAAnew\n");
        assert_eq!(value["docker_ci"], want);

        // Clearing drops the whole object, the rest of the config stays.
        let args = parse_set(&["--clear-docker-ci"]).unwrap();
        let cur = serde_json::json!({"target": "p-box", "docker_ci": docker_ci()});
        let value = super::merged(&cur, &args.delivery).unwrap();
        assert!(value.get("docker_ci").is_none());
        assert_eq!(value["target"], "p-box");

        // No docker_ci flag at all leaves the stored object alone.
        let args = parse_set(&["--slots", "3"]).unwrap();
        let cur = serde_json::json!({"docker_ci": docker_ci()});
        let value = super::merged(&cur, &args.delivery).unwrap();
        assert_eq!(value["docker_ci"], docker_ci());
    }

    #[test]
    fn docker_ci_needs_all_six_values_and_clear_conflicts() {
        // Nothing stored: a partial set names the flags that are missing.
        let args = parse_set(&["--docker-ci-destination", "root@vm.example"]).unwrap();
        let err = super::merged(&serde_json::json!({}), &args.delivery)
            .unwrap_err()
            .to_string();
        for flag in [
            "--docker-ci-port",
            "--docker-ci-known-hosts",
            "--docker-ci-reset-destination",
            "--docker-ci-reset-port",
            "--docker-ci-reset-known-hosts",
        ] {
            assert!(err.contains(flag), "{err}");
        }
        // With a stored object, one flag is enough.
        let args = parse_set(&["--docker-ci-port", "2251"]).unwrap();
        let cur = serde_json::json!({"docker_ci": docker_ci()});
        assert!(super::merged(&cur, &args.delivery).is_ok());

        // Clear and set at once is a usage error.
        assert!(parse_set(&["--clear-docker-ci", "--docker-ci-port", "2251"]).is_err());
    }

    #[test]
    fn delivery_show_prints_the_docker_ci_endpoints_and_pins() {
        let ctx = Ctx::for_test(Arc::new(MockTransport::new()), None);
        super::print_delivery(
            &ctx,
            &serde_json::json!({
                "delivery": {
                    "target": "p-box", "recipe": "just ci::validate",
                    "docker_ci": docker_ci(),
                },
                "enabled": true, "slots": 2,
            }),
        );
        let out = ctx.out.captured();
        assert!(
            out.contains("docker ci:  root@vm.example:2250 (reset reset@reset.example:22)"),
            "{out}"
        );
        assert!(
            out.contains("docker pin: vm.example ssh-ed25519 AAAAplace-holder-pin"),
            "{out}"
        );
        assert!(
            out.contains("reset pin:  reset.example ssh-ed25519 AAAAplace-holder-reset"),
            "{out}"
        );

        let ctx = Ctx::for_test(Arc::new(MockTransport::new()), None);
        super::print_delivery(
            &ctx,
            &serde_json::json!({"delivery": {}, "enabled": false, "slots": 2}),
        );
        assert!(ctx.out.captured().contains("docker ci:  -"));
    }

    #[test]
    fn merge_secret_flags_parse_clear_and_refuse_literals() {
        #[derive(Parser)]
        struct Args {
            #[command(flatten)]
            delivery: super::DeliverySetArgs,
        }
        let parse = |flags: &[&str]| {
            Args::try_parse_from(std::iter::once("test").chain(flags.iter().copied()))
        };
        let args = parse(&["--merge-secret", "KEY=@secret:vm.private_key"]).unwrap();
        let value = super::merged(&serde_json::json!({}), &args.delivery).unwrap();
        assert_eq!(value["merge_secrets"]["KEY"], "@secret:vm.private_key");
        let clear = parse(&["--clear-merge-secrets"]).unwrap();
        assert_eq!(
            super::merged(&value, &clear.delivery).unwrap()["merge_secrets"],
            serde_json::json!({})
        );
        for value in ["KEY=literal", "KEY", "=@secret:vm"] {
            let args = parse(&["--merge-secret", value]).unwrap();
            assert!(super::merged(&serde_json::json!({}), &args.delivery).is_err());
        }
        assert!(parse(&["--clear-merge-secrets", "--merge-secret", "KEY=@secret:vm"]).is_err());
    }
    use std::sync::Arc;

    use super::*;
    use crate::transport::mock::MockTransport;

    #[test]
    fn planned_accepts_only_bounded_positive_durations() {
        assert_eq!(planned(&None).unwrap(), None);
        for value in ["20m", "1h30m", "30d"] {
            assert_eq!(
                planned(&Some(format!(" {value} "))).unwrap(),
                Some(value.into())
            );
        }
        for value in ["", "0m", "-1m", "31d", "2026-11-08T15:00:00Z"] {
            assert!(planned(&Some(value.into())).is_err(), "{value}");
        }
    }

    #[test]
    fn stats_print_the_rates_counts_and_top_targets() {
        let ctx = Ctx::for_test(Arc::new(MockTransport::new()), None);
        let targets: Vec<Value> = (0..12)
            .map(|i| {
                json!({"target": format!("-p app --test t{i:02}"), "failed": 12 - i,
                            "first_attempt": 1})
            })
            .collect();
        let v = json!({
            "project": "p", "since": "2026-11-01T00:00:00.000Z", "jobs": 13,
            "first_attempt": {"ended": 5, "passed": 2, "failed": 3, "pass_rate": 0.4},
            "validate": {"ended": 3, "passed": 1, "failed": 2, "pass_rate": 1.0 / 3.0},
            "merge": {"ended": 2, "passed": 1, "failed": 1, "pass_rate": 0.5},
            "reruns": {"ended": 0, "passed": 0, "failed": 0, "pass_rate": null},
            "conflicts": 1, "errors": 2, "cancelled": 1, "open": 1, "splits": 1,
            "failed_steps": [{"step": "test", "failed": 4}],
            "failed_targets": targets,
            "untargeted_test_fails": 1,
        });
        print_stats(&ctx, &v);
        let out = ctx.out.captured();
        assert!(
            out.contains("jobs of p since 2026-11-01T00:00:00.000Z: 13"),
            "{out}"
        );
        assert!(out.contains("first-attempt pass rate: 40% (2/5)"), "{out}");
        assert!(out.contains("validate 33% (1/3), merge 50% (1/2)"), "{out}");
        assert!(out.contains("reruns passed: - (0/0)"), "{out}");
        assert!(
            out.contains("conflicts 1, could not run 2, cancelled 1, open 1, split halves 1"),
            "{out}"
        );
        assert!(out.contains("failed steps: test 4"), "{out}");
        assert!(out.contains("top 10 of 12, --json for all"), "{out}");
        assert!(out.contains("(clipped): 1"), "{out}");
        assert!(out.contains("-p app --test t09"), "{out}");
        assert!(!out.contains("-p app --test t10"), "{out}");
    }
}
