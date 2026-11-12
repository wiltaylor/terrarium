//! Direct service-account sessions. JOB_LIST containment precedes execution.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::outbox::Outbox;
use crate::protocol::{CHUNK_SIZE, ExitReason, StreamTag};
use crate::session::{CAPTURE_LIMIT, ExecPlan, ExitInfo, Running, SessionInput, SpawnError};
use terrarium_winapi::{Child, Limits, Size};

fn reader(mut file: std::fs::File, stream: StreamTag, outbox: Arc<Outbox>,
    capture: Option<Arc<Mutex<Vec<u8>>>>, overflow: mpsc::UnboundedSender<()>) -> tokio::task::JoinHandle<()> {
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let mut buf = vec![0; CHUNK_SIZE];
        while let Ok(n) = file.read(&mut buf) {
            if n == 0 { break; }
            if let Some(capture) = &capture {
                let mut data = capture.lock().unwrap_or_else(|e| e.into_inner());
                let room = CAPTURE_LIMIT.saturating_sub(data.len()).min(n);
                data.extend_from_slice(&buf[..room]);
            }
            if rt.block_on(outbox.push(stream, buf[..n].to_vec())).is_err() {
                let _ = overflow.send(());
                break;
            }
        }
    })
}

pub fn spawn(plan: ExecPlan, outbox: Arc<Outbox>) -> Result<Running, SpawnError> {
    let Child { job, process, console, stdin, stdout, stderr } = terrarium_winapi::spawn(
        std::path::Path::new(&plan.program), &plan.args, plan.cwd.as_deref(), &plan.env,
        plan.pty.map(|p| Size { cols: p.cols, rows: p.rows }), Limits::default())
        .map_err(|source| SpawnError::Io { program: plan.program.clone(), source })?;
    let pid = Some(process.id());
    let capture = plan.capture_stdout.then(|| Arc::new(Mutex::new(Vec::new())));
    let (overflow, mut overflow_rx) = mpsc::unbounded_channel();
    let output = reader(stdout, StreamTag::Stdout, outbox.clone(), capture.clone(), overflow.clone());
    let error = stderr.map(|file| reader(file, StreamTag::Stderr, outbox, None, overflow.clone()));
    let (write, writes) = std::sync::mpsc::sync_channel::<Option<Vec<u8>>>(8);
    let writer = tokio::task::spawn_blocking(move || {
        let mut stdin = stdin;
        while let Ok(Some(data)) = writes.recv() {
            if stdin.write_all(&data).and_then(|()| stdin.flush()).is_err() { break; }
        }
    });
    let (input, mut messages) = mpsc::unbounded_channel();
    let done = tokio::spawn(async move {
        // Declaration order guarantees tree termination before console close,
        // including when this future is cancelled by runtime shutdown.
        struct Containment { job: terrarium_winapi::Job, console: Option<terrarium_winapi::Console> }
        let containment = Containment { job, console };
        let _private_file = plan.private_file;
        let deadline = async {
            match plan.timeout {
                Some(t) => tokio::time::sleep(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline);
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut write = Some(write);
        let mut info = ExitInfo { code: None, signal: None, reason: ExitReason::Exited, stdout: Vec::new() };
        loop {
            tokio::select! {
                _ = tick.tick() => match process.try_wait() {
                    Ok(Some(code)) => { info.code = Some(code as i32); break; }
                    Ok(None) => {}
                    Err(e) => { tracing::warn!(error = %e, "Windows process wait failed"); break; }
                },
                () = &mut deadline => { info.reason = ExitReason::Timeout; break; }
                Some(()) = overflow_rx.recv() => { info.reason = ExitReason::OutputOverflow; break; }
                message = messages.recv() => match message {
                    Some(SessionInput::Stdin(data)) => {
                        if write.as_ref().is_some_and(|w| w.try_send(Some(data)).is_err()) { break; }
                    }
                    Some(SessionInput::Eof) => { write.take(); }
                    Some(SessionInput::Resize(size)) => {
                        if let Some(console) = &containment.console {
                            if let Err(e) = console.resize(Size { cols: size.cols, rows: size.rows }) {
                                tracing::warn!(error = %e, "Windows terminal resize failed");
                            }
                        }
                    }
                    Some(SessionInput::Signal(signal)) => {
                        info.signal = Some(signal as i32); info.reason = ExitReason::Signaled; break;
                    }
                    Some(SessionInput::Kill(reason)) => { info.reason = reason; break; }
                    None => break,
                }
            }
        }
        // Always kill descendants, even when the original process exited normally.
        let _ = containment.job.terminate();
        drop(write);
        // Keep readers active while closing ConPTY, which can drain buffered output.
        let close = tokio::task::spawn_blocking(move || drop(containment));
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = output.await;
            if let Some(error) = error { let _ = error.await; }
            let _ = writer.await;
            let _ = close.await;
        }).await;
        if let Some(capture) = capture { info.stdout = std::mem::take(&mut *capture.lock().unwrap_or_else(|e| e.into_inner())); }
        info
    });
    Ok(Running { pid, input, done })
}
