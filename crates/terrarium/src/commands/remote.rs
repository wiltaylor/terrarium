//! `exec`, `shell`, and `cp`: thin clients of the exec sessions of
//! `docs/api.md` §17 (targets). They use plain requests only, so they work
//! the same over the agent tunnel and over HTTP (docs/agent-rpc.md,
//! "Streams over the tunnel"):
//!
//! 1. `POST /api/targets/{t}/exec` (or `/cp`) opens a session.
//! 2. `POST .../sessions/{id}/input` sends stdin, resize, eof, or the end
//!    of a push.
//! 3. `GET .../sessions/{id}/output?since=&wait=` long-polls the output.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::api::{CliError, ServerError, query};
use crate::ctx::Ctx;
use crate::transport::encode;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// The bytes of one input request, before base64.
pub const CHUNK: usize = 256 * 1024;
/// Seconds that one output poll waits.
const POLL_WAIT: &str = "25";

/// The options of `terrarium exec`.
#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    pub cwd: Option<String>,
    pub env: Vec<String>,
    pub timeout: Option<u64>,
    pub stdin: bool,
    pub tty: bool,
    /// The holder of a `lab_from_workspace` target.
    pub agent: Option<String>,
}

fn session_path(target: &str, sid: &str) -> String {
    format!("/api/targets/{}/sessions/{}", encode(target), encode(sid))
}

async fn open(ctx: &Ctx, target: &str, what: &str, body: Value) -> anyhow::Result<String> {
    let path = format!("/api/targets/{}/{what}", encode(target));
    let r = ctx.api.post(&path, Some(body)).await?;
    r["session_id"]
        .as_str()
        .map(str::to_owned)
        .context("the daemon answered without `session_id`")
}

async fn send(ctx: &Ctx, target: &str, sid: &str, body: Value) -> anyhow::Result<()> {
    ctx.api
        .post(&format!("{}/input", session_path(target, sid)), Some(body))
        .await?;
    Ok(())
}

/// Send stdin bytes. False when the daemon answers `409`: the command has
/// ended before the bytes came (a fast command, or one that reads no
/// input). That is not an error; the end comes from the output stream, as
/// for a late `eof` (docs/api.md §17.4).
async fn send_stdin(ctx: &Ctx, target: &str, sid: &str, bytes: &[u8]) -> anyhow::Result<bool> {
    match send(ctx, target, sid, json!({"data": B64.encode(bytes)})).await {
        Ok(()) => Ok(true),
        Err(e)
            if e.downcast_ref::<ServerError>()
                .is_some_and(|s| s.status == 409) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// One output poll: the chunks as `(stream, bytes)`, the next offset, and
/// the end once the session is done.
struct Poll {
    chunks: Vec<(String, Vec<u8>)>,
    next: u64,
    end: Option<Value>,
}

async fn poll(ctx: &Ctx, target: &str, sid: &str, since: u64) -> anyhow::Result<Poll> {
    let q = query([
        ("since", Some(since.to_string())),
        ("wait", Some(POLL_WAIT.to_owned())),
    ]);
    let o = ctx
        .api
        .get(&format!("{}/output", session_path(target, sid)), q)
        .await?;
    let mut chunks = Vec::new();
    for c in o["chunks"].as_array().into_iter().flatten() {
        let data = B64
            .decode(c["data"].as_str().unwrap_or_default())
            .context("the daemon sent output that is not base64")?;
        chunks.push((c["stream"].as_str().unwrap_or("stdout").to_owned(), data));
    }
    Ok(Poll {
        chunks,
        next: o["next"].as_u64().unwrap_or(since),
        end: (o["done"] == true).then(|| o["end"].clone()),
    })
}

/// The exit code of the CLI for the end of a remote command.
fn exit_of(end: &Value) -> anyhow::Result<()> {
    match end["type"].as_str() {
        Some("exit") => {
            let code = match (end["code"].as_i64(), end["signal"].as_i64()) {
                (Some(c), _) => c,
                (None, Some(s)) => 128 + s,
                (None, None) => 1,
            };
            if code == 0 {
                Ok(())
            } else {
                Err(CliError::RemoteExit(u8::try_from(code.clamp(1, 255)).unwrap_or(1)).into())
            }
        }
        Some("cp") => Ok(()),
        _ => anyhow::bail!(
            "the remote run failed: {} [{}]",
            end["message"].as_str().unwrap_or("no end"),
            end["code"].as_str().unwrap_or("error")
        ),
    }
}

fn parse_env(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    pairs
        .iter()
        .map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .ok_or_else(|| CliError::usage(format!("bad --env `{p}`: use KEY=VALUE")))
        })
        .collect()
}

/// `terrarium exec <target> [--cwd d] [-e K=V] -- <cmd>`.
pub async fn exec(ctx: &Ctx, target: &str, o: &ExecOpts, cmd: &[String]) -> anyhow::Result<()> {
    if cmd.is_empty() {
        return Err(CliError::usage(
            "name the command after `--`: terrarium exec <target> -- <cmd>",
        ));
    }
    let mut body = json!({"cmd": cmd});
    if let Some(c) = &o.cwd {
        body["cwd"] = c.clone().into();
    }
    let env = parse_env(&o.env)?;
    if !env.is_empty() {
        body["env"] = json!(env);
    }
    if let Some(t) = o.timeout {
        body["timeout_secs"] = t.into();
    }
    if let Some(a) = &o.agent {
        body["agent"] = a.clone().into();
    }
    if o.tty {
        let (rows, cols) = crate::attach::terminal_size().unwrap_or((24, 80));
        body["pty"] = json!({"rows": rows, "cols": cols});
    }
    let sid = open(ctx, target, "exec", body).await?;
    let mut ended = false;
    if o.stdin {
        let input = ctx.text("-")?;
        for chunk in input.as_bytes().chunks(CHUNK) {
            if !send_stdin(ctx, target, &sid, chunk).await? {
                ended = true;
                break;
            }
        }
    }
    if !o.tty && !ended {
        send(ctx, target, &sid, json!({"eof": true})).await?;
    }
    let mut since = 0;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let end = loop {
        let p = poll(ctx, target, &sid, since).await?;
        for (stream, bytes) in &p.chunks {
            let is_err = stream == "stderr";
            if ctx.json {
                if is_err { &mut err } else { &mut out }.extend_from_slice(bytes);
            } else {
                ctx.out.write_bytes(is_err, bytes);
            }
        }
        since = p.next;
        if let Some(end) = p.end {
            break end;
        }
    };
    if ctx.json {
        ctx.line(serde_json::to_string_pretty(&json!({
            "session_id": sid,
            "stdout": String::from_utf8_lossy(&out),
            "stderr": String::from_utf8_lossy(&err),
            "end": end,
        }))?);
    }
    exit_of(&end)
}

/// `terrarium shell <target>`: a login shell in a PTY. The terminal goes to
/// raw mode; Ctrl-] detaches and ends the session.
pub async fn shell(ctx: &Ctx, target: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    use tokio::signal::unix::{SignalKind, signal};

    let (rows, cols) = crate::attach::terminal_size().unwrap_or((24, 80));
    let sid = open(
        ctx,
        target,
        "exec",
        json!({"shell": true, "pty": {"rows": rows, "cols": cols}}),
    )
    .await?;
    let raw = match crate::attach::RawMode::enter() {
        Ok(r) => {
            eprint!("shell on {target}. Press Ctrl-] to detach.\r\n");
            Some(r)
        }
        Err(_) => None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut winch = signal(SignalKind::window_change())?;

    let output = async {
        let mut since = 0;
        loop {
            let p = poll(ctx, target, &sid, since).await?;
            for (_, bytes) in &p.chunks {
                let mut o = std::io::stdout().lock();
                o.write_all(bytes)?;
                o.flush()?;
            }
            since = p.next;
            if let Some(end) = p.end {
                return Ok::<_, anyhow::Error>(Some(end));
            }
        }
    };
    let input = async {
        let mut open = true;
        loop {
            tokio::select! {
                chunk = rx.recv(), if open => match chunk {
                    Some(bytes) => {
                        let (data, detach) = match crate::attach::detach_at(&bytes) {
                            Some(n) => (&bytes[..n], true),
                            None => (&bytes[..], false),
                        };
                        // A 409 means the shell has ended; the output
                        // loop reads the end.
                        if !data.is_empty() {
                            send_stdin(ctx, target, &sid, data).await?;
                        }
                        if detach {
                            return Ok::<_, anyhow::Error>(());
                        }
                    }
                    None => {
                        open = false;
                        send(ctx, target, &sid, json!({"eof": true})).await?;
                    }
                },
                _ = winch.recv() => {
                    if let Some((rows, cols)) = crate::attach::terminal_size() {
                        send(ctx, target, &sid, json!({"resize": {"rows": rows, "cols": cols}})).await?;
                    }
                }
            }
        }
    };
    let end = tokio::select! {
        r = output => r?,
        r = input => { r?; None }
    };
    drop(raw);
    match end {
        Some(end) => {
            eprint!("\r\n");
            exit_of(&end)
        }
        None => {
            let _ = ctx.api.delete(&session_path(target, &sid), vec![]).await;
            eprintln!("\r\ndetached; the shell on {target} is closed.");
            Ok(())
        }
    }
}

/// `target:path` when the part before the colon is a target name.
pub fn split_remote(s: &str) -> Option<(&str, &str)> {
    let (t, p) = s.split_once(':')?;
    let name_ok = !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    (name_ok && !p.is_empty()).then_some((t, p))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `terrarium cp <src> <target>:<dst>` and `cp <target>:<src> <dst>`: one
/// regular file, in chunks, checked with SHA-256 at the end.
pub async fn cp(ctx: &Ctx, src: &str, dst: &str) -> anyhow::Result<()> {
    match (split_remote(src), split_remote(dst)) {
        (None, Some((t, remote))) => push(ctx, Path::new(src), t, remote).await,
        (Some((t, remote)), None) => pull(ctx, t, remote, Path::new(dst)).await,
        (Some(_), Some(_)) => Err(CliError::usage(
            "both sides name a target; copy through the workspace",
        )),
        (None, None) => Err(CliError::usage("name the target side as <target>:<path>")),
    }
}

async fn push(ctx: &Ctx, local: &Path, target: &str, remote: &str) -> anyhow::Result<()> {
    let meta =
        std::fs::metadata(local).with_context(|| format!("cannot read {}", local.display()))?;
    if !meta.is_file() {
        return Err(CliError::usage(format!(
            "{} is not a regular file; cp copies one file (tar a directory first)",
            local.display()
        )));
    }
    let data = std::fs::read(local).with_context(|| format!("cannot read {}", local.display()))?;
    let remote = if remote.ends_with('/') {
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{remote}{name}")
    } else {
        remote.to_owned()
    };
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    let sid = open(
        ctx,
        target,
        "cp",
        json!({"direction": "push", "path": remote, "mode": mode}),
    )
    .await?;
    for chunk in data.chunks(CHUNK) {
        send(ctx, target, &sid, json!({"data": B64.encode(chunk)})).await?;
    }
    let digest = hex(&Sha256::digest(&data));
    send(
        ctx,
        target,
        &sid,
        json!({"finish": {"bytes": data.len(), "sha256": digest}}),
    )
    .await?;
    let end = loop {
        let p = poll(ctx, target, &sid, 0).await?;
        if let Some(end) = p.end {
            break end;
        }
    };
    exit_of(&end)?;
    if end["sha256"].as_str() != Some(digest.as_str()) {
        anyhow::bail!(
            "the target reports SHA-256 {} for {remote}; the file has {digest}",
            end["sha256"]
        );
    }
    ctx.emit(&end, |c, _| {
        c.line(format!(
            "copied {} bytes to {target}:{remote} (sha256 {digest})",
            data.len()
        ));
    })
}

async fn pull(ctx: &Ctx, target: &str, remote: &str, local: &Path) -> anyhow::Result<()> {
    let local: PathBuf = if local.is_dir() {
        let name = Path::new(remote)
            .file_name()
            .context("the remote path has no file name")?;
        local.join(name)
    } else {
        local.to_owned()
    };
    let sid = open(
        ctx,
        target,
        "cp",
        json!({"direction": "pull", "path": remote}),
    )
    .await?;
    let part = local.with_file_name(format!(
        ".{}.terrarium-part",
        local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let mut file =
        std::fs::File::create(&part).with_context(|| format!("cannot write {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut since = 0;
    let result = async {
        loop {
            let p = poll(ctx, target, &sid, since).await?;
            for (_, bytes) in &p.chunks {
                hasher.update(bytes);
                count += bytes.len() as u64;
                file.write_all(bytes)?;
            }
            since = p.next;
            if let Some(end) = p.end {
                return Ok::<_, anyhow::Error>(end);
            }
        }
    }
    .await;
    let end = match result.and_then(|end| exit_of(&end).map(|()| end)) {
        Ok(e) => e,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    let digest = hex(&hasher.finalize());
    if end["sha256"].as_str() != Some(digest.as_str()) || end["bytes"].as_u64() != Some(count) {
        let _ = std::fs::remove_file(&part);
        anyhow::bail!(
            "the copy does not match: got {count} bytes with SHA-256 {digest}; the target reports {} and {}",
            end["bytes"],
            end["sha256"]
        );
    }
    file.sync_all()?;
    drop(file);
    std::fs::rename(&part, &local)
        .with_context(|| format!("cannot move the file into {}", local.display()))?;
    ctx.emit(&end, |c, _| {
        c.line(format!(
            "copied {count} bytes from {target}:{remote} to {} (sha256 {digest})",
            local.display()
        ));
    })
}
