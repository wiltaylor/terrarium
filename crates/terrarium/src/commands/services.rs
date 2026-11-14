//! `service`: services and their request queue (TER-1264, Services C1).
//! A definition comes from `services/<name>.toml` or `service new`; a call
//! returns a request ID at once. `service result` fetches the result of an
//! ended request (TER-1268, C5).

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

/// A response path from the daemon as a relative path, or `None` when it
/// is not plain (a root, `.`, `..`, an empty part). The daemon checks the
/// paths; the CLI checks again before it writes.
fn safe_rel(p: &str) -> Option<std::path::PathBuf> {
    if p.is_empty() || p.starts_with('/') || p.contains('\\') {
        return None;
    }
    let mut out = std::path::PathBuf::new();
    for part in p.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        out.push(part);
    }
    out.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        .then_some(out)
}

/// The URL of response file `rel` of request `id`, each part encoded.
fn result_file_path(id: &str, rel: &str) -> String {
    let parts: Vec<String> = rel.split('/').map(encode).collect();
    format!("{}/result/{}", request_path(id), parts.join("/"))
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `service result <id> --out <dir>`: `<dir>/result.json` and the files
/// under `<dir>/files/`. Each file is fetched in chunks (the agent
/// transports carry JSON only), written to `<file>.partial`, checked
/// against the size and sha256 of the list, then renamed.
async fn fetch_result(
    ctx: &Ctx,
    id: &str,
    out: &std::path::Path,
    force: bool,
) -> anyhow::Result<Value> {
    use sha2::Digest as _;
    use std::io::Write as _;
    let v = ctx
        .api
        .get(&format!("{}/result", request_path(id)), vec![])
        .await?;
    let files_dir = out.join("files");
    let mut plan = Vec::new();
    for f in v["files"].as_array().cloned().unwrap_or_default() {
        let rel = f["path"].as_str().unwrap_or("");
        let local = safe_rel(rel)
            .ok_or_else(|| anyhow::anyhow!("the daemon answered a bad file path `{rel}`"))?;
        plan.push((
            rel.to_owned(),
            files_dir.join(local),
            f["size"].as_u64().unwrap_or(0),
            f["sha256"].as_str().unwrap_or("").to_owned(),
        ));
    }
    let record = out.join("result.json");
    if !force {
        for to in std::iter::once(&record).chain(plan.iter().map(|p| &p.1)) {
            if to.exists() {
                return Err(CliError::usage(format!(
                    "{} is there already; add --force to overwrite it",
                    to.display()
                )));
            }
        }
    }
    std::fs::create_dir_all(out).with_context(|| format!("cannot make {}", out.display()))?;
    let mut bytes = 0u64;
    for (rel, to, size, sha) in &plan {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot make {}", parent.display()))?;
        }
        let mut part_name = to.as_os_str().to_owned();
        part_name.push(".partial");
        let part = std::path::PathBuf::from(part_name);
        let mut file = std::fs::File::create(&part)
            .with_context(|| format!("cannot write {}", part.display()))?;
        let mut hash = sha2::Sha256::new();
        let mut offset = 0u64;
        loop {
            let c = ctx
                .api
                .get(
                    &result_file_path(id, rel),
                    query([
                        ("format", Some("json".to_owned())),
                        ("offset", Some(offset.to_string())),
                    ]),
                )
                .await?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(c["data_base64"].as_str().unwrap_or(""))
                .map_err(|e| anyhow::anyhow!("the daemon answered bad base64 for {rel}: {e}"))?;
            file.write_all(&data)?;
            hash.update(&data);
            offset += data.len() as u64;
            if c["eof"].as_bool() != Some(false) || data.is_empty() {
                break;
            }
        }
        file.flush()?;
        drop(file);
        let got = lower_hex(&hash.finalize());
        if offset != *size || got != *sha {
            let _ = std::fs::remove_file(&part);
            return Err(anyhow::anyhow!(
                "{rel}: got {offset} bytes with sha256 {got}, the list has {size} bytes with \
                 sha256 {sha} (the file changed or broke on the way; try again)"
            ));
        }
        std::fs::rename(&part, to).with_context(|| format!("cannot write {}", to.display()))?;
        bytes += offset;
    }
    std::fs::write(&record, serde_json::to_vec_pretty(&v)?)
        .with_context(|| format!("cannot write {}", record.display()))?;
    Ok(json!({
        "id": id,
        "state": v["state"],
        "exit_code": v["exit_code"],
        "out": out,
        "files": plan.len(),
        "bytes": bytes,
        "files_cut": v["files_cut"],
        "files_deleted_at": v["files_deleted_at"],
    }))
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
        ServiceCmd::Result { id, out, force } => {
            let v = fetch_result(ctx, id, out, *force).await?;
            ctx.emit(&v, |c, v| {
                let mut line = format!(
                    "request {id} {}: {} files ({} bytes) and result.json in {}",
                    v["state"].as_str().unwrap_or(""),
                    v["files"].as_u64().unwrap_or(0),
                    v["bytes"].as_u64().unwrap_or(0),
                    out.display()
                );
                if let Some(c) = v["exit_code"].as_i64() {
                    line.push_str(&format!(" (exit code {c})"));
                }
                c.line(line);
                if let Some(at) = v["files_deleted_at"].as_str() {
                    c.line(format!("the files went with the 7-day retention at {at}"));
                }
                if v["files_cut"].as_bool() == Some(true) {
                    c.line("the daemon listed only the first 10000 files");
                }
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

    fn mock_ctx(mock: &crate::transport::mock::MockTransport) -> Ctx {
        Ctx::for_test(std::sync::Arc::new(mock.clone()), None)
    }

    /// The list of request `01R` with one file `sub/a.txt` of `sha`.
    fn listing(sha: &str) -> Value {
        json!({"id": "01R", "state": "done", "exit_code": 0, "response": {"ok": true},
               "files": [{"path": "sub/a.txt", "size": 5, "sha256": sha}],
               "files_cut": false, "files_deleted_at": null})
    }

    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// Review rev-1268 F4: the fetch writes `result.json` and `files/`,
    /// reads each file in chunks, and refuses to overwrite without
    /// `--force`.
    #[tokio::test]
    async fn the_fetch_writes_the_result_and_the_files() {
        let mock = crate::transport::mock::MockTransport::new();
        mock.reply(200, listing(HELLO_SHA))
            .reply(200, json!({"data_base64": "aGVs", "eof": false}))
            .reply(200, json!({"data_base64": "bG8=", "eof": true}));
        let ctx = mock_ctx(&mock);
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let v = fetch_result(&ctx, "01R", &out, false).await.unwrap();
        assert_eq!(v["files"], 1);
        assert_eq!(v["bytes"], 5);
        assert_eq!(std::fs::read(out.join("files/sub/a.txt")).unwrap(), b"hello");
        assert!(!out.join("files/sub/a.txt.partial").exists());
        let rec: Value =
            serde_json::from_slice(&std::fs::read(out.join("result.json")).unwrap()).unwrap();
        assert_eq!(rec["response"], json!({"ok": true}));
        let reqs = mock.requests();
        assert_eq!(reqs[0].path, "/api/service-requests/01R/result");
        assert_eq!(reqs[1].path, "/api/service-requests/01R/result/sub/a.txt");
        let q = |i: usize| reqs[i].query.clone();
        assert!(q(1).contains(&("offset".to_owned(), "0".to_owned())), "{:?}", q(1));
        assert!(q(2).contains(&("offset".to_owned(), "3".to_owned())), "{:?}", q(2));
        // Again: refused without --force, done with it.
        mock.reply(200, listing(HELLO_SHA));
        let e = fetch_result(&ctx, "01R", &out, false).await.unwrap_err();
        assert!(e.to_string().contains("--force"), "{e}");
        mock.reply(200, listing(HELLO_SHA))
            .reply(200, json!({"data_base64": "aGVsbG8=", "eof": true}));
        fetch_result(&ctx, "01R", &out, true).await.unwrap();
    }

    /// A file whose bytes do not match the list is refused, and nothing is
    /// left of it.
    #[tokio::test]
    async fn the_fetch_refuses_a_bad_sha256() {
        let mock = crate::transport::mock::MockTransport::new();
        mock.reply(200, listing(&"0".repeat(64)))
            .reply(200, json!({"data_base64": "aGVsbG8=", "eof": true}));
        let ctx = mock_ctx(&mock);
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let e = fetch_result(&ctx, "01R", &out, false).await.unwrap_err();
        assert!(e.to_string().contains("sha256"), "{e}");
        assert!(!out.join("files/sub/a.txt").exists());
        assert!(!out.join("files/sub/a.txt.partial").exists());
        assert!(!out.join("result.json").exists());
    }

    #[test]
    fn a_result_path_must_be_plain() {
        assert_eq!(
            safe_rel("sub/out.mp4"),
            Some(std::path::PathBuf::from("sub").join("out.mp4"))
        );
        for bad in ["", "/etc/x", "../x", "a/../x", "./x", "a//b", "a\\b"] {
            assert!(safe_rel(bad).is_none(), "{bad}");
        }
        assert_eq!(
            result_file_path("01R", "sub/a b.txt"),
            "/api/service-requests/01R/result/sub/a%20b.txt"
        );
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
