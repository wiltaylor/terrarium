//! `terrarium exec-env -- <program> [args...]`: start a program with the
//! env vars of the agent's secret grants (PLAN §16b.4, docs/api.md §5.2).
//!
//! The sandbox starts the harness through this command. It asks the daemon
//! for the vars (`POST /api/agent/secrets/env`, through the runner socket),
//! adds each var that the env does not hold yet, and replaces itself with
//! the program (`execvp`). So no value is in the launch spec, in the
//! runner spec file, on a command line, or in the env of `bwrap` outside
//! the sandbox.
//!
//! A var that the env holds already wins: the harness and its account set
//! it. When the daemon does not answer, the command prints a warning and
//! starts the program with no secret var.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;

use serde_json::Value;

use crate::ctx::Ctx;

/// The exit code when the program does not start (as a shell does).
const NOT_STARTED: i32 = 127;

/// Ask for the vars, then exec `argv`. The answer is an exit code only
/// when the exec fails.
pub async fn exec(ctx: &Ctx, argv: &[String]) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        eprintln!("terrarium exec-env: no program");
        return 2;
    };
    let vars = match ctx.api.post("/api/agent/secrets/env", None).await {
        Ok(v) => parse(&v),
        Err(e) => {
            eprintln!("terrarium exec-env: the env vars of the secret grants are not set: {e:#}");
            BTreeMap::new()
        }
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    for (k, v) in to_set(vars, |k| std::env::var_os(k).is_some()) {
        cmd.env(k, v);
    }
    let err = cmd.exec();
    eprintln!("terrarium exec-env: cannot start {program}: {err}");
    NOT_STARTED
}

/// The `vars` object of the answer. A name that is not a usable env var
/// name (`[A-Z][A-Z0-9_]*`) is left out.
fn parse(v: &Value) -> BTreeMap<String, String> {
    v["vars"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| usable(k))
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

fn usable(name: &str) -> bool {
    let mut b = name.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_uppercase())
        && b.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// The vars to add: each one that `present` does not hold already.
fn to_set(vars: BTreeMap<String, String>, present: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    vars.into_iter().filter(|(k, _)| !present(k)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_env_wins_and_bad_names_are_left_out() {
        let v = json!({"vars": {"DB_URL": "pg://x", "GH_TOKEN": "ghp_x", "bad name": "y",
                                "LOW": 1, "A=B": "z"}});
        let vars = parse(&v);
        assert_eq!(vars.keys().collect::<Vec<_>>(), ["DB_URL", "GH_TOKEN"]);
        let set = to_set(vars, |k| k == "GH_TOKEN");
        assert_eq!(set, [("DB_URL".to_owned(), "pg://x".to_owned())]);
        assert!(parse(&json!({})).is_empty());
    }
}
