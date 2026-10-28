//! `POST /api/targets/{name}/test` (PLAN §16b.6a, docs/api.md §17.11):
//! one login to an `ssh` target with its key, and a result that tells the
//! operator what to fix.
//!
//! The test runs `true` on the target with the same `ssh` command line as
//! an exec (`crate::executor::ssh_plan`): `BatchMode=yes`, the key of the
//! target in a 0600 file of one run, and the daemon's own known hosts file
//! `<state>/ssh/known_hosts`. The host key policy is the policy of each
//! exec: trust on first use (`StrictHostKeyChecking=accept-new`). A new
//! host key goes into the file, and the report says so (`host_key:
//! "added"`), so the daemon never takes a new key silently. A changed key
//! is refused (`host_key_mismatch`); the operator removes the old line
//! with `ssh-keygen -R <host> -f <state>/ssh/known_hosts`.
//!
//! | `result` | ssh said |
//! |---|---|
//! | `ok` | exit 0 |
//! | `auth_refused` | `Permission denied (...)` |
//! | `host_key_mismatch` | `REMOTE HOST IDENTIFICATION HAS CHANGED` |
//! | `host_key_unknown` | `Host key verification failed.` with no change |
//! | `unreachable` | `Connection refused`, `timed out`, `No route to host`, `Could not resolve hostname`, and the like |
//! | `error` | anything else; see `stderr` |
//!
//! The private key never goes into the report: ssh does not print it,
//! and the report holds only the tail of ssh's stderr.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use terrarium_core::config::{TargetDef, TargetKind};

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{RemoteCmd, ssh_plan};
use crate::secrets::{RunDirs, Secret};
use crate::store::targets;

/// The connect timeout of a test, in seconds. `ssh` takes the first
/// `ConnectTimeout`, so this one wins over the exec default.
pub const CONNECT_TIMEOUT_SECS: u64 = 10;
/// The longest a test may run, in all.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(20);
/// The most bytes of stderr that a report holds.
const STDERR_TAIL: usize = 2000;

/// The host key policy of `ssh` targets, as the report names it.
pub const HOST_KEY_POLICY: &str = "trust on first use (StrictHostKeyChecking=accept-new) \
    in the daemon's own <state>/ssh/known_hosts; a changed key is refused";

/// The result of a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    AuthRefused,
    HostKeyUnknown,
    HostKeyMismatch,
    Unreachable,
    Error,
}

/// What ssh did with the host key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKey {
    /// The key was in the known hosts file and matched.
    Known,
    /// The key was new; ssh added it to the known hosts file.
    Added,
    /// The key is not known, and ssh did not add it.
    Unknown,
    /// The key differs from the known one.
    Mismatch,
    /// ssh did not get as far as the host key.
    NotChecked,
}

/// The answer of a test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestReport {
    pub target: String,
    pub kind: &'static str,
    pub address: Option<String>,
    pub result: Outcome,
    /// One line for a person: what happened and what to do.
    pub message: String,
    pub host_key: HostKey,
    pub host_key_policy: &'static str,
    /// The secret that `credential` names, and the fingerprint of its key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The exit code of ssh; `null` when it did not end in time.
    pub exit_code: Option<i32>,
    /// The tail of ssh's stderr.
    pub stderr: String,
    pub elapsed_ms: u64,
}

fn has(stderr: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| stderr.contains(n))
}

/// Classify the end of `ssh ... true`: its exit code (`None`: it timed
/// out) and its stderr, from the error strings of OpenSSH.
pub fn classify(exit: Option<i32>, stderr: &str) -> (Outcome, HostKey, String) {
    let mismatch = stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || (stderr.contains("Host key for ") && stderr.contains(" has changed"));
    let verify_failed = stderr.contains("Host key verification failed");
    let added = stderr.contains("Permanently added");
    let host_key = if mismatch {
        HostKey::Mismatch
    } else if verify_failed {
        HostKey::Unknown
    } else if added {
        HostKey::Added
    } else {
        HostKey::Known
    };
    let Some(code) = exit else {
        return (
            Outcome::Unreachable,
            HostKey::NotChecked,
            format!(
                "no answer in {} s: the host is down, or a firewall drops the connection",
                TEST_TIMEOUT.as_secs()
            ),
        );
    };
    if code == 0 {
        let m = if added {
            "the login works; the host key was new, and the daemon recorded it (trust on \
             first use). Check that it is the right host."
        } else {
            "the login works"
        };
        return (Outcome::Ok, host_key, m.to_owned());
    }
    if mismatch {
        return (
            Outcome::HostKeyMismatch,
            HostKey::Mismatch,
            "the host key changed since the first connection; ssh refused it. If the change \
             is expected, remove the old key with `ssh-keygen -R <host> -f \
             <state>/ssh/known_hosts`"
                .to_owned(),
        );
    }
    if verify_failed {
        return (
            Outcome::HostKeyUnknown,
            HostKey::Unknown,
            "the host key is not known, and ssh did not record it; check that \
             <state>/ssh/known_hosts is writable"
                .to_owned(),
        );
    }
    if has(
        stderr,
        &["Permission denied", "Too many authentication failures"],
    ) {
        return (
            Outcome::AuthRefused,
            host_key,
            "the host refused the key: add the public key of the secret to \
             ~/.ssh/authorized_keys of the user on the target"
                .to_owned(),
        );
    }
    if has(
        stderr,
        &[
            "Connection refused",
            "Connection timed out",
            "timed out",
            "No route to host",
            "Network is unreachable",
            "Could not resolve hostname",
            "Name or service not known",
            "Temporary failure in name resolution",
            "Host is unreachable",
        ],
    ) {
        return (
            Outcome::Unreachable,
            HostKey::NotChecked,
            "ssh cannot reach the host: check the address, the port, and the network".to_owned(),
        );
    }
    let m = if code == 255 {
        "ssh failed; see stderr".to_owned()
    } else {
        format!("the login worked, but `true` exited {code} on the target")
    };
    (Outcome::Error, host_key, m)
}

/// The last [`STDERR_TAIL`] bytes of `s`, on a character boundary.
fn tail(s: &str) -> String {
    let s = s.trim_end();
    if s.len() <= STDERR_TAIL {
        return s.to_owned();
    }
    let mut start = s.len() - STDERR_TAIL;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_owned()
}

/// Run `ssh ... true` once. The answer is the exit code (`None` on a
/// timeout) and the stderr.
pub async fn run_ssh(
    ssh_bin: &str,
    def: &TargetDef,
    key: Option<&Path>,
    known_hosts: &Path,
    timeout: Duration,
) -> ApiResult<(Option<i32>, String)> {
    let cmd = RemoteCmd {
        argv: vec!["true".to_owned()],
        ..RemoteCmd::default()
    };
    let plan = ssh_plan(ssh_bin, def, key, known_hosts, &cmd)?;
    let mut c = tokio::process::Command::new(&plan.program);
    c.arg("-o")
        .arg(format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"))
        // The "Permanently added" line of a new host key needs INFO.
        .args(["-o", "LogLevel=INFO"])
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = c
        .spawn()
        .map_err(|e| ApiError::internal(format!("cannot run `{}`: {e}", plan.program)))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok((
            Some(out.status.code().unwrap_or(-1)),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )),
        Ok(Err(e)) => Err(ApiError::internal(format!("ssh failed: {e}"))),
        Err(_) => Ok((None, String::new())),
    }
}

/// Test a target. Only `ssh` targets: other kinds are `422 invalid`, as
/// is an `ssh` target whose `credential` does not work.
pub async fn test(state: &Shared, def: &TargetDef) -> ApiResult<TestReport> {
    if def.kind != TargetKind::Ssh {
        return Err(ApiError::invalid(format!(
            "`target test` is not supported for a `{}` target, only for ssh; for a target \
             with a host agent, `terrarium admin target verb {} status` asks the host",
            targets::kind_str(def.kind),
            def.name
        )));
    }
    let d = def.clone();
    let cred = state
        .db
        .read(move |c| targets::credential_view(c, &d))
        .await?;
    if let Some(w) = cred.as_ref().and_then(|c| c.warning.clone()) {
        return Err(ApiError::invalid(w));
    }
    // The key goes into a 0600 file of one run; the dir goes on drop.
    let mut run = None;
    let mut key = None;
    if let Some(name) = &def.credential {
        match crate::secrets::load(state, def.project.as_deref(), name).await? {
            Secret::SshKey(k) => {
                let r = RunDirs::new(&state.state_dir).run_dir()?;
                key = Some(r.write_private("id", k.as_bytes())?);
                run = Some(r);
            }
            other => {
                return Err(ApiError::invalid(format!(
                    "secret `{name}` is a {}; an ssh target needs an ssh_key",
                    other.kind_label()
                )));
            }
        }
    }
    let known = crate::executor::known_hosts(&state.state_dir)?;
    let started = Instant::now();
    let (exit, stderr) = run_ssh(
        &state.executor.ssh_bin,
        def,
        key.as_deref(),
        &known,
        TEST_TIMEOUT,
    )
    .await?;
    drop(run);
    let (result, host_key, message) = classify(exit, &stderr);
    Ok(TestReport {
        target: def.name.clone(),
        kind: targets::kind_str(def.kind),
        address: def.address.clone(),
        result,
        message,
        host_key,
        host_key_policy: HOST_KEY_POLICY,
        credential: cred.as_ref().map(|c| c.name.clone()),
        fingerprint: cred.and_then(|c| c.fingerprint),
        exit_code: exit,
        stderr: tail(&stderr),
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The real OpenSSH messages, with the exit code that ssh gives.
    const CASES: [(&str, i32, &str, Outcome, HostKey); 9] = [
        ("ok", 0, "", Outcome::Ok, HostKey::Known),
        (
            "ok_new_host",
            0,
            "Warning: Permanently added '[box]:2222' (ED25519) to the list of known hosts.\n",
            Outcome::Ok,
            HostKey::Added,
        ),
        (
            "refused",
            255,
            "build@box: Permission denied (publickey).\n",
            Outcome::AuthRefused,
            HostKey::Known,
        ),
        (
            "refused_new_host",
            255,
            "Warning: Permanently added 'box' (ED25519) to the list of known hosts.\nbuild@box: Permission denied (publickey,password).\n",
            Outcome::AuthRefused,
            HostKey::Added,
        ),
        (
            "mismatch",
            255,
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
             @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\n\
             Host key for box has changed and you have requested strict checking.\n\
             Host key verification failed.\n",
            Outcome::HostKeyMismatch,
            HostKey::Mismatch,
        ),
        (
            "unknown",
            255,
            "No ED25519 host key is known for box and you have requested strict checking.\nHost key verification failed.\n",
            Outcome::HostKeyUnknown,
            HostKey::Unknown,
        ),
        (
            "refused_port",
            255,
            "ssh: connect to host box port 2222: Connection refused\n",
            Outcome::Unreachable,
            HostKey::NotChecked,
        ),
        (
            "no_dns",
            255,
            "ssh: Could not resolve hostname box: Name or service not known\n",
            Outcome::Unreachable,
            HostKey::NotChecked,
        ),
        (
            "bad_key",
            255,
            "Load key \"/state/credentials/.run/x/id\": invalid format\nbuild@box: Permission denied (publickey).\n",
            Outcome::AuthRefused,
            HostKey::Known,
        ),
    ];

    #[test]
    fn openssh_errors_map_to_results() {
        for (name, code, stderr, want, key) in CASES {
            let (got, k, msg) = classify(Some(code), stderr);
            assert_eq!(got, want, "{name}: {msg}");
            assert_eq!(k, key, "{name}");
        }
        let (o, _, _) = classify(None, "");
        assert_eq!(o, Outcome::Unreachable);
        let (o, _, m) = classify(Some(1), "");
        assert_eq!(o, Outcome::Error);
        assert!(m.contains("exited 1"), "{m}");
        let (o, _, _) = classify(
            Some(255),
            "kex_exchange_identification: Connection closed\n",
        );
        assert_eq!(o, Outcome::Error);
    }

    #[test]
    fn the_tail_keeps_the_end() {
        let long = "é".repeat(STDERR_TAIL);
        let t = tail(&long);
        assert!(t.len() <= STDERR_TAIL);
        assert!(t.ends_with('é'));
    }

    fn def() -> TargetDef {
        let mut d: TargetDef =
            toml::from_str("kind = \"ssh\"\naddress = \"build@box\"\nport = 2222\n").unwrap();
        d.name = "box".into();
        d
    }

    /// A fake `ssh` on PATH: it records its arguments, prints the stderr
    /// of a case, and exits with its code. No network.
    fn fake_ssh(dir: &Path, stderr: &str, code: i32, sleep: bool) -> String {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let err = dir.join("stderr.txt");
        std::fs::write(&err, stderr).unwrap();
        let args = dir.join("args.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{}cat '{}' >&2\nexit {code}\n",
            args.display(),
            if sleep { "sleep 5\n" } else { "" },
            err.display()
        );
        let p = bin.join("ssh");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    #[tokio::test]
    async fn a_fake_ssh_gives_each_result() {
        for (name, code, stderr, want, _) in CASES {
            let dir = tempfile::tempdir().unwrap();
            let ssh = fake_ssh(dir.path(), stderr, code, false);
            let key = dir.path().join("id");
            let (exit, err) = run_ssh(
                &ssh,
                &def(),
                Some(&key),
                &dir.path().join("kh"),
                TEST_TIMEOUT,
            )
            .await
            .unwrap();
            assert_eq!(exit, Some(code), "{name}");
            assert_eq!(classify(exit, &err).0, want, "{name}");
            let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
            let args: Vec<&str> = args.lines().collect();
            // The test's short timeout comes first, so ssh takes it.
            assert_eq!(args[..2], ["-o", "ConnectTimeout=10"], "{name}");
            for want in [
                "BatchMode=yes",
                "StrictHostKeyChecking=accept-new",
                "IdentitiesOnly=yes",
                "build@box",
                "exec 'true'",
            ] {
                assert!(args.contains(&want), "{name}: {want} in {args:?}");
            }
            assert!(args.iter().any(|a| a.ends_with("/kh")), "{args:?}");
        }
    }

    #[tokio::test]
    async fn a_hanging_ssh_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(dir.path(), "", 0, true);
        let (exit, err) = run_ssh(
            &ssh,
            &def(),
            None,
            &dir.path().join("kh"),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert_eq!(exit, None);
        assert_eq!(classify(exit, &err).0, Outcome::Unreachable);
    }
}
