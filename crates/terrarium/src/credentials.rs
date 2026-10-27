//! The stored CLI sessions of `terrarium login`: one token per daemon URL
//! in `$XDG_CONFIG_HOME/terrarium/credentials.json` (else
//! `~/.config/terrarium/`). The dir is mode 0700, the file 0600.
//!
//! ```json
//! {"http://terrarium.lan:8080": {"token": "eyJ...", "expires_at": "2027-01-25T10:00:00Z"}}
//! ```
//!
//! `TERRARIUM_TOKEN` (or `--token`) wins over a stored token. Agent mode
//! never reads the file: it talks to the runner socket.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::transport::Endpoint;

/// The file name in the config dir.
pub const FILE: &str = "credentials.json";

/// One stored session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// The config dir of the CLI: `$XDG_CONFIG_HOME/terrarium`, else
/// `$HOME/.config/terrarium`.
pub fn config_dir() -> Option<PathBuf> {
    let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    non_empty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("terrarium"))
}

/// The key of a daemon in the file: its URL as the CLI shows it.
pub fn key(endpoint: &Endpoint) -> Option<String> {
    match endpoint {
        Endpoint::Tcp { .. } => Some(endpoint.to_string()),
        Endpoint::Unix(_) => None,
    }
}

/// The credentials file in one dir.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The store in [`config_dir`], if there is a home.
    pub fn default_location() -> Option<Self> {
        config_dir().map(Self::new)
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    /// Every stored session. No file: none.
    pub fn load(&self) -> anyhow::Result<BTreeMap<String, Entry>> {
        let path = self.path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn get(&self, key: &str) -> anyhow::Result<Option<Entry>> {
        Ok(self.load()?.remove(key))
    }

    /// Write every session: the dir with mode 0700, then a temp file with
    /// mode 0600 and a rename.
    fn save(&self, all: &BTreeMap<String, Entry>) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot set the mode of {}", self.dir.display()))?;
        let path = self.path();
        let tmp = self.dir.join(format!("{FILE}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        f.write_all(&serde_json::to_vec_pretty(all)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    pub fn put(&self, key: &str, entry: Entry) -> anyhow::Result<()> {
        let mut all = self.load()?;
        all.insert(key.to_owned(), entry);
        self.save(&all)
    }

    /// Remove the session of `key`. Answers it, if there was one.
    pub fn remove(&self, key: &str) -> anyhow::Result<Option<Entry>> {
        let mut all = self.load()?;
        let gone = all.remove(key);
        if gone.is_some() {
            self.save(&all)?;
        }
        Ok(gone)
    }
}

/// Where the bearer token of a request comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenSource {
    /// No token: the Unix socket, or a URL with no token.
    #[default]
    None,
    /// `TERRARIUM_TOKEN` or `--token`.
    Env,
    /// The stored session of `terrarium login`.
    Stored,
}

/// The token for `endpoint`: the env (`--token`, `TERRARIUM_TOKEN`) first,
/// else the stored session of the URL. The Unix socket needs none.
pub fn resolve(
    env_token: Option<String>,
    endpoint: &Endpoint,
    store: Option<&Store>,
) -> (Option<String>, TokenSource) {
    if let Some(t) = env_token.filter(|t| !t.is_empty()) {
        return (Some(t), TokenSource::Env);
    }
    let stored = key(endpoint)
        .zip(store)
        .and_then(|(k, s)| s.get(&k).ok().flatten());
    match stored {
        Some(e) => (Some(e.token), TokenSource::Stored),
        None => (None, TokenSource::None),
    }
}

/// The hint for a `401` with a stored session.
pub const EXPIRED_HINT: &str = "session expired: run terrarium login";

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn tcp(url: &str) -> Endpoint {
        Endpoint::from_url(url).unwrap()
    }

    fn entry(t: &str) -> Entry {
        Entry {
            token: t.into(),
            expires_at: Some("2027-01-01T00:00:00Z".into()),
        }
    }

    #[test]
    fn put_get_remove_and_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("terrarium");
        let s = Store::new(&dir);
        assert!(s.load().unwrap().is_empty());
        s.put("http://a:8080", entry("t1")).unwrap();
        s.put("http://b:8080", entry("t2")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&s.path()), 0o600);
        assert_eq!(s.get("http://a:8080").unwrap(), Some(entry("t1")));
        assert_eq!(s.remove("http://a:8080").unwrap(), Some(entry("t1")));
        assert_eq!(s.remove("http://a:8080").unwrap(), None);
        assert_eq!(s.get("http://b:8080").unwrap(), Some(entry("t2")));
        assert_eq!(mode(&s.path()), 0o600);
        // A dir that was too open is closed again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        s.put("http://c:8080", entry("t3")).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn env_wins_over_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path());
        let url = tcp("http://localhost:8080/");
        let k = key(&url).unwrap();
        assert_eq!(k, "http://localhost:8080");
        assert_eq!(resolve(None, &url, Some(&s)), (None, TokenSource::None));
        s.put(&k, entry("stored")).unwrap();
        assert_eq!(
            resolve(None, &url, Some(&s)),
            (Some("stored".into()), TokenSource::Stored)
        );
        assert_eq!(
            resolve(Some("env".into()), &url, Some(&s)),
            (Some("env".into()), TokenSource::Env)
        );
        // An empty env value does not count.
        assert_eq!(
            resolve(Some(String::new()), &url, Some(&s)).1,
            TokenSource::Stored
        );
        // Another URL, the Unix socket: no stored token.
        assert_eq!(
            resolve(None, &tcp("http://other:8080"), Some(&s)).1,
            TokenSource::None
        );
        assert_eq!(
            resolve(None, &Endpoint::Unix("/x.sock".into()), Some(&s)).1,
            TokenSource::None
        );
    }
}
