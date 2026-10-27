//! The credential store (PLAN §9, §21 "Git credentials reach an agent").
//!
//! A credential is a git token or an SSH private key that the daemon uses
//! for clones and fetches, and that it hands to git through the credential
//! helper for pushes. Layout:
//!
//! | Path | Mode | Holds |
//! |---|---|---|
//! | `<state>/credentials/` | 0700 | the store |
//! | `<state>/credentials/<name>` | 0600 | the secret, raw |
//! | `<state>/credentials/<name>.json` | 0600 | `{kind, username?, created_at}`, no secret |
//! | `<state>/credentials/.run/<ulid>/` | 0700 | one-shot helper files of one git run, removed after it |
//!
//! The same directory also holds `subscriptions/` (stored API keys,
//! `crate::login_session`), `accounts/` and `backup/` (the plan accounts,
//! `crate::accounts`). A credential cannot take one of those names
//! ([`RESERVED_NAMES`]).
//!
//! The API is write-only: no call returns a secret, except the credential
//! helper answer of `POST /api/repos/credential`, which goes to git inside
//! the agent's own sandbox for a repo of its own project. The sandbox never
//! binds this store. It binds only `accounts/<subscription>/store` of the
//! plan accounts in the agent's pool (`crate::accounts`, PLAN §16.3).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::store::{check_name, repos};

/// The user name git sends with a token when the credential has none.
/// GitHub, GitLab, and Gitea accept any user name with a token.
pub const DEFAULT_TOKEN_USER: &str = "x-access-token";

const RUN_DIR: &str = ".run";

/// The directories of other stores in `<state>/credentials/`: the stored
/// API keys (`subscriptions/`), the plan accounts, and the migration
/// backups (`crate::accounts`). No credential takes their name.
pub const RESERVED_NAMES: [&str; 3] = [
    "subscriptions",
    crate::accounts::ACCOUNTS_DIR,
    crate::accounts::BACKUP_DIR,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// An HTTPS token (a personal access token or a deploy token).
    Token,
    /// An SSH private key, OpenSSH or PEM format.
    SshKey,
    /// A password for a `vnc` or `rdp` target (docs/api.md §17.10). The
    /// daemon sends it only to the target's host agent, in the `display`
    /// frame.
    Password,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::SshKey => "ssh_key",
            Self::Password => "password",
        }
    }
}

/// What the API shows of a credential: never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialInfo {
    pub name: String,
    pub kind: CredentialKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub created_at: String,
}

/// The body of `POST /api/credentials`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCredential {
    pub name: String,
    pub kind: CredentialKind,
    pub value: String,
    /// The user name for a token. The default is `x-access-token`.
    #[serde(default)]
    pub username: Option<String>,
}

/// A secret, loaded for one git run. `Debug` does not print it.
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    Token {
        username: String,
        token: String,
    },
    SshKey(String),
    /// A `vnc` or `rdp` password, with the default user name.
    Password {
        username: Option<String>,
        password: String,
    },
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token { username, .. } => write!(f, "Token({username}, ***)"),
            Self::SshKey(_) => write!(f, "SshKey(***)"),
            Self::Password { username, .. } => write!(f, "Password({username:?}, ***)"),
        }
    }
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> ApiError {
    tracing::error!(path = %path.display(), "credentials: {what}: {e}");
    ApiError::internal(format!("cannot {what} {}: {e}", path.display()))
}

/// Write `bytes` to `path` with mode 0600, through a temp file and a rename.
fn write_private(path: &Path, bytes: &[u8]) -> ApiResult<()> {
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_err("write", path, e));
    }
    Ok(())
}

/// A directory that holds the one-shot helper files of one git run. Mode
/// 0700, inside the 0700 store; removed on drop.
#[derive(Debug)]
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write a 0600 file in the run dir.
    pub fn write_private(&self, name: &str, bytes: &[u8]) -> ApiResult<PathBuf> {
        let p = self.path.join(name);
        write_private(&p, bytes)?;
        Ok(p)
    }

    /// Write a 0700 script in the run dir.
    pub fn write_script(&self, name: &str, text: &str) -> ApiResult<PathBuf> {
        let p = self.write_private(name, text.as_bytes())?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("set the mode of", &p, e))?;
        Ok(p)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.path) {
            tracing::warn!(path = %self.path.display(), "cannot remove a credential run dir: {e}");
        }
    }
}

/// The store under `<state>/credentials`.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    dir: PathBuf,
}

impl CredentialStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("credentials"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Make the store dir with mode 0700, and set the mode if it exists.
    fn ensure_dir(&self) -> ApiResult<()> {
        fs::create_dir_all(&self.dir).map_err(|e| io_err("create", &self.dir, e))?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("set the mode of", &self.dir, e))
    }

    fn secret_path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    /// Store a credential. An existing one with the same name is replaced.
    pub fn put(&self, c: &NewCredential) -> ApiResult<CredentialInfo> {
        check_name("credential", &c.name)?;
        if RESERVED_NAMES.contains(&c.name.as_str()) {
            return Err(ApiError::invalid(format!(
                "`{}` is the name of a directory in the store; pick another name",
                c.name
            )));
        }
        let mut value = c.value.clone();
        let username = match c.kind {
            CredentialKind::Token => {
                let token = value.trim();
                if token.is_empty() {
                    return Err(ApiError::invalid("the token is empty"));
                }
                // A newline would inject lines into the git credential
                // protocol.
                if token.contains(['\n', '\r', '\0']) {
                    return Err(ApiError::invalid("a token is one line"));
                }
                value = token.to_owned();
                let user = c
                    .username
                    .clone()
                    .unwrap_or_else(|| DEFAULT_TOKEN_USER.to_owned());
                if user.is_empty() || user.contains(['\n', '\r', '\0']) {
                    return Err(ApiError::invalid("bad username"));
                }
                Some(user)
            }
            CredentialKind::SshKey => {
                if c.username.is_some() {
                    return Err(ApiError::invalid(
                        "an ssh_key has no username; put the user in the repo URL",
                    ));
                }
                if !value.contains("PRIVATE KEY-----") {
                    return Err(ApiError::invalid(
                        "an ssh_key value is a private key in OpenSSH or PEM format",
                    ));
                }
                // ssh refuses a key file without a final newline.
                if !value.ends_with('\n') {
                    value.push('\n');
                }
                None
            }
            CredentialKind::Password => {
                // Keep the password as it is: spaces can be part of it.
                if value.is_empty() || value.len() > 1024 {
                    return Err(ApiError::invalid("a password is 1 to 1024 bytes"));
                }
                if value.contains(['\n', '\r', '\0']) {
                    return Err(ApiError::invalid("a password is one line"));
                }
                match &c.username {
                    Some(u)
                        if u.is_empty() || u.len() > 256 || u.chars().any(|ch| ch.is_control()) =>
                    {
                        return Err(ApiError::invalid("bad username"));
                    }
                    u => u.clone(),
                }
            }
        };
        self.ensure_dir()?;
        let info = CredentialInfo {
            name: c.name.clone(),
            kind: c.kind,
            username,
            created_at: crate::db::now(),
        };
        write_private(&self.secret_path(&c.name), value.as_bytes())?;
        write_private(&self.meta_path(&c.name), &serde_json::to_vec(&info)?)?;
        Ok(info)
    }

    /// The credentials by name, without secrets.
    pub fn list(&self) -> ApiResult<Vec<CredentialInfo>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err("read", &self.dir, e)),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if let Some(info) = self.info(stem)? {
                out.push(info);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// The metadata of one credential.
    pub fn info(&self, name: &str) -> ApiResult<Option<CredentialInfo>> {
        if !crate::store::valid_name(name) {
            return Ok(None);
        }
        let path = self.meta_path(name);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("read", &path, e)),
        };
        if !self.secret_path(name).is_file() {
            return Ok(None);
        }
        Ok(serde_json::from_slice(&bytes).ok())
    }

    /// Delete a credential. False when it does not exist.
    pub fn delete(&self, name: &str) -> ApiResult<bool> {
        if self.info(name)?.is_none() {
            return Ok(false);
        }
        for p in [self.secret_path(name), self.meta_path(name)] {
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err("remove", &p, e)),
            }
        }
        Ok(true)
    }

    /// Load the secret of a credential. Daemon use only: for a git run, or
    /// for the credential helper answer.
    pub fn load(&self, name: &str) -> ApiResult<Option<Secret>> {
        let Some(info) = self.info(name)? else {
            return Ok(None);
        };
        let path = self.secret_path(name);
        let value = fs::read_to_string(&path).map_err(|e| io_err("read", &path, e))?;
        Ok(Some(match info.kind {
            CredentialKind::Token => Secret::Token {
                username: info
                    .username
                    .unwrap_or_else(|| DEFAULT_TOKEN_USER.to_owned()),
                token: value.trim().to_owned(),
            },
            CredentialKind::SshKey => Secret::SshKey(value),
            CredentialKind::Password => Secret::Password {
                username: info.username,
                password: value,
            },
        }))
    }

    /// A fresh 0700 dir for the one-shot files of one git run.
    pub fn run_dir(&self) -> ApiResult<RunDir> {
        self.ensure_dir()?;
        let base = self.dir.join(RUN_DIR);
        fs::create_dir_all(&base).map_err(|e| io_err("create", &base, e))?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("set the mode of", &base, e))?;
        let path = base.join(ulid::Ulid::new().to_string());
        fs::create_dir(&path).map_err(|e| io_err("create", &path, e))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("set the mode of", &path, e))?;
        Ok(RunDir { path })
    }
}

/// The credential of a repo: the `credential` key of the repo (from the
/// `[[repos]]` entry of the project definition, or the API attach), and its
/// secret. `None` when the repo names no credential. A name with no secret
/// in the store is `invalid`, so the operator learns that the file is gone.
pub fn credential_for_repo(
    conn: &Connection,
    store: &CredentialStore,
    project: &str,
    repo: &str,
) -> ApiResult<Option<(String, Secret)>> {
    let r = repos::find_by_name(conn, project, repo)?
        .ok_or_else(|| ApiError::not_found(format!("no repo `{project}/{repo}`")))?;
    secret_of(store, r.credential.as_deref())
}

/// The secret for an optional credential name.
pub fn secret_of(
    store: &CredentialStore,
    name: Option<&str>,
) -> ApiResult<Option<(String, Secret)>> {
    let Some(name) = name else {
        return Ok(None);
    };
    match store.load(name)? {
        Some(s) => Ok(Some((name.to_owned(), s))),
        None => Err(ApiError::invalid(format!(
            "credential `{name}` is not in the store; add it with POST /api/credentials"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn token(name: &str, value: &str) -> NewCredential {
        NewCredential {
            name: name.into(),
            kind: CredentialKind::Token,
            value: value.into(),
            username: None,
        }
    }

    #[test]
    fn put_sets_private_modes_and_list_hides_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path());
        let info = store.put(&token("github-deploy", " ghp_secret\n")).unwrap();
        assert_eq!(info.kind, CredentialKind::Token);
        assert_eq!(info.username.as_deref(), Some(DEFAULT_TOKEN_USER));
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&store.dir().join("github-deploy")), 0o600);
        assert_eq!(mode(&store.dir().join("github-deploy.json")), 0o600);
        let meta = fs::read_to_string(store.dir().join("github-deploy.json")).unwrap();
        assert!(!meta.contains("ghp_secret"));

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        let json = serde_json::to_string(&list).unwrap();
        assert!(!json.contains("ghp_secret"), "{json}");

        assert_eq!(
            store.load("github-deploy").unwrap(),
            Some(Secret::Token {
                username: DEFAULT_TOKEN_USER.into(),
                token: "ghp_secret".into()
            })
        );
        assert!(!format!("{:?}", store.load("github-deploy").unwrap()).contains("ghp_secret"));
        assert!(store.delete("github-deploy").unwrap());
        assert!(!store.delete("github-deploy").unwrap());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_password_keeps_its_spaces_and_never_shows() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path());
        let pw = |value: &str, username: Option<&str>| NewCredential {
            name: "vnc".into(),
            kind: CredentialKind::Password,
            value: value.into(),
            username: username.map(Into::into),
        };
        let info = store.put(&pw(" pass word ", Some("Bob Smith"))).unwrap();
        assert_eq!(info.kind, CredentialKind::Password);
        assert_eq!(info.username.as_deref(), Some("Bob Smith"));
        let list = serde_json::to_string(&store.list().unwrap()).unwrap();
        assert!(!list.contains("pass word"), "{list}");
        let s = store.load("vnc").unwrap().unwrap();
        assert_eq!(
            s,
            Secret::Password {
                username: Some("Bob Smith".into()),
                password: " pass word ".into()
            }
        );
        assert!(!format!("{s:?}").contains("pass word"));
        assert!(store.put(&pw("", None)).is_err());
        assert!(store.put(&pw("a\nb", None)).is_err());
        assert!(store.put(&pw("ok", Some(""))).is_err());
    }

    #[test]
    fn put_refuses_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path());
        assert!(store.put(&token("x", "a\nb")).is_err());
        assert!(store.put(&token("x", "  ")).is_err());
        assert!(store.put(&token("../x", "a")).is_err());
        let key = NewCredential {
            name: "k".into(),
            kind: CredentialKind::SshKey,
            value: "not a key".into(),
            username: None,
        };
        assert!(store.put(&key).is_err());
        let key = NewCredential {
            value: "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----"
                .into(),
            ..key
        };
        store.put(&key).unwrap();
        match store.load("k").unwrap() {
            Some(Secret::SshKey(k)) => assert!(k.ends_with("-----\n")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn run_dirs_are_private_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path());
        let path = {
            let run = store.run_dir().unwrap();
            assert_eq!(mode(run.path()), 0o700);
            let f = run.write_private("answer", b"x").unwrap();
            assert_eq!(mode(&f), 0o600);
            run.path().to_owned()
        };
        assert!(!path.exists());
        // The run dir does not show as a credential.
        assert!(store.list().unwrap().is_empty());
    }
}
