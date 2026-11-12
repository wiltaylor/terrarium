//! Shared parts of the verb builders: the plan type, result parsers, and
//! argument checks. [`crate::docker`] and [`crate::vmlab`] build on these.

use std::time::Duration;

use crate::config::PathPolicy;
use crate::protocol::{ErrorCode, ExecRequest};
use crate::session::ExecPlan;

/// A verb turned into a process to run, and how to read its stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbPlan {
    pub exec: ExecPlan,
    pub parser: Option<ResultParser>,
}

impl VerbPlan {
    pub fn plain(exec: ExecPlan) -> Self {
        Self { exec, parser: None }
    }

    pub fn parsed(mut exec: ExecPlan, parser: ResultParser) -> Self {
        exec.capture_stdout = true;
        Self {
            exec,
            parser: Some(parser),
        }
    }
}

/// Parsers for verbs that have a `result` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultParser {
    DockerRunId,
    DockerInspect(std::path::PathBuf),
    DockerPs,
    VmlabStatus,
    VmlabSnapshotList,
}

impl ResultParser {
    /// Parse the captured stdout. `code` is the exit code.
    pub fn parse(self, stdout: &[u8], code: Option<i32>) -> serde_json::Value {
        let text = String::from_utf8_lossy(stdout);
        match self {
            ResultParser::DockerRunId => crate::docker::parse_run_id(&text),
            ResultParser::DockerInspect(root) => {
                crate::docker::parse_inspect_workspaces(&text, code, &root)
            }
            ResultParser::DockerPs => crate::docker::parse_ps(&text),
            ResultParser::VmlabStatus => crate::vmlab::parse_status(&text),
            ResultParser::VmlabSnapshotList => crate::vmlab::parse_snapshot_list(&text),
        }
    }
}

/// A verb that cannot be built. It becomes an `error` frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct BuildError {
    pub code: ErrorCode,
    pub message: String,
}

impl BuildError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::BadRequest,
            message: message.into(),
        }
    }

    pub fn path(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::PathDenied,
            message: message.into(),
        }
    }
}

/// Check a positional value (an image, a container, a machine). It must not
/// be empty, must not start with `-` (so it cannot become a flag), and must
/// not hold control characters.
pub fn ident<'a>(what: &str, v: &'a str) -> Result<&'a str, BuildError> {
    if v.is_empty() {
        return Err(BuildError::bad(format!("{what} is empty")));
    }
    if v.starts_with('-') {
        return Err(BuildError::bad(format!("{what} {v:?} starts with '-'")));
    }
    if v.chars().any(char::is_control) {
        return Err(BuildError::bad(format!("{what} has control characters")));
    }
    Ok(v)
}

/// Check an environment variable name.
pub fn env_key(k: &str) -> Result<&str, BuildError> {
    if k.is_empty() || k.contains('=') || k.contains('\0') {
        return Err(BuildError::bad(format!("bad environment name {k:?}")));
    }
    Ok(k)
}

pub fn secs(t: Option<u64>) -> Option<Duration> {
    t.filter(|s| *s > 0).map(Duration::from_secs)
}

/// A plain `exec` frame.
pub fn plan_exec(req: &ExecRequest, policy: &PathPolicy) -> Result<VerbPlan, BuildError> {
    if req.windows_run_as.is_some() {
        return Err(BuildError::bad(
            "Windows run-as logon is not supported by this host",
        ));
    }
    if req.cmd.is_empty() {
        return Err(BuildError::bad("cmd is empty"));
    }
    for k in req.env.keys() {
        env_key(k)?;
    }
    let cwd = match &req.cwd {
        Some(c) => Some(policy.check(c).map_err(BuildError::path)?),
        None => None,
    };
    Ok(VerbPlan::plain(ExecPlan {
        #[cfg(windows)]
        windows_holder: None,
        #[cfg(windows)]
        windows_identity: None,
        program: req.cmd.clone(),
        args: req.args.clone(),
        cwd,
        env: req.env.clone(),
        pty: req.pty,
        timeout: secs(req.timeout_secs),
        capture_stdout: false,
        scope: None,
        private_file: None,
    }))
}

/// Run a probe command with a short timeout. Returns the exit code and stdout.
pub async fn probe(bin: &str, args: &[&str]) -> Option<(Option<i32>, String)> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .ok()?
        .ok()?;
    Some((
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn idents_refuse_flags() {
        assert!(ident("image", "alpine:3").is_ok());
        assert!(ident("image", "--privileged").is_err());
        assert!(ident("image", "").is_err());
        assert!(ident("image", "a\nb").is_err());
    }

    #[test]
    fn exec_checks_the_cwd() {
        let policy = PathPolicy {
            roots: Some(vec![PathBuf::from("/srv/allowed")]),
        };
        let mut req = ExecRequest {
            windows_run_as: None,
            session: 1,
            cmd: "ls".into(),
            args: vec![],
            cwd: Some("/etc".into()),
            env: Default::default(),
            pty: None,
            timeout_secs: Some(0),
            limits: None,
        };
        let err = plan_exec(&req, &policy).unwrap_err();
        assert_eq!(err.code, ErrorCode::PathDenied);
        req.cwd = None;
        let plan = plan_exec(&req, &policy).unwrap();
        assert_eq!(plan.exec.timeout, None, "0 means no timeout");
    }
}
