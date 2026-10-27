//! The processes that the display drivers run (`vmlab vm ...`, `docker
//! exec ... xdotool`, `tesseract`), behind a trait, so the tests can fake
//! them and check each command line.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::io::AsyncWriteExt;

/// The most stdout bytes the host agent keeps from one display command.
pub const MAX_STDOUT: usize = 64 * 1024 * 1024;

/// One command to run.
#[derive(Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// Added to the host agent's environment.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    /// Bytes for stdin (text to type). Not shown by `Debug`.
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cmd")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env)
            .field("cwd", &self.cwd)
            .field("stdin", &self.stdin.as_ref().map(Vec::len))
            .finish()
    }
}

impl Cmd {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env: Vec::new(),
            cwd: None,
            stdin: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// The command line, for tests and logs: the program and its arguments.
    pub fn line(&self) -> Vec<String> {
        let mut v = vec![self.program.clone()];
        v.extend(self.args.iter().cloned());
        v
    }
}

/// What a command did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOut {
    /// The exit code. `None` when a signal ended the process.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CmdOut {
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Self {
        Self {
            code: Some(code),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The last 1000 characters of stderr, trimmed.
    pub fn stderr_tail(&self) -> String {
        let s = String::from_utf8_lossy(&self.stderr);
        let s = s.trim();
        let n = s.chars().count();
        s.chars().skip(n.saturating_sub(1000)).collect()
    }
}

/// Runs commands. `Err` is a spawn failure (for example, no such program).
pub trait Runner: Send + Sync {
    fn run(&self, cmd: Cmd) -> BoxFuture<'_, std::io::Result<CmdOut>>;
}

/// Runs real processes, each in its own process group, killed at the
/// timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, cmd: Cmd) -> BoxFuture<'_, std::io::Result<CmdOut>> {
        Box::pin(async move {
            let mut c = tokio::process::Command::new(&cmd.program);
            c.args(&cmd.args)
                .envs(cmd.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .stdin(if cmd.stdin.is_some() {
                    std::process::Stdio::piped()
                } else {
                    std::process::Stdio::null()
                })
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .process_group(0);
            if let Some(d) = &cmd.cwd {
                c.current_dir(d);
            }
            let mut child = c.spawn()?;
            if let (Some(bytes), Some(mut stdin)) = (cmd.stdin.clone(), child.stdin.take()) {
                tokio::spawn(async move {
                    let _ = stdin.write_all(&bytes).await;
                    let _ = stdin.shutdown().await;
                });
            }
            match tokio::time::timeout(cmd.timeout, child.wait_with_output()).await {
                Ok(out) => {
                    let mut out = out?;
                    out.stdout.truncate(MAX_STDOUT);
                    Ok(CmdOut {
                        code: out.status.code(),
                        stdout: out.stdout,
                        stderr: out.stderr,
                    })
                }
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{} did not end in {} s", cmd.program, cmd.timeout.as_secs()),
                )),
            }
        })
    }
}

type Respond = dyn Fn(&Cmd) -> std::io::Result<CmdOut> + Send + Sync;

/// A runner for tests: it records each command and answers with a
/// function of the command.
#[derive(Clone)]
pub struct FakeRunner {
    pub calls: Arc<Mutex<Vec<Cmd>>>,
    respond: Arc<Respond>,
}

impl FakeRunner {
    pub fn new(respond: impl Fn(&Cmd) -> std::io::Result<CmdOut> + Send + Sync + 'static) -> Self {
        Self {
            calls: Arc::default(),
            respond: Arc::new(respond),
        }
    }

    /// A runner that answers every command with exit 0 and no output.
    pub fn ok() -> Self {
        Self::new(|_| Ok(CmdOut::ok(Vec::new())))
    }

    /// The command lines so far.
    pub fn lines(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(Cmd::line)
            .collect()
    }

    /// The commands so far.
    pub fn cmds(&self) -> Vec<Cmd> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Runner for FakeRunner {
    fn run(&self, cmd: Cmd) -> BoxFuture<'_, std::io::Result<CmdOut>> {
        let out = (self.respond)(&cmd);
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cmd);
        Box::pin(async move { out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn system_runner_feeds_stdin_and_reads_stdout() {
        let mut c = Cmd::new("cat", vec![]);
        c.stdin = Some(b"hello".to_vec());
        let out = SystemRunner.run(c).await.unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, b"hello");
        let missing = SystemRunner
            .run(Cmd::new("/nonexistent/xdotool", vec![]))
            .await;
        assert_eq!(missing.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn system_runner_times_out() {
        let mut c = Cmd::new("sleep", vec!["5".into()]);
        c.timeout = Duration::from_millis(100);
        let e = SystemRunner.run(c).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn debug_hides_stdin() {
        let mut c = Cmd::new("xdotool", vec!["type".into()]);
        c.stdin = Some(b"secret".to_vec());
        assert!(!format!("{c:?}").contains("secret"));
    }
}
