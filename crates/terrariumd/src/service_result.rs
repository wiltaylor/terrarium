//! Services C5 (TER-1268, design D6): the result of a request, and the one
//! mail to its caller at the end.
//!
//! - **The response files** are the files under
//!   `<state>/services/payloads/<id>/response/` (`store::services::response_dir`).
//!   C3 (script kind) and C4 (agent kind) put the run's output files there
//!   before they end the request. [`files`] lists them; [`resolve`] maps a
//!   listed path back to its file and refuses any other path.
//! - **The result mail** is an outbox: an ended request with no
//!   `mailed_at` is due its mail. [`send_due`] writes each due mail and
//!   marks it in the same write, so that each request gets exactly one,
//!   whatever ended it (C2-C4, the sweep, a cancel). The sweep calls it
//!   each minute, the cancel route at once; C3 and C4 call it after they
//!   end a request. A caller that cancels its own request gets no mail.
//! - **Failure:** no retry (Q4 A). The mail carries the error and the log
//!   tail, and says how to send the request again.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use sha2::Digest;
use terrarium_core::service::State;
use tracing::warn;

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::services::{self as store, RequestRow};

/// At most this many response files are listed (a run that writes more
/// is cut at this count; the listing says so).
pub const MAX_FILES: usize = 10_000;

/// The response JSON in a mail is cut at this many bytes; the full JSON
/// is in the result.
pub const MAIL_RESPONSE_BYTES: usize = 16 * 1024;

/// The log tail in a mail is cut to its last this many bytes.
pub const MAIL_LOG_BYTES: usize = 8 * 1024;

/// One response file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultFile {
    /// The path under the response directory, `/`-separated.
    pub path: String,
    pub size: u64,
    /// Lower-case hex; `None` when [`files`] was asked not to hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// The regular files under `dir`, sorted by path, at most [`MAX_FILES`]:
/// `(files, cut)`. Symbolic links and other special files are left out,
/// and the walk does not follow a linked directory. A missing `dir` is no
/// files. `hash`: also the sha256 of each file.
pub fn files(dir: &Path, hash: bool) -> std::io::Result<(Vec<ResultFile>, bool)> {
    let mut out = Vec::new();
    let mut cut = false;
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => walk(dir, "", hash, &mut out, &mut cut)?,
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((out, cut))
}

fn walk(
    dir: &Path,
    prefix: &str,
    hash: bool,
    out: &mut Vec<ResultFile>,
    cut: &mut bool,
) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for e in entries {
        let Some(name) = e.file_name().to_str().map(str::to_owned) else {
            continue; // a name that is not UTF-8 cannot be fetched by path
        };
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let m = std::fs::symlink_metadata(e.path())?;
        if m.is_dir() {
            walk(&e.path(), &rel, hash, out, cut)?;
        } else if m.is_file() {
            if out.len() >= MAX_FILES {
                *cut = true;
                return Ok(());
            }
            let sha256 = if hash {
                Some(sha256_file(&e.path())?)
            } else {
                None
            };
            out.push(ResultFile {
                path: rel,
                size: m.len(),
                sha256,
            });
        }
    }
    Ok(())
}

fn sha256_file(p: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Is `rel` a plain relative path: no root, no `.` or `..`, no empty part?
pub fn plain_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && rel.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// The file of response path `rel` under `dir`: `None` unless `rel` is a
/// plain path to a regular file with no symbolic link on the way, inside
/// `dir`.
pub fn resolve(dir: &Path, rel: &str) -> Option<PathBuf> {
    if !plain_path(rel) {
        return None;
    }
    let mut p = dir.to_path_buf();
    if !std::fs::symlink_metadata(&p).ok()?.is_dir() {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        p.push(part);
        let m = std::fs::symlink_metadata(&p).ok()?;
        let last = i + 1 == parts.len();
        if (last && !m.is_file()) || (!last && !m.is_dir()) {
            return None;
        }
    }
    Some(p)
}

/// Cut `s` to at most `max` bytes at a char boundary, from the start
/// (`tail`: false) or keeping the end (`tail`: true). `true` when cut.
fn cut(s: &str, max: usize, tail: bool) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    if tail {
        let mut i = s.len() - max;
        while !s.is_char_boundary(i) {
            i += 1;
        }
        (&s[i..], true)
    } else {
        let mut i = max;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        (&s[..i], true)
    }
}

/// A fenced block of `text` whose fence is longer than any backtick run
/// in it, so that the text cannot close the block.
fn fenced(lang: &str, text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    let nl = if text.ends_with('\n') { "" } else { "\n" };
    format!("{fence}{lang}\n{text}{nl}{fence}\n")
}

/// The subject and the body of the result mail of ended request `r`.
/// `files`: the count and the bytes of its response files.
pub fn mail_text(r: &RequestRow, files: (usize, u64)) -> (String, String) {
    let state = match r.state {
        State::TimedOut => "timed out".to_owned(),
        s => s.to_string(),
    };
    let subject = format!("Service {}: request {} {state}", r.service, r.id);
    let mut b = String::new();
    let head = match r.state {
        State::Done => "ended: it is `done`.".to_owned(),
        State::Failed => "failed. It is not tried again.".to_owned(),
        State::Cancelled => "was cancelled.".to_owned(),
        State::TimedOut if r.started_at.is_none() => format!(
            "did not start before its wait limit ({}), so it ended as `timed_out`. It was not run.",
            r.wait_until
        ),
        State::TimedOut => {
            "ran past the service's maximum run time and ended as `timed_out`.".to_owned()
        }
        s => format!("is `{s}`."),
    };
    b.push_str(&format!(
        "Your request `{}` to service `{}` {head}\n\n",
        r.id, r.service
    ));
    b.push_str(&format!("- State: `{}`\n", r.state));
    if let Some(c) = r.exit_code {
        b.push_str(&format!("- Exit code: {c}\n"));
    }
    if let Some(e) = &r.error {
        b.push_str(&format!("- Error: {}\n", e.replace('\n', " ")));
    }
    if let Some(at) = &r.ended_at {
        b.push_str(&format!("- Ended: {at}\n"));
    }
    b.push_str(&format!(
        "- Response files: {} ({} bytes)\n",
        files.0, files.1
    ));
    if let Some(resp) = &r.response {
        let text = serde_json::to_string_pretty(resp).unwrap_or_else(|_| resp.to_string());
        let (shown, was_cut) = cut(&text, MAIL_RESPONSE_BYTES, false);
        b.push_str("\nThe response JSON");
        if was_cut {
            b.push_str(&format!(
                " (the first {MAIL_RESPONSE_BYTES} bytes; the result has all of it)"
            ));
        }
        b.push_str(":\n\n");
        b.push_str(&fenced("json", shown));
    }
    if let Some(log) = r.log_tail.as_deref().filter(|l| !l.trim().is_empty()) {
        let (shown, _) = cut(log, MAIL_LOG_BYTES, true);
        b.push_str("\nThe end of the log:\n\n");
        b.push_str(&fenced("", shown));
    }
    let has_result = r.response.is_some() || files.0 > 0 || r.state == State::Done;
    b.push('\n');
    if has_result {
        b.push_str(&format!(
            "Fetch the result (the response JSON and the files): `terrarium service result {} --out <dir>`. \
             It is kept for 7 days from the end.\n",
            r.id
        ));
    } else {
        b.push_str(&format!(
            "See the request with `terrarium service status {}`.\n",
            r.id
        ));
    }
    if matches!(r.state, State::Failed | State::TimedOut) {
        b.push_str(&format!(
            "To try again, send the request again with `terrarium service call {}`.\n",
            r.service
        ));
    }
    (subject, b)
}

/// The caller's address of `r`: the operator, or the agent of its label.
fn caller_address(r: &RequestRow) -> Option<Address> {
    let Some(id) = &r.caller_agent else {
        return Some(Address::Operator);
    };
    let (project, name) = r.caller.strip_prefix("agent:")?.split_once('/')?;
    Some(Address::Agent {
        id: id.clone(),
        name: name.to_owned(),
        project: project.to_owned(),
    })
}

/// Write the result mail of each ended request whose mail is due, and
/// mark it. The number of mails written. A mail that cannot be written
/// (its caller is gone) is marked with a warning, so that it is not tried
/// each minute for ever.
pub async fn send_due(state: &Shared) -> ApiResult<usize> {
    let mut sent = 0;
    loop {
        let due = state.db.read(|c| store::unmailed(c, 100)).await?;
        if due.is_empty() {
            return Ok(sent);
        }
        for r in due {
            let dir = store::response_dir(&state.state_dir, &r.id);
            let summary = tokio::task::spawn_blocking(move || files(&dir, false))
                .await
                .ok()
                .and_then(Result::ok)
                .map(|(f, _)| (f.len(), f.iter().map(|x| x.size).sum::<u64>()))
                .unwrap_or((0, 0));
            let row = r.clone();
            let res = state
                .db
                .write("system", move |t| {
                    if !store::mark_mailed(t, &row.id)? {
                        return Ok(false);
                    }
                    let Some(to) = caller_address(&row) else {
                        return Ok(false);
                    };
                    let (subject, body) = mail_text(&row, summary);
                    mail::send(
                        t,
                        Outgoing {
                            from: &Address::System,
                            to: &to,
                            subject: &subject,
                            body: &body,
                            thread: None,
                            in_reply_to: None,
                        },
                    )?;
                    Ok(true)
                })
                .await;
            match res {
                Ok(true) => sent += 1,
                Ok(false) => {}
                Err(e) => {
                    warn!(request = %r.id, "cannot mail the result of a service request: {}", e.message);
                    let id = r.id.clone();
                    state
                        .db
                        .write("system", move |t| store::mark_mailed(t, &id))
                        .await?;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(state: State) -> RequestRow {
        RequestRow {
            id: "01REQ".into(),
            service: "video-gen".into(),
            project: Some("p".into()),
            caller: "agent:p/w1".into(),
            caller_agent: Some("01AG".into()),
            state,
            request: json!({}),
            payload_size: 0,
            payload_sha256: None,
            response: None,
            exit_code: None,
            error: None,
            wait_until: "2026-11-15T00:00:00.000Z".into(),
            created_at: "2026-11-14T00:00:00.000Z".into(),
            started_at: Some("2026-11-14T00:01:00.000Z".into()),
            ended_at: Some("2026-11-14T00:02:00.000Z".into()),
            payload_deleted_at: None,
            log_tail: None,
            mailed_at: None,
            position: None,
        }
    }

    #[test]
    fn a_done_mail_has_the_result_and_the_fetch_command() {
        let mut r = row(State::Done);
        r.exit_code = Some(0);
        r.response = Some(json!({"frames": 120}));
        let (s, b) = mail_text(&r, (2, 300));
        assert_eq!(s, "Service video-gen: request 01REQ done");
        assert!(b.contains("- Exit code: 0"), "{b}");
        assert!(b.contains("- Response files: 2 (300 bytes)"), "{b}");
        assert!(b.contains("\"frames\": 120"), "{b}");
        assert!(
            b.contains("`terrarium service result 01REQ --out <dir>`"),
            "{b}"
        );
        assert!(!b.contains("send the request again"), "{b}");
    }

    #[test]
    fn a_failure_mail_has_the_log_tail_and_no_retry() {
        let mut r = row(State::Failed);
        r.exit_code = Some(2);
        r.error = Some("the script exited with 2".into());
        r.log_tail = Some(format!("{}\nTraceback: ```boom```\n", "x".repeat(20_000)));
        let (s, b) = mail_text(&r, (0, 0));
        assert_eq!(s, "Service video-gen: request 01REQ failed");
        assert!(b.contains("It is not tried again"), "{b}");
        assert!(b.contains("Traceback: ```boom```"), "{b}");
        // The fence outlasts the backticks of the log, and the log is cut.
        assert!(b.contains("````\n"), "{b}");
        assert!(b.len() < MAIL_LOG_BYTES + 2_000, "{}", b.len());
        assert!(b.contains("terrarium service call video-gen"), "{b}");
        assert!(b.contains("terrarium service status 01REQ"), "{b}");
    }

    #[test]
    fn a_time_out_mail_tells_a_wait_from_a_run() {
        let mut r = row(State::TimedOut);
        r.started_at = None;
        let (s, b) = mail_text(&r, (0, 0));
        assert_eq!(s, "Service video-gen: request 01REQ timed out");
        assert!(b.contains("did not start before its wait limit"), "{b}");
        let r = row(State::TimedOut);
        let (_, b) = mail_text(&r, (0, 0));
        assert!(b.contains("maximum run time"), "{b}");
    }

    #[test]
    fn a_long_response_is_cut_at_a_char_boundary() {
        let mut r = row(State::Done);
        r.response = Some(json!({"text": "é".repeat(20_000)}));
        let (_, b) = mail_text(&r, (0, 0));
        assert!(b.contains("the result has all of it"), "{b}");
        assert!(b.len() < MAIL_RESPONSE_BYTES + 2_000);
    }

    #[test]
    fn the_files_are_listed_without_links() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("response");
        assert_eq!(files(&root, true).unwrap(), (vec![], false));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("out.mp4"), b"abc").unwrap();
        std::fs::write(root.join("sub/log.txt"), b"").unwrap();
        std::fs::write(d.path().join("secret"), b"no").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.path().join("secret"), root.join("link")).unwrap();
            std::os::unix::fs::symlink(d.path(), root.join("updir")).unwrap();
        }
        let (f, cut) = files(&root, true).unwrap();
        assert!(!cut);
        let paths: Vec<&str> = f.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, ["out.mp4", "sub/log.txt"]);
        assert_eq!(f[0].size, 3);
        assert_eq!(
            f[0].sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert!(resolve(&root, "out.mp4").is_some());
        assert!(resolve(&root, "sub/log.txt").is_some());
        for bad in [
            "", "/etc/passwd", "../secret", "sub/../out.mp4", "./out.mp4", "sub", "link",
            "updir/secret", "sub//log.txt", "nope",
        ] {
            assert!(resolve(&root, bad).is_none(), "{bad}");
        }
    }
}
