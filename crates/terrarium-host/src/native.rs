//! Native GPU enforcement (TER-345, docs/native-gpu-helper.md).
//!
//! The host agent is not root. A native process that it starts in a
//! systemd user scope can open every GPU device node, so an exclusive GPU
//! reservation does not hold for native sessions. This module has the
//! three parts that close that gap:
//!
//! - The root helper ([`Helper`], the `terrarium-host-helper` binary): a
//!   small systemd service with a typed Unix-socket API. It starts each
//!   native session as a transient system service, as the configured
//!   workload user, with `DevicePolicy=closed` and a `DeviceAllow=` entry
//!   for each granted GPU only. The policy is in place before the first
//!   instruction of user code runs. The helper never runs a command as
//!   root, never takes a unit name, a property, a device number, or a
//!   host path from its caller, and accepts only the trusted host-agent
//!   UID (`SO_PEERCRED`) on a socket that only that UID can open.
//! - The wire protocol: length-prefixed frames ([`Kind`]), a JSON
//!   [`Request`] first, then the stdio of a session.
//! - The shim ([`shim`], `terrarium-host native`): an unprivileged child
//!   of the host agent. It sends the request and relays its own stdin,
//!   stdout, stderr, signals, and terminal size, so the session runner of
//!   the host agent works as for any other process.
//!
//! The guarantee is managed-session exclusivity: no two holders' native
//! sessions on this host hold one exclusively reserved GPU at the same
//! time, and a native session opens only the GPUs that the daemon granted
//! it. Processes that Terrarium did not start are outside it.

use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use terrarium_core::resources::{RuntimeLimits, validate_gpu_id};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// The version of the helper protocol. A host agent and a helper with
/// different versions do not talk: the host agent then reports native
/// enforcement as unavailable (fail closed).
pub const PROTOCOL_VERSION: u32 = 1;
/// The default socket of the helper.
pub const DEFAULT_SOCKET: &str = "/run/terrarium-host-helper/helper.sock";
/// The default config file of the helper.
pub const DEFAULT_CONFIG: &str = "/etc/terrarium-host-helper/config.toml";
/// The environment variable that carries the request to the shim. An
/// environment variable is readable only by the same UID and root, unlike
/// the command line.
pub const REQUEST_ENV: &str = "TERRARIUM_NATIVE_REQUEST";
/// The start of the name of every unit that the helper starts.
pub const UNIT_PREFIX: &str = "terrarium-native-";
/// The NVIDIA character device major of `/dev/nvidia<minor>`.
pub const NVIDIA_MAJOR: u64 = 195;
/// The oldest systemd with `systemd-run --expand-environment=no`.
pub const MIN_SYSTEMD: u32 = 254;
/// The largest frame payload.
pub const MAX_FRAME: usize = 1 << 20;
/// The groups that a workload user must not have: each one gives root,
/// the Docker socket, or a way around the device policy.
pub const DENIED_GROUPS: &[&str] = &[
    "root",
    "wheel",
    "sudo",
    "admin",
    "adm",
    "docker",
    "libvirt",
    "lxd",
    "incus-admin",
    "disk",
    "kvm",
    "systemd-journal",
];
/// Signals that the shim may forward to a session.
const FORWARDED_SIGNALS: &[i32] = &[1, 2, 3, 10, 12, 15];
const MAX_GPUS: usize = 16;
const MAX_ARGS: usize = 4096;
const MAX_ENV: usize = 1024;
const MAX_NAME: usize = 256;
/// How long a caller may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a `systemctl` call or the GPU inventory may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------ wire

/// The frame types. Each frame is one type byte, a big-endian `u32`
/// length, and the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Caller to helper, first frame: a JSON [`Request`].
    Request,
    /// Caller to helper: session input.
    Stdin,
    /// Caller to helper: the end of the input.
    StdinEof,
    /// Caller to helper: a signal number as a big-endian `i32`.
    Signal,
    /// Caller to helper: rows and cols as two big-endian `u16`.
    Resize,
    /// Helper to caller: a JSON [`Reply`].
    Reply,
    /// Helper to caller: session output.
    Stdout,
    /// Helper to caller: session error output.
    Stderr,
    /// Helper to caller, last frame of a session: a JSON [`Exit`].
    Exit,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Stdin => 2,
            Self::StdinEof => 3,
            Self::Signal => 4,
            Self::Resize => 5,
            Self::Reply => 10,
            Self::Stdout => 11,
            Self::Stderr => 12,
            Self::Exit => 13,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Request,
            2 => Self::Stdin,
            3 => Self::StdinEof,
            4 => Self::Signal,
            5 => Self::Resize,
            10 => Self::Reply,
            11 => Self::Stdout,
            12 => Self::Stderr,
            13 => Self::Exit,
            _ => return None,
        })
    }
}

/// Write one frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: Kind,
    payload: &[u8],
) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|n| (*n as usize) <= MAX_FRAME)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut buf = Vec::with_capacity(payload.len() + 5);
    buf.push(kind.byte());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf).await?;
    w.flush().await
}

/// Read one frame. `None` at a clean end of the stream.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<(Kind, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut head[1..]).await?;
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
    let kind = Kind::from_byte(head[0]).ok_or_else(|| bad("unknown frame type"))?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(bad("frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some((kind, payload)))
}

/// What a caller asks the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// The version handshake. Needs no right.
    Hello(HelloRequest),
    /// Set the limits of a holder's slice (right `session`).
    Apply(ApplyRequest),
    /// Stop a holder's slice and every session in it (right `session`).
    Stop(SliceRequest),
    /// Start a session (right `session`). Session frames follow.
    Start(StartRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceRequest {
    pub target: String,
    pub holder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    pub target: String,
    pub holder: String,
    pub limits: RuntimeLimits,
}

/// One native session. The helper derives the unit name, the slice, the
/// device nodes, and every property from these typed fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    /// The session ID of the host agent.
    pub session: u32,
    /// The target and the holder: the slice is
    /// [`crate::limits::slice_name`] of them.
    pub target: String,
    pub holder: String,
    /// The reservation that grants the exclusive GPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// The GPUs that the session may open. Empty: no GPU.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<String>,
    /// The part of `gpus` that `lease` reserves exclusively.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusive_gpus: Vec<String>,
    /// Slice limits to set before the start. `None` leaves the slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<RuntimeLimits>,
    /// Rows and cols of a PTY. `None`: pipes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<(u16, u16)>,
    /// The working directory: under the workspace root of the helper.
    pub cwd: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A right of the trusted caller. Each operation needs its own right, so
/// a helper installed for one purpose cannot be used for another. The
/// quota operations of TER-251 get their own right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Right {
    /// Start, limit, and stop native sessions with device policy.
    Session,
}

/// Why the helper refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The caller is not the trusted host-agent UID.
    Unauthorized,
    /// The config does not grant the right of the operation.
    Forbidden,
    /// Another holder's session holds a GPU of the request.
    Busy,
    /// The request is malformed or unsafe.
    Invalid,
    /// This host cannot do it (protocol version, GPU, device nodes).
    Unsupported,
    /// systemd failed.
    Failed,
}

/// The answer to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Hello { version: u32, rights: Vec<Right> },
    Ok,
    Started { unit: String },
    Error { code: ErrorKind, message: String },
}

/// The end of a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// A refusal of the helper.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Refusal {
    pub code: ErrorKind,
    pub message: String,
}

impl Refusal {
    pub fn new(code: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }
    fn reply(&self) -> Reply {
        Reply::Error {
            code: self.code,
            message: self.message.clone(),
        }
    }
}

// ---------------------------------------------------------------- config

/// The root-owned config of the helper (`/etc/terrarium-host-helper/config.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperConfig {
    #[serde(default = "default_socket")]
    pub socket: PathBuf,
    /// The trusted host-agent user. Only it may connect.
    pub agent_user: String,
    /// The pool of users that native sessions run as: each holder gets one
    /// of them, and never one that another holder's live session uses.
    /// Each is not root, not the agent user, has no linger, and is in none
    /// of [`DENIED_GROUPS`].
    pub workload_users: Vec<String>,
    /// Every session working directory is under it.
    pub workspace_root: PathBuf,
    /// The rights of the agent user. Empty: only `hello`.
    #[serde(default)]
    pub rights: Vec<Right>,
    /// Where the helper keeps the environment file of each session.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_systemd_run")]
    pub systemd_run: PathBuf,
    #[serde(default = "default_systemctl")]
    pub systemctl: PathBuf,
    #[serde(default)]
    pub gpu: GpuConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuConfig {
    #[serde(default = "default_nvidia_smi")]
    pub nvidia_smi: PathBuf,
    /// The directory of the GPU device nodes.
    #[serde(default = "default_dev_dir")]
    pub dev_dir: PathBuf,
    /// Shared driver nodes that a session with a GPU also needs. Each
    /// must exist as a character device, or GPU sessions are refused.
    #[serde(default = "default_support_nodes")]
    pub support_nodes: Vec<PathBuf>,
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            nvidia_smi: default_nvidia_smi(),
            dev_dir: default_dev_dir(),
            support_nodes: default_support_nodes(),
        }
    }
}

fn default_socket() -> PathBuf {
    DEFAULT_SOCKET.into()
}
fn default_state_dir() -> PathBuf {
    "/run/terrarium-host-helper/sessions".into()
}
fn default_systemd_run() -> PathBuf {
    "/usr/bin/systemd-run".into()
}
fn default_systemctl() -> PathBuf {
    "/usr/bin/systemctl".into()
}
fn default_nvidia_smi() -> PathBuf {
    "/usr/bin/nvidia-smi".into()
}
fn default_dev_dir() -> PathBuf {
    "/dev".into()
}
fn default_support_nodes() -> Vec<PathBuf> {
    ["/dev/nvidiactl", "/dev/nvidia-uvm", "/dev/nvidia-uvm-tools"]
        .map(PathBuf::from)
        .to_vec()
}

impl HelperConfig {
    pub fn parse(text: &str) -> Result<Self, String> {
        let cfg: Self = toml::from_str(text).map_err(|e| format!("helper config: {e}"))?;
        for (name, p) in [
            ("socket", &cfg.socket),
            ("workspace_root", &cfg.workspace_root),
            ("state_dir", &cfg.state_dir),
            ("systemd_run", &cfg.systemd_run),
            ("systemctl", &cfg.systemctl),
            ("gpu.nvidia_smi", &cfg.gpu.nvidia_smi),
            ("gpu.dev_dir", &cfg.gpu.dev_dir),
        ] {
            if !p.is_absolute() {
                return Err(format!("helper config: {name} must be an absolute path"));
            }
        }
        if cfg.gpu.support_nodes.iter().any(|p| !p.is_absolute()) {
            return Err("helper config: gpu.support_nodes must be absolute paths".into());
        }
        if cfg.workload_users.is_empty() {
            return Err("helper config: workload_users must name at least one user".into());
        }
        for (i, u) in cfg.workload_users.iter().enumerate() {
            if *u == cfg.agent_user {
                return Err("helper config: workload_users must not contain agent_user".into());
            }
            if cfg.workload_users[..i].contains(u) {
                return Err(format!("helper config: workload user {u} is listed twice"));
            }
        }
        Ok(cfg)
    }
}

/// A file or directory that the helper trusts must be owned by root and
/// not writable by group or others.
pub fn check_root_owned(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.uid() != 0 {
        return Err(format!("{} must be owned by root", path.display()));
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err(format!(
            "{} must not be writable by group or others",
            path.display()
        ));
    }
    Ok(())
}

/// The most symlinks that [`check_root_chain`] follows in one path.
pub const MAX_SYMLINK_HOPS: usize = 40;

/// Resolve `path` one component at a time and check each step: every
/// directory and the final file must pass [`check_root_owned`], and every
/// symlink on the way (also one in the middle of a chain, or a directory
/// symlink) must be owned by root and sit in a directory that passed. So
/// no step can be repointed by another user. Returns the resolved path:
/// the helper runs and reads through it, not through the path as written.
pub fn check_root_chain(path: &Path) -> Result<PathBuf, String> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(format!("{} must be an absolute path", path.display()));
    }
    let root = Path::new("/");
    let meta = std::fs::symlink_metadata(root).map_err(|e| format!("/: {e}"))?;
    check_root_owned(root, &meta)?;
    // The checked real path so far, and the components still to resolve.
    let mut done = PathBuf::from("/");
    let mut todo: std::collections::VecDeque<std::ffi::OsString> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect();
    let mut hops = 0;
    while let Some(name) = todo.pop_front() {
        if name == ".." {
            // `done` holds no symlink, so its parent is the real parent.
            done.pop();
            continue;
        }
        let next = done.join(&name);
        let meta =
            std::fs::symlink_metadata(&next).map_err(|e| format!("{}: {e}", next.display()))?;
        if meta.file_type().is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                return Err(format!(
                    "{}: more than {MAX_SYMLINK_HOPS} symlinks",
                    path.display()
                ));
            }
            if meta.uid() != 0 {
                return Err(format!(
                    "{}: symlink {} must be owned by root",
                    path.display(),
                    next.display()
                ));
            }
            let target =
                std::fs::read_link(&next).map_err(|e| format!("{}: {e}", next.display()))?;
            if target.is_absolute() {
                done = PathBuf::from("/");
            }
            let rest: Vec<std::ffi::OsString> = target
                .components()
                .filter_map(|c| match c {
                    Component::Normal(n) => Some(n.to_owned()),
                    Component::ParentDir => Some("..".into()),
                    _ => None,
                })
                .collect();
            for c in rest.into_iter().rev() {
                todo.push_front(c);
            }
            continue;
        }
        if !todo.is_empty() && !meta.is_dir() {
            return Err(format!("{}: not a directory", next.display()));
        }
        check_root_owned(&next, &meta).map_err(|e| {
            if next == path {
                e
            } else {
                format!("{} resolves through {e}", path.display())
            }
        })?;
        done = next;
    }
    Ok(done)
}

/// Check the helper's state directory before it is created or changed:
/// the existing part of its parent chain must be root-owned, and the
/// directory itself must not be a symlink (the helper would chmod and
/// write through it).
pub fn check_state_dir(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{} must be an absolute path", path.display()));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!("{} must not be a symlink", path.display()));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(format!("{} must be a directory", path.display()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    // The nearest parent that exists: create_dir_all makes the rest as
    // root, under it.
    let mut parent = path.parent();
    while let Some(p) = parent {
        match std::fs::symlink_metadata(p) {
            Ok(_) => return check_root_chain(p).map(|_| ()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => parent = p.parent(),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    }
    Ok(())
}

/// A resolved user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// Check the workload identity against the trusted agent UID. `groups` are
/// the names of all groups of the workload user. `linger` is true when
/// systemd keeps a user manager for it: from that manager a session could
/// start a process outside its device policy.
pub fn check_workload(
    agent_uid: u32,
    workload: &Identity,
    groups: &[(u32, String)],
    linger: bool,
) -> Result<(), String> {
    if workload.uid == 0 || workload.gid == 0 {
        return Err(format!(
            "workload user {} must not be root or in group root",
            workload.name
        ));
    }
    if workload.uid == agent_uid || agent_uid == 0 {
        return Err(format!(
            "workload user {} and agent_user must be distinct non-root users",
            workload.name
        ));
    }
    if let Some((_, name)) = groups
        .iter()
        .find(|(gid, name)| *gid == 0 || DENIED_GROUPS.contains(&name.as_str()))
    {
        return Err(format!(
            "workload user {} must not be in group {name}",
            workload.name
        ));
    }
    if linger {
        return Err(format!(
            "workload user {} has linger enabled; run `loginctl disable-linger {}`",
            workload.name, workload.name
        ));
    }
    Ok(())
}

/// The GIDs and names of a user's groups.
pub type Groups = Vec<(u32, String)>;

/// Look up a user, its groups, and its linger file.
pub fn resolve_user(name: &str) -> Result<(Identity, Groups, bool), String> {
    use nix::unistd::{Group, User};
    let user = User::from_name(name)
        .map_err(|e| format!("look up user {name}: {e}"))?
        .ok_or_else(|| format!("no user {name}"))?;
    let id = Identity {
        name: user.name.clone(),
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
    };
    let cname = std::ffi::CString::new(name).map_err(|_| format!("bad user name {name:?}"))?;
    let groups = nix::unistd::getgrouplist(&cname, user.gid)
        .map_err(|e| format!("groups of {name}: {e}"))?
        .into_iter()
        .map(|g| {
            let n = Group::from_gid(g)
                .ok()
                .flatten()
                .map(|g| g.name)
                .unwrap_or_default();
            (g.as_raw(), n)
        })
        .collect();
    let linger = Path::new("/var/lib/systemd/linger").join(name).exists();
    Ok((id, groups, linger))
}

/// The check of a workload user before each session: a later `usermod`
/// or `loginctl enable-linger` must not reach a new session.
pub trait WorkloadCheck: Send + Sync {
    fn check(&self, w: &Identity) -> Result<(), String>;
}

/// The real check: the passwd and group databases and the linger file.
pub struct HostWorkloadCheck {
    pub agent_uid: u32,
}

impl WorkloadCheck for HostWorkloadCheck {
    fn check(&self, w: &Identity) -> Result<(), String> {
        let (now, groups, linger) = resolve_user(&w.name)?;
        if now != *w {
            return Err(format!(
                "workload user {} changed its UID or GID since the helper started; restart the helper",
                w.name
            ));
        }
        check_workload(self.agent_uid, w, &groups, linger)
    }
}

/// The version in the first line of `systemctl --version`:
/// `systemd 255 (255.4-1ubuntu8)`.
pub fn parse_systemd_version(text: &str) -> Option<u32> {
    let mut w = text.lines().next()?.split_whitespace();
    if w.next()? != "systemd" {
        return None;
    }
    w.next()?.parse().ok()
}

// --------------------------------------------------------------- devices

/// Device node checks. The real one reads the file system; tests use fake
/// device nodes.
pub trait DeviceNodes: Send + Sync {
    /// The major and minor of the character device at `path`, without
    /// following a symlink.
    fn char_device(&self, path: &Path) -> Result<(u64, u64), String>;
}

/// The file system.
pub struct HostDevices;

impl DeviceNodes for HostDevices {
    fn char_device(&self, path: &Path) -> Result<(u64, u64), String> {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| format!("device node {}: {e}", path.display()))?;
        if !meta.file_type().is_char_device() {
            return Err(format!("{} is not a character device", path.display()));
        }
        let dev = meta.rdev();
        Ok((nix::sys::stat::major(dev), nix::sys::stat::minor(dev)))
    }
}

/// Parse `nvidia-smi --query-gpu=uuid,minor_number --format=csv,noheader,nounits`.
pub fn parse_minors(text: &str) -> Result<Vec<(String, u32)>, String> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (uuid, minor) = line
            .split_once(',')
            .ok_or_else(|| format!("bad GPU inventory line {line:?}"))?;
        let (uuid, minor) = (uuid.trim(), minor.trim());
        validate_gpu_id(uuid)?;
        let minor: u32 = minor
            .parse()
            .map_err(|_| format!("bad GPU minor number {minor:?}"))?;
        if minor > 254 || out.iter().any(|(u, m)| u == uuid || *m == minor) {
            return Err(format!("bad GPU inventory line {line:?}"));
        }
        out.push((uuid.to_owned(), minor));
    }
    Ok(out)
}

// ------------------------------------------------------------------ plan

/// What the helper needs to plan a session.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Canonical.
    pub workspace_root: PathBuf,
    pub state_dir: PathBuf,
    pub dev_dir: PathBuf,
    pub support_nodes: Vec<PathBuf>,
}

/// A planned session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub unit: String,
    pub slice: String,
    /// The `systemd-run` arguments.
    pub args: Vec<String>,
    pub env_file: PathBuf,
    /// The content of `env_file`.
    pub env: String,
    /// `systemctl` arguments that set the slice limits first.
    pub slice_args: Option<Vec<String>>,
    pub claim: Claim,
}

fn check_name(what: &str, s: &str, empty_ok: bool) -> Result<(), Refusal> {
    if (!empty_ok && s.is_empty()) || s.len() > MAX_NAME || s.chars().any(char::is_control) {
        return Err(Refusal::invalid(format!("bad {what}")));
    }
    Ok(())
}

fn check_lease(lease: &str) -> Result<(), Refusal> {
    if lease.is_empty()
        || lease.len() > 64
        || !lease
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Refusal::invalid("bad lease"));
    }
    Ok(())
}

fn valid_env_name(k: &str) -> bool {
    let mut c = k.chars();
    k.len() <= MAX_NAME
        && matches!(c.next(), Some(x) if x.is_ascii_alphabetic() || x == '_')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// One line of a systemd environment file. In double quotes, systemd
/// takes a backslash before `"`, `\`, `` ` ``, and `$` as the plain
/// character, and expands nothing.
pub fn env_line(key: &str, value: &str) -> String {
    let mut out = format!("{key}=\"");
    for c in value.chars() {
        if matches!(c, '"' | '\\' | '`' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push_str("\"\n");
    out
}

/// The unit of a session. Only digits and hex: the caller chooses neither.
pub fn unit_name(session: u32, nonce: u32) -> String {
    format!("{UNIT_PREFIX}{session}-{nonce:08x}.service")
}

/// Check a request and build its `systemd-run` call. Every value that
/// reaches systemd is typed or checked here.
pub fn plan_start(
    ctx: &Ctx,
    req: &StartRequest,
    inventory: &[(String, u32)],
    nodes: &dyn DeviceNodes,
    nonce: u32,
    workload: &Identity,
) -> Result<Plan, Refusal> {
    check_name("target", &req.target, true)?;
    check_name("holder", &req.holder, false)?;
    if let Some(lease) = &req.lease {
        check_lease(lease)?;
    }
    if req.gpus.len() > MAX_GPUS {
        return Err(Refusal::invalid("too many GPUs"));
    }
    for (i, g) in req.gpus.iter().enumerate() {
        if g.len() > 64 {
            return Err(Refusal::invalid("bad GPU UUID"));
        }
        validate_gpu_id(g).map_err(Refusal::invalid)?;
        if req.gpus[..i].contains(g) {
            return Err(Refusal::invalid("a GPU is listed twice"));
        }
    }
    if req.exclusive_gpus.iter().any(|g| !req.gpus.contains(g)) {
        return Err(Refusal::invalid("an exclusive GPU is not in gpus"));
    }
    if !req.exclusive_gpus.is_empty() && req.lease.is_none() {
        return Err(Refusal::invalid("exclusive GPUs need a lease"));
    }
    if req.argv.is_empty() || req.argv[0].is_empty() || req.argv.len() > MAX_ARGS {
        return Err(Refusal::invalid("bad argv"));
    }
    let mut size = 0usize;
    for a in &req.argv {
        if a.contains('\0') {
            return Err(Refusal::invalid("argv contains NUL"));
        }
        size += a.len() + 1;
    }
    if req.env.len() > MAX_ENV {
        return Err(Refusal::invalid("too many environment variables"));
    }
    let mut env = String::new();
    for (k, v) in &req.env {
        if !valid_env_name(k) || v.contains(['\0', '\n', '\r']) {
            return Err(Refusal::invalid(format!("bad environment variable {k:?}")));
        }
        size += k.len() + v.len() + 2;
        env.push_str(&env_line(k, v));
    }
    if size > MAX_FRAME {
        return Err(Refusal::invalid("command and environment too large"));
    }
    if let Some((rows, cols)) = req.pty
        && (rows == 0 || cols == 0)
    {
        return Err(Refusal::invalid("bad PTY size"));
    }
    if let Some(l) = &req.limits {
        l.validate().map_err(Refusal::invalid)?;
    }
    let cwd = safe_cwd(&ctx.workspace_root, &req.cwd)?;
    // `safe_cwd` checks the working directory for WorkingDirectory=; the
    // path list rules below are only for the mount properties.
    systemd_path(&ctx.workspace_root)?;
    // Each session sees only the workspace scope of its holder (see
    // [`holder_workspace`]): its working directory is in it, or is the
    // empty workspace root itself.
    let own = holder_workspace(&ctx.workspace_root, &req.holder)?;
    if cwd != ctx.workspace_root && !own.as_ref().is_some_and(|w| cwd.starts_with(w)) {
        return Err(Refusal::invalid(format!(
            "working directory {} is outside the workspace of holder {}",
            cwd.display(),
            req.holder
        )));
    }

    let mut devices = Vec::new();
    if !req.gpus.is_empty() {
        for g in &req.gpus {
            let minor = inventory
                .iter()
                .find(|(u, _)| u == g)
                .map(|(_, m)| *m)
                .ok_or_else(|| {
                    Refusal::new(
                        ErrorKind::Unsupported,
                        format!("GPU {g} is not on this host"),
                    )
                })?;
            let node = ctx.dev_dir.join(format!("nvidia{minor}"));
            let found = nodes
                .char_device(&node)
                .map_err(|e| Refusal::new(ErrorKind::Unsupported, e))?;
            if found != (NVIDIA_MAJOR, u64::from(minor)) {
                return Err(Refusal::new(
                    ErrorKind::Unsupported,
                    format!(
                        "{} is {}:{}, not the device of GPU {g}",
                        node.display(),
                        found.0,
                        found.1
                    ),
                ));
            }
            devices.push(node);
        }
        for node in &ctx.support_nodes {
            nodes
                .char_device(node)
                .map_err(|e| Refusal::new(ErrorKind::Unsupported, e))?;
            devices.push(node.clone());
        }
    }

    let unit = unit_name(req.session, nonce);
    let slice = crate::limits::slice_name(&req.target, &req.holder);
    let env_file = ctx.state_dir.join(format!("{unit}.env"));
    let mut args: Vec<String> = [
        "--system",
        "--quiet",
        "--collect",
        "--wait",
        "--expand-environment=no",
        "--service-type=exec",
    ]
    .map(str::to_owned)
    .to_vec();
    args.push(if req.pty.is_some() { "--pty" } else { "--pipe" }.to_owned());
    args.push(format!("--unit={unit}"));
    args.push(format!("--slice={slice}"));
    args.push(format!("--uid={}", workload.uid));
    args.push(format!("--gid={}", workload.gid));
    for p in [
        "NoNewPrivileges=yes",
        "CapabilityBoundingSet=",
        "AmbientCapabilities=",
        "RestrictSUIDSGID=yes",
        // A pool UID passes to another holder when its holder's last
        // session ends. What the UID can reach from one holder to the next:
        // /tmp and /var/tmp are private to the unit; systemd removes the
        // UID's System V and POSIX IPC (also /dev/shm) when the last unit
        // of the UID stops; the rest of the tree is read-only, so no
        // world-writable directory (/run/lock, /var/crash, ...) keeps a
        // file of the UID. The workspace root is an empty read-only tmpfs
        // with only this holder's workspace scope bound in (below,
        // [`holder_workspace`]): a file that an earlier holder of the UID
        // left in another trust domain, also a 0600 one, does not exist in
        // the unit.
        "PrivateTmp=yes",
        "RemoveIPC=yes",
        "ProtectSystem=strict",
        "KillMode=control-group",
        "DevicePolicy=closed",
        "DeviceAllow=char-pts rw",
    ] {
        args.push("-p".into());
        args.push(p.into());
    }
    for d in &devices {
        args.push("-p".into());
        args.push(format!("DeviceAllow={} rw", d.display()));
    }
    match &own {
        // The operator: the whole workspace root, as it is.
        Some(w) if *w == ctx.workspace_root => {
            args.push("-p".into());
            args.push(format!("ReadWritePaths={}", w.display()));
        }
        own => {
            args.push("-p".into());
            args.push(format!(
                "TemporaryFileSystem={}:ro",
                ctx.workspace_root.display()
            ));
            if let Some(w) = own {
                args.push("-p".into());
                args.push(format!("BindPaths={}", w.display()));
                args.push("-p".into());
                args.push(format!("ReadWritePaths={}", w.display()));
            }
        }
    }
    args.push("-p".into());
    args.push(format!("WorkingDirectory={}", cwd.display()));
    args.push("-p".into());
    args.push(format!("EnvironmentFile={}", env_file.display()));
    args.push("--".into());
    args.extend(req.argv.iter().cloned());

    let slice_args = match &req.limits {
        None => None,
        Some(l) => Some(slice_args(&slice, l)?),
    };
    Ok(Plan {
        unit,
        slice,
        args,
        env_file,
        env,
        slice_args,
        claim: Claim {
            holder: req.holder.clone(),
            lease: req.lease.clone(),
            gpus: req.gpus.clone(),
            exclusive: req.exclusive_gpus.clone(),
            uid: workload.uid,
            stuck: false,
        },
    })
}

/// `systemctl set-property --runtime` of a slice, from typed limits only.
pub fn slice_args(slice: &str, l: &RuntimeLimits) -> Result<Vec<String>, Refusal> {
    let mut a = vec![
        "set-property".to_owned(),
        "--runtime".to_owned(),
        "--".to_owned(),
        slice.to_owned(),
    ];
    a.extend(crate::limits::reservation_properties(l).map_err(Refusal::invalid)?);
    Ok(a)
}

/// The working directory, resolved by root, under the workspace root.
/// The values reach a unit property, so `%`, a leading `-`, and control
/// characters are refused.
pub fn safe_cwd(root: &Path, cwd: &str) -> Result<PathBuf, Refusal> {
    if !cwd.starts_with('/') || cwd.contains('%') || cwd.chars().any(char::is_control) {
        return Err(Refusal::invalid("bad working directory"));
    }
    let real = std::fs::canonicalize(cwd)
        .map_err(|e| Refusal::invalid(format!("working directory {cwd}: {e}")))?;
    if !real.starts_with(root) || !real.is_dir() {
        return Err(Refusal::invalid(format!(
            "working directory {cwd} is not under the workspace root"
        )));
    }
    let s = real.to_string_lossy();
    if s.contains('%') || s.chars().any(char::is_control) || real.to_str().is_none() {
        return Err(Refusal::invalid("bad working directory"));
    }
    Ok(real)
}

/// A path that goes into a systemd property as one entry of a
/// space-separated list (`TemporaryFileSystem=`, `BindPaths=`,
/// `ReadWritePaths=`): absolute, UTF-8, and no character that systemd
/// would read as a separator, a quote, a specifier, or an option.
fn systemd_path(p: &Path) -> Result<(), Refusal> {
    match p.to_str() {
        Some(t)
            if t.starts_with('/')
                && !t.contains(|c: char| {
                    c.is_whitespace()
                        || c.is_control()
                        || matches!(c, '%' | '"' | '\'' | '\\' | ':')
                }) =>
        {
            Ok(())
        }
        _ => Err(Refusal::new(
            ErrorKind::Unsupported,
            format!(
                "{} cannot be a path in a systemd property (whitespace, quotes, %, :, or \\)",
                p.display()
            ),
        )),
    }
}

/// The project services whose holder is `<project>/<service>`: they run in
/// the project's trust domain, not as one agent.
pub const PROJECT_SERVICES: &[&str] = &["jobs", "pipelines", "monitors"];

/// The workspace scope of a holder, by its trust domain (the host agent's
/// layout is `<root>/<project>/<agent>`):
/// - `operator`: the whole `root` (trusted);
/// - a project service `<project>/jobs`, `/pipelines`, `/monitors`: the
///   project's directory `<root>/<project>`, never another project's;
/// - an agent `<project>/<agent>`: only `<root>/<project>/<agent>`.
///
/// `None` for any other holder, or a scope directory that is not there
/// yet: the session then gets no workspace (the empty root). A scope that
/// is a symlink, leads out of `root`, or is not a directory is refused.
pub fn holder_workspace(root: &Path, holder: &str) -> Result<Option<PathBuf>, Refusal> {
    if holder == "operator" {
        return Ok(Some(root.to_path_buf()));
    }
    let mut parts = holder.split('/');
    let (Some(project), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Ok(None);
    };
    for part in [project, name] {
        if part.is_empty() || part == "." || part == ".." {
            return Err(Refusal::invalid(format!("bad holder {holder:?}")));
        }
    }
    let dir = if PROJECT_SERVICES.contains(&name) {
        root.join(project)
    } else {
        root.join(project).join(name)
    };
    match std::fs::canonicalize(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Refusal::invalid(format!(
            "workspace {}: {e}",
            dir.display()
        ))),
        Ok(real) if real != dir || !real.is_dir() => Err(Refusal::invalid(format!(
            "workspace {} must be a directory, not a symlink",
            dir.display()
        ))),
        Ok(real) => {
            systemd_path(&real)?;
            Ok(Some(real))
        }
    }
}

/// `workspace_root` at helper start: not under /tmp or /var/tmp, which
/// `PrivateTmp=yes` replaces in each unit (every session would fail).
pub fn check_workspace_root(root: &Path) -> Result<(), String> {
    for tmp in ["/tmp", "/var/tmp"] {
        if root.starts_with(tmp) {
            return Err(format!(
                "workspace_root {} is under {tmp}, which each session gets a private copy of \
                 (PrivateTmp=yes): put the workspace elsewhere, for example /srv/terrarium/work",
                root.display()
            ));
        }
    }
    systemd_path(root).map_err(|r| r.message)
}

/// The slice of a holder for `apply` and `stop`. `stop` never takes the
/// shared parent slices.
pub fn holder_slice(target: &str, holder: &str, stop: bool) -> Result<String, Refusal> {
    check_name("target", target, true)?;
    check_name("holder", holder, true)?;
    if stop && (holder.is_empty() || target.is_empty()) {
        return Err(Refusal::invalid("stop needs a target and a holder"));
    }
    Ok(crate::limits::slice_name(target, holder))
}

// -------------------------------------------------------------- sessions

/// What one live session holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub holder: String,
    pub lease: Option<String>,
    pub gpus: Vec<String>,
    pub exclusive: Vec<String>,
    /// The workload UID of the session.
    pub uid: u32,
    /// The unit was not confirmed stopped: its GPUs are unavailable to
    /// everyone and its UID stays with its holder until it is.
    pub stuck: bool,
}

/// The live sessions of the helper, by unit. The helper stops its units
/// on start, so the table is complete.
#[derive(Debug, Default)]
pub struct Sessions {
    live: BTreeMap<String, Claim>,
}

impl Sessions {
    /// The workload user of a holder: the UID of its live sessions, else a
    /// pool user that no live session of another holder uses.
    pub fn pick(&self, holder: &str, pool: &[Identity]) -> Result<Identity, Refusal> {
        if let Some(c) = self.live.values().find(|c| c.holder == holder)
            && let Some(id) = pool.iter().find(|w| w.uid == c.uid)
        {
            return Ok(id.clone());
        }
        pool.iter()
            .find(|w| !self.live.values().any(|c| c.uid == w.uid))
            .cloned()
            .ok_or_else(|| {
                Refusal::new(
                    ErrorKind::Busy,
                    format!(
                        "no free workload user: all {} of the pool are in use by other holders' sessions",
                        pool.len()
                    ),
                )
            })
    }

    /// Admit a session. A GPU that either side holds exclusively may be
    /// shared only by sessions of the same lease (one reservation; several
    /// agents may use a target's reservation). A GPU of a stuck unit is
    /// unavailable. A UID never serves two holders at once.
    pub fn admit(&mut self, unit: &str, claim: Claim) -> Result<(), Refusal> {
        for (other_unit, other) in &self.live {
            if other.uid == claim.uid && other.holder != claim.holder {
                return Err(Refusal::new(
                    ErrorKind::Forbidden,
                    "the workload user serves another holder",
                ));
            }
            for g in &claim.gpus {
                if !other.gpus.contains(g) {
                    continue;
                }
                if other.stuck {
                    return Err(Refusal::new(
                        ErrorKind::Busy,
                        format!(
                            "GPU {g} is unavailable: unit {other_unit} is not confirmed stopped"
                        ),
                    ));
                }
                let exclusive = other.exclusive.contains(g) || claim.exclusive.contains(g);
                let same = claim.lease.is_some() && other.lease == claim.lease;
                if exclusive && !same {
                    return Err(Refusal::new(
                        ErrorKind::Busy,
                        format!("GPU {g} is held by a session of another reservation"),
                    ));
                }
            }
        }
        self.live.insert(unit.to_owned(), claim);
        Ok(())
    }

    /// Keep the claim of a unit whose stop is not confirmed.
    pub fn mark_stuck(&mut self, unit: &str) {
        if let Some(c) = self.live.get_mut(unit) {
            c.stuck = true;
        }
    }

    /// Free the claim. Only after the unit is confirmed stopped.
    pub fn end(&mut self, unit: &str) {
        self.live.remove(unit);
    }

    pub fn claim(&self, unit: &str) -> Option<&Claim> {
        self.live.get(unit)
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

// ---------------------------------------------------------------- helper

/// The running helper.
pub struct Helper {
    pub ctx: Ctx,
    pub agent_uid: u32,
    pub rights: Vec<Right>,
    /// The workload users, in config order.
    pub pool: Vec<Identity>,
    /// Re-checks a workload user before each session.
    pub checker: Box<dyn WorkloadCheck>,
    pub systemd_run: PathBuf,
    pub systemctl: PathBuf,
    pub nvidia_smi: PathBuf,
    /// The cgroup v2 mount ([`CGROUP_ROOT`]; a fake tree in tests).
    pub cgroup_root: PathBuf,
    /// `Ok` when `nvidia_smi` was verified root-owned at start. Otherwise
    /// GPU sessions are refused with the reason.
    pub gpu: Result<(), String>,
    pub nodes: Box<dyn DeviceNodes>,
    pub sessions: Mutex<Sessions>,
    /// How long to wait for a unit to be inactive after its kill.
    pub stop_timeout: Duration,
    /// How often to try again for a stuck unit.
    pub stop_retry: Duration,
}

/// Where the cgroup v2 tree is mounted.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Fail unless `root` is a cgroup v2 tree (`cgroup.controllers` at its
/// top). On v1 or hybrid hosts the unit cgroups are not under it, and a
/// stop could not be confirmed from `cgroup.events`.
pub fn check_cgroup_v2(root: &Path) -> Result<(), String> {
    if root.join("cgroup.controllers").is_file() {
        Ok(())
    } else {
        Err(format!(
            "{} is not a cgroup v2 tree (no cgroup.controllers): the helper needs cgroup v2",
            root.display()
        ))
    }
}

/// True when the cgroup `cgroup` (a `ControlGroup` value, relative to
/// `root`) holds no process: no cgroup value, a cgroup directory that is
/// gone, or `populated 0` in its `cgroup.events`. Fails closed: a root that
/// is not cgroup v2, a cgroup directory without `cgroup.events`, or a value
/// that is not a clean absolute path is an error, not "empty".
pub fn cgroup_empty(root: &Path, cgroup: &str) -> Result<bool, String> {
    if cgroup.is_empty() {
        return Ok(true);
    }
    let rel = Path::new(cgroup);
    if !rel.is_absolute()
        || rel
            .components()
            .skip(1)
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!("bad ControlGroup {cgroup:?}"));
    }
    check_cgroup_v2(root)?;
    let dir = root.join(rel.strip_prefix("/").unwrap_or(rel));
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
        Ok(m) if !m.is_dir() => return Err(format!("{} is not a cgroup", dir.display())),
        Ok(_) => {}
    }
    let events = dir.join("cgroup.events");
    let text =
        std::fs::read_to_string(&events).map_err(|e| format!("{}: {e}", events.display()))?;
    let populated = text
        .lines()
        .find_map(|l| l.strip_prefix("populated "))
        .ok_or_else(|| format!("{}: no populated line", events.display()))?;
    Ok(populated.trim() == "0")
}

/// The default [`Helper::stop_timeout`].
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// The default [`Helper::stop_retry`].
pub const STOP_RETRY: Duration = Duration::from_secs(10);

/// Run a command to its end with a timeout. The error holds the first
/// line of stderr.
async fn call(bin: &Path, args: &[String]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(CALL_TIMEOUT, cmd.output()).await {
        Err(_) => return Err(format!("`{}` did not end", bin.display())),
        Ok(Err(e)) => return Err(format!("cannot run `{}`: {e}", bin.display())),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(format!(
        "`{} {}` failed ({}): {}",
        bin.display(),
        args.first().map(String::as_str).unwrap_or(""),
        out.status,
        err.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
    ))
}

impl Helper {
    fn allowed(&self, right: Right) -> Result<(), Refusal> {
        if self.rights.contains(&right) {
            Ok(())
        } else {
            Err(Refusal::new(
                ErrorKind::Forbidden,
                format!("the helper config does not grant the {right:?} right"),
            ))
        }
    }

    async fn systemctl(&self, args: Vec<String>) -> Result<String, Refusal> {
        call(&self.systemctl, &args)
            .await
            .map_err(|e| Refusal::new(ErrorKind::Failed, e))
    }

    async fn inventory(&self) -> Result<Vec<(String, u32)>, Refusal> {
        let text = call(
            &self.nvidia_smi,
            &[
                "--query-gpu=uuid,minor_number".to_owned(),
                "--format=csv,noheader,nounits".to_owned(),
            ],
        )
        .await
        .map_err(|e| Refusal::new(ErrorKind::Unsupported, e))?;
        parse_minors(&text).map_err(|e| Refusal::new(ErrorKind::Unsupported, e))
    }

    fn kill_unit(&self, unit: &str, signal: i32) -> BoxFuture<'static, ()> {
        let bin = self.systemctl.clone();
        let args = vec![
            "kill".to_owned(),
            format!("--signal={signal}"),
            "--".to_owned(),
            unit.to_owned(),
        ];
        Box::pin(async move {
            // A unit that is gone already fails here; the stop check after
            // it decides.
            if let Err(e) = call(&bin, &args).await {
                tracing::debug!(error = %e, "unit kill");
            }
        })
    }

    /// One property of a unit, as `systemctl show` prints it.
    async fn unit_property(&self, unit: &str, property: &str) -> Result<String, String> {
        call(
            &self.systemctl,
            &[
                "show".to_owned(),
                format!("--property={property}"),
                "--value".to_owned(),
                "--".to_owned(),
                unit.to_owned(),
            ],
        )
        .await
        .map(|v| v.trim().to_owned())
    }

    /// True when systemd reports the unit inactive, failed, or unknown,
    /// and its cgroup holds no process. A failed unit can keep processes
    /// (a stop that timed out): it counts as stopped only once its cgroup
    /// is empty.
    async fn unit_stopped(&self, unit: &str) -> Result<bool, String> {
        let state = self.unit_property(unit, "ActiveState").await?;
        if !matches!(state.as_str(), "inactive" | "failed" | "") {
            return Ok(false);
        }
        let cgroup = self.unit_property(unit, "ControlGroup").await?;
        cgroup_empty(&self.cgroup_root, &cgroup)
    }

    /// SIGKILL the unit and wait until systemd reports it stopped, at most
    /// `stop_timeout`.
    pub async fn stop_confirmed(&self, unit: &str) -> bool {
        let deadline = tokio::time::Instant::now() + self.stop_timeout;
        loop {
            self.kill_unit(unit, 9).await;
            match self.unit_stopped(unit).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => tracing::warn!(%unit, error = %e, "cannot read the unit state"),
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Free a session's claim only after its unit is confirmed stopped.
    /// Until then the claim stays, marked stuck, and a task tries again.
    async fn finish(self: Arc<Self>, unit: String) {
        if self.stop_confirmed(&unit).await {
            self.sessions.lock().expect("sessions lock").end(&unit);
            return;
        }
        tracing::warn!(%unit, timeout = ?self.stop_timeout, "the unit is not confirmed stopped; its GPUs stay unavailable and its UID stays held");
        self.sessions
            .lock()
            .expect("sessions lock")
            .mark_stuck(&unit);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(self.stop_retry).await;
                if self.stop_confirmed(&unit).await {
                    tracing::warn!(%unit, "the stuck unit is now stopped; its claim is free");
                    self.sessions.lock().expect("sessions lock").end(&unit);
                    return;
                }
                tracing::warn!(%unit, "the unit is still not confirmed stopped");
            }
        });
    }

    /// Stop the units of an earlier run of the helper: their sessions
    /// ended with it. Never another unit. Fails closed: when the list
    /// fails, or a unit is not confirmed stopped, the helper must not
    /// serve, since a leftover could still hold a GPU.
    pub async fn kill_leftovers(&self) -> Result<(), String> {
        let list = [
            "list-units",
            "--type=service",
            "--all",
            "--plain",
            "--no-legend",
            "--no-pager",
        ]
        .map(str::to_owned)
        .into_iter()
        .chain([format!("{UNIT_PREFIX}*")])
        .collect::<Vec<_>>();
        let text = call(&self.systemctl, &list)
            .await
            .map_err(|e| format!("cannot list leftover session units: {e}"))?;
        for unit in parse_units(&text) {
            tracing::warn!(%unit, "stopping a session unit that an earlier run left");
            if !self.stop_confirmed(&unit).await {
                return Err(format!(
                    "leftover session unit {unit} is not confirmed stopped after {:?}",
                    self.stop_timeout
                ));
            }
        }
        if let Ok(dir) = std::fs::read_dir(&self.ctx.state_dir) {
            for e in dir.flatten() {
                let name = e.file_name();
                if name.to_string_lossy().starts_with(UNIT_PREFIX) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(())
    }

    /// Serve one connection.
    pub async fn handle(self: Arc<Self>, stream: UnixStream) {
        let peer = stream.peer_cred().map(|c| c.uid()).ok();
        let (mut rd, mut wr) = stream.into_split();
        if peer != Some(self.agent_uid) {
            tracing::warn!(?peer, "refused a caller that is not the agent user");
            let r = Refusal::new(ErrorKind::Unauthorized, "caller is not the agent user");
            let _ = send_reply(&mut wr, &r.reply()).await;
            return;
        }
        let req = match tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut rd)).await {
            Ok(Ok(Some((Kind::Request, body)))) => serde_json::from_slice::<Request>(&body)
                .map_err(|e| Refusal::invalid(format!("bad request: {e}"))),
            _ => Err(Refusal::invalid("expected a request frame")),
        };
        let req = match req {
            Ok(r) => r,
            Err(r) => {
                let _ = send_reply(&mut wr, &r.reply()).await;
                return;
            }
        };
        let reply = match req {
            Request::Hello(h) if h.version != PROTOCOL_VERSION => Err(Refusal::new(
                ErrorKind::Unsupported,
                format!(
                    "protocol version {} is not the helper's {PROTOCOL_VERSION}",
                    h.version
                ),
            )),
            Request::Hello(_) => Ok(Reply::Hello {
                version: PROTOCOL_VERSION,
                rights: self.rights.clone(),
            }),
            Request::Apply(a) => self.apply(&a).await.map(|()| Reply::Ok),
            Request::Stop(s) => self.stop(&s).await.map(|()| Reply::Ok),
            Request::Start(s) => {
                self.start(s, rd, wr).await;
                return;
            }
        };
        let reply = reply.unwrap_or_else(|r| r.reply());
        let _ = send_reply(&mut wr, &reply).await;
    }

    async fn apply(&self, a: &ApplyRequest) -> Result<(), Refusal> {
        self.allowed(Right::Session)?;
        let slice = holder_slice(&a.target, &a.holder, false)?;
        tracing::info!(%slice, "apply limits");
        self.systemctl(slice_args(&slice, &a.limits)?)
            .await
            .map(|_| ())
    }

    async fn stop(&self, s: &SliceRequest) -> Result<(), Refusal> {
        self.allowed(Right::Session)?;
        let slice = holder_slice(&s.target, &s.holder, true)?;
        tracing::info!(%slice, "stop holder");
        self.systemctl(vec!["stop".into(), "--".into(), slice])
            .await
            .map(|_| ())
    }

    async fn start(
        self: Arc<Self>,
        req: StartRequest,
        rd: tokio::net::unix::OwnedReadHalf,
        mut wr: tokio::net::unix::OwnedWriteHalf,
    ) {
        let plan = match self.admit(&req).await {
            Ok(p) => p,
            Err(r) => {
                tracing::info!(session = req.session, holder = %req.holder, error = %r, "refused a session");
                let _ = send_reply(&mut wr, &r.reply()).await;
                return;
            }
        };
        let exit = self.clone().run(&req, &plan, rd, wr).await;
        let _ = std::fs::remove_file(&plan.env_file);
        tracing::info!(unit = %plan.unit, ?exit, "session ended");
        self.finish(plan.unit.clone()).await;
    }

    /// Plan, claim the GPUs, write the environment file, and set the slice
    /// limits. On a failure after the claim, the claim goes.
    async fn admit(&self, req: &StartRequest) -> Result<Plan, Refusal> {
        self.allowed(Right::Session)?;
        let inventory = if req.gpus.is_empty() {
            vec![]
        } else {
            if let Err(e) = &self.gpu {
                return Err(Refusal::new(
                    ErrorKind::Unsupported,
                    format!("GPU sessions are off on this helper: {e}"),
                ));
            }
            self.inventory().await?
        };
        #[allow(clippy::cast_possible_truncation)]
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or_default()
            ^ std::process::id();
        let plan = {
            let mut sessions = self.sessions.lock().expect("sessions lock");
            let workload = sessions.pick(&req.holder, &self.pool)?;
            self.checker
                .check(&workload)
                .map_err(|e| Refusal::new(ErrorKind::Unsupported, e))?;
            let plan = plan_start(
                &self.ctx,
                req,
                &inventory,
                self.nodes.as_ref(),
                nonce,
                &workload,
            )?;
            sessions.admit(&plan.unit, plan.claim.clone())?;
            plan
        };
        let ready = async {
            write_private(&plan.env_file, plan.env.as_bytes())
                .map_err(|e| Refusal::new(ErrorKind::Failed, format!("environment file: {e}")))?;
            if let Some(args) = &plan.slice_args {
                self.systemctl(args.clone()).await?;
            }
            Ok::<(), Refusal>(())
        }
        .await;
        if let Err(r) = ready {
            let _ = std::fs::remove_file(&plan.env_file);
            self.sessions.lock().expect("sessions lock").end(&plan.unit);
            return Err(r);
        }
        tracing::info!(unit = %plan.unit, slice = %plan.slice, holder = %req.holder, uid = plan.claim.uid, lease = ?req.lease, gpus = ?req.gpus, "session start");
        Ok(plan)
    }

    /// Start `systemd-run` and relay frames until it ends. A caller that
    /// goes away kills the unit.
    async fn run(
        self: Arc<Self>,
        req: &StartRequest,
        plan: &Plan,
        mut rd: tokio::net::unix::OwnedReadHalf,
        wr: tokio::net::unix::OwnedWriteHalf,
    ) -> Exit {
        let (out_tx, out_rx) = mpsc::channel::<(Kind, Vec<u8>)>(64);
        let writer = tokio::spawn(write_all_frames(wr, out_rx));
        // `started` goes first: output frames come only after it.
        let started = Reply::Started {
            unit: plan.unit.clone(),
        };
        let _ = out_tx
            .send((
                Kind::Reply,
                serde_json::to_vec(&started).unwrap_or_default(),
            ))
            .await;
        let child = match spawn_child(&self.systemd_run, &plan.args, req.pty, out_tx.clone()) {
            Ok(c) => c,
            Err(e) => {
                let r = Refusal::new(ErrorKind::Failed, format!("cannot start systemd-run: {e}"));
                let _ = out_tx
                    .send((
                        Kind::Reply,
                        serde_json::to_vec(&r.reply()).unwrap_or_default(),
                    ))
                    .await;
                drop(out_tx);
                let _ = writer.await;
                return Exit::default();
            }
        };
        let Child {
            input,
            mut done,
            resize,
            output,
        } = child;
        let mut caller_gone = false;
        let exit = loop {
            tokio::select! {
                exit = &mut done => break exit.unwrap_or_default(),
                frame = read_frame(&mut rd), if !caller_gone => match frame {
                    Ok(Some((Kind::Stdin, data))) => { let _ = input.send(Some(data)); }
                    Ok(Some((Kind::StdinEof, _))) => { let _ = input.send(None); }
                    Ok(Some((Kind::Signal, data))) if data.len() == 4 => {
                        let sig = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                        if FORWARDED_SIGNALS.contains(&sig) {
                            self.kill_unit(&plan.unit, sig).await;
                        }
                    }
                    Ok(Some((Kind::Resize, data))) if data.len() == 4 => {
                        let rows = u16::from_be_bytes([data[0], data[1]]);
                        let cols = u16::from_be_bytes([data[2], data[3]]);
                        if let Some(resize) = &resize && rows > 0 && cols > 0 {
                            let _ = resize.send((rows, cols));
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        caller_gone = true;
                        self.kill_unit(&plan.unit, 9).await;
                    }
                },
            }
        };
        // The output of the last moments, then the exit.
        if let Some(output) = output {
            let _ = tokio::time::timeout(Duration::from_secs(2), output).await;
        }
        let _ = out_tx
            .send((Kind::Exit, serde_json::to_vec(&exit).unwrap_or_default()))
            .await;
        drop(out_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
        exit
    }
}

/// The unit names of `systemctl list-units --plain --no-legend` output
/// that are session units of the helper.
pub fn parse_units(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| {
            u.strip_prefix(UNIT_PREFIX)
                .and_then(|r| r.strip_suffix(".service"))
                .is_some_and(|r| r.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        })
        .map(str::to_owned)
        .collect()
}

/// Write a file that only root can read, never over an existing file.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

async fn send_reply<W: AsyncWrite + Unpin>(w: &mut W, reply: &Reply) -> std::io::Result<()> {
    write_frame(
        w,
        Kind::Reply,
        &serde_json::to_vec(reply).unwrap_or_default(),
    )
    .await
}

async fn write_all_frames(
    mut wr: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::Receiver<(Kind, Vec<u8>)>,
) {
    while let Some((kind, data)) = rx.recv().await {
        if write_frame(&mut wr, kind, &data).await.is_err() {
            // The caller went away: drain, so the senders never block.
            while rx.recv().await.is_some() {}
            return;
        }
    }
}

/// A started `systemd-run`.
struct Child {
    /// Input; `None` ends it (closes stdin, or ^D on a PTY).
    input: mpsc::UnboundedSender<Option<Vec<u8>>>,
    done: tokio::sync::oneshot::Receiver<Exit>,
    resize: Option<mpsc::UnboundedSender<(u16, u16)>>,
    /// Ends when all output is sent.
    output: Option<tokio::task::JoinHandle<()>>,
}

fn exit_of(status: std::process::ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    Exit {
        code: status.code(),
        signal: status.signal(),
    }
}

fn spawn_child(
    program: &Path,
    args: &[String],
    pty: Option<(u16, u16)>,
    out: mpsc::Sender<(Kind, Vec<u8>)>,
) -> std::io::Result<Child> {
    match pty {
        None => spawn_pipes(program, args, out),
        Some(size) => {
            spawn_pty(program, args, size, out).map_err(|e| std::io::Error::other(format!("{e:#}")))
        }
    }
}

fn spawn_pipes(
    program: &Path,
    args: &[String],
    out: mpsc::Sender<(Kind, Vec<u8>)>,
) -> std::io::Result<Child> {
    use std::process::Stdio;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()?;
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (input, mut input_rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    tokio::spawn(async move {
        while let Some(item) = input_rx.recv().await {
            match (item, stdin.as_mut()) {
                (Some(data), Some(s)) => {
                    if s.write_all(&data).await.is_err() {
                        stdin = None;
                    }
                }
                (None, _) => stdin = None,
                (Some(_), None) => {}
            }
        }
    });
    let pump = |r: Option<Box<dyn AsyncRead + Send + Unpin>>, kind: Kind| {
        let out = out.clone();
        tokio::spawn(async move {
            let Some(mut r) = r else { return };
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match r.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.send((kind, buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        })
    };
    let a = pump(
        stdout.map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>),
        Kind::Stdout,
    );
    let b = pump(
        stderr.map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>),
        Kind::Stderr,
    );
    let (done_tx, done) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let exit = child.wait().await.map(exit_of).unwrap_or_default();
        let _ = done_tx.send(exit);
    });
    let output = tokio::spawn(async move {
        let _ = a.await;
        let _ = b.await;
    });
    Ok(Child {
        input,
        done,
        resize: None,
        output: Some(output),
    })
}

fn spawn_pty(
    program: &Path,
    args: &[String],
    (rows, cols): (u16, u16),
    out: mpsc::Sender<(Kind, Vec<u8>)>,
) -> anyhow::Result<Child> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    cmd.env_clear();
    cmd.env(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    cmd.env("TERM", "xterm-256color");
    cmd.cwd("/");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let master = pair.master;
    let handle = tokio::runtime::Handle::current();
    let output = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if handle
                        .block_on(out.send((Kind::Stdout, buf[..n].to_vec())))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    let (input, mut input_rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    tokio::task::spawn_blocking(move || {
        while let Some(item) = input_rx.blocking_recv() {
            let data = item.unwrap_or_else(|| vec![0x04]);
            if writer.write_all(&data).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });
    let (resize, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    tokio::spawn(async move {
        while let Some((rows, cols)) = resize_rx.recv().await {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    });
    let (done_tx, done) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let exit = match child.wait() {
            Ok(status) => Exit {
                code: i32::try_from(status.exit_code()).ok(),
                signal: None,
            },
            Err(_) => Exit::default(),
        };
        let _ = done_tx.send(exit);
    });
    Ok(Child {
        input,
        done,
        resize: Some(resize),
        output: Some(output),
    })
}

/// Bind the socket: only the agent user may open it (mode 0600, owned by
/// that UID). A stale socket file is replaced; any other file is an error.
pub fn bind(socket: &Path, agent_uid: u32) -> std::io::Result<UnixListener> {
    match std::fs::symlink_metadata(socket) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(socket)?,
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "{} exists and is not a socket",
                socket.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o177));
    let listener = UnixListener::bind(socket);
    nix::sys::stat::umask(old);
    let listener = listener?;
    std::os::unix::fs::chown(socket, Some(agent_uid), None)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Accept connections until the listener fails.
pub async fn serve(helper: Arc<Helper>, listener: UnixListener) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(helper.clone().handle(stream));
    }
}

// ------------------------------------------------------------------ shim

/// The host agent side: where the helper is, and the shim binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeOptions {
    pub socket: PathBuf,
    /// The `terrarium-host` binary, which has the `native` subcommand.
    pub shim: PathBuf,
}

/// Ask the helper for its version and rights.
pub async fn hello(socket: &Path) -> Result<Vec<Right>, String> {
    let fut = async {
        let mut s = UnixStream::connect(socket)
            .await
            .map_err(|e| format!("cannot reach the helper at {}: {e}", socket.display()))?;
        let req = Request::Hello(HelloRequest {
            version: PROTOCOL_VERSION,
        });
        write_frame(
            &mut s,
            Kind::Request,
            &serde_json::to_vec(&req).unwrap_or_default(),
        )
        .await
        .map_err(|e| format!("helper: {e}"))?;
        match read_frame(&mut s).await {
            Ok(Some((Kind::Reply, body))) => match serde_json::from_slice::<Reply>(&body) {
                Ok(Reply::Hello { version, rights }) if version == PROTOCOL_VERSION => Ok(rights),
                Ok(Reply::Hello { version, .. }) => Err(format!(
                    "helper protocol version {version}, need {PROTOCOL_VERSION}"
                )),
                Ok(Reply::Error { message, .. }) => Err(format!("helper: {message}")),
                Ok(other) => Err(format!("helper: unexpected reply {other:?}")),
                Err(e) => Err(format!("helper: bad reply: {e}")),
            },
            Ok(_) => Err("helper: no reply".into()),
            Err(e) => Err(format!("helper: {e}")),
        }
    };
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .unwrap_or_else(|_| Err("the helper did not answer".into()))
}

/// The native enforcement capability of this host agent: the helper
/// answers with this protocol version and grants the `session` right.
pub async fn capability(socket: &Path) -> terrarium_core::protocol::NativeGpuCapability {
    match hello(socket).await {
        Ok(rights) if rights.contains(&Right::Session) => {
            terrarium_core::protocol::NativeGpuCapability {
                enforced: true,
                version: PROTOCOL_VERSION,
                reason: None,
            }
        }
        Ok(_) => terrarium_core::protocol::NativeGpuCapability {
            enforced: false,
            version: PROTOCOL_VERSION,
            reason: Some("the helper does not grant the session right".into()),
        },
        Err(e) => terrarium_core::protocol::NativeGpuCapability {
            enforced: false,
            version: PROTOCOL_VERSION,
            reason: Some(e),
        },
    }
}

/// The largest request that fits in [`REQUEST_ENV`]: Linux limits one
/// environment string (`NAME=value`) to 128 KiB (`MAX_ARG_STRLEN`).
pub const MAX_SHIM_REQUEST: usize = 128 * 1024 - REQUEST_ENV.len() - 2;

/// Turn a plan into a shim call. The request goes in [`REQUEST_ENV`]. A
/// request that does not fit is refused with its size, before a spawn
/// would fail with `E2BIG`.
pub fn shim_plan(
    opts: &NativeOptions,
    request: &Request,
    mut plan: crate::session::ExecPlan,
) -> Result<crate::session::ExecPlan, String> {
    let text = serde_json::to_string(request).map_err(|e| e.to_string())?;
    if text.len() > MAX_SHIM_REQUEST {
        return Err(format!(
            "the command and its environment are too large for a native session: {} bytes, the limit is {MAX_SHIM_REQUEST}",
            text.len()
        ));
    }
    plan.program = opts.shim.to_string_lossy().into_owned();
    plan.args = vec![
        "native".into(),
        "--socket".into(),
        opts.socket.to_string_lossy().into_owned(),
    ];
    plan.env = BTreeMap::from([(REQUEST_ENV.to_owned(), text)]);
    plan.scope = None;
    Ok(plan)
}

/// Build the start request of an `exec` plan. `cwd` defaults to
/// `work_dir`.
pub fn start_request(
    plan: &crate::session::ExecPlan,
    session: u32,
    l: Option<&crate::protocol::ExecLimits>,
    work_dir: &Path,
) -> StartRequest {
    let mut argv = vec![plan.program.clone()];
    argv.extend(plan.args.iter().cloned());
    let mut env = plan.env.clone();
    if plan.pty.is_some() {
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
    }
    let native = l.and_then(|l| l.native_gpu.clone()).unwrap_or_default();
    let limits = l.and_then(|l| {
        l.reservation.clone().or_else(|| {
            (!l.limits.is_empty()).then(|| RuntimeLimits {
                limits: l.limits,
                ..Default::default()
            })
        })
    });
    StartRequest {
        session,
        target: l.map(|l| l.target.clone()).unwrap_or_default(),
        holder: l
            .map(|l| l.holder.clone())
            .unwrap_or_else(|| "operator".into()),
        lease: native.lease,
        gpus: l
            .and_then(|l| l.reservation.as_ref())
            .map(|r| r.gpu_devices.clone())
            .unwrap_or_default(),
        exclusive_gpus: native.exclusive,
        limits,
        pty: plan.pty.map(|p| (p.rows, p.cols)),
        cwd: plan
            .cwd
            .clone()
            .unwrap_or_else(|| work_dir.to_path_buf())
            .to_string_lossy()
            .into_owned(),
        argv,
        env,
    }
}

/// `terrarium-host native`: send the request in [`REQUEST_ENV`] to the
/// helper. For `start`, relay stdio, signals, and the terminal size until
/// the session ends. Returns the exit code of the process.
///
/// With `user_slice`, a `stop` also stops the holder's slice in the agent
/// user's manager first: the holder can have sessions in both (see
/// `Client::slice_stop_plan`). It exits 0 only when both stops succeed.
pub async fn shim(socket: &Path, user_slice: bool) -> i32 {
    let Some(text) = std::env::var_os(REQUEST_ENV) else {
        eprintln!("terrarium-host native: {REQUEST_ENV} is not set");
        return 125;
    };
    let req: Request = match serde_json::from_slice(text.as_encoded_bytes()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("terrarium-host native: bad request: {e}");
            return 125;
        }
    };
    let mut user_failed = false;
    if user_slice
        && let Request::Stop(s) = &req
        && let Err(e) = crate::limits::stop_user_slice(&s.target, &s.holder).await
    {
        eprintln!("terrarium-host native: {e}");
        user_failed = true;
    }
    let pty = matches!(&req, Request::Start(s) if s.pty.is_some());
    let stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "terrarium-host native: cannot reach the helper at {}: {e}",
                socket.display()
            );
            return 125;
        }
    };
    let (mut rd, mut wr) = stream.into_split();
    if let Err(e) = write_frame(
        &mut wr,
        Kind::Request,
        &serde_json::to_vec(&req).unwrap_or_default(),
    )
    .await
    {
        eprintln!("terrarium-host native: {e}");
        return 125;
    }
    match read_frame(&mut rd).await {
        Ok(Some((Kind::Reply, body))) => match serde_json::from_slice::<Reply>(&body) {
            Ok(Reply::Ok | Reply::Hello { .. }) => return if user_failed { 125 } else { 0 },
            Ok(Reply::Started { .. }) => {}
            Ok(Reply::Error { code, message }) => {
                eprintln!("terrarium-host native: refused ({code:?}): {message}");
                return 125;
            }
            Err(e) => {
                eprintln!("terrarium-host native: bad reply: {e}");
                return 125;
            }
        },
        _ => {
            eprintln!("terrarium-host native: the helper sent no reply");
            return 125;
        }
    }
    if pty {
        // The inner PTY of the session does the line discipline.
        let _ = tokio::process::Command::new("stty")
            .args(["raw", "-echo"])
            .stdin(std::process::Stdio::inherit())
            .status()
            .await;
    }
    let (tx, rx) = mpsc::channel::<(Kind, Vec<u8>)>(64);
    let writer = tokio::spawn(async move {
        let mut rx = rx;
        while let Some((kind, data)) = rx.recv().await {
            if write_frame(&mut wr, kind, &data).await.is_err() {
                break;
            }
        }
    });
    let stdin_tx = tx.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = stdin_tx.send((Kind::StdinEof, vec![])).await;
                    break;
                }
                Ok(n) => {
                    if stdin_tx
                        .send((Kind::Stdin, buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    forward_signals(tx.clone(), pty);
    drop(tx);
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let code = loop {
        match read_frame(&mut rd).await {
            Ok(Some((Kind::Stdout, data))) => {
                let _ = stdout.write_all(&data).await;
                let _ = stdout.flush().await;
            }
            Ok(Some((Kind::Stderr, data))) => {
                let _ = stderr.write_all(&data).await;
                let _ = stderr.flush().await;
            }
            Ok(Some((Kind::Exit, body))) => {
                let exit: Exit = serde_json::from_slice(&body).unwrap_or_default();
                break match (exit.code, exit.signal) {
                    (_, Some(sig)) => 128 + sig,
                    (Some(code), None) => code,
                    (None, None) => 125,
                };
            }
            Ok(Some((Kind::Reply, body))) => {
                if let Ok(Reply::Error { message, .. }) = serde_json::from_slice::<Reply>(&body) {
                    eprintln!("terrarium-host native: {message}");
                }
                break 125;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break 125,
        }
    };
    writer.abort();
    code
}

/// Forward the signals that a session runner sends to the shim's process
/// group, and the terminal size after a `SIGWINCH`.
fn forward_signals(tx: mpsc::Sender<(Kind, Vec<u8>)>, pty: bool) {
    use tokio::signal::unix::{SignalKind, signal};
    for sig in FORWARDED_SIGNALS {
        let Ok(mut stream) = signal(SignalKind::from_raw(*sig)) else {
            continue;
        };
        let tx = tx.clone();
        let sig = *sig;
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                if tx
                    .send((Kind::Signal, sig.to_be_bytes().to_vec()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    if pty && let Ok(mut winch) = signal(SignalKind::window_change()) {
        tokio::spawn(async move {
            while winch.recv().await.is_some() {
                let Ok(out) = tokio::process::Command::new("stty")
                    .arg("size")
                    .stdin(std::process::Stdio::inherit())
                    .output()
                    .await
                else {
                    continue;
                };
                if let Some((rows, cols)) = parse_stty_size(&String::from_utf8_lossy(&out.stdout)) {
                    let mut data = rows.to_be_bytes().to_vec();
                    data.extend_from_slice(&cols.to_be_bytes());
                    if tx.send((Kind::Resize, data)).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}

/// `stty size` prints `<rows> <cols>`.
pub fn parse_stty_size(text: &str) -> Option<(u16, u16)> {
    let mut w = text.split_whitespace();
    let rows = w.next()?.parse().ok()?;
    let cols = w.next()?.parse().ok()?;
    (rows > 0 && cols > 0).then_some((rows, cols))
}

/// [`verb_plan`] of a `stop` that also stops the holder's user slice
/// (`--user-slice`, see [`shim`]).
pub fn stop_plan(
    opts: &NativeOptions,
    request: &SliceRequest,
) -> Result<crate::verb::VerbPlan, crate::verb::BuildError> {
    let mut plan = verb_plan(opts, &Request::Stop(request.clone()))?;
    plan.exec.args.push("--user-slice".into());
    Ok(plan)
}

/// Ask the helper for one `apply` or `stop`, as a plan for the session
/// runner: the shim exits 0 when the helper says ok.
pub fn verb_plan(
    opts: &NativeOptions,
    request: &Request,
) -> Result<crate::verb::VerbPlan, crate::verb::BuildError> {
    shim_plan(
        opts,
        request,
        crate::session::ExecPlan {
            timeout: Some(CALL_TIMEOUT * 2),
            ..Default::default()
        },
    )
    .map(crate::verb::VerbPlan::plain)
    .map_err(crate::verb::BuildError::bad)
}

#[cfg(test)]
mod tests;
