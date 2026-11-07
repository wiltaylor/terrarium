//! `terrarium admin update ...`: the self-update (docs/api.md §34). The
//! operator and a sysadmin (an agent with the role too) use `status`,
//! `check`, `apply`, and `rollback`. The updater uses the hidden `drain`
//! and `resume` in the terrarium container.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::UpdateCmd;
use crate::ctx::Ctx;
use crate::output::field;

/// The time between two reads of the drain.
const DRAIN_POLL: Duration = Duration::from_secs(2);

pub async fn update(ctx: &Ctx, cmd: &UpdateCmd) -> anyhow::Result<()> {
    match cmd {
        UpdateCmd::Status => {
            let v = ctx.api.get("/api/system/update", vec![]).await?;
            ctx.emit(&v, print_status)
        }
        UpdateCmd::Check => {
            let v = ctx.api.post("/api/system/update/check", None).await?;
            ctx.emit(&v, print_status)
        }
        UpdateCmd::Apply { version } => {
            let body = match version {
                Some(v) => json!({ "version": v }),
                None => json!({}),
            };
            let v = ctx.api.post("/api/system/update", Some(body)).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "update to {} started (run {}). The daemon drains, backs up, and \
                     restarts; follow it with `terrarium admin update status`",
                    field(v, "target"),
                    field(v, "run")
                ));
            })
        }
        UpdateCmd::Rollback => {
            let v = ctx.api.post("/api/system/update/rollback", None).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "rollback started (run {}). It restores the backup of the last \
                     update: the data since then is lost (it stays in a new backup)",
                    field(v, "run")
                ));
            })
        }
        UpdateCmd::Drain { wait } => drain(ctx, Duration::from_secs(*wait)).await,
        UpdateCmd::Resume => {
            let v = ctx.api.post("/api/system/update/resume", None).await?;
            ctx.emit(&v, |c, _| {
                c.line("the drain ended: the daemon takes new turns")
            })
        }
    }
}

/// Start the drain, then wait for the running turns and jobs, at most
/// `wait`. The end of the wait is no error: the turns that still run end
/// with the stop, and resume after the start; the jobs that still run
/// are killed and queued again by the new daemon (TER-123).
async fn drain(ctx: &Ctx, wait: Duration) -> anyhow::Result<()> {
    let mut v = ctx.api.post("/api/system/update/drain", None).await?;
    let started = Instant::now();
    loop {
        let busy = v["busy"].as_array().map_or(0, Vec::len);
        let jobs = v["jobs"].as_array().map_or(0, Vec::len);
        if busy == 0 && jobs == 0 {
            break;
        }
        if started.elapsed() >= wait {
            if !ctx.json {
                ctx.line(format!(
                    "{busy} agent(s) still run a turn and {jobs} job(s) still run after {}s: {}; {}",
                    wait.as_secs(),
                    field(&v, "busy"),
                    field(&v, "jobs")
                ));
            }
            v["timed_out"] = Value::Bool(true);
            break;
        }
        tokio::time::sleep(DRAIN_POLL).await;
        let r = ctx.api.get("/api/system/update", vec![]).await?;
        v = r["drain"].clone();
    }
    v["waited_s"] = json!(started.elapsed().as_secs());
    ctx.emit(&v, |c, v| {
        c.line(format!(
            "drained: no new turns or jobs; waited {}s for the running turns and jobs",
            field(v, "waited_s")
        ));
    })
}

fn image_text(v: &Value) -> String {
    if v.is_null() {
        return "-".into();
    }
    match v["version"].as_str() {
        Some(ver) => format!("{ver} ({})", field(v, "reference")),
        None => field(v, "reference"),
    }
}

fn print_status(c: &Ctx, v: &Value) {
    let cur = &v["current"];
    c.line(format!(
        "running:   {}{}",
        field(cur, "version"),
        if cur["dirty"].as_bool() == Some(true) {
            " (dirty)"
        } else {
            ""
        }
    ));
    let latest = &v["latest"];
    if latest.is_null() {
        c.line(format!(
            "newest:    unknown{}",
            v["check_error"]
                .as_str()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        ));
    } else {
        c.line(format!(
            "newest:    {} ({}){}",
            field(latest, "version"),
            field(v, "channel"),
            if v["update_available"].as_bool() == Some(true) {
                "  UPDATE AVAILABLE: terrarium admin update apply"
            } else {
                "  up to date"
            }
        ));
    }
    c.line(format!(
        "policy:    {} (TERRARIUM_SELF_UPDATE), channel {}",
        field(v, "policy"),
        field(v, "channel")
    ));
    if let Some(w) = v["maintenance"].as_object() {
        c.line(format!(
            "window:    {} until {}",
            w.get("title").and_then(Value::as_str).unwrap_or("-"),
            w.get("end").and_then(Value::as_str).unwrap_or("-")
        ));
    }
    let u = &v["updater"];
    if u["reachable"].as_bool() == Some(true) {
        let s = &u["status"];
        c.line(format!(
            "updater:   {} (verify {}), image {}",
            field(s, "updater_version"),
            field(s, "verify"),
            image_text(&s["current"])
        ));
        if !s["previous"].is_null() {
            c.line(format!("rollback:  to {}", image_text(&s["previous"])));
        }
    } else {
        c.line(format!("updater:   not reachable ({})", field(u, "error")));
    }
    if v["drain"]["on"].as_bool() == Some(true) {
        c.line(format!(
            "drain:     on since {} (busy: {}; jobs: {})",
            field(&v["drain"], "since"),
            field(&v["drain"], "busy"),
            field(&v["drain"], "jobs")
        ));
    }
    let r = &v["last_result"];
    if !r.is_null() {
        c.line(format!(
            "last run:  {} {} -> {}: {}{}",
            field(r, "kind"),
            field(r, "target"),
            image_text(&r["to"]),
            field(r, "outcome"),
            r["error"]
                .as_str()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        ));
        for s in r["steps"].as_array().into_iter().flatten() {
            c.line(format!(
                "  {:<8} {:<8} {}",
                field(s, "name"),
                field(s, "state"),
                field(s, "detail")
            ));
        }
    }
}
