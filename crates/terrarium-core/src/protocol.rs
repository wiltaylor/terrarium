//! The executor protocol (PLAN §19). The daemon sends requests to a host
//! agent or an SSH executor. The executor streams responses back. Many exec
//! sessions share one connection, so every message carries an [`ExecId`].
//!
//! The wire framing (WebSocket text frames with one JSON message each) is an
//! open item (PLAN §22, item 4). These types fix the message shapes only.

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
