//! `terrariumd password set` with no daemon: it writes
//! `<state>/operator.password` (mode 0600), with only the Argon2 hash.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

fn set(state: &std::path::Path, stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrariumd"))
        .args(["--state-dir"])
        .arg(state)
        .args(["password", "set", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn password_set_offline() {
    let dir = tempfile::tempdir().unwrap();
    let out = set(dir.path(), "an offline password\n");
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("version 1"));
    let path = dir.path().join(terrariumd::password::PASSWORD_FILE);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("$argon2id$") && !text.contains("an offline password"));
    let store = terrariumd::password::PasswordStore::new(dir.path());
    assert!(store.current().unwrap().verify("an offline password"));
    // Too short: refused, the old password stays.
    let out = set(dir.path(), "short\n");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("12 characters"));
    // A second set bumps the version.
    assert!(set(dir.path(), "a second password\n").status.success());
    let cur = store.current().unwrap();
    assert_eq!(cur.version, 2);
    assert!(cur.verify("a second password"));
}

/// `TERRARIUM_PASSWORD` sets the first password, then the daemon starts
/// again without it: its own env (and so every child's) no longer holds
/// the value, and no argument names it.
#[test]
fn the_first_password_leaves_the_env() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for sub in ["state", "config", "run", "work", "repos"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    let socket = d.join("run/api.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrariumd"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", d)
        .env("TERRARIUM_PASSWORD", "a password from the env")
        .env("TERRARIUM_HOST_LISTEN", "")
        .arg("--socket")
        .arg(&socket)
        .args(["--state-dir"])
        .arg(d.join("state"))
        .args(["--config-dir"])
        .arg(d.join("config"))
        .args(["--runtime-dir"])
        .arg(d.join("run"))
        .args(["--work-dir"])
        .arg(d.join("work"))
        .args(["--repos-dir"])
        .arg(d.join("repos"))
        .args(["--prompts-dir"])
        .arg(d)
        .args([
            "--artifact-listen",
            "",
            "--sandbox",
            "none",
            "--runner-mode",
            "inprocess",
            "--tools-dir",
            "",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The fresh daemon creates its schema and hashes the password before it
    // binds the socket. Under merge-job load (load average 40) that took
    // over 30 s (TER-648). Only a failing run waits out the cap, and a
    // daemon that exits fails at once.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut exited = None;
    while !socket.exists() && std::time::Instant::now() < deadline {
        exited = child.try_wait().unwrap();
        if exited.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let pid = child.id();
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        socket.exists(),
        "the daemon did not start (exit: {exited:?})"
    );
    let environ = String::from_utf8_lossy(&environ);
    assert!(environ.contains("TERRARIUM_HOST_LISTEN="), "read the env");
    assert!(!environ.contains("TERRARIUM_PASSWORD"), "{environ}");
    assert!(!String::from_utf8_lossy(&cmdline).contains("a password from the env"));
    let store = terrariumd::password::PasswordStore::new(&d.join("state"));
    assert!(store.current().unwrap().verify("a password from the env"));
}
