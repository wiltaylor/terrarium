//! The host agent configuration file and the path policy.
//!
//! The file is TOML, at `/etc/terrarium-host/config.toml` or `--config`:
//!
//! ```toml
//! daemon_url = "wss://terrarium.lan:8082"
//! token_file = "/etc/terrarium-host/token"
//! name = "lab1"
//! max_sessions = 32
//! allowed_cwd_roots = ["/home/wil/labs", "/srv/build"]
//! ```

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::{CONNECT_PATH, DEFAULT_BUFFER_LIMIT};

/// The default config path.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/terrarium-host/config.toml";

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
            buffer_limit_bytes: default_buffer_limit(),
            resume_grace_secs: default_resume_grace(),
            docker_bin: default_docker(),
            vmlab_bin: default_vmlab(),
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
        Self::parse(&text, path)
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
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "localhost".into())
    }

    pub fn policy(&self) -> PathPolicy {
        PathPolicy {
            roots: self.allowed_cwd_roots.clone(),
        }
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
allowed_cwd_roots = ["/home/wil/labs", "/srv/build"]
"#;
        let c = Config::parse(text, Path::new("config.toml")).unwrap();
        assert_eq!(c.name.as_deref(), Some("lab1"));
        assert_eq!(c.max_sessions, 16);
        assert_eq!(c.buffer_limit_bytes, DEFAULT_BUFFER_LIMIT);
        assert_eq!(c.docker_bin, "docker");
        assert_eq!(c.allowed_cwd_roots.as_ref().map(Vec::len), Some(2));
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
