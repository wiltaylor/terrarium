#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;
use terrarium_host::outbox::Outbox;
use terrarium_host::protocol::{ExitReason, PtySize};
use terrarium_host::session::{ExecPlan, SessionInput, spawn};

fn plan(script: &str) -> ExecPlan {
    ExecPlan { program: r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".into(),
        args: vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), script.into()],
        capture_stdout: true, ..Default::default() }
}

#[test]
fn linux_update_offer_is_refused_before_staging() {
    use terrarium_host::protocol::UpdateOffer;
    use terrarium_host::selfupdate::decide;
    let mut offer = UpdateOffer { os: Some("linux".into()), arch: Some("x86_64".into()),
        version: "99.0.0+test".into(), path: "/install/terrarium-host?arch=x86_64".into(),
        sha256: "0".repeat(64), release_sha256: Some("0".repeat(64)) };
    assert!(decide(&offer, "0.1.0+old", true, true, None).unwrap_err().contains("foreign asset"));
    offer.os = None; offer.arch = None;
    assert!(decide(&offer, "0.1.0+old", true, true, None).unwrap_err().contains("foreign asset"));
    offer.os = Some("windows".into()); offer.arch = Some("aarch64".into());
    assert!(decide(&offer, "0.1.0+old", true, true, None).unwrap_err().contains("foreign asset"));
    offer.arch = Some("x86_64".into()); offer.release_sha256 = None;
    assert!(decide(&offer, "0.1.0+old", true, true, None).unwrap_err().contains("no release checksum"));
}

#[tokio::test]
async fn pipe_session_stdin_capture_and_exit() {
    let running = spawn(plan("$x=[Console]::ReadLine(); [Console]::WriteLine($x); exit 7"), Arc::new(Outbox::new(1, 65536))).unwrap();
    running.input.send(SessionInput::Stdin(b"host session\r\n".to_vec())).unwrap();
    running.input.send(SessionInput::Eof).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), running.done).await.unwrap().unwrap();
    assert_eq!(result.code, Some(7));
    assert!(String::from_utf8_lossy(&result.stdout).contains("host session"));
}

#[tokio::test]
async fn conpty_session_output_and_resize() {
    let mut plan = plan("[Console]::WriteLine('host-conpty')");
    plan.pty = Some(PtySize::default());
    let running = spawn(plan, Arc::new(Outbox::new(2, 65536))).unwrap();
    running.input.send(SessionInput::Resize(PtySize { cols: 100, rows: 40 })).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), running.done).await.unwrap().unwrap();
    assert_eq!(result.code, Some(0));
    assert!(String::from_utf8_lossy(&result.stdout).contains("host-conpty"));
}

#[tokio::test]
async fn timeout_terminates_session() {
    let mut plan = plan("Start-Sleep -Seconds 120");
    plan.timeout = Some(Duration::from_millis(200));
    let running = spawn(plan, Arc::new(Outbox::new(3, 65536))).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), running.done).await.unwrap().unwrap();
    assert_eq!(result.reason, ExitReason::Timeout);
}

#[tokio::test]
async fn transfer_verifies_checksum_and_replaces_destination() {
    use terrarium_host::transfer::{PushFile, sha256_hex};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("transfer.bin");
    for (session, data) in [(4, b"first".as_slice()), (5, b"replacement".as_slice())] {
        let mut file = PushFile::open(&path, Some(0o600), false, session).await.unwrap();
        file.write(data).await.unwrap();
        file.finish(data.len() as u64, &sha256_hex(data)).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }
}

#[test]
fn service_update_replacement_never_removes_current_path() {
    use terrarium_host::selfupdate::{self, sidecar};
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("host.exe");
    std::fs::write(&binary, b"old").unwrap();
    std::fs::write(sidecar(&binary, selfupdate::NEW), b"new").unwrap();
    selfupdate::windows_prepare_replacement(&binary, "test-new").unwrap();
    assert_eq!(std::fs::read(&binary).unwrap(), b"old");
    assert_eq!(std::fs::read(sidecar(&binary, selfupdate::PREV)).unwrap(), b"old");
    assert_eq!(selfupdate::confirm(&binary), None);
    selfupdate::windows_replace(&sidecar(&binary, selfupdate::NEW), &binary).unwrap();
    assert_eq!(std::fs::read(&binary).unwrap(), b"new");
    selfupdate::windows_replace(&sidecar(&binary, selfupdate::PREV), &binary).unwrap();
    assert_eq!(std::fs::read(&binary).unwrap(), b"old");
}

#[tokio::test]
async fn bare_program_resolves_from_request_path() {
    let mut plan = plan("[Console]::WriteLine('bare-program-ok')");
    plan.program = "powershell.exe".into();
    let running = spawn(plan, Arc::new(Outbox::new(6, 65536))).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), running.done).await.unwrap().unwrap();
    assert_eq!(result.code, Some(0));
}
