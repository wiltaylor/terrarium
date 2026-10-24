//! `agent attach`: raw PTY passthrough over the WebSocket of `docs/api.md`
//! §6.5. The terminal goes to raw mode with `stty`, so the CLI needs no
//! unsafe code and no terminal crate. Ctrl-] detaches. SIGWINCH sends a
//! resize.

use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio_tungstenite::tungstenite::Message;

use crate::client::HttpTransport;

/// The detach key: Ctrl-].
pub const DETACH: u8 = 0x1d;

fn stty(args: &[&str]) -> Option<String> {
    let tty = File::open("/dev/tty").ok()?;
    let out = Command::new("stty")
        .args(args)
        .stdin(Stdio::from(tty))
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `(rows, cols)` of the controlling terminal.
pub fn terminal_size() -> Option<(u16, u16)> {
    parse_size(&stty(&["size"])?)
}

fn parse_size(s: &str) -> Option<(u16, u16)> {
    let mut it = s.split_whitespace();
    let rows = it.next()?.parse().ok()?;
    let cols = it.next()?.parse().ok()?;
    (rows > 0 && cols > 0).then_some((rows, cols))
}

/// Raw mode while it lives; the old settings come back on drop.
struct RawMode {
    saved: String,
}

impl RawMode {
    fn enter() -> anyhow::Result<Self> {
        let saved = stty(&["-g"]).context("attach needs a terminal (stty -g failed)")?;
        stty(&["raw", "-echo"]).context("cannot put the terminal in raw mode")?;
        Ok(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = stty(&[self.saved.as_str()]);
    }
}

/// Split input at the detach key. `Some(n)` means: send `n` bytes, then
/// detach.
pub fn detach_at(chunk: &[u8]) -> Option<usize> {
    chunk.iter().position(|&b| b == DETACH)
}

fn resize_msg(rows: u16, cols: u16) -> Message {
    Message::Text(
        json!({"type": "resize", "rows": rows, "cols": cols})
            .to_string()
            .into(),
    )
}

pub async fn attach(http: &HttpTransport, path: &str, agent: &str) -> anyhow::Result<()> {
    let ws = http.websocket(path).await?;
    eprint!("attached to {agent}. Press Ctrl-] to detach.\r\n");
    let raw = RawMode::enter()?;
    let (mut sink, mut stream) = ws.split();
    if let Some((rows, cols)) = terminal_size() {
        sink.send(resize_msg(rows, cols)).await?;
    }

    // Stdin on its own task, so that a blocked read never holds the loop.
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
    let mut stdout = std::io::stdout();

    let reason = loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    stdout.write_all(&b)?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Text(t))) => {
                    stdout.write_all(t.as_bytes())?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Close(_))) | None => break "the agent stopped",
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    drop(raw);
                    return Err(anyhow::Error::new(e).context("the PTY stream broke"));
                }
            },
            input = rx.recv() => match input {
                Some(bytes) => {
                    let (send, detach) = match detach_at(&bytes) {
                        Some(n) => (&bytes[..n], true),
                        None => (&bytes[..], false),
                    };
                    if !send.is_empty() {
                        sink.send(Message::Binary(send.to_vec().into())).await?;
                    }
                    if detach {
                        break "detached";
                    }
                }
                None => break "stdin closed",
            },
            _ = winch.recv() => {
                if let Some((rows, cols)) = terminal_size() {
                    sink.send(resize_msg(rows, cols)).await?;
                }
            }
        }
    };
    let _ = sink.send(Message::Close(None)).await;
    drop(raw);
    eprintln!("\r\n{reason}.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("40 120\n"), Some((40, 120)));
        assert_eq!(parse_size("0 0"), None);
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn detach_key_splits() {
        assert_eq!(detach_at(b"ls\r"), None);
        assert_eq!(detach_at(b"ab\x1dcd"), Some(2));
        assert_eq!(detach_at(b"\x1d"), Some(0));
    }
}
