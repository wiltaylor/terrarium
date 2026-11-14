//! `service`: services and their request queue (TER-1264, Services C1).
//! A definition comes from `services/<name>.toml` or `service new`; a call
//! returns a request ID at once.

use anyhow::Context as _;
use base64::Engine as _;
use serde_json::{Value, json};
use terrarium_core::service::ServiceDef;

use super::{print_object, print_table};
use crate::api::{CliError, query};
use crate::cli::ServiceCmd;
use crate::ctx::Ctx;
use crate::transport::encode;

fn path(name: &str) -> String {
    format!("/api/services/{}", encode(name))
}

fn request_path(id: &str) -> String {
    format!("/api/service-requests/{}", encode(id))
}

/// One row of `service ls`.
fn ls_row(mut s: Value) -> Value {
    s["owner"] = s["project"].as_str().unwrap_or("global").into();
    let r = &s["resources"];
    let gpus = r["gpus"].as_u64().unwrap_or(0);
    s["gpu"] = if gpus == 0 {
        "-".into()
    } else if r["gpu_exclusive"].as_bool() == Some(true) {
        format!("{gpus} exclusive").into()
    } else {
        format!("{gpus} shared").into()
    };
    s["conc"] = s["concurrency"].as_u64().unwrap_or(1).into();
    s
}

/// The body of `service new`: the TOML file as the API's JSON, named.
fn new_body(name: Option<&str>, file: &std::path::Path) -> anyhow::Result<Value> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let mut def = ServiceDef::from_toml(&text)
        .map_err(|e| CliError::usage(format!("{}: {e}", file.display())))?;
    def.name = match name {
        Some(n) => n.to_owned(),
        None => file
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| CliError::usage("give the service name with --name"))?
            .to_owned(),
    };
    def.check().map_err(CliError::usage)?;
    Ok(serde_json::to_value(&def)?)
}

/// The body of `service call`.
fn call_body(json_arg: Option<&str>, payload: Option<&std::path::Path>) -> anyhow::Result<Value> {
    let request: Value = match json_arg {
        None => json!({}),
        Some(a) => {
            let text = match a.strip_prefix('@') {
                Some(f) => {
                    std::fs::read_to_string(f).with_context(|| format!("cannot read {f}"))?
                }
                None => a.to_owned(),
            };
            serde_json::from_str(&text)
                .map_err(|e| CliError::usage(format!("--request is not JSON: {e}")))?
        }
    };
    let mut body = json!({"request": request});
    if let Some(p) = payload {
        let data = std::fs::read(p).with_context(|| format!("cannot read {}", p.display()))?;
        body["payload_base64"] = base64::engine::general_purpose::STANDARD
            .encode(data)
            .into();
    }
    Ok(body)
}

const REQUEST_COLS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("STATE", "state"),
    ("CALLER", "caller"),
    ("PAYLOAD", "payload_size"),
    ("CREATED", "created_at"),
    ("WAIT UNTIL", "wait_until"),
];

pub async fn service(ctx: &Ctx, cmd: &ServiceCmd) -> anyhow::Result<()> {
    match cmd {
        ServiceCmd::Ls => {
            let v = ctx.api.get("/api/services", vec![]).await?;
            ctx.emit(&v, |c, v| {
                let rows: Vec<Value> = v
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(ls_row)
                    .collect();
                print_table(
                    c,
                    &rows,
                    &[
                        ("NAME", "name"),
                        ("KIND", "kind"),
                        ("TARGET", "target"),
                        ("OWNER", "owner"),
                        ("GPU", "gpu"),
                        ("CONC", "conc"),
                        ("SOURCE", "source"),
                    ],
                    "no services",
                );
            })
        }
        ServiceCmd::Show { name } => {
            let v = ctx.api.get(&path(name), vec![]).await?;
            ctx.emit(&v, print_object)
        }
        ServiceCmd::New { name, file } => {
            let body = new_body(name.as_deref(), file)?;
            let v = ctx.api.post("/api/services", Some(body)).await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "service {} defined ({}, target {})",
                    v["name"].as_str().unwrap_or(""),
                    v["kind"].as_str().unwrap_or(""),
                    v["target"].as_str().unwrap_or("")
                ));
            })
        }
        ServiceCmd::Rm { name } => {
            ctx.api.delete(&path(name), vec![]).await?;
            ctx.emit(&json!({"removed": name}), |c, _| {
                c.line(format!("removed service {name}"));
            })
        }
        ServiceCmd::Call {
            name,
            request,
            payload,
        } => {
            let body = call_body(request.as_deref(), payload.as_deref())?;
            let v = ctx
                .api
                .post_upload(&format!("{}/requests", path(name)), body)
                .await?;
            ctx.emit(&v, |c, v| {
                let id = v["id"].as_str().unwrap_or("");
                c.line(format!(
                    "request {id} {} for service {name} (place {} in the queue; wait limit {})",
                    v["state"].as_str().unwrap_or(""),
                    v["position"].as_u64().unwrap_or(0),
                    v["wait_until"].as_str().unwrap_or("")
                ));
                c.line(format!("status: terrarium service status {id}"));
            })
        }
        ServiceCmd::Requests { name, state, limit } => {
            let v = ctx
                .api
                .get(
                    &format!("{}/requests", path(name)),
                    query([("state", state.clone()), ("limit", Some(limit.to_string()))]),
                )
                .await?;
            ctx.emit(&v, |c, v| {
                let rows = v.as_array().cloned().unwrap_or_default();
                print_table(c, &rows, REQUEST_COLS, "no requests");
            })
        }
        ServiceCmd::Status { id } => {
            let v = ctx.api.get(&request_path(id), vec![]).await?;
            ctx.emit(&v, print_object)
        }
        ServiceCmd::Cancel { id } => {
            let v = ctx
                .api
                .post(&format!("{}/cancel", request_path(id)), None)
                .await?;
            ctx.emit(&v, |c, v| {
                c.line(format!(
                    "request {id} {}",
                    v["state"].as_str().unwrap_or("cancelled")
                ));
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_call_body_carries_the_json_and_the_payload() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("in.bin");
        std::fs::write(&p, b"BYTES").unwrap();
        let b = call_body(Some(r#"{"seconds":5}"#), Some(&p)).unwrap();
        assert_eq!(
            b,
            json!({"request": {"seconds": 5}, "payload_base64": "QllURVM="})
        );
        let j = dir.path().join("r.json");
        std::fs::write(&j, r#"{"a":1}"#).unwrap();
        let b = call_body(Some(&format!("@{}", j.display())), None).unwrap();
        assert_eq!(b, json!({"request": {"a": 1}}));
        assert!(call_body(Some("not json"), None).is_err());
        assert_eq!(call_body(None, None).unwrap(), json!({"request": {}}));
    }

    #[test]
    fn new_takes_the_name_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("video-gen.toml");
        std::fs::write(
            &p,
            "kind = \"script\"\ntarget = \"helios\"\ncommand = [\"/opt/run\"]\n",
        )
        .unwrap();
        let b = new_body(None, &p).unwrap();
        assert_eq!(b["name"], "video-gen");
        assert_eq!(new_body(Some("other"), &p).unwrap()["name"], "other");
        std::fs::write(&p, "kind = \"script\"\ntarget = \"helios\"\n").unwrap();
        assert!(new_body(None, &p).is_err());
    }
}
