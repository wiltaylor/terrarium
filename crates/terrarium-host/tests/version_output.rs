//! The `--version` output of the real `terrarium-host` binary passes the
//! self-update check of the host agents in the field (TER-195).

use std::process::Command;

/// The check of host agents 0.5.0 and older: the last word of the first
/// line of `<new binary> --version` is the version of the staged update.
fn old_check(stdout: &str) -> &str {
    stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().last())
        .unwrap_or_default()
}

#[test]
fn an_old_host_agent_reads_the_version_of_this_binary() {
    let out = Command::new(env!("CARGO_BIN_EXE_terrarium-host"))
        .arg("--version")
        .output()
        .expect("run terrarium-host --version");
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(old_check(&stdout), terrarium_version::FULL, "{stdout:?}");
    assert_eq!(
        stdout.lines().nth(1),
        Some(format!("built {}", terrarium_version::BUILD_TIME).as_str()),
        "{stdout:?}"
    );
}
