//! The executor protocol (PLAN §19). The daemon sends requests to a host
//! agent or an SSH executor. The executor streams responses back. Many exec
//! sessions share one connection, so every message carries an [`ExecId`].
//!
//! These types fix the daemon-side message shapes. The host agent wire
//! protocol (framing, sessions, acks, reconnects) is in
//! `docs/host-protocol.md`, with its frame types in
//! `terrarium_host::protocol`. That protocol uses [`HostCapabilities`],
//! [`ExitReason`], [`CpDirection`], and [`TargetState`] from here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use crate::ids::ExecId;

/// Run a command on a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub id: ExecId,
    pub target: String,
    /// The program and its arguments.
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Kill the command after this many seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Run the command in a PTY (used by `terrarium shell`).
    #[serde(default)]
    pub pty: bool,
}

/// The direction of a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpDirection {
    /// From the agent workspace to the target.
    Push,
    /// From the target to the agent workspace.
    Pull,
}

/// Copy files between the agent workspace and a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpRequest {
    pub id: ExecId,
    pub target: String,
    pub direction: CpDirection,
    pub src: PathBuf,
    pub dst: PathBuf,
}

/// Target lifecycle verbs. Not every target kind supports every verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleVerb {
    Up,
    Down,
    Destroy,
    Snapshot,
    Restore,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRequest {
    pub id: ExecId,
    pub target: String,
    pub verb: LifecycleVerb,
    /// The snapshot name for `snapshot` and `restore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// Daemon to executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ExecutorRequest {
    Exec(ExecRequest),
    /// Bytes for the stdin of a running exec.
    Stdin {
        id: ExecId,
        data: Vec<u8>,
    },
    /// Resize the PTY of a running exec.
    Resize {
        id: ExecId,
        cols: u16,
        rows: u16,
    },
    /// Send a signal to a running exec.
    Kill {
        id: ExecId,
        signal: i32,
    },
    Cp(CpRequest),
    Lifecycle(LifecycleRequest),
    /// The daemon received output up to `offset`. The executor may drop it.
    Ack {
        id: ExecId,
        offset: u64,
    },
}

/// Which output stream a chunk belongs to. A PTY exec has only `stdout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// The state of a target, as the executor reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetState {
    Running,
    Stopped,
    Absent,
    Unknown,
}

/// What a host agent can do. Sent on connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub docker: bool,
    #[serde(default)]
    pub vmlab: bool,
    /// The host agent's own desktop is open to `desktop` targets.
    #[serde(default)]
    pub display: bool,
}

/// One external tool on a host agent's machine, as `terrarium-host`
/// detects it on start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolCapability {
    /// The tool answered its version probe.
    #[serde(default)]
    pub present: bool,
    /// The version that the tool reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The detailed capabilities that a host agent sends in its `hello` frame
/// (docs/host-protocol.md). [`Capabilities`] is the flat summary of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HostCapabilities {
    /// Physical host capacity for reservation accounting. Absent on older
    /// agents: reservations must fail closed rather than guess a budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<crate::resources::HostCapacity>,
    #[serde(default)]
    pub docker: ToolCapability,
    #[serde(default)]
    pub vmlab: ToolCapability,
    /// The shell that `terrarium shell` starts on a `host` target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Target displays (PLAN §19b). An older host agent sends none: the
    /// default is "off".
    #[serde(default)]
    pub display: DisplayCapability,
    /// The host agent sends `metrics` frames (CPU, memory, network, disk)
    /// when the daemon asks in `welcome`. An older host agent sends none:
    /// false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub metrics: bool,
    /// What the host agent can do for target resource limits. An older
    /// host agent sends none: the daemon refuses a session on a target
    /// with limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::limits::LimitsCapability>,
    /// The host agent has no agent workspaces: its `work_dir` is not a
    /// directory on this machine (a remote host agent with no copy of the
    /// daemon's work root). The daemon then starts a docker target on it
    /// without workspace mounts, and says so (TER-41). An older host agent
    /// sends none: false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_workspaces: bool,
}

/// What the host agent can do for target displays (docs/host-protocol.md,
/// "`display`").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DisplayCapability {
    /// The desktop of this machine (`desktop` targets): on only when the
    /// host agent's own config has `[display] enabled = true`. The daemon
    /// cannot turn it on.
    #[serde(default)]
    pub enabled: bool,
    /// The displays that a `desktop` target may name. Empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_displays: Vec<String>,
    /// `tesseract` is on the host: OCR for the backends that have none.
    #[serde(default)]
    pub tesseract: bool,
    /// The X11 and Wayland tools on this machine, for a `desktop` target:
    /// for example `xdotool`, `import`, `grim`, `wtype`, `ydotool`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// The display drivers this host agent has: `vmlab`, `docker`,
    /// `desktop`, and (later) `vnc`, `rdp`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drivers: Vec<String>,
}

impl HostCapabilities {
    /// The flat summary, for the dashboard and for policy checks.
    pub fn summary(&self, os: &str, arch: &str) -> Capabilities {
        Capabilities {
            os: os.to_owned(),
            arch: arch.to_owned(),
            docker: self.docker.present,
            vmlab: self.vmlab.present,
            display: self.display.enabled,
        }
    }
}

/// Why an exec session ended. A host agent sends it in its `exit` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The process exited by itself. `code` is set.
    Exited,
    /// A signal ended the process. `signal` is set.
    Signaled,
    /// The session ran past its `timeout_secs`. The host agent killed it.
    Timeout,
    /// The link to the daemon was down and the unacknowledged output filled
    /// the replay buffer. The host agent killed the process.
    OutputOverflow,
    /// The link to the daemon stayed down past the resume grace. The host
    /// agent killed the process.
    DetachedTimeout,
}

/// Executor to daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ExecutorResponse {
    /// The first message from a host agent after it dials in.
    Hello {
        host: String,
        version: String,
        capabilities: Capabilities,
    },
    /// A chunk of output. `offset` is the byte offset of the chunk in its stream.
    Output {
        id: ExecId,
        stream: Stream,
        offset: u64,
        data: Vec<u8>,
    },
    /// The command finished.
    Exit {
        id: ExecId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
    },
    /// A copy finished.
    CpDone { id: ExecId, bytes: u64 },
    /// A lifecycle verb finished.
    LifecycleDone {
        id: ExecId,
        state: TargetState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// The request failed before or while it ran.
    Error { id: ExecId, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_request_round_trips() {
        let req = ExecutorRequest::Exec(ExecRequest {
            id: ExecId::new(),
            target: "buildbox".into(),
            cmd: vec!["cargo".into(), "test".into()],
            cwd: Some("/src".into()),
            env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
            timeout_secs: Some(600),
            pty: false,
        });
        let json = serde_json::to_value(&req).expect("serialise");
        assert_eq!(json["op"], "exec");
        assert_eq!(json["target"], "buildbox");
        let back: ExecutorRequest = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, req);
    }

    #[test]
    fn host_capabilities_summarise() {
        let caps: HostCapabilities = serde_json::from_str(
            r#"{"docker":{"present":true,"version":"29.8.0"},"vmlab":{"present":false}}"#,
        )
        .expect("deserialise");
        assert_eq!(caps.docker.version.as_deref(), Some("29.8.0"));
        let flat = caps.summary("linux", "x86_64");
        assert!(flat.docker);
        assert!(!flat.vmlab);
        assert_eq!(
            serde_json::to_value(ExitReason::OutputOverflow).expect("serialise"),
            "output_overflow"
        );
    }

    #[test]
    fn responses_round_trip() {
        let id = ExecId::new();
        for resp in [
            ExecutorResponse::Hello {
                host: "lab1".into(),
                version: "0.1.0".into(),
                capabilities: Capabilities {
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    docker: true,
                    vmlab: true,
                    display: false,
                },
            },
            ExecutorResponse::Output {
                id,
                stream: Stream::Stderr,
                offset: 0,
                data: b"warning".to_vec(),
            },
            ExecutorResponse::Exit {
                id,
                code: Some(0),
                signal: None,
            },
            ExecutorResponse::LifecycleDone {
                id,
                state: TargetState::Running,
                detail: None,
            },
        ] {
            let json = serde_json::to_string(&resp).expect("serialise");
            let back: ExecutorResponse = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, resp);
        }
    }
}
