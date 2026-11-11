//! The exec runner: one process, with pipes or a PTY.
//!
//! [`spawn`] starts the process and returns a [`Running`] handle. Output
//! goes into the session's [`Outbox`]. The caller sends [`SessionInput`] for
//! stdin, resize, and signals. The task in [`Running::done`] ends with an
//! [`ExitInfo`] when the process has exited and its output is drained.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::outbox::Outbox;
use crate::protocol::{CHUNK_SIZE, ExitReason, PtySize, StreamTag};

/// How long to wait for the output pipes to close after the process exits.
/// A background grandchild can hold them open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The largest stdout a session keeps for a result parser.
pub const CAPTURE_LIMIT: usize = 4 * 1024 * 1024;

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Added to the host agent's environment.
    pub env: BTreeMap<String, String>,
    /// Run in a PTY of this size.
    pub pty: Option<PtySize>,
    pub timeout: Option<Duration>,
    /// Keep stdout (up to [`CAPTURE_LIMIT`]) for a result parser.
    pub capture_stdout: bool,
    /// The systemd scope that the process runs in (a target with resource
    /// limits, [`crate::limits::wrap`]). A kill of the session kills the
    /// scope too.
    pub scope: Option<String>,
    /// A private file that the program reads (a `docker --env-file`). The
    /// session holds it until the process exits, then it is removed.
    pub private_file: Option<PrivateFile>,
}

/// A file with mode 0600 in a private dir (mode 0700), removed when the
/// last clone is dropped. It carries values that must not show in `ps` or
/// reach the env of the program itself. A crash of the host agent can leave
/// one behind: [`remove_private_files`] at the next start deletes it.
#[derive(Debug, Clone)]
pub struct PrivateFile(Arc<tempfile::TempPath>);

impl PrivateFile {
    /// Write `data` to a new file `<prefix><random>` in `dir`. The dir is
    /// checked with [`private_dir`] first.
    pub fn new(dir: &Path, prefix: &str, data: &[u8]) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        private_dir(dir)?;
        let mut f = tempfile::Builder::new()
            .prefix(prefix)
            .permissions(std::fs::Permissions::from_mode(0o600))
            .tempfile_in(dir)?;
        f.write_all(data)?;
        f.flush()?;
        Ok(Self(Arc::new(f.into_temp_path())))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl PartialEq for PrivateFile {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path()
    }
}

impl Eq for PrivateFile {}

/// Create `dir` (and its parents, each with mode 0700) and make sure that
/// it is private: `dir` must be a real dir (not a symlink) of this user, and
/// its parent a real dir of this user or a sticky dir of root (like `/tmp`,
/// where no other user can rename or delete `dir`). `dir` gets mode 0700.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let euid = nix::unistd::geteuid().as_raw();
    for (d, is_parent) in [(Some(dir), false), (dir.parent(), true)] {
        let Some(d) = d else { continue };
        let m = std::fs::symlink_metadata(d)?;
        if !owned_ok(m.file_type().is_dir(), m.uid(), m.mode(), euid, is_parent) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a dir of this user (uid {euid}); refusing it for private files",
                    d.display()
                ),
            ));
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// The ownership rule of [`private_dir`] for one dir: a real dir of `euid`,
/// or for the parent also a sticky (`S_ISVTX`) dir of root.
fn owned_ok(is_dir: bool, uid: u32, mode: u32, euid: u32, is_parent: bool) -> bool {
    is_dir && (uid == euid || (is_parent && uid == 0 && mode & 0o1000 != 0))
}

/// Delete the files in `dir` whose name starts with `prefix`: the private
/// files that a crashed host agent left behind. Run it at the start, before
/// any session. It does not recurse and follows no symlink. A file that it
/// cannot delete is logged, and it goes on. Returns how many it deleted. A
/// missing dir is no error. Two host agents on one data dir would delete
/// each other's live files: run one per user.
pub fn remove_private_files(dir: &Path, prefix: &str) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(prefix)
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => n += 1,
            Err(e) => tracing::warn!(
                path = %entry.path().display(),
                error = %e,
                "cannot delete a leftover private file"
            ),
        }
    }
    Ok(n)
}

/// Input for a running session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    Stdin(Vec<u8>),
    Eof,
    Resize(PtySize),
    Signal(Signal),
    /// Kill the process group and report this reason.
    Kill(ExitReason),
}

/// How the session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: ExitReason,
    /// Stdout, when the plan asked for it.
    pub stdout: Vec<u8>,
}

/// A started session.
pub struct Running {
    pub pid: Option<u32>,
    pub input: mpsc::UnboundedSender<SessionInput>,
    pub done: JoinHandle<ExitInfo>,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("cannot start {program}: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot start {program} in a pty: {message}")]
    Pty { program: String, message: String },
    /// The working directory is missing or unusable. A spawn reports a
    /// failed `chdir` with the program name (ENOENT reads as a missing
    /// binary), so it is checked first (TER-639).
    #[error("working directory {}", cwd_problem(.path, .source))]
    Cwd {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn cwd_problem(path: &Path, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => format!("{} does not exist", path.display()),
        std::io::ErrorKind::NotADirectory => format!("{} is not a directory", path.display()),
        _ => format!("{}: {e}", path.display()),
    }
}

/// The working directory of `plan`, if it has one, is a directory.
fn check_cwd(plan: &ExecPlan) -> Result<(), SpawnError> {
    let Some(path) = &plan.cwd else {
        return Ok(());
    };
    let err = |source| SpawnError::Cwd {
        path: path.clone(),
        source,
    };
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(err(std::io::Error::from(std::io::ErrorKind::NotADirectory))),
        Err(e) => Err(err(e)),
    }
}

/// Start the plan. Output goes to `outbox`.
pub fn spawn(plan: ExecPlan, outbox: Arc<Outbox>) -> Result<Running, SpawnError> {
    check_cwd(&plan)?;
    if plan.pty.is_some() {
        spawn_pty(plan, outbox)
    } else {
        spawn_pipes(plan, outbox)
    }
}

/// Shared stdout capture for result parsers.
#[derive(Clone, Default)]
struct Capture(Option<Arc<std::sync::Mutex<Vec<u8>>>>);

impl Capture {
    fn new(on: bool) -> Self {
        Self(on.then(Default::default))
    }

    fn add(&self, data: &[u8]) {
        if let Some(buf) = &self.0 {
            let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
            let room = CAPTURE_LIMIT.saturating_sub(b.len());
            b.extend_from_slice(&data[..data.len().min(room)]);
        }
    }

    fn take(&self) -> Vec<u8> {
        match &self.0 {
            Some(buf) => std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner())),
            None => Vec::new(),
        }
    }
}

fn signal_group(pid: Option<u32>, sig: Signal) {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) else {
        return;
    };
    if let Err(e) = killpg(Pid::from_raw(pid), sig) {
        tracing::debug!(pid, ?sig, error = %e, "killpg failed");
    }
}

/// Kill the process group, and the scope when the session has one: a
/// process that left the group ends too.
fn kill_all(pid: Option<u32>, scope: Option<&str>) {
    signal_group(pid, Signal::SIGKILL);
    if let Some(scope) = scope {
        crate::limits::kill_scope(scope);
    }
}

/// Kills the process group of a session when the session task drops
/// before the process exits (TER-526): a runtime that shuts down, as in a
/// host agent that stops or a test that ends early, leaves no child of
/// the session. `kill_on_drop` alone kills only the group leader, and the
/// children of a recipe kept running with its open fds. The guard sends no
/// scope kill: that needs a runtime, and a drop can run without one.
struct GroupGuard(Option<u32>);

impl GroupGuard {
    /// The process exited: its group id may be reused, so do not kill it.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        signal_group(self.0, Signal::SIGKILL);
    }
}

/// Sleep until the deadline, or forever without one.
async fn deadline(timeout: Option<Duration>) {
    match timeout {
        Some(t) => tokio::time::sleep(t).await,
        None => std::future::pending().await,
    }
}

// ------------------------------------------------------------------ pipes

fn spawn_pipes(plan: ExecPlan, outbox: Arc<Outbox>) -> Result<Running, SpawnError> {
    let mut cmd = tokio::process::Command::new(&plan.program);
    cmd.args(&plan.args)
        .envs(&plan.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(cwd) = &plan.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|source| SpawnError::Io {
        program: plan.program.clone(),
        source,
    })?;
    let pid = child.id();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<ExitReason>();
    let capture = Capture::new(plan.capture_stdout);

    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(
            out,
            StreamTag::Stdout,
            outbox.clone(),
            kill_tx.clone(),
            capture.clone(),
        )));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(
            err,
            StreamTag::Stderr,
            outbox.clone(),
            kill_tx.clone(),
            Capture::default(),
        )));
    }

    // Stdin gets its own task, so a full pipe does not block signals.
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut stdin = child.stdin.take();
    let stdin_task = tokio::spawn(async move {
        while let Some(data) = stdin_rx.recv().await {
            let Some(w) = stdin.as_mut() else { break };
            if w.write_all(&data).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
        // Dropping the pipe closes stdin.
    });

    let timeout = plan.timeout;
    let scope = plan.scope.clone();
    let private_file = plan.private_file;
    let mut group = GroupGuard(pid);
    let done = tokio::spawn(async move {
        // Removed when the task ends, after the process has exited.
        let _private_file = private_file;
        let scope = scope.as_deref();
        let mut stdin_tx = Some(stdin_tx);
        let mut forced: Option<ExitReason> = None;
        let timer = deadline(timeout);
        tokio::pin!(timer);
        let mut timed_out = false;
        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                input = input_rx.recv() => match input {
                    Some(SessionInput::Stdin(data)) => {
                        if let Some(tx) = &stdin_tx {
                            let _ = tx.send(data);
                        }
                    }
                    Some(SessionInput::Eof) => stdin_tx = None,
                    Some(SessionInput::Resize(_)) => {}
                    Some(SessionInput::Signal(sig)) => signal_group(pid, sig),
                    Some(SessionInput::Kill(reason)) => {
                        forced.get_or_insert(reason);
                        kill_all(pid, scope);
                    }
                    // Nobody holds the session any more.
                    None => {
                        kill_all(pid, scope);
                        let _ = child.start_kill();
                        break child.wait().await;
                    }
                },
                Some(reason) = kill_rx.recv() => {
                    forced.get_or_insert(reason);
                    kill_all(pid, scope);
                }
                () = &mut timer, if !timed_out => {
                    timed_out = true;
                    forced.get_or_insert(ExitReason::Timeout);
                    kill_all(pid, scope);
                }
            }
        };
        group.disarm();
        drop(stdin_tx);
        stdin_task.abort();
        drain(pumps).await;
        if forced.is_none() {
            // An overflow can land after the exit while the pipes drain.
            if let Ok(reason) = kill_rx.try_recv() {
                forced = Some(reason);
            }
        }
        let (code, signal) = match &status {
            Ok(st) => {
                use std::os::unix::process::ExitStatusExt;
                (st.code(), st.signal())
            }
            Err(e) => {
                tracing::warn!(error = %e, "wait failed");
                (None, None)
            }
        };
        ExitInfo {
            code,
            signal,
            reason: reason_for(forced, signal),
            stdout: capture.take(),
        }
    });
    Ok(Running {
        pid,
        input: input_tx,
        done,
    })
}

fn reason_for(forced: Option<ExitReason>, signal: Option<i32>) -> ExitReason {
    match (forced, signal) {
        (Some(r), _) => r,
        (None, Some(_)) => ExitReason::Signaled,
        (None, None) => ExitReason::Exited,
    }
}

async fn drain(pumps: Vec<JoinHandle<()>>) {
    for mut p in pumps {
        if tokio::time::timeout(DRAIN_GRACE, &mut p).await.is_err() {
            p.abort();
        }
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    stream: StreamTag,
    outbox: Arc<Outbox>,
    kill: mpsc::UnboundedSender<ExitReason>,
    capture: Capture,
) {
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                capture.add(&buf[..n]);
                if outbox.push(stream, buf[..n].to_vec()).await.is_err() {
                    let _ = kill.send(ExitReason::OutputOverflow);
                    break;
                }
            }
        }
    }
}

// ------------------------------------------------------------------ pty

fn spawn_pty(plan: ExecPlan, outbox: Arc<Outbox>) -> Result<Running, SpawnError> {
    use portable_pty::{CommandBuilder, PtySize as PortableSize, native_pty_system};

    let pty_err = |e: anyhow::Error| SpawnError::Pty {
        program: plan.program.clone(),
        message: format!("{e:#}"),
    };
    let size = plan.pty.unwrap_or_default();
    let pair = native_pty_system()
        .openpty(PortableSize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(pty_err)?;
    let mut cmd = CommandBuilder::new(&plan.program);
    cmd.args(&plan.args);
    if !plan.env.contains_key("TERM") {
        cmd.env("TERM", "xterm-256color");
    }
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    match &plan.cwd {
        Some(cwd) => cmd.cwd(cwd),
        None => {
            if let Ok(cwd) = std::env::current_dir() {
                cmd.cwd(cwd);
            }
        }
    }
    let mut child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
    drop(pair.slave);
    let pid = child.process_id();
    let mut reader = pair.master.try_clone_reader().map_err(pty_err)?;
    let mut writer = pair.master.take_writer().map_err(pty_err)?;
    let master = pair.master;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<ExitReason>();
    let capture = Capture::new(plan.capture_stdout);

    // Reader: a blocking thread that pushes into the async outbox.
    let handle = tokio::runtime::Handle::current();
    let reader_outbox = outbox.clone();
    let reader_capture = capture.clone();
    let reader_task = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            match reader.read(&mut buf) {
                // EIO after the last slave fd closes is the normal end.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    reader_capture.add(&buf[..n]);
                    let data = buf[..n].to_vec();
                    if handle
                        .block_on(reader_outbox.push(StreamTag::Stdout, data))
                        .is_err()
                    {
                        let _ = kill_tx.send(ExitReason::OutputOverflow);
                        break;
                    }
                }
            }
        }
    });

    // Writer: a blocking thread fed by a channel. `None` writes ^D.
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    let writer_task = tokio::task::spawn_blocking(move || {
        while let Some(item) = stdin_rx.blocking_recv() {
            let data = item.unwrap_or_else(|| vec![0x04]);
            if writer.write_all(&data).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    // Waiter: a blocking thread for the child's exit.
    let (exit_tx, mut exit_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _ = exit_tx.send(child.wait());
    });

    let timeout = plan.timeout;
    let scope = plan.scope.clone();
    let private_file = plan.private_file;
    let mut group = GroupGuard(pid);
    let done = tokio::spawn(async move {
        // Removed when the task ends, after the process has exited.
        let _private_file = private_file;
        let scope = scope.as_deref();
        let mut forced: Option<ExitReason> = None;
        let timer = deadline(timeout);
        tokio::pin!(timer);
        let mut timed_out = false;
        let status = loop {
            tokio::select! {
                status = &mut exit_rx => break status.ok(),
                input = input_rx.recv() => match input {
                    Some(SessionInput::Stdin(data)) => { let _ = stdin_tx.send(Some(data)); }
                    Some(SessionInput::Eof) => { let _ = stdin_tx.send(None); }
                    Some(SessionInput::Resize(sz)) => {
                        let _ = master.resize(PortableSize {
                            rows: sz.rows,
                            cols: sz.cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    Some(SessionInput::Signal(sig)) => signal_group(pid, sig),
                    Some(SessionInput::Kill(reason)) => {
                        forced.get_or_insert(reason);
                        kill_all(pid, scope);
                    }
                    None => {
                        kill_all(pid, scope);
                        break (&mut exit_rx).await.ok();
                    }
                },
                Some(reason) = kill_rx.recv() => {
                    forced.get_or_insert(reason);
                    kill_all(pid, scope);
                }
                () = &mut timer, if !timed_out => {
                    timed_out = true;
                    forced.get_or_insert(ExitReason::Timeout);
                    kill_all(pid, scope);
                }
            }
        };
        group.disarm();
        drop(stdin_tx);
        writer_task.abort();
        let mut reader_task = reader_task;
        if tokio::time::timeout(DRAIN_GRACE, &mut reader_task)
            .await
            .is_err()
        {
            // A grandchild holds the slave. Stop waiting; the thread ends
            // when the master closes below.
            reader_task.abort();
        }
        drop(master);
        if forced.is_none()
            && let Ok(reason) = kill_rx.try_recv()
        {
            forced = Some(reason);
        }
        let (code, signal) = match status {
            Some(Ok(st)) => match st.signal() {
                Some(name) => (None, signal_number(name)),
                None => (i32::try_from(st.exit_code()).ok(), None),
            },
            Some(Err(e)) => {
                tracing::warn!(error = %e, "pty wait failed");
                (None, None)
            }
            None => (None, None),
        };
        ExitInfo {
            code,
            signal,
            reason: reason_for(forced, signal),
            stdout: capture.take(),
        }
    });
    Ok(Running {
        pid,
        input: input_tx,
        done,
    })
}

/// portable-pty reports a signal by its `strsignal` text. Map the common ones
/// back to numbers (glibc and musl agree on these).
pub fn signal_number(text: &str) -> Option<i32> {
    use Signal as S;
    let sig = match text {
        "Hangup" => S::SIGHUP,
        "Interrupt" => S::SIGINT,
        "Quit" => S::SIGQUIT,
        "Illegal instruction" => S::SIGILL,
        "Trace/breakpoint trap" => S::SIGTRAP,
        "Aborted" => S::SIGABRT,
        "Bus error" => S::SIGBUS,
        "Floating point exception" => S::SIGFPE,
        "Killed" => S::SIGKILL,
        "User defined signal 1" => S::SIGUSR1,
        "Segmentation fault" => S::SIGSEGV,
        "User defined signal 2" => S::SIGUSR2,
        "Broken pipe" => S::SIGPIPE,
        "Alarm clock" => S::SIGALRM,
        "Terminated" => S::SIGTERM,
        other => {
            return other
                .strip_prefix("Signal ")
                .and_then(|n| n.trim().parse().ok());
        }
    };
    Some(sig as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::Outgoing;

    fn plan(program: &str, args: &[&str]) -> ExecPlan {
        ExecPlan {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn collect(rx: &mut mpsc::UnboundedReceiver<Outgoing>, tag: StreamTag) -> Vec<u8> {
        let mut v = Vec::new();
        while let Ok(o) = rx.try_recv() {
            if let Outgoing::Data(d) = o
                && d.stream == tag
            {
                v.extend(d.payload);
            }
        }
        v
    }

    #[tokio::test]
    async fn pipes_split_streams_and_report_the_code() {
        let ob = Arc::new(Outbox::new(1, 1 << 20));
        let (tx, mut rx) = mpsc::unbounded_channel();
        ob.attach(tx, 0);
        let mut p = plan("sh", &["-c", "echo out; echo err 1>&2; exit 3"]);
        p.capture_stdout = true;
        let run = spawn(p, ob.clone()).unwrap();
        let info = run.done.await.unwrap();
        assert_eq!(info.code, Some(3));
        assert_eq!(info.reason, ExitReason::Exited);
        assert_eq!(info.stdout, b"out\n");
        let mut all = Vec::new();
        while let Ok(o) = rx.try_recv() {
            all.push(o);
        }
        let out: Vec<u8> = all
            .iter()
            .filter_map(|o| match o {
                Outgoing::Data(d) if d.stream == StreamTag::Stdout => Some(d.payload.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        let err: Vec<u8> = all
            .iter()
            .filter_map(|o| match o {
                Outgoing::Data(d) if d.stream == StreamTag::Stderr => Some(d.payload.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(out, b"out\n");
        assert_eq!(err, b"err\n");
    }

    /// TER-639: a missing working directory is named, not the program
    /// (a failed chdir reads as "cannot start <program>: No such file").
    #[tokio::test]
    async fn a_missing_working_directory_is_named_not_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        for pty in [None, Some(PtySize::default())] {
            let ob = Arc::new(Outbox::new(1, 1 << 20));
            let mut p = plan("true", &[]);
            p.pty = pty;
            p.cwd = Some(missing.clone());
            let e = spawn(p.clone(), ob.clone()).err().unwrap().to_string();
            assert_eq!(
                e,
                format!("working directory {} does not exist", missing.display())
            );
            assert!(!e.contains("cannot start"), "{e}");
            p.cwd = Some(file.clone());
            let e = spawn(p, ob).err().unwrap().to_string();
            assert_eq!(
                e,
                format!("working directory {} is not a directory", file.display())
            );
        }
    }

    #[tokio::test]
    async fn stdin_reaches_the_process() {
        let ob = Arc::new(Outbox::new(1, 1 << 20));
        let (tx, mut rx) = mpsc::unbounded_channel();
        ob.attach(tx, 0);
        let run = spawn(plan("cat", &[]), ob.clone()).unwrap();
        run.input
            .send(SessionInput::Stdin(b"ping\n".to_vec()))
            .unwrap();
        run.input.send(SessionInput::Eof).unwrap();
        let info = run.done.await.unwrap();
        assert_eq!(info.code, Some(0));
        assert_eq!(collect(&mut rx, StreamTag::Stdout), b"ping\n");
    }

    #[tokio::test]
    async fn timeout_kills_the_process_group() {
        let ob = Arc::new(Outbox::new(1, 1 << 20));
        let mut p = plan("sh", &["-c", "sleep 30 & sleep 30"]);
        p.timeout = Some(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let info = spawn(p, ob).unwrap().done.await.unwrap();
        assert_eq!(info.reason, ExitReason::Timeout);
        assert_eq!(info.signal, Some(9));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// TER-526: a runtime that shuts down while a session runs (a test
    /// that ends early) kills the whole process group, not only the
    /// leader: no background child of the session stays.
    #[test]
    fn a_dropped_session_kills_its_process_group() {
        let pidfile = std::env::temp_dir().join(format!(
            "terrarium-host-ter526-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&pidfile);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
        let (run, child) = rt.block_on(async {
            let run = spawn(
                plan("sh", &["-c", &script]),
                Arc::new(Outbox::new(1, 1 << 20)),
            )
            .unwrap();
            let started = std::time::Instant::now();
            loop {
                let text = std::fs::read_to_string(&pidfile).unwrap_or_default();
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break (run, pid);
                }
                assert!(started.elapsed() < Duration::from_secs(30), "no child pid");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        drop(rt);
        let _ = std::fs::remove_file(&pidfile);
        // TER-590: under load a killed child can take seconds to reach
        // the zombie state, so a SIGKILL that landed counts as killed.
        let killed = || child_killed(child);
        let started = std::time::Instant::now();
        while !killed() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let left = (!killed()).then(|| child_state(child));
        if left.is_some() {
            let _ = nix::sys::signal::kill(Pid::from_raw(child), Signal::SIGKILL);
        }
        drop(run);
        if let Some(state) = left {
            panic!("the background child {child} outlived the session: {state}");
        }
    }

    /// The child is gone, a zombie, exiting, or has a SIGKILL pending.
    fn child_killed(pid: i32) -> bool {
        if nix::sys::signal::kill(Pid::from_raw(pid), None).is_err() {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return true;
        };
        // After the comm: state, ppid, pgrp, session, tty_nr, tpgid, flags.
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .map(|(_, r)| r.split_whitespace().collect())
            .unwrap_or_default();
        const PF_EXITING: u64 = 0x4;
        let state_dead = fields
            .first()
            .is_some_and(|s| matches!(*s, "Z" | "X" | "x"));
        let exiting = fields
            .get(6)
            .and_then(|f| f.parse::<u64>().ok())
            .is_some_and(|f| f & PF_EXITING != 0);
        state_dead || exiting || sigkill_pending(pid)
    }

    /// SIGKILL is in the thread or the shared pending set of the process.
    fn sigkill_pending(pid: i32) -> bool {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false;
        };
        const SIGKILL_BIT: u64 = 1 << (9 - 1);
        status.lines().any(|line| {
            ["SigPnd:", "ShdPnd:"].iter().any(|key| {
                line.strip_prefix(key)
                    .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
                    .is_some_and(|mask| mask & SIGKILL_BIT != 0)
            })
        })
    }

    /// The stat line and pending signals of a process, for a failure.
    fn child_state(pid: i32) -> String {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let pending: Vec<&str> = status
            .lines()
            .filter(|l| l.starts_with("SigPnd:") || l.starts_with("ShdPnd:"))
            .collect();
        let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan")).unwrap_or_default();
        format!("stat={} {} wchan={wchan}", stat.trim(), pending.join(" "))
    }

    #[tokio::test]
    async fn signal_frames_reach_the_group() {
        let ob = Arc::new(Outbox::new(1, 1 << 20));
        let run = spawn(plan("sleep", &["30"]), ob).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        run.input
            .send(SessionInput::Signal(Signal::SIGTERM))
            .unwrap();
        let info = run.done.await.unwrap();
        assert_eq!(info.reason, ExitReason::Signaled);
        assert_eq!(info.signal, Some(15));
    }

    #[tokio::test]
    async fn detached_overflow_kills_with_a_reason() {
        let ob = Arc::new(Outbox::new(1, 64 * 1024));
        // No link: the buffer fills and the session is killed.
        let p = plan("sh", &["-c", "yes overflow"]);
        let info = spawn(p, ob.clone()).unwrap().done.await.unwrap();
        assert_eq!(info.reason, ExitReason::OutputOverflow);
        assert!(ob.buffered() <= 64 * 1024);
    }

    #[tokio::test]
    async fn pty_reports_exit_and_resizes() {
        let ob = Arc::new(Outbox::new(1, 1 << 20));
        let (tx, mut rx) = mpsc::unbounded_channel();
        ob.attach(tx, 0);
        let mut p = plan("sh", &["-c", "read x; stty size; exit 4"]);
        p.pty = Some(PtySize { rows: 24, cols: 80 });
        let run = spawn(p, ob).unwrap();
        run.input
            .send(SessionInput::Resize(PtySize {
                rows: 33,
                cols: 101,
            }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        run.input
            .send(SessionInput::Stdin(b"go\n".to_vec()))
            .unwrap();
        let info = run.done.await.unwrap();
        assert_eq!(info.code, Some(4));
        let out = String::from_utf8_lossy(&collect(&mut rx, StreamTag::Stdout)).to_string();
        assert!(out.contains("33 101"), "pty output: {out:?}");
    }

    #[test]
    fn a_sticky_root_parent_is_accepted_and_other_owners_are_refused() {
        use super::owned_ok;
        // The dir itself: only a real dir of this user.
        assert!(owned_ok(true, 1001, 0o700, 1001, false));
        assert!(!owned_ok(true, 0, 0o1777, 1001, false));
        assert!(!owned_ok(false, 1001, 0o700, 1001, false));
        // The parent: this user, or a sticky dir of root (/tmp).
        assert!(owned_ok(true, 1001, 0o755, 1001, true));
        assert!(owned_ok(true, 0, 0o1777, 1001, true));
        assert!(!owned_ok(true, 0, 0o777, 1001, true));
        assert!(!owned_ok(true, 1002, 0o1777, 1001, true));
        assert!(!owned_ok(false, 0, 0o1777, 1001, true));
    }

    #[test]
    fn a_private_dir_in_the_temp_dir_is_accepted() {
        // /tmp (root, sticky) as the parent, as a user other than root.
        let tmp = std::path::Path::new("/tmp");
        let sticky_root = std::fs::symlink_metadata(tmp).is_ok_and(|m| {
            use std::os::unix::fs::MetadataExt;
            m.is_dir() && m.uid() == 0 && m.mode() & 0o1000 != 0
        });
        if !sticky_root {
            return;
        }
        let dir = tmp.join(format!(
            "terrarium-host-test-private-{}",
            std::process::id()
        ));
        let res = super::private_dir(&dir);
        let _ = std::fs::remove_dir(&dir);
        res.unwrap();
    }

    #[test]
    fn signal_text_maps_to_numbers() {
        assert_eq!(signal_number("Killed"), Some(9));
        assert_eq!(signal_number("Terminated"), Some(15));
        assert_eq!(signal_number("Signal 34"), Some(34));
        assert_eq!(signal_number("whatever"), None);
    }
}
