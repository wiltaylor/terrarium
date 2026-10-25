//! `harness.update`: bump the harness pins on the Docker host of the
//! Terrarium container (docs/host-protocol.md, `harness.update`).
//!
//! The verb runs `scripts/harness.py` of the checkout in
//! `terrarium_repo_dir`, through `uv`, as one process. Its output streams
//! like `exec`. The host agent runs it only when the config sets
//! `terrarium_repo_dir`; else the answer is `unsupported`. The script edits
//! `harnesses.toml`, builds the image, and runs the dry compatibility suite.
//! It never rolls the stack out (`--no-rollout`; `update` has no rollout):
//! the container cannot recreate itself, so the operator runs `just up`.

use std::path::Path;

use crate::protocol::{ErrorCode, HarnessNames, HarnessUpdate};
use crate::session::ExecPlan;
use crate::verb::{BuildError, VerbPlan, ident, secs};

/// The script, relative to `terrarium_repo_dir`.
pub const SCRIPT: &str = "scripts/harness.py";

/// The arguments after the `uv` binary.
pub fn argv(names: &HarnessNames) -> Result<Vec<String>, BuildError> {
    let mut args: Vec<String> = ["run", "python", SCRIPT]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    match names {
        HarnessNames::Word(w) if w == "all" => {
            args.extend(["latest".into(), "--no-rollout".into()]);
        }
        HarnessNames::Word(w) => {
            return Err(BuildError::bad(format!(
                "names is {w:?}: send \"all\" or a list of one harness name"
            )));
        }
        HarnessNames::List(list) => {
            let [name] = list.as_slice() else {
                return Err(BuildError::bad(format!(
                    "names lists {} harnesses: send \"all\" or a list of one harness name",
                    list.len()
                )));
            };
            let name = ident("harness name", name)?;
            if !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(BuildError::bad(format!(
                    "harness name {name:?} is not [a-z0-9-]"
                )));
            }
            args.extend(["update".into(), name.to_owned(), "--allow-major".into()]);
        }
    }
    Ok(args)
}

/// Build the process for a `harness.update` frame. `repo_dir` is
/// `terrarium_repo_dir` of the config; `uv_bin` is `uv_bin`.
pub fn plan(
    req: &HarnessUpdate,
    repo_dir: Option<&Path>,
    uv_bin: &str,
) -> Result<VerbPlan, BuildError> {
    let Some(dir) = repo_dir else {
        return Err(BuildError {
            code: ErrorCode::Unsupported,
            message: "this host agent does not update harnesses: its config sets no \
                      terrarium_repo_dir (the checkout of Terrarium on this host)"
                .into(),
        });
    };
    let args = argv(&req.names)?;
    if !dir.join(SCRIPT).is_file() {
        return Err(BuildError {
            code: ErrorCode::Unsupported,
            message: format!(
                "terrarium_repo_dir {} has no {SCRIPT}: point it at the Terrarium checkout",
                dir.display()
            ),
        });
    }
    Ok(VerbPlan::plain(ExecPlan {
        program: uv_bin.to_owned(),
        args,
        cwd: Some(dir.to_path_buf()),
        env: Default::default(),
        pty: None,
        timeout: secs(req.timeout_secs),
        capture_stdout: false,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn req(names: HarnessNames) -> HarnessUpdate {
        HarnessUpdate {
            session: 5,
            names,
            timeout_secs: Some(3600),
        }
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("scripts")).unwrap();
        std::fs::write(d.path().join(SCRIPT), "").unwrap();
        d
    }

    #[test]
    fn refuses_without_terrarium_repo_dir() {
        let e = plan(&req(HarnessNames::Word("all".into())), None, "uv").unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported);
        assert!(e.message.contains("terrarium_repo_dir"), "{}", e.message);
    }

    #[test]
    fn refuses_a_dir_without_the_script() {
        let d = tempfile::tempdir().unwrap();
        let e = plan(&req(HarnessNames::Word("all".into())), Some(d.path()), "uv").unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported);
        assert!(e.message.contains(SCRIPT), "{}", e.message);
    }

    #[test]
    fn argv_golden() {
        let d = repo();
        let all = plan(
            &req(HarnessNames::Word("all".into())),
            Some(d.path()),
            "/usr/bin/uv",
        )
        .unwrap();
        assert_eq!(all.exec.program, "/usr/bin/uv");
        assert_eq!(
            all.exec.args,
            [
                "run",
                "python",
                "scripts/harness.py",
                "latest",
                "--no-rollout"
            ]
        );
        assert_eq!(all.exec.cwd.as_deref(), Some(d.path()));
        assert_eq!(all.exec.timeout, Some(std::time::Duration::from_secs(3600)));
        assert!(all.exec.pty.is_none() && all.exec.env.is_empty() && all.parser.is_none());

        let one = plan(
            &req(HarnessNames::List(vec!["codex".into()])),
            Some(d.path()),
            "uv",
        )
        .unwrap();
        assert_eq!(
            one.exec.args,
            [
                "run",
                "python",
                "scripts/harness.py",
                "update",
                "codex",
                "--allow-major"
            ]
        );
    }

    #[test]
    fn bad_names_are_bad_requests() {
        for names in [
            HarnessNames::Word("everything".into()),
            HarnessNames::List(vec![]),
            HarnessNames::List(vec!["claude".into(), "codex".into()]),
            HarnessNames::List(vec!["--dry-run".into()]),
            HarnessNames::List(vec!["Codex".into()]),
            HarnessNames::List(vec!["co dex".into()]),
        ] {
            let e = argv(&names).unwrap_err();
            assert_eq!(e.code, ErrorCode::BadRequest, "{names:?}");
        }
    }

    #[test]
    fn the_frame_parses_both_shapes() {
        use crate::protocol::{DaemonFrame, decode_control, encode_control};
        let f: DaemonFrame =
            decode_control(r#"{"type":"harness.update","session":2,"names":"all"}"#).unwrap();
        assert!(
            matches!(&f, DaemonFrame::HarnessUpdate(r) if r.names == HarnessNames::Word("all".into()))
        );
        assert_eq!(f.session(), Some(2));
        let f: DaemonFrame = decode_control(
            r#"{"type":"harness.update","session":3,"names":["pi"],"timeout_secs":60}"#,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&encode_control(&f).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "harness.update", "session": 3, "names": ["pi"], "timeout_secs": 60})
        );
    }
}
