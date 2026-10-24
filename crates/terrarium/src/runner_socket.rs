//! The agent-mode client of the runner socket: newline-delimited JSON over a
//! Unix socket. See `docs/runner.md`. It uses only std, so that
//! `terrarium hook` starts and exits fast.

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};
use terrarium_core::paths;

/// `terrarium hook` gives up on a socket write after this long.
const HOOK_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest hook payload that `terrarium hook` reads from stdin.
const MAX_PAYLOAD: u64 = 4 * 1024 * 1024;

/// Turn the hook's stdin into the payload. Text that is not JSON goes in
/// `{"raw": ...}`, so the runner still sees it.
pub fn parse_payload(input: &str) -> Value {
    let text = input.trim();
    if text.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(text).unwrap_or_else(|_| json!({ "raw": text }))
}

/// Send one hook message. Fire and forget: the runner sends no answer.
pub fn send_hook(socket: &Path, hook: &str, payload: Value) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to {}", socket.display()))?;
    stream.set_write_timeout(Some(HOOK_TIMEOUT))?;
    let mut line = serde_json::to_vec(&json!({
        "type": "hook",
        "hook": hook,
        "payload": payload,
    }))?;
    line.push(b'\n');
    stream.write_all(&line).context("cannot send the hook")?;
    Ok(())
}

/// `terrarium hook <event>`. It always exits 0, so that a hook never breaks
/// the harness. A failure goes to stderr.
pub fn hook_main(hook: &str) -> ExitCode {
    let mut input = String::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal()
        && let Err(e) = stdin.lock().take(MAX_PAYLOAD).read_to_string(&mut input)
    {
        eprintln!("terrarium hook: cannot read stdin: {e}");
    }
    let payload = parse_payload(&input);
    let Some(socket) = std::env::var_os(paths::ENV_AGENT_SOCKET).filter(|s| !s.is_empty()) else {
        eprintln!(
            "terrarium hook: {} is not set; hook `{hook}` dropped",
            paths::ENV_AGENT_SOCKET
        );
        return ExitCode::SUCCESS;
    };
    if let Err(e) = send_hook(Path::new(&socket), hook, payload) {
        eprintln!("terrarium hook: {e:#}");
    }
    ExitCode::SUCCESS
}

/// Send one request and wait for its response. Returns the `result`, or an
/// error made from the `error`.
pub fn request(socket: &Path, method: &str, params: Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to the runner at {}", socket.display()))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_vec(&json!({
        "type": "request",
        "id": 1,
        "method": method,
        "params": params,
    }))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply: Value = serde_json::from_str(&reply).context("bad answer from the runner")?;
    if let Some(err) = reply.get("error") {
        anyhow::bail!(
            "{}: {}",
            err["code"].as_str().unwrap_or("error"),
            err["message"].as_str().unwrap_or_default()
        );
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn payload_parses() {
        assert_eq!(parse_payload(""), Value::Null);
        assert_eq!(parse_payload(" {\"a\":1}\n"), json!({"a": 1}));
        assert_eq!(parse_payload("oops"), json!({"raw": "oops"}));
    }

    #[test]
    fn hook_sends_one_line() {
        let dir = std::env::temp_dir().join(format!("terrarium-hook-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("a.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        send_hook(&path, "Stop", json!({"session_id": "s1"})).expect("send");
        let (stream, _) = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).expect("read");
        let msg: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(
            msg,
            json!({"type": "hook", "hook": "Stop", "payload": {"session_id": "s1"}})
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hook_to_a_missing_socket_fails_fast() {
        let err = send_hook(
            Path::new("/nonexistent/terrarium.sock"),
            "Stop",
            Value::Null,
        );
        assert!(err.is_err());
    }
}
