//! `terrarium exec-env [--require-credentials] -- <program> [args...]`:
//! start a program with the env vars of the agent's secret grants and the
//! credentials of its harness (PLAN §16b.4, docs/api.md §5.2).
//!
//! The sandbox starts the harness through this command. It asks the daemon
//! for the vars (`POST /api/agent/secrets/env`, through the runner socket),
//! sets them, and replaces itself with the program (`execvp`). So no value
//! is in the launch spec, in the runner spec file, on a command line, or
//! in the env of `bwrap` outside the sandbox.
//!
//! Precedence, as the launch had it when the values were in the spec:
//!
//! - A credential var (`credentials`: the subscription's API key, the
//!   Copilot account token) wins over a var of the same name in the env.
//! - A var of a secret grant (`vars`) that the env holds already loses:
//!   the harness and its account set it.
//!
//! - A credential file (`credential_files`: the service-account JSON of a
//!   GCP subscription) is written with mode 0600 to a directory of the
//!   agent ([`credential_dir`]), and its var
//!   (`GOOGLE_APPLICATION_CREDENTIALS`) gets the path, over the env.
//!
//! When the daemon does not answer: with `--require-credentials` (the
//! harness has credentials to get), the command tries again a few times,
//! then exits with [`EXEC_ENV_NO_CREDENTIALS`] and does not start the
//! harness, because a harness with no credentials cannot work. Without
//! it, the command prints a warning and starts the program with no secret
//! var.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use terrarium_core::launch::EXEC_ENV_NO_CREDENTIALS;
use zeroize::Zeroizing;

use crate::ctx::Ctx;

/// The exit code when the program does not start (as a shell does).
const NOT_STARTED: i32 = 127;

/// The waits between the tries with `--require-credentials`: 5 tries in
/// about 3 s. A daemon restart or a busy store gets time to answer.
const RETRY_WAITS: [Duration; 4] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
];

/// The env vars to set, by kind.
#[derive(Default)]
struct EnvVars {
    /// The vars of the secret grants: set when the env does not hold them.
    grants: BTreeMap<String, Zeroizing<String>>,
    /// The harness credentials: always set.
    credentials: BTreeMap<String, Zeroizing<String>>,
    /// The credential files: var to content. Written 0600; the var gets
    /// the path and is always set.
    files: BTreeMap<String, Zeroizing<String>>,
}

/// Ask for the vars, then exec `argv`. The answer is an exit code only
/// when the exec does not happen.
pub async fn exec(ctx: &Ctx, require_credentials: bool, argv: &[String]) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        eprintln!("terrarium exec-env: no program");
        return 2;
    };
    let tries = if require_credentials {
        RETRY_WAITS.len() + 1
    } else {
        1
    };
    let mut answer = None;
    for i in 0..tries {
        match ctx.api.post("/api/agent/secrets/env", None).await {
            Ok(v) => {
                answer = Some(parse(&v));
                break;
            }
            Err(e) if require_credentials => {
                eprintln!(
                    "terrarium exec-env: try {} of {tries}: the daemon did not give the harness \
                     credentials: {e:#}",
                    i + 1
                );
                if let Some(wait) = RETRY_WAITS.get(i) {
                    tokio::time::sleep(*wait).await;
                }
            }
            Err(e) => {
                eprintln!(
                    "terrarium exec-env: the env vars of the secret grants are not set: {e:#}"
                );
            }
        }
    }
    let vars = match answer {
        Some(v) => v,
        None if require_credentials => {
            eprintln!(
                "terrarium exec-env: cannot get the harness credentials from the daemon; \
                 {program} does not start (exit {EXEC_ENV_NO_CREDENTIALS})"
            );
            return EXEC_ENV_NO_CREDENTIALS;
        }
        None => EnvVars::default(),
    };
    let paths = match write_files(&credential_dir(), &vars.files) {
        Ok(p) => p,
        Err(e) if require_credentials => {
            eprintln!(
                "terrarium exec-env: cannot write the harness credential files: {e}; \
                 {program} does not start (exit {EXEC_ENV_NO_CREDENTIALS})"
            );
            return EXEC_ENV_NO_CREDENTIALS;
        }
        Err(e) => {
            eprintln!("terrarium exec-env: cannot write the credential files: {e}");
            Vec::new()
        }
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    for (k, v) in to_set(&vars, |k| std::env::var_os(k).is_some()) {
        cmd.env(k, v);
    }
    for (k, path) in &paths {
        cmd.env(k, path);
    }
    let err = cmd.exec();
    eprintln!("terrarium exec-env: cannot start {program}: {err}");
    NOT_STARTED
}

/// The `vars` and `credentials` objects of the answer. A name that is not
/// a usable env var name (`[A-Z][A-Z0-9_]*`) is left out.
fn parse(v: &Value) -> EnvVars {
    let read = |key: &str| -> BTreeMap<String, Zeroizing<String>> {
        v[key]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| usable(k))
                    .filter_map(|(k, v)| {
                        v.as_str()
                            .map(|s| (k.clone(), Zeroizing::new(s.to_owned())))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    EnvVars {
        grants: read("vars"),
        credentials: read("credentials"),
        files: read("credential_files"),
    }
}

/// The directory of the credential files of the agent
/// (`TERRARIUM_AGENT`): `$XDG_RUNTIME_DIR/terrarium-credentials/<agent>`
/// when the runtime dir is set (a tmpfs of the user), else
/// `$HOME/.terrarium/credentials/<agent>` (the sandbox home is a tmpfs, so
/// the files go with the sandbox). One directory per agent, so two
/// agents with sandbox mode `none` never share a file.
fn credential_dir() -> PathBuf {
    let agent: String = std::env::var("TERRARIUM_AGENT")
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let agent = if agent.is_empty() { "agent".to_owned() } else { agent };
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("terrarium-credentials").join(agent),
        None => std::env::var_os("HOME")
            .map_or_else(std::env::temp_dir, PathBuf::from)
            .join(".terrarium")
            .join("credentials")
            .join(agent),
    }
}

/// Write each credential file to `dir` (mode 0700) with mode 0600,
/// through a temp file and a rename. Returns the var and the path of each.
/// An error never has the content.
fn write_files(
    dir: &Path,
    files: &BTreeMap<String, Zeroizing<String>>,
) -> std::io::Result<Vec<(String, PathBuf)>> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if files.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let mut out = Vec::new();
    for (var, content) in files {
        let path = dir.join(format!("{}.json", var.to_ascii_lowercase()));
        let tmp = dir.join(format!(".{}.{}.tmp", var.to_ascii_lowercase(), std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        out.push((var.clone(), path));
    }
    Ok(out)
}

fn usable(name: &str) -> bool {
    let mut b = name.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_uppercase())
        && b.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// The vars to set: each credential var, and each grant var that
/// `present` does not hold already and that is not a credential var.
fn to_set(vars: &EnvVars, present: impl Fn(&str) -> bool) -> Vec<(&str, &str)> {
    let grants = vars
        .grants
        .iter()
        .filter(|(k, _)| !present(k) && !vars.credentials.contains_key(*k));
    grants
        .chain(vars.credentials.iter())
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_env_wins_over_a_grant_and_bad_names_are_left_out() {
        let v = json!({"vars": {"DB_URL": "pg://x", "GH_TOKEN": "ghp_x", "bad name": "y",
                                "LOW": 1, "A=B": "z"}});
        let vars = parse(&v);
        assert_eq!(
            vars.grants.keys().collect::<Vec<_>>(),
            ["DB_URL", "GH_TOKEN"]
        );
        assert!(vars.credentials.is_empty());
        let set = to_set(&vars, |k| k == "GH_TOKEN");
        assert_eq!(set, [("DB_URL", "pg://x")]);
        assert!(parse(&json!({})).grants.is_empty());
    }

    /// A credential file is written 0600 and its var points to it; the
    /// content is in no var.
    #[test]
    fn credential_files_are_0600_and_the_var_has_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let v = json!({
            "credentials": {"CLAUDE_CODE_USE_VERTEX": "1"},
            "credential_files": {"GOOGLE_APPLICATION_CREDENTIALS": "{\"type\":\"service_account\"}",
                                 "bad name": "x"}
        });
        let vars = parse(&v);
        assert_eq!(
            vars.files.keys().collect::<Vec<_>>(),
            ["GOOGLE_APPLICATION_CREDENTIALS"]
        );
        let set = to_set(&vars, |_| false);
        assert!(set.iter().all(|(_, v)| !v.contains("service_account")));
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("creds");
        let paths = write_files(&dir, &vars.files).unwrap();
        assert_eq!(paths.len(), 1);
        let (var, path) = &paths[0];
        assert_eq!(var, "GOOGLE_APPLICATION_CREDENTIALS");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "{\"type\":\"service_account\"}"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // A second start writes the file again.
        assert_eq!(write_files(&dir, &vars.files).unwrap()[0].1, *path);
        assert!(write_files(&dir, &BTreeMap::new()).unwrap().is_empty());
    }

    /// A credential var wins over the env and over a grant of the same
    /// name, as when the launch spec held it.
    #[test]
    fn a_credential_wins_over_the_env_and_a_grant() {
        let v = json!({
            "vars": {"ANTHROPIC_API_KEY": "from-a-grant", "DB_URL": "pg://x"},
            "credentials": {"ANTHROPIC_API_KEY": "sk-sub", "COPILOT_GITHUB_TOKEN": "gho_x",
                            "bad": "y"}
        });
        let vars = parse(&v);
        let mut set = to_set(&vars, |k| {
            k == "ANTHROPIC_API_KEY" || k == "COPILOT_GITHUB_TOKEN"
        });
        set.sort_unstable();
        assert_eq!(
            set,
            [
                ("ANTHROPIC_API_KEY", "sk-sub"),
                ("COPILOT_GITHUB_TOKEN", "gho_x"),
                ("DB_URL", "pg://x")
            ]
        );
    }
}
