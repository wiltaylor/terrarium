#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;
use terrarium_host::outbox::Outbox;
use terrarium_host::protocol::{ExitReason, PtySize};
use terrarium_host::session::{ExecPlan, SessionInput, spawn};

fn plan(script: &str) -> ExecPlan {
    ExecPlan {
        program: r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ],
        capture_stdout: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn disabled_run_as_never_attempts_password_logon() {
    let error = terrarium_host::windows_run_as::logon(
        Some(terrarium_host::protocol::WindowsRunAs {
            account: "INVALID\\unprovisioned".into(),
            password: "non-secret-fixture".into(),
        }),
        &Default::default(),
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("disabled"));
    assert!(!error.message.contains("non-secret-fixture"));
}

#[tokio::test]
async fn windows_copy_worker_commits_and_aborts_temporary_files() {
    use terrarium_host::transfer::{PushFile, sha256_hex};
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("output.bin");
    let mut file = PushFile::open(&destination, None, false, 1).await.unwrap();
    file.write(b"binary\0fixture").await.unwrap();
    let expected = sha256_hex(b"binary\0fixture");
    assert_eq!(file.finish(14, &expected).await.unwrap(), (14, expected));
    assert_eq!(std::fs::read(&destination).unwrap(), b"binary\0fixture");
    let mut partial = PushFile::open(&destination, None, false, 2).await.unwrap();
    partial.write(b"partial").await.unwrap();
    partial.abort().await;
    assert_eq!(std::fs::read(&destination).unwrap(), b"binary\0fixture");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn linux_update_offer_is_refused_before_staging() {
    use terrarium_host::protocol::UpdateOffer;
    use terrarium_host::selfupdate::decide;
    let mut offer = UpdateOffer {
        os: Some("linux".into()),
        arch: Some("x86_64".into()),
        version: "99.0.0+test".into(),
        path: "/install/terrarium-host?arch=x86_64".into(),
        sha256: "0".repeat(64),
        release_sha256: Some("0".repeat(64)),
    };
    assert!(
        decide(&offer, "0.1.0+old", true, true, None)
            .unwrap_err()
            .contains("foreign asset")
    );
    offer.os = None;
    offer.arch = None;
    assert!(
        decide(&offer, "0.1.0+old", true, true, None)
            .unwrap_err()
            .contains("foreign asset")
    );
    offer.os = Some("windows".into());
    offer.arch = Some("aarch64".into());
    assert!(
        decide(&offer, "0.1.0+old", true, true, None)
            .unwrap_err()
            .contains("foreign asset")
    );
    offer.arch = Some("x86_64".into());
    offer.release_sha256 = None;
    assert!(
        decide(&offer, "0.1.0+old", true, true, None)
            .unwrap_err()
            .contains("no release checksum")
    );
}

#[tokio::test]
async fn pipe_session_stdin_capture_and_exit() {
    let running = spawn(
        plan("$x=[Console]::ReadLine(); [Console]::WriteLine($x); exit 7"),
        Arc::new(Outbox::new(1, 65536)),
    )
    .unwrap();
    running
        .input
        .send(SessionInput::Stdin(b"host session\r\n".to_vec()))
        .unwrap();
    running.input.send(SessionInput::Eof).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), running.done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.code, Some(7));
    assert!(String::from_utf8_lossy(&result.stdout).contains("host session"));
}

#[tokio::test]
async fn conpty_session_output_and_resize() {
    let mut plan = plan("[Console]::WriteLine('host-conpty')");
    plan.pty = Some(PtySize::default());
    let running = spawn(plan, Arc::new(Outbox::new(2, 65536))).unwrap();
    running
        .input
        .send(SessionInput::Resize(PtySize {
            cols: 100,
            rows: 40,
        }))
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), running.done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.code, Some(0));
    assert!(String::from_utf8_lossy(&result.stdout).contains("host-conpty"));
}

#[tokio::test]
async fn timeout_terminates_session() {
    let mut plan = plan("Start-Sleep -Seconds 120");
    plan.timeout = Some(Duration::from_millis(200));
    let running = spawn(plan, Arc::new(Outbox::new(3, 65536))).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), running.done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.reason, ExitReason::Timeout);
}

#[tokio::test]
async fn transfer_verifies_checksum_and_replaces_destination() {
    use terrarium_host::transfer::{PushFile, sha256_hex};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("transfer.bin");
    for (session, data) in [(4, b"first".as_slice()), (5, b"replacement".as_slice())] {
        let mut file = PushFile::open(&path, Some(0o600), false, session)
            .await
            .unwrap();
        file.write(data).await.unwrap();
        file.finish(data.len() as u64, &sha256_hex(data))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }
}

#[test]
fn versioned_update_preserves_both_images_across_rollback() {
    use terrarium_host::update_journal::Journal;
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("host.exe");
    let verified = root.path().join("verified.exe");
    std::fs::write(&binary, b"old").unwrap();
    std::fs::write(&verified, b"new").unwrap();
    let transaction = Journal::stage(&verified, &binary, root.path(), "test-new").unwrap();
    let mut selected = binary.clone();
    transaction
        .activate(&root.path().join("pending.json"), |next| {
            selected = next.to_owned();
            Ok(())
        })
        .unwrap();
    assert_eq!(std::fs::read(&selected).unwrap(), b"new");
    assert_eq!(std::fs::read(&binary).unwrap(), b"old");
    transaction
        .rollback(|old| {
            selected = old.to_owned();
            Ok(())
        })
        .unwrap();
    assert_eq!(std::fs::read(&selected).unwrap(), b"old");
    assert_eq!(std::fs::read(&transaction.next).unwrap(), b"new");
}

#[tokio::test]
async fn bare_program_resolves_from_request_path() {
    let mut plan = plan("[Console]::WriteLine('bare-program-ok')");
    plan.program = "powershell.exe".into();
    let running = spawn(plan, Arc::new(Outbox::new(6, 65536))).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), running.done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.code, Some(0));
}

#[tokio::test]
async fn delayed_stdin_reader_receives_more_than_eight_chunks() {
    let running = spawn(plan("Start-Sleep -Seconds 2; $data=[Console]::In.ReadToEnd(); [Console]::WriteLine($data.Length); exit 23"), Arc::new(Outbox::new(20, 65536))).unwrap();
    for _ in 0..64 {
        running
            .input
            .send(SessionInput::Stdin(vec![b'a'; 4096]))
            .unwrap();
    }
    running.input.send(SessionInput::Eof).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(20), running.done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.code, Some(23));
    assert_eq!(result.reason, ExitReason::Exited);
    assert!(String::from_utf8_lossy(&result.stdout).contains("262144"));
}

#[tokio::test]
async fn kill_bypasses_full_stdin_queue() {
    let running = spawn(
        plan("Start-Sleep -Seconds 120"),
        Arc::new(Outbox::new(21, 65536)),
    )
    .unwrap();
    for _ in 0..64 {
        running
            .input
            .send(SessionInput::Stdin(vec![b'a'; 4096]))
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    running
        .input
        .send(SessionInput::Kill(ExitReason::Timeout))
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), running.done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.reason, ExitReason::Timeout);
}
