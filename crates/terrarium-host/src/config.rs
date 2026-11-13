//! The host agent configuration file and the path policy.
//!
//! The file is TOML, at `/etc/terrarium-host/config.toml` or `--config`:
//!
//! ```toml
//! daemon_url = "wss://terrarium.lan:8082"
//! token_file = "/etc/terrarium-host/token"
//! name = "lab1"
//! max_sessions = 32
//! allowed_cwd_roots = ["/home/alice/labs", "/srv/build"]
//! # server_fingerprint_sha256 = "AB:CD:..."   # pin a self-signed daemon
//! # allow_insecure = false                    # ws:// to a remote host
//! ```

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::{CONNECT_PATH, DEFAULT_BUFFER_LIMIT};

/// The default config path.
#[cfg(unix)]
pub const DEFAULT_CONFIG_PATH: &str = "/etc/terrarium-host/config.toml";
#[cfg(windows)]
pub const DEFAULT_CONFIG_PATH: &str = r"C:\ProgramData\Terrarium\host\config.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `ws://`, `wss://`, `http://`, or `https://`. The path defaults to
    /// `/hosts/connect`.
    #[serde(default)]
    pub daemon_url: Option<String>,
    /// A file that holds the token. Preferred over `token`.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// The token itself.
    #[serde(default)]
    pub token: Option<String>,
    /// The host name. The default is the machine's hostname.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    /// When set, every cwd and file path must be under one of these roots.
    #[serde(default)]
    pub allowed_cwd_roots: Option<Vec<PathBuf>>,
    /// A PEM file of CA certificates to trust for `wss://`, in place of the
    /// system roots.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Pin the daemon's certificate by its SHA-256 (`just host::cert`
    /// prints it). For a daemon with a self-signed certificate.
    #[serde(default)]
    pub server_fingerprint_sha256: Option<String>,
    /// Allow plaintext `ws://` to a host that is not loopback. The token
    /// and all exec traffic then cross the network in clear text.
    #[serde(default)]
    pub allow_insecure: bool,
    /// The replay buffer limit for each session.
    #[serde(default = "default_buffer_limit")]
    pub buffer_limit_bytes: usize,
    /// How long a session lives with no daemon connection.
    #[serde(default = "default_resume_grace")]
    pub resume_grace_secs: u64,
    #[serde(default = "default_docker")]
    pub docker_bin: String,
    #[serde(default = "default_vmlab")]
    pub vmlab_bin: String,
    #[serde(default = "default_git")]
    pub git_bin: String,
    /// The host path of the daemon's work root (`TERRARIUM_WORK_DIR`): the
    /// agent workspaces are `<work_dir>/<project>/<agent>`. On the Docker
    /// host of the Terrarium container, it is the source of the `./work`
    /// bind mount. Default: `./work` in the host agent's working directory.
    #[serde(default)]
    pub work_dir: Option<PathBuf>,
    /// Where `vmlab.prepare` makes lab instances. Default:
    /// `$XDG_DATA_HOME/terrarium-host/instances` (`~/.local/share/...`).
    #[serde(default)]
    pub instances_dir: Option<PathBuf>,
    /// The local bare repos for a `lab_repo` with a local URL. Default:
    /// `./repos` in the host agent's working directory.
    #[serde(default)]
    pub repos_dir: Option<PathBuf>,
    /// The most docker and vmlab targets that may run on this host at one
    /// time. The daemon enforces it (`429 rate_limited`).
    #[serde(default = "default_max_running_targets")]
    pub max_running_targets: u32,
    /// Replace this binary with the daemon's build when the daemon offers
    /// it (docs/host-protocol.md, "Self-update"). Default: true.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// Maximum seconds to close admission for one staged update attempt.
    #[serde(default = "default_auto_update_drain_secs")]
    pub auto_update_drain_secs: u64,
    /// Target displays (`[display]`): the opt-in for `desktop` targets.
    #[serde(default)]
    pub display: DisplayConfig,
    /// `[native_gpu]`: native GPU enforcement through the root helper
    /// (TER-345, docs/native-gpu-helper.md). Off by default.
    #[serde(default)]
    pub native_gpu: NativeGpuConfig,
    /// Windows password logon and elevation selection, configured only here.
    #[serde(default)]
    pub windows_run_as: crate::windows_run_as::Policy,
    /// Windows account workspaces (TER-274), configured only here.
    #[serde(default)]
    pub windows_workspaces: crate::windows_workspace::Policy,
}

/// `[native_gpu]` in the config. When it is on, every native `exec` that
/// the daemon marks for it runs through the root helper at `socket`, as
/// the helper's workload user, in a device policy with only its granted
/// GPUs. Only this file turns it on; the daemon cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeGpuConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_helper_socket")]
    pub socket: PathBuf,
}

impl Default for NativeGpuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket: default_helper_socket(),
        }
    }
}

fn default_helper_socket() -> PathBuf {
    crate::native::DEFAULT_SOCKET.into()
}

/// `[display]` in the config (PLAN §19b). It opens the desktop of this
/// machine to `desktop` targets: an agent with an exclusive lease can then
/// see the screen and use the mouse and the keyboard as the host agent's
/// user. Off by default. Only this file turns it on; the daemon cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayConfig {
    /// Open the desktop of this machine to `desktop` targets.
    #[serde(default)]
    pub enabled: bool,
    /// The displays a `desktop` target may name, for example `[":0"]`.
    /// Empty: any.
    #[serde(default)]
    pub allowed_displays: Vec<String>,
    /// The display of a `desktop` target that names none. Default: `:0`
    /// (X11) or `wayland-0` (Wayland).
    #[serde(default)]
    pub default_display: Option<String>,
    /// `XAUTHORITY` for the X tools, for example `/home/alice/.Xauthority`.
    /// A systemd service does not have the desktop session's environment.
    #[serde(default)]
    pub xauthority: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR` for the Wayland tools, for example `/run/user/1000`.
    #[serde(default)]
    pub runtime_dir: Option<PathBuf>,
    /// The `tesseract` binary, for OCR on the backends that have none.
    #[serde(default = "default_tesseract")]
    pub tesseract_bin: String,
    /// The pinned TLS certificates of RDP targets (trust on first use):
    /// one `<address> <sha256>` line for each server. Default:
    /// `$XDG_DATA_HOME/terrarium-host/rdp-known-hosts`
    /// (`~/.local/share/...`). `terrarium-host rdp-forget <address>`
    /// removes a pin.
    #[serde(default)]
    pub rdp_known_hosts: Option<PathBuf>,
}

impl DisplayConfig {
    /// `rdp_known_hosts`, or its default, as an absolute path.
    pub fn rdp_known_hosts_path(&self) -> PathBuf {
        absolute(
            self.rdp_known_hosts
                .clone()
                .unwrap_or_else(|| data_dir().join("terrarium-host").join("rdp-known-hosts")),
        )
    }
}

fn default_tesseract() -> String {
    "tesseract".into()
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_displays: Vec::new(),
            default_display: None,
            xauthority: None,
            runtime_dir: None,
            tesseract_bin: default_tesseract(),
            rdp_known_hosts: None,
        }
    }
}

/// The default of `max_running_targets`.
pub const DEFAULT_MAX_RUNNING_TARGETS: u32 = 4;

fn default_max_running_targets() -> u32 {
    DEFAULT_MAX_RUNNING_TARGETS
}
fn default_auto_update_drain_secs() -> u64 {
    600
}

fn default_true() -> bool {
    true
}
fn default_git() -> String {
    "git".into()
}

/// The directories of the host agent, as absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPaths {
    pub work_dir: PathBuf,
    pub instances_dir: PathBuf,
    pub repos_dir: PathBuf,
}

impl Default for HostPaths {
    fn default() -> Self {
        Self::resolve(None, None, None)
    }
}

/// `$XDG_DATA_HOME`, else `~/.local/share`, else `/var/lib`.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    return PathBuf::from(r"C:\ProgramData\Terrarium");
    #[cfg(unix)]
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/var/lib"))
}

/// The private dir of the `docker --env-file` files (TER-490):
/// `<data_dir>/terrarium-host/env`, mode 0700. With neither `XDG_DATA_HOME`
/// nor `HOME`, [`data_dir`] is `/var/lib`, which a user agent cannot write:
/// then `<temp_dir>/terrarium-host-<euid>/env`. The host agent checks the
/// dir and deletes the leftover files in it at the start.
#[cfg(unix)]
pub fn default_env_dir() -> PathBuf {
    let has_home = ["XDG_DATA_HOME", "HOME"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
    let base = if has_home {
        data_dir().join("terrarium-host")
    } else {
        #[cfg(unix)]
        let suffix = nix::unistd::geteuid().to_string();
        #[cfg(windows)]
        let suffix = "system".to_owned();
        std::env::temp_dir().join(format!("terrarium-host-{suffix}"))
    };
    absolute(base.join("env"))
}

#[cfg(windows)]
pub fn default_env_dir() -> PathBuf {
    data_dir().join("host").join("env")
}

fn absolute(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
    }
}

impl HostPaths {
    /// The configured values, or the defaults, made absolute against the
    /// working directory.
    pub fn resolve(
        work_dir: Option<&Path>,
        instances_dir: Option<&Path>,
        repos_dir: Option<&Path>,
    ) -> Self {
        let data = data_dir();
        Self {
            work_dir: absolute(work_dir.map_or_else(|| PathBuf::from("work"), Path::to_path_buf)),
            instances_dir: absolute(instances_dir.map_or_else(
                || data.join("terrarium-host").join("instances"),
                Path::to_path_buf,
            )),
            repos_dir: absolute(
                repos_dir.map_or_else(|| PathBuf::from("repos"), Path::to_path_buf),
            ),
        }
    }
}

fn default_max_sessions() -> u32 {
    32
}
fn default_buffer_limit() -> usize {
    DEFAULT_BUFFER_LIMIT
}
fn default_resume_grace() -> u64 {
    600
}
fn default_docker() -> String {
    "docker".into()
}
fn default_vmlab() -> String {
    "vmlab".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon_url: None,
            token_file: None,
            token: None,
            name: None,
            max_sessions: default_max_sessions(),
            allowed_cwd_roots: None,
            ca_file: None,
            server_fingerprint_sha256: None,
            allow_insecure: false,
            buffer_limit_bytes: default_buffer_limit(),
            resume_grace_secs: default_resume_grace(),
            docker_bin: default_docker(),
            vmlab_bin: default_vmlab(),
            git_bin: default_git(),
            work_dir: None,
            instances_dir: None,
            repos_dir: None,
            max_running_targets: default_max_running_targets(),
            auto_update: true,
            auto_update_drain_secs: default_auto_update_drain_secs(),
            display: DisplayConfig::default(),
            native_gpu: NativeGpuConfig::default(),
            windows_run_as: Default::default(),
            windows_workspaces: Default::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("{0}")]
    Invalid(String),
}

impl Config {
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Toml {
            path: path.to_owned(),
            source: Box::new(e),
        })
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        let config = Self::parse(&text, path)?;
        #[cfg(windows)]
        if config.windows_run_as.enabled || config.windows_workspaces.enabled {
            // Re-read from the verified, locked native handle, not the pathname
            // we read above. Untrusted config can never enable full-token logon.
            let protected = terrarium_winapi::read_host_policy_file(path).map_err(|source| {
                ConfigError::Io {
                    path: path.to_owned(),
                    source,
                }
            })?;
            return Self::parse(&protected, path);
        }
        Ok(config)
    }

    /// The token: `token_file` first, then `token`.
    pub fn resolve_token(&self) -> Result<String, ConfigError> {
        if let Some(path) = &self.token_file {
            let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
                path: path.clone(),
                source,
            })?;
            let t = text.trim();
            if t.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "token file {} is empty",
                    path.display()
                )));
            }
            return Ok(t.to_owned());
        }
        match self.token.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => Ok(t.to_owned()),
            _ => Err(ConfigError::Invalid(
                "no token: set token_file or token (or TERRARIUM_HOST_TOKEN)".into(),
            )),
        }
    }

    /// The host name: `name`, else the hostname.
    pub fn resolve_name(&self) -> String {
        if let Some(n) = self.name.as_deref().filter(|n| !n.trim().is_empty()) {
            return n.trim().to_owned();
        }
        #[cfg(windows)]
        return std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into());
        #[cfg(unix)]
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "localhost".into())
    }

    /// The transport to the daemon: the URL, the TLS mode, and the rustls
    /// config. Refuses `ws://` to a host that is not loopback unless
    /// `allow_insecure` is set.
    pub fn transport(&self) -> Result<crate::tls::Transport, ConfigError> {
        let raw = self.daemon_url.as_deref().ok_or_else(|| {
            ConfigError::Invalid(
                "no daemon_url: set it in the config file or with --daemon-url".into(),
            )
        })?;
        let url = connect_url(raw)?;
        let settings = crate::tls::TlsSettings {
            allow_insecure: self.allow_insecure,
            ca_file: self.ca_file.as_deref(),
            server_fingerprint_sha256: self.server_fingerprint_sha256.as_deref(),
        };
        crate::tls::transport(&url, &settings).map_err(|e| ConfigError::Invalid(e.to_string()))
    }

    /// The path policy. With `allowed_cwd_roots`, the work dir and the
    /// instances dir are roots too: the host agent makes the lab instances
    /// there, and attaches the workspaces from there.
    pub fn policy(&self) -> PathPolicy {
        let paths = self.paths();
        PathPolicy {
            roots: self.allowed_cwd_roots.clone().map(|mut r| {
                for p in [paths.work_dir, paths.instances_dir] {
                    if !r.contains(&p) {
                        r.push(p);
                    }
                }
                r
            }),
        }
    }

    /// `work_dir`, `instances_dir`, and `repos_dir`, absolute.
    pub fn paths(&self) -> HostPaths {
        HostPaths::resolve(
            self.work_dir.as_deref(),
            self.instances_dir.as_deref(),
            self.repos_dir.as_deref(),
        )
    }
}

/// Turn the configured daemon URL into the WebSocket URL to dial.
pub fn connect_url(raw: &str) -> Result<String, ConfigError> {
    let raw = raw.trim();
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| ConfigError::Invalid(format!("daemon_url {raw:?} has no scheme")))?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "ws" | "http" => "ws",
        "wss" | "https" => "wss",
        other => {
            return Err(ConfigError::Invalid(format!(
                "daemon_url scheme {other:?} is not ws, wss, http, or https"
            )));
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "daemon_url {raw:?} has no host"
        )));
    }
    let path = if path.is_empty() || path == "/" {
        CONNECT_PATH
    } else {
        path
    };
    Ok(format!("{scheme}://{authority}{path}"))
}

/// Write a secret (a token) to `path` with mode 0600, through a temporary
/// file and a rename, so a reader never sees half a token. A missing
/// parent directory is made with mode 0700.
pub fn write_secret(path: &Path, secret: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(parent)?;
        #[cfg(windows)]
        crate::session::private_dir(parent)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut f = options.open(&tmp)?;
    writeln!(f, "{}", secret.trim())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// The `allowed_cwd_roots` check. With no roots, every absolute path passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathPolicy {
    pub roots: Option<Vec<PathBuf>>,
}

impl PathPolicy {
    /// Check a path. The path must be absolute and have no `..`. The nearest
    /// existing ancestor is resolved (symlinks included), so a link cannot
    /// lead out of a root. Returns the path to use.
    pub fn check(&self, path: &Path) -> Result<PathBuf, String> {
        if !path.is_absolute() {
            return Err(format!("{} is not an absolute path", path.display()));
        }
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!("{} contains ..", path.display()));
        }
        let Some(roots) = &self.roots else {
            return Ok(path.to_owned());
        };
        let resolved = resolve_existing_prefix(path);
        for root in roots {
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&root) {
                return Ok(path.to_owned());
            }
        }
        Err(format!("{} is outside allowed_cwd_roots", path.display()))
    }
}

/// Canonicalise the longest existing prefix of `path` and append the rest.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut existing = path.to_owned();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = std::fs::canonicalize(&existing) {
            let mut out = c;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (
            existing.file_name().map(|n| n.to_owned()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_owned();
            }
            _ => return path.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let text = r#"
daemon_url = "wss://terrarium.lan:8082"
token_file = "/etc/terrarium-host/token"
name = "lab1"
max_sessions = 16
allowed_cwd_roots = ["/home/alice/labs", "/srv/build"]
"#;
        let c = Config::parse(text, Path::new("config.toml")).unwrap();
        assert_eq!(c.name.as_deref(), Some("lab1"));
        assert_eq!(c.max_sessions, 16);
        assert_eq!(c.buffer_limit_bytes, DEFAULT_BUFFER_LIMIT);
        assert_eq!(c.docker_bin, "docker");
        assert_eq!(c.allowed_cwd_roots.as_ref().map(Vec::len), Some(2));
        assert!(c.auto_update, "self-update is on by default");
        let off = Config::parse("auto_update = false", Path::new("x")).unwrap();
        assert!(!off.auto_update);
        let back = Config::parse(&toml::to_string(&c).unwrap(), Path::new("x")).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("bogus = 1", Path::new("x")).is_err());
    }

    #[test]
    fn token_file_wins_and_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("token");
        std::fs::write(&f, "  abc\n").unwrap();
        let c = Config {
            token_file: Some(f),
            token: Some("other".into()),
            ..Default::default()
        };
        assert_eq!(c.resolve_token().unwrap(), "abc");
        assert!(Config::default().resolve_token().is_err());
    }

    #[test]
    fn transport_refuses_plaintext_to_a_remote_host() {
        let mut c = Config {
            daemon_url: Some("ws://terrarium.lan:8082".into()),
            ..Default::default()
        };
        let e = c.transport().unwrap_err().to_string();
        assert!(e.contains("allow_insecure"), "{e}");
        c.allow_insecure = true;
        assert_eq!(
            c.transport().unwrap().mode,
            crate::tls::TlsMode::PlainInsecure
        );
        c.daemon_url = Some("http://localhost:8082".into());
        c.allow_insecure = false;
        assert_eq!(
            c.transport().unwrap().mode,
            crate::tls::TlsMode::PlainLoopback
        );
        let c = Config::parse(
            "daemon_url = \"wss://d:8082\"\nserver_fingerprint_sha256 = \"sha256:00\"",
            Path::new("x"),
        )
        .unwrap();
        assert!(c.transport().is_err(), "a short pin is an error");
    }

    #[test]
    fn write_secret_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/token");
        write_secret(&p, "  abc\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "abc\n");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        write_secret(&p, "def").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "def\n");
    }

    #[test]
    fn connect_urls() {
        assert_eq!(
            connect_url("ws://d:8082").unwrap(),
            "ws://d:8082/hosts/connect"
        );
        assert_eq!(
            connect_url("https://d:8082/").unwrap(),
            "wss://d:8082/hosts/connect"
        );
        assert_eq!(connect_url("ws://d/custom").unwrap(), "ws://d/custom");
        assert!(connect_url("ftp://d").is_err());
        assert!(connect_url("d:8082").is_err());
    }

    #[test]
    fn policy_limits_paths_to_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a")).unwrap();
        let policy = PathPolicy {
            roots: Some(vec![root.clone()]),
        };
        assert!(policy.check(&root.join("a")).is_ok());
        assert!(policy.check(&root.join("new/file.txt")).is_ok());
        assert!(policy.check(dir.path()).is_err());
        assert!(policy.check(&root.join("a/../../x")).is_err());
        assert!(policy.check(Path::new("relative")).is_err());
        // A symlink out of the root does not pass.
        std::os::unix::fs::symlink(dir.path(), root.join("out")).unwrap();
        assert!(policy.check(&root.join("out/file")).is_err());
        // No roots: any absolute path passes.
        assert!(PathPolicy::default().check(Path::new("/etc")).is_ok());
    }
}
