#![cfg(windows)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use terrarium_winapi::{Limits, Size, spawn};

fn shell(script: &str, pty: bool, limits: Limits) -> terrarium_winapi::Child {
    spawn(Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        &["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), script.into()],
        None, &BTreeMap::new(), pty.then_some(Size { cols: 80, rows: 24 }), limits).unwrap()
}

fn wait(child: &terrarium_winapi::Process) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(code) = child.try_wait().unwrap() { return code; }
        assert!(Instant::now() < deadline, "child timeout");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn pipes_stdin_and_exit_code() {
    let mut child = shell("$x=[Console]::ReadLine(); [Console]::Out.WriteLine($x); [Console]::Error.WriteLine('error'); exit 7", false, Limits::default());
    child.stdin.write_all(b"a b\r\n").unwrap();
    drop(child.stdin);
    assert_eq!(wait(&child.process), 7);
    let mut out = String::new(); child.stdout.read_to_string(&mut out).unwrap();
    let mut err = String::new(); child.stderr.unwrap().read_to_string(&mut err).unwrap();
    assert!(out.contains("a b"), "{out:?}"); assert!(err.contains("error"));
}

#[test]
fn conpty_without_parent_console() {
    let child = shell("[Console]::Out.WriteLine('conpty-ok')", true, Limits::default());
    child.console.as_ref().unwrap().resize(Size { cols: 100, rows: 40 }).unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut reader = child.stdout;
        let mut buf = [0; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 { break; }
            out.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&out).contains("conpty-ok") { break; }
        }
        let _ = send.send(out);
    });
    let out = recv.recv_timeout(Duration::from_secs(15)).expect("ConPTY output timeout");
    assert!(String::from_utf8_lossy(&out).contains("conpty-ok"));
    assert_eq!(wait(&child.process), 0);
    child.job.terminate().unwrap();
    drop(child.console);
}

#[test]
fn atomic_job_assignment_kills_immediate_detached_grandchild() {
    for _ in 0..20 {
        // This is the first statement in the child, not a delayed spawn.
        let child = shell("$c=Start-Process powershell.exe -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 120' -PassThru; [Console]::WriteLine($c.Id); Start-Sleep -Seconds 120", false, Limits::default());
        let mut line = String::new();
        BufReader::new(child.stdout).read_line(&mut line).unwrap();
        let pid: u32 = line.trim().parse().unwrap();
        drop(child.job); // No PID-based kill. The sole job handle kills the tree.
        // Windows uses exit code zero for kill-on-job-close. Wait for the
        // process handle to signal, rather than interpreting that as success.
        wait(&child.process);
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("$end=[DateTime]::UtcNow.AddSeconds(5); do {{ $p=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if (!$p -or $p.HasExited) {{ exit 0 }}; Start-Sleep -Milliseconds 20 }} while ([DateTime]::UtcNow -lt $end); exit 1")])
            .status().unwrap();
        assert!(status.success(), "grandchild {pid} survived job close");
    }
}

#[test]
#[ignore = "launched only inside a test-owned job"]
fn immediate_child_helper() {
    use std::os::windows::process::CommandExt;
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "grandchild_helper"])
        .creation_flags(0x00000008 | 0x00000200) // detached, new process group, not breakaway
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).spawn().unwrap();
    println!("OWNED={}", child.id());
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(120));
}

#[test]
#[ignore = "launched only as the test-owned grandchild"]
fn grandchild_helper() { std::thread::sleep(Duration::from_secs(120)); }

#[test]
fn atomic_job_assignment_direct_creation_race() {
    for _ in 0..20 {
        let child = spawn(&std::env::current_exe().unwrap(),
            &["--ignored".into(), "--exact".into(), "immediate_child_helper".into(), "--nocapture".into()],
            None, &BTreeMap::new(), None, Limits::default()).unwrap();
        let mut reader = BufReader::new(child.stdout);
        let mut line = String::new();
        let pid = loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0, "helper exited without grandchild");
            if let Some(pid) = line.trim().strip_prefix("OWNED=") { break pid.parse::<u32>().unwrap(); }
        };
        drop(child.job);
        wait(&child.process);
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("$end=[DateTime]::UtcNow.AddSeconds(5); do {{ $p=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if (!$p -or $p.HasExited) {{ exit 0 }}; Start-Sleep -Milliseconds 20 }} while ([DateTime]::UtcNow -lt $end); exit 1")])
            .status().unwrap();
        assert!(status.success(), "direct grandchild {pid} survived job close");
    }
}

#[test]
fn job_limits_and_validation() {
    assert!(terrarium_winapi::Job::new(Limits { cpu_rate: Some(0), ..Limits::default() }).is_err());
    let child = shell("$c=Start-Process powershell.exe -PassThru -ErrorAction Stop; exit 1", false,
        Limits { processes: Some(1), memory_bytes: Some(512 * 1024 * 1024), cpu_rate: Some(5000) });
    assert_ne!(wait(&child.process), 0);
}

#[test]
fn nested_holder_budget_is_shared_and_session_kill_is_independent() {
    let holder = terrarium_winapi::Job::new(Limits { processes: Some(2),
        memory_bytes: Some(512 * 1024 * 1024), cpu_rate: Some(5000) }).unwrap();
    let program = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    let args = ["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), "Start-Sleep -Seconds 120".into()];
    let run = || terrarium_winapi::spawn_in_holder(program, &args, None, &BTreeMap::new(), None, Limits::default(), Some(&holder));
    let a = run().unwrap();
    let b = run().unwrap();
    assert!(run().is_err(), "third session exceeded shared process budget");
    a.job.terminate().unwrap(); wait(&a.process);
    assert!(b.process.try_wait().unwrap().is_none(), "sibling died on session kill");
    holder.set_limits(Limits { processes: Some(2), memory_bytes: Some(512 * 1024 * 1024), cpu_rate: Some(2500) }).unwrap();
    drop(holder);
    wait(&b.process);
}
