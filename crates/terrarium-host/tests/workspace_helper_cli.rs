//! Metadata must work without protected configuration or service startup.
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use std::os::unix::process::CommandExt;
use std::process::{Command, Output};

fn helper(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_terrarium-workspace-helper"));
    command.args(args).env_clear().current_dir("/");
    // Never let a root test runner exercise the privileged service path.
    if nix::unistd::geteuid().is_root() {
        command.gid(65534).uid(65534);
    }
    command.output().expect("run unprivileged workspace helper")
}

#[test]
fn version_matches_release_binary_version_ref_convention() {
    let out = helper(&["--version"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("terrarium-workspace-helper {}\n", terrarium_version::FULL)
    );
    assert!(out.stderr.is_empty(), "{out:?}");
}

#[test]
fn capabilities_do_not_enable_admission() {
    let out = helper(&["--capabilities"]);
    assert!(out.status.success(), "{out:?}");
    let capability: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(capability["ready"], false);
    assert_eq!(capability["reason"], "daemon_activation_disabled");
    assert!(out.stderr.is_empty(), "{out:?}");
}

#[test]
fn unknown_or_combined_arguments_fail_closed() {
    for args in [
        vec!["--unknown"],
        vec!["--version", "--capabilities"],
        vec!["--capabilities", "--version"],
        vec!["--version", "extra"],
    ] {
        let out = helper(&args);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("only --version or --capabilities"));
    }
}

#[test]
fn no_arguments_without_root_fails_before_provisioning() {
    let out = helper(&[]);
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("workspace helper requires root"));
}
