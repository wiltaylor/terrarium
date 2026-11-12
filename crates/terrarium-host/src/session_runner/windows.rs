//! Safe per-session containment. Only the runner joins the job, never the service.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::{Input, Output, Plan, read_frame, write_frame};
use crate::outbox::{Outbox, Outgoing};
use crate::protocol::{ExitReason, StreamTag};
use crate::session::{ExecPlan, ExitInfo, Running, SessionInput, SpawnError, CAPTURE_LIMIT};

fn failure() -> ExitInfo {
    ExitInfo { code: Some(1), signal: None, reason: ExitReason::Exited, stdout: Vec::new() }
}

/// Service side. Killing this owned runner closes its sole job handle,
/// terminating every descendant, including detached grandchildren.
pub fn spawn(plan: ExecPlan, outbox: Arc<Outbox>) -> Result<Running, SpawnError> {
    let io_err = |source| SpawnError::Io { program: plan.program.clone(), source };
    let exe = std::env::current_exe().map_err(io_err)?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("session-runner")
        .creation_flags(0x08000000) // CREATE_NO_WINDOW, not CREATE_BREAKAWAY_FROM_JOB.
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(io_err)?;
    let pid = child.id();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (input, mut input_rx) = mpsc::unbounded_channel();
    let (writer, mut writer_rx) = mpsc::channel(8);
    let request = Plan::from(&plan);
    let writer_task = tokio::spawn(async move {
        write_frame(&mut stdin, &request).await?;
        while let Some(item) = writer_rx.recv().await {
            write_frame(&mut stdin, &item).await?;
        }
        stdin.shutdown().await
    });
    let capture = plan.capture_stdout;
    let mut reader_task = tokio::spawn(async move {
        let mut captured = Vec::new();
        loop {
            match read_frame::<_, Output>(&mut stdout).await? {
                Output::Data(stream, data) => {
                    if capture && stream == StreamTag::Stdout {
                        let n = data.len().min(CAPTURE_LIMIT.saturating_sub(captured.len()));
                        captured.extend_from_slice(&data[..n]);
                    }
                    if outbox.push(stream, data).await.is_err() {
                        return Ok(ExitInfo { reason: ExitReason::OutputOverflow, ..failure() });
                    }
                }
                Output::Exit { code, signal, reason } => {
                    return Ok::<_, std::io::Error>(ExitInfo { code, signal, reason, stdout: captured });
                }
                Output::Error(message) => return Err(std::io::Error::other(message)),
            }
        }
    });
    let done = tokio::spawn(async move {
        let _private_file = plan.private_file;
        let timer = async {
            match plan.timeout {
                Some(t) => tokio::time::sleep(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timer);
        let info = loop {
            tokio::select! {
                result = &mut reader_task => {
                    break match result {
                        Ok(Ok(info)) => info,
                        other => {
                            tracing::warn!(?other, "Windows session runner failed");
                            failure()
                        }
                    };
                }
                () = &mut timer => break ExitInfo { reason: ExitReason::Timeout, ..failure() },
                message = input_rx.recv() => {
                    let message = match message {
                        Some(SessionInput::Kill(reason)) => break ExitInfo { reason, ..failure() },
                        Some(SessionInput::Signal(sig)) => break ExitInfo { signal: Some(sig as i32), reason: ExitReason::Signaled, ..failure() },
                        Some(SessionInput::Stdin(data)) => Input::Stdin(data),
                        Some(SessionInput::Resize(size)) => Input::Resize(size),
                        Some(SessionInput::Eof) => Input::Eof,
                        None => break failure(),
                    };
                    if writer.try_send(message).is_err() {
                        break failure();
                    }
                }
            }
        };
        // The process handle, not a name or a recycled PID, identifies the runner.
        let _ = child.kill().await;
        writer_task.abort();
        reader_task.abort();
        info
    });
    Ok(Running { pid, input, done })
}

/// Hidden runner entry point. Join the job before reading a command or
/// spawning anything. No child receives the job handle or breakaway permission.
pub fn run() -> anyhow::Result<()> {
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&info)?;
    job.assign_current_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(relay());
    rt.shutdown_timeout(Duration::from_millis(100));
    // This also kills this runner. All protocol output has been flushed.
    drop(job);
    result
}

async fn relay() -> anyhow::Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let plan: Plan = read_frame(&mut stdin).await?;
    let outbox = Arc::new(Outbox::new(0, 1 << 20));
    let (tx, mut rx) = mpsc::unbounded_channel();
    outbox.attach(tx, 0);
    let mut running = match crate::session::spawn_direct(plan.into(), outbox.clone()) {
        Ok(r) => r,
        Err(e) => {
            write_frame(&mut stdout, &Output::Error(e.to_string())).await?;
            return Ok(());
        }
    };
    let input = running.input.clone();
    let mut input_task = tokio::spawn(async move {
        loop {
            let item = match read_frame::<_, Input>(&mut stdin).await? {
                Input::Stdin(data) => SessionInput::Stdin(data),
                Input::Resize(size) => SessionInput::Resize(size),
                Input::Eof => SessionInput::Eof,
            };
            input.send(item).map_err(std::io::Error::other)?;
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    });
    let mut acknowledged = 0;
    let info = loop {
        tokio::select! {
            item = rx.recv() => {
                if let Some(Outgoing::Data(data)) = item {
                    acknowledged += data.payload.len() as u64;
                    write_frame(&mut stdout, &Output::Data(data.stream, data.payload)).await?;
                    outbox.ack(acknowledged);
                }
            }
            result = &mut running.done => break result?,
            _ = &mut input_task => {
                // The service died or closed the pipe. Returning drops the job.
                return Ok(());
            }
        }
    };
    while let Ok(Outgoing::Data(data)) = rx.try_recv() {
        write_frame(&mut stdout, &Output::Data(data.stream, data.payload)).await?;
    }
    write_frame(&mut stdout, &Output::Exit { code: info.code, signal: info.signal, reason: info.reason }).await?;
    input_task.abort();
    Ok(())
}
