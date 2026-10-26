//! The host agent wire protocol: frame types and the codec.
//!
//! `docs/host-protocol.md` is the contract. In short: one WebSocket per host
//! agent. Text frames carry one JSON control frame each, tagged by `type`.
//! Binary frames carry stream bytes, with a 5-byte header: a 4-byte
//! big-endian session ID and a 1-byte stream tag.
//!
//! The types here are transport-agnostic. The codec reads and writes `&str`
//! and `&[u8]`, so the daemon can use them with axum's WebSocket and the host
//! agent with tokio-tungstenite.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use terrarium_core::protocol::{
    CpDirection, ExitReason, HostCapabilities, TargetState, ToolCapability,
};

/// The protocol version in `hello`. The daemon refuses versions it does not know.
pub const PROTOCOL_VERSION: u32 = 1;
/// The HTTP path of the dial-in endpoint on the daemon.
pub const CONNECT_PATH: &str = "/hosts/connect";
/// The default port of the dial-in listener.
pub const DEFAULT_PORT: u16 = 8082;
/// The length of the binary data frame header.
pub const DATA_HEADER_LEN: usize = 5;
/// The largest payload a receiver must accept in one data frame.
pub const MAX_DATA_PAYLOAD: usize = 1024 * 1024;
/// The payload size the host agent uses when it reads output and files.
pub const CHUNK_SIZE: usize = 32 * 1024;
/// The default replay buffer limit for each session.
pub const DEFAULT_BUFFER_LIMIT: usize = 8 * 1024 * 1024;
/// The heartbeat interval the host agent uses until `welcome` sets one.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 15;

/// A session ID. The daemon picks it. It is unique among the live sessions
/// of one host agent. `0` is reserved and never names a session.
pub type SessionId = u32;

/// The stream that a data frame belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StreamTag {
    /// Daemon to host: bytes for the process's stdin.
    Stdin = 0,
    /// Host to daemon: process stdout. A PTY session has stdout only.
    Stdout = 1,
    /// Host to daemon: process stderr.
    Stderr = 2,
    /// Either way: file content of a `cp` session.
    File = 3,
}

impl StreamTag {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Stdin),
            1 => Some(Self::Stdout),
            2 => Some(Self::Stderr),
            3 => Some(Self::File),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Errors from the codec.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("data frame of {0} bytes is shorter than the 5-byte header")]
    Short(usize),
    #[error("data frame has unknown stream tag {0}")]
    UnknownStream(u8),
    #[error("data frame names session 0, which is reserved")]
    ZeroSession,
    #[error("data frame payload of {0} bytes is over the limit")]
    TooLarge(usize),
    #[error("control frame: {0}")]
    Json(#[from] serde_json::Error),
}

/// One binary data frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrame {
    pub session: SessionId,
    pub stream: StreamTag,
    pub payload: Vec<u8>,
}

impl DataFrame {
    pub fn new(session: SessionId, stream: StreamTag, payload: Vec<u8>) -> Self {
        Self {
            session,
            stream,
            payload,
        }
    }

    /// Encode as `session (u32 BE) | stream (u8) | payload`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DATA_HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.session.to_be_bytes());
        out.push(self.stream.as_u8());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() < DATA_HEADER_LEN {
            return Err(CodecError::Short(bytes.len()));
        }
        let session = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if session == 0 {
            return Err(CodecError::ZeroSession);
        }
        let stream = StreamTag::from_u8(bytes[4]).ok_or(CodecError::UnknownStream(bytes[4]))?;
        let payload = &bytes[DATA_HEADER_LEN..];
        if payload.len() > MAX_DATA_PAYLOAD {
            return Err(CodecError::TooLarge(payload.len()));
        }
        Ok(Self {
            session,
            stream,
            payload: payload.to_vec(),
        })
    }
}

/// Encode a control frame as the text of one WebSocket text frame.
pub fn encode_control<T: Serialize>(frame: &T) -> Result<String, CodecError> {
    Ok(serde_json::to_string(frame)?)
}

/// Decode one WebSocket text frame.
pub fn decode_control<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, CodecError> {
    Ok(serde_json::from_str(text)?)
}

/// A terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

// ---------------------------------------------------------------- handshake

/// The first frame on every connection, host to daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The host agent's name, as targets refer to it.
    pub host: String,
    /// The `terrarium-host` version.
    pub version: String,
    pub os: String,
    pub arch: String,
    pub capabilities: HostCapabilities,
    pub max_sessions: u32,
    /// The most docker and vmlab targets that may run on this host at one
    /// time (`max_running_targets` in the host config). The daemon enforces
    /// it before an `up`. Absent from an older host agent: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_running_targets: Option<u32>,
    /// Present when the host agent reconnects and has a resume token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
}

/// The resume offer in `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The `resume_token` of the last `welcome`.
    pub token: String,
    /// Every session the host agent still holds.
    #[serde(default)]
    pub sessions: Vec<ResumeOffer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeOffer {
    pub session: SessionId,
    /// Output bytes the host agent produced for the session so far.
    pub sent: u64,
    /// The session has ended. Its terminal frames wait for the daemon.
    #[serde(default)]
    pub finished: bool,
}

/// The daemon's answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The daemon's stable ID for this host (from the host registry).
    pub host_id: String,
    pub heartbeat_secs: u64,
    /// The token to offer in the next `hello`.
    pub resume_token: String,
    /// The sessions the daemon takes back, with the bytes it holds of each.
    /// The host agent ends and forgets every session it holds that is not
    /// in this list.
    #[serde(default)]
    pub resume: Vec<ResumeAccept>,
    /// A new long-lived host token, sent once when the host agent
    /// connected with a short-lived enrol token. The host agent stores it
    /// in its `token_file` and uses it for every later connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeAccept {
    pub session: SessionId,
    /// Output bytes of the session the daemon has. Replay starts here.
    pub received: u64,
}

// ---------------------------------------------------------------- requests

/// Run a command on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub session: SessionId,
    /// The program. It is run directly, not through a shell.
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Added to the host agent's own environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Run in a PTY of this size. `null` runs with pipes.
    #[serde(default)]
    pub pty: Option<PtySize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Write a file on the host. File data frames follow, then `cp_end`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpPush {
    pub session: SessionId,
    /// The absolute destination path on the host.
    pub path: PathBuf,
    /// The file mode, for example `0o644` as the number 420.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Make missing parent directories.
    #[serde(default = "yes")]
    pub mkdirs: bool,
}

/// Read a file on the host. The host agent sends file data frames, then `cp_done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpPull {
    pub session: SessionId,
    pub path: PathBuf,
}

/// An agent workspace to attach to a target: `path` is `<project>/<agent>`,
/// relative to the host agent's `work_dir`. The target sees it at
/// `/work/<path>`, the same path as in the agent's sandbox
/// ([`WORKSPACE_MOUNT_ROOT`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceMount {
    pub path: String,
}

/// Where an attached workspace appears inside a target and in the sandbox.
pub const WORKSPACE_MOUNT_ROOT: &str = "/work";

impl WorkspaceMount {
    /// The path inside the target: `/work/<project>/<agent>`.
    pub fn target(&self) -> String {
        format!("{WORKSPACE_MOUNT_ROOT}/{}", self.path)
    }

    /// Check the path: two or more plain segments of `[A-Za-z0-9._-]`, no
    /// `.` or `..`, nothing absolute.
    pub fn check(&self) -> Result<(), String> {
        let segs: Vec<&str> = self.path.split('/').collect();
        let plain = |s: &&str| {
            !s.is_empty()
                && *s != "."
                && *s != ".."
                && !s.starts_with('-')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        };
        if segs.len() < 2 || !segs.iter().all(plain) {
            return Err(format!(
                "bad workspace path {:?}: use <project>/<agent>",
                self.path
            ));
        }
        Ok(())
    }
}

/// A bind mount or a named volume for `docker.run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMount {
    /// An absolute host path (a bind mount) or a volume name.
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerRun {
    pub session: SessionId,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `-d`. The default is true: the session ends when the container starts.
    #[serde(default = "yes")]
    pub detach: bool,
    /// `--rm`.
    #[serde(default)]
    pub rm: bool,
    /// `--init`: an init process as PID 1 that forwards signals, so that a
    /// command such as `sleep infinity` stops at once on `docker stop`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub init: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeMount>,
    /// Agent workspaces to bind: `-v <work_dir>/<path>:/work/<path>`. The
    /// host agent resolves the source under its `work_dir`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// The command and arguments after the image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// For an attached run (`detach = false`) in a terminal.
    #[serde(default)]
    pub pty: Option<PtySize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerExec {
    pub session: SessionId,
    pub container: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub pty: Option<PtySize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// `docker start CONTAINER`: start a stopped container. Its files stay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerStart {
    pub session: SessionId,
    pub container: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerStop {
    pub session: SessionId,
    pub container: String,
    /// `docker stop -t`: seconds before Docker kills the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerRm {
    pub session: SessionId,
    pub container: String,
    #[serde(default)]
    pub force: bool,
    /// `-v`: remove anonymous volumes too.
    #[serde(default)]
    pub volumes: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerInspect {
    pub session: SessionId,
    pub container: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerPs {
    pub session: SessionId,
    /// `-a`: stopped containers too.
    #[serde(default)]
    pub all: bool,
    /// `--filter label=K=V` for each entry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerVolumeCreate {
    pub session: SessionId,
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerVolumeRm {
    pub session: SessionId,
    pub name: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// `vmlab up|down|destroy` on one lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabLifecycle {
    pub session: SessionId,
    /// The lab directory, or the path of its `vmlab.wcl`.
    pub lab: PathBuf,
    /// A subset of machines (`up` and `down`). Empty means the whole lab.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<String>,
    /// `down --force`.
    #[serde(default)]
    pub force: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabStatus {
    pub session: SessionId,
    pub lab: PathBuf,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabExec {
    pub session: SessionId,
    pub lab: PathBuf,
    pub machine: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `--user`: a login label from the lab file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Passed to `vmlab exec --timeout` and enforced by the host agent too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabShell {
    pub session: SessionId,
    pub lab: PathBuf,
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub pty: PtySize,
}

/// `vmlab cp` between a host path and a guest path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabCp {
    pub session: SessionId,
    pub lab: PathBuf,
    pub machine: String,
    /// `push`: host to guest. `pull`: guest to host.
    pub direction: CpDirection,
    /// An absolute path on the host agent's machine.
    pub host_path: PathBuf,
    pub guest_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabSnapshot {
    pub session: SessionId,
    pub lab: PathBuf,
    /// The snapshot name.
    pub name: String,
    /// `--vm`. None means every machine in the lab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// `restore --discard-guest-changes`.
    #[serde(default)]
    pub discard_guest_changes: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabSnapshotList {
    pub session: SessionId,
    pub lab: PathBuf,
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Where the lab definition of a `vmlab.prepare` comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LabSource {
    /// A git repo: `git worktree add <instances>/<instance>/src <ref>`.
    /// Exactly one of `local` (a repo under the host agent's `repos_dir`,
    /// for example `labs.git`) and `url` (a mirror under the instances dir,
    /// fetched with the credential in `env`).
    Repo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// A branch, a tag, or a commit.
        #[serde(rename = "ref")]
        git_ref: String,
        /// The directory in the repo that holds `vmlab.wcl`. `""` or `.`:
        /// the root.
        #[serde(default)]
        path: String,
    },
    /// An agent workspace (`<project>/<agent>` under `work_dir`).
    Workspace {
        workspace: String,
        /// The directory in the workspace that holds `vmlab.wcl`.
        #[serde(default)]
        path: String,
    },
}

/// `vmlab.prepare`: make the lab instance `<instances>/<instance>/lab` from
/// a source, check the definition against the allowlist, attach the
/// workspaces, and `vmlab validate` it. The `result` is
/// `{"lab", "instance", "source", "attached", "dev", "head"?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabPrepare {
    pub session: SessionId,
    /// The instance name, `[A-Za-z0-9._-]`, for example the target name.
    pub instance: String,
    pub source: LabSource,
    /// The machine that gets the workspace (`@dev`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// At most one: a vmlab machine syncs one workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceMount>,
    /// The git credential for a `url` source: `TERRARIUM_GIT_USERNAME` and
    /// `TERRARIUM_GIT_PASSWORD`, or `TERRARIUM_GIT_SSH_KEY`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// `vmlab.release`: remove the instance dir `<instances>/<instance>`, and
/// `git worktree remove` its `src` worktree. Run it after `vmlab destroy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmlabRelease {
    pub session: SessionId,
    pub instance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

fn yes() -> bool {
    true
}

// ---------------------------------------------------------------- frames

/// Control frames from the daemon to the host agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonFrame {
    Welcome(Welcome),
    /// The daemon closes the connection. `reason` is a snake_case word, for
    /// example `token_revoked`, `replaced`, or `protocol`.
    Goodbye {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Exec(ExecRequest),
    /// Close the stdin of a session. In a PTY the host agent writes `^D`.
    Eof {
        session: SessionId,
    },
    Resize {
        session: SessionId,
        rows: u16,
        cols: u16,
    },
    /// Send a signal to the session's process group. `signal` is a name
    /// without the `SIG` prefix: `INT`, `TERM`, `KILL`, `HUP`, `QUIT`,
    /// `USR1`, `USR2`, `WINCH`, `CONT`, `STOP`.
    Signal {
        session: SessionId,
        signal: String,
    },
    /// The daemon holds `bytes` output bytes of the session.
    Ack {
        session: SessionId,
        bytes: u64,
    },
    /// The daemon has the terminal frames of the session. The host agent
    /// forgets it. On a live session this abandons it: the process is killed.
    Release {
        session: SessionId,
    },
    CpPush(CpPush),
    /// The end of a push.
    CpEnd {
        session: SessionId,
        bytes: u64,
        sha256: String,
    },
    CpPull(CpPull),
    #[serde(rename = "docker.run")]
    DockerRun(DockerRun),
    #[serde(rename = "docker.exec")]
    DockerExec(DockerExec),
    #[serde(rename = "docker.start")]
    DockerStart(DockerStart),
    #[serde(rename = "docker.stop")]
    DockerStop(DockerStop),
    #[serde(rename = "docker.rm")]
    DockerRm(DockerRm),
    #[serde(rename = "docker.inspect")]
    DockerInspect(DockerInspect),
    #[serde(rename = "docker.ps")]
    DockerPs(DockerPs),
    #[serde(rename = "docker.volume_create")]
    DockerVolumeCreate(DockerVolumeCreate),
    #[serde(rename = "docker.volume_rm")]
    DockerVolumeRm(DockerVolumeRm),
    #[serde(rename = "vmlab.up")]
    VmlabUp(VmlabLifecycle),
    #[serde(rename = "vmlab.down")]
    VmlabDown(VmlabLifecycle),
    #[serde(rename = "vmlab.destroy")]
    VmlabDestroy(VmlabLifecycle),
    #[serde(rename = "vmlab.status")]
    VmlabStatus(VmlabStatus),
    #[serde(rename = "vmlab.exec")]
    VmlabExec(VmlabExec),
    #[serde(rename = "vmlab.shell")]
    VmlabShell(VmlabShell),
    #[serde(rename = "vmlab.cp")]
    VmlabCp(VmlabCp),
    #[serde(rename = "vmlab.snapshot_take")]
    VmlabSnapshotTake(VmlabSnapshot),
    #[serde(rename = "vmlab.snapshot_restore")]
    VmlabSnapshotRestore(VmlabSnapshot),
    #[serde(rename = "vmlab.snapshot_list")]
    VmlabSnapshotList(VmlabSnapshotList),
    #[serde(rename = "vmlab.prepare")]
    VmlabPrepare(VmlabPrepare),
    #[serde(rename = "vmlab.release")]
    VmlabRelease(VmlabRelease),
}

impl DaemonFrame {
    /// The session the frame is about, if any.
    pub fn session(&self) -> Option<SessionId> {
        use DaemonFrame as F;
        Some(match self {
            F::Welcome(_) | F::Goodbye { .. } | F::Ping { .. } | F::Pong { .. } => return None,
            F::Exec(r) => r.session,
            F::Eof { session }
            | F::Resize { session, .. }
            | F::Signal { session, .. }
            | F::Ack { session, .. }
            | F::Release { session }
            | F::CpEnd { session, .. } => *session,
            F::CpPush(r) => r.session,
            F::CpPull(r) => r.session,
            F::DockerRun(r) => r.session,
            F::DockerExec(r) => r.session,
            F::DockerStart(r) => r.session,
            F::DockerStop(r) => r.session,
            F::DockerRm(r) => r.session,
            F::DockerInspect(r) => r.session,
            F::DockerPs(r) => r.session,
            F::DockerVolumeCreate(r) => r.session,
            F::DockerVolumeRm(r) => r.session,
            F::VmlabUp(r) | F::VmlabDown(r) | F::VmlabDestroy(r) => r.session,
            F::VmlabStatus(r) => r.session,
            F::VmlabExec(r) => r.session,
            F::VmlabShell(r) => r.session,
            F::VmlabCp(r) => r.session,
            F::VmlabSnapshotTake(r) | F::VmlabSnapshotRestore(r) => r.session,
            F::VmlabSnapshotList(r) => r.session,
            F::VmlabPrepare(r) => r.session,
            F::VmlabRelease(r) => r.session,
        })
    }
}

/// Error codes in an `error` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The frame did not parse or a field is not valid.
    BadRequest,
    /// The host agent already runs `max_sessions` sessions.
    Busy,
    /// The session ID is in use.
    DuplicateSession,
    /// The frame names a session the host agent does not hold.
    UnknownSession,
    /// The tool for the verb (Docker, vmlab) is not on this host.
    Unsupported,
    /// A path or cwd is outside `allowed_cwd_roots`, or is not absolute.
    PathDenied,
    /// The process did not start.
    SpawnFailed,
    /// A file operation failed.
    Io,
    /// A pushed file's byte count or SHA-256 does not match `cp_end`.
    ChecksumMismatch,
    /// The connection dropped during a push. The partial file is removed.
    Interrupted,
    /// A lab definition uses a feature outside the allowlist of
    /// `vmlab.prepare` (a host path, a script, a host port, ...).
    Refused,
}

/// Control frames from the host agent to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// Boxed: it is much larger than the other frames.
    Hello(Box<Hello>),
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// The session's process started. Not replayed after a reconnect.
    Started {
        session: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// Structured output of a verb (`docker.inspect`, `vmlab.status`, ...).
    /// It comes just before the session's `exit`.
    Result {
        session: SessionId,
        value: serde_json::Value,
    },
    /// Terminal: the process ended.
    Exit {
        session: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
        reason: ExitReason,
    },
    /// Terminal: the session failed before or while it ran.
    Error {
        session: SessionId,
        code: ErrorCode,
        message: String,
    },
    /// Terminal: a `cp` finished. For a push the host agent verified the
    /// digest. For a pull the daemon verifies it.
    CpDone {
        session: SessionId,
        bytes: u64,
        sha256: String,
    },
}

impl HostFrame {
    /// True for the frames that end a session: `exit`, `error`, `cp_done`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            HostFrame::Exit { .. } | HostFrame::Error { .. } | HostFrame::CpDone { .. }
        )
    }
}

/// Parse a signal name from a `signal` frame.
pub fn parse_signal(name: &str) -> Option<nix::sys::signal::Signal> {
    use nix::sys::signal::Signal as S;
    let upper = name.trim().to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    Some(match name {
        "INT" => S::SIGINT,
        "TERM" => S::SIGTERM,
        "KILL" => S::SIGKILL,
        "HUP" => S::SIGHUP,
        "QUIT" => S::SIGQUIT,
        "USR1" => S::SIGUSR1,
        "USR2" => S::SIGUSR2,
        "WINCH" => S::SIGWINCH,
        "CONT" => S::SIGCONT,
        "STOP" => S::SIGSTOP,
        _ => return None,
    })
}

/// Lowercase hex of a digest.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_frame_layout_is_big_endian_session_then_tag() {
        let f = DataFrame::new(0x0102_0304, StreamTag::Stderr, b"hi".to_vec());
        let bytes = f.encode();
        assert_eq!(bytes, vec![1, 2, 3, 4, 2, b'h', b'i']);
        assert_eq!(DataFrame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn data_frame_rejects_bad_input() {
        assert!(matches!(
            DataFrame::decode(&[0, 0, 1]),
            Err(CodecError::Short(3))
        ));
        assert!(matches!(
            DataFrame::decode(&[0, 0, 0, 0, 1]),
            Err(CodecError::ZeroSession)
        ));
        assert!(matches!(
            DataFrame::decode(&[0, 0, 0, 1, 9]),
            Err(CodecError::UnknownStream(9))
        ));
        let mut big = vec![0, 0, 0, 1, 1];
        big.resize(DATA_HEADER_LEN + MAX_DATA_PAYLOAD + 1, 0);
        assert!(matches!(
            DataFrame::decode(&big),
            Err(CodecError::TooLarge(_))
        ));
        // An empty payload is valid.
        assert!(DataFrame::decode(&[0, 0, 0, 1, 0]).is_ok());
    }

    #[test]
    fn exec_frame_matches_the_documented_shape() {
        let text = r#"{"type":"exec","session":7,"cmd":"sh","args":["-c","echo hi"],
            "cwd":"/srv","env":{"A":"1"},"pty":{"rows":40,"cols":120},"timeout_secs":60}"#;
        let frame: DaemonFrame = decode_control(text).unwrap();
        let DaemonFrame::Exec(req) = &frame else {
            panic!("not exec: {frame:?}")
        };
        assert_eq!(req.session, 7);
        assert_eq!(
            req.pty,
            Some(PtySize {
                rows: 40,
                cols: 120
            })
        );
        assert_eq!(frame.session(), Some(7));
        let back: DaemonFrame = decode_control(&encode_control(&frame).unwrap()).unwrap();
        assert_eq!(back, frame);
        // Minimal exec: pty null, no cwd.
        let min: DaemonFrame =
            decode_control(r#"{"type":"exec","session":1,"cmd":"true","pty":null}"#).unwrap();
        assert!(matches!(
            min,
            DaemonFrame::Exec(ExecRequest { pty: None, .. })
        ));
    }

    #[test]
    fn dotted_verb_names_round_trip() {
        let text = r#"{"type":"vmlab.snapshot_take","session":3,"lab":"/labs/win","name":"clean"}"#;
        let frame: DaemonFrame = decode_control(text).unwrap();
        assert!(matches!(&frame, DaemonFrame::VmlabSnapshotTake(s) if s.name == "clean"));
        let v: serde_json::Value = serde_json::from_str(&encode_control(&frame).unwrap()).unwrap();
        assert_eq!(v["type"], "vmlab.snapshot_take");

        let run: DaemonFrame =
            decode_control(r#"{"type":"docker.run","session":4,"image":"alpine:3"}"#).unwrap();
        let DaemonFrame::DockerRun(r) = run else {
            panic!()
        };
        assert!(r.detach, "detach defaults to true");
    }

    #[test]
    fn host_frames_round_trip() {
        let hello = HostFrame::Hello(Box::new(Hello {
            protocol: PROTOCOL_VERSION,
            host: "lab1".into(),
            version: "0.1.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            capabilities: HostCapabilities {
                docker: ToolCapability {
                    present: true,
                    version: Some("29.8.0".into()),
                },
                vmlab: ToolCapability::default(),
                shell: Some("/bin/bash".into()),
            },
            max_sessions: 32,
            max_running_targets: Some(4),
            resume: Some(Resume {
                token: "r1".into(),
                sessions: vec![ResumeOffer {
                    session: 9,
                    sent: 100,
                    finished: false,
                }],
            }),
        }));
        for f in [
            hello,
            HostFrame::Exit {
                session: 1,
                code: Some(3),
                signal: None,
                reason: ExitReason::Exited,
            },
            HostFrame::Error {
                session: 2,
                code: ErrorCode::PathDenied,
                message: "no".into(),
            },
            HostFrame::CpDone {
                session: 3,
                bytes: 5,
                sha256: "ab".into(),
            },
            HostFrame::Result {
                session: 4,
                value: serde_json::json!({"state":"running"}),
            },
        ] {
            let text = encode_control(&f).unwrap();
            let back: HostFrame = decode_control(&text).unwrap();
            assert_eq!(back, f);
        }
        let v: serde_json::Value = serde_json::to_value(HostFrame::Exit {
            session: 1,
            code: None,
            signal: Some(9),
            reason: ExitReason::Timeout,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type":"exit","session":1,"signal":9,"reason":"timeout"})
        );
    }

    #[test]
    fn signals_parse_with_or_without_prefix() {
        use nix::sys::signal::Signal;
        assert_eq!(parse_signal("INT"), Some(Signal::SIGINT));
        assert_eq!(parse_signal("sigterm"), Some(Signal::SIGTERM));
        assert_eq!(parse_signal("BOGUS"), None);
    }

    #[test]
    fn hex_is_lowercase() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }
}
