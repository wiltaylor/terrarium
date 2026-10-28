//! The secret manager (PLAN §16b). It replaces the credential files of
//! PLAN §9 and §19.
//!
//! A secret is a row of the `secrets` table: a scope (`global` or a
//! project), a name, a kind ([`Kind`]), the plain fields (`plain`, JSON),
//! the names of the secret fields (`secret_fields`, JSON), and `sealed`:
//! the XChaCha20-Poly1305 ciphertext of the JSON of the secret fields
//! ([`crypto`]). The master key is `<state>/secrets.key` (mode 0600). A
//! database backup without the key file shows no secret.
//!
//! | Path | Mode | Holds |
//! |---|---|---|
//! | `<state>/secrets.key` | 0600 | the master key, 32 bytes |
//! | `<state>/credentials/` | 0700 | the other stores, and the run dirs |
//! | `<state>/credentials/.run/<ulid>/` | 0700 | one-shot helper files of one git or ssh run, removed after it |
//!
//! `<state>/credentials/` also holds `subscriptions/` (stored API keys,
//! `crate::login_session`), `accounts/` and `backup/` (the plan accounts,
//! `crate::accounts`). They stay on disk ([`RESERVED_NAMES`]). The old
//! credential files (`<state>/credentials/<name>` and `<name>.json`) move
//! into the table at the start ([`migrate`]).
//!
//! The API never returns a secret field, except a reveal by the operator
//! (`POST /api/secrets/{scope}/{name}/reveal`) and the credential helper
//! answer of `POST /api/repos/credential`, which goes to git inside the
//! agent's own sandbox for a repo of its own project. The daemon decrypts
//! a secret only when it uses one, into zeroizing buffers. The sandbox
//! never binds the key file or the database.

pub mod crypto;
pub mod kinds;
pub mod migrate;
pub mod ssh;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

pub use self::crypto::MasterKey;
pub use self::kinds::{DEFAULT_TOKEN_USER, Kind, SecretFields, SecretInput};
use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::store::check_name;

/// The scope of a global secret. A project named `global` cannot have
/// project secrets.
pub const GLOBAL: &str = "global";

const RUN_DIR: &str = ".run";

/// The directories of other stores in `<state>/credentials/`: the stored
/// API keys (`subscriptions/`), the plan accounts, and the migration
/// backups (`crate::accounts`). The migration of the old credential files
/// leaves them as they are.
pub const RESERVED_NAMES: [&str; 3] = [
    "subscriptions",
    crate::accounts::ACCOUNTS_DIR,
    crate::accounts::BACKUP_DIR,
];

/// What the API shows of a secret: never a secret field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretInfo {
    pub id: String,
    /// `global` or a project name.
    pub scope: String,
    pub name: String,
    pub kind: Kind,
    /// The plain fields: a user name, a URL, a host, the public key.
    pub fields: Map<String, Value>,
    /// The names of the secret fields that the secret holds.
    pub secret_fields: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: String,
}

/// A secret with its secret fields open. Daemon use only. `Debug` shows no
/// value.
pub struct Opened {
    pub info: SecretInfo,
    pub values: SecretFields,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("info", &self.info)
            .field("values", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A secret, loaded for one git run, one ssh session, or one `display`
/// frame. `Debug` does not print it.
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    /// A `token` or a `github_token`: git gets it through a credential
    /// helper.
    Token {
        username: String,
        token: Zeroizing<String>,
    },
    /// An `ssh_key`, in a form that `ssh -i` reads with no prompt.
    SshKey(Zeroizing<String>),
    /// A `login`: a `vnc` or `rdp` password, with the default user name.
    Login {
        username: Option<String>,
        password: Zeroizing<String>,
    },
    /// A kind that git and the targets do not use (`api_key`, `generic`).
    Other(Kind),
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token { username, .. } => write!(f, "Token({username}, ***)"),
            Self::SshKey(_) => write!(f, "SshKey(***)"),
            Self::Login { username, .. } => write!(f, "Login({username:?}, ***)"),
            Self::Other(k) => write!(f, "Other({k})"),
        }
    }
}

impl Secret {
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Token { .. } => "token",
            Self::SshKey(_) => "ssh_key",
            Self::Login { .. } => "login",
            Self::Other(k) => k.as_str(),
        }
    }
}

impl Opened {
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(|v| v.as_str())
    }

    fn plain_str(&self, name: &str) -> Option<String> {
        self.info
            .fields
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// The form that git, ssh, and the targets use.
    pub fn to_secret(&self) -> ApiResult<Secret> {
        let missing = |f: &str| {
            ApiError::invalid(format!(
                "secret `{}/{}` has no `{f}`",
                self.info.scope, self.info.name
            ))
        };
        Ok(match self.info.kind {
            Kind::Token | Kind::GithubToken => Secret::Token {
                username: self
                    .plain_str("username")
                    .unwrap_or_else(|| DEFAULT_TOKEN_USER.to_owned()),
                token: Zeroizing::new(
                    self.value("value")
                        .ok_or_else(|| missing("value"))?
                        .to_owned(),
                ),
            },
            Kind::SshKey => Secret::SshKey(ssh::usable_key(
                self.value("private_key")
                    .ok_or_else(|| missing("private_key"))?,
                self.value("passphrase"),
            )?),
            Kind::Login => Secret::Login {
                username: self.plain_str("username"),
                password: Zeroizing::new(
                    self.value("password")
                        .ok_or_else(|| missing("password"))?
                        .to_owned(),
                ),
            },
            k => Secret::Other(k),
        })
    }
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> ApiError {
    tracing::error!(path = %path.display(), "secrets: {what}: {e}");
    ApiError::internal(format!("cannot {what} {}: {e}", path.display()))
}

/// Write `bytes` to `path` with mode 0600, through a temp file and a rename.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> ApiResult<()> {
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
        return Err(io_err("write", path, &e));
    }
    Ok(())
}

/// Make `dir` with mode 0700, and set the mode if it exists.
pub(crate) fn private_dir(dir: &Path) -> ApiResult<()> {
    fs::create_dir_all(dir).map_err(|e| io_err("create", dir, &e))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| io_err("set the mode of", dir, &e))
}

/// A directory that holds the one-shot helper files of one git or ssh run.
/// Mode 0700, inside the 0700 `credentials/`; removed on drop.
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
            .map_err(|e| io_err("set the mode of", &p, &e))?;
        Ok(p)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.path) {
            tracing::warn!(path = %self.path.display(), "cannot remove a secret run dir: {e}");
        }
    }
}

/// Where the run dirs go: `<state>/credentials/.run/`.
#[derive(Debug, Clone)]
pub struct RunDirs {
    dir: PathBuf,
}

impl RunDirs {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: credentials_dir(state_dir),
        }
    }

    /// A fresh 0700 dir for the one-shot files of one run.
    pub fn run_dir(&self) -> ApiResult<RunDir> {
        private_dir(&self.dir)?;
        let base = self.dir.join(RUN_DIR);
        private_dir(&base)?;
        let path = base.join(ulid::Ulid::new().to_string());
        fs::create_dir(&path).map_err(|e| io_err("create", &path, &e))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("set the mode of", &path, &e))?;
        Ok(RunDir { path })
    }
}

/// `<state>/credentials`.
pub fn credentials_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("credentials")
}

/// The master key of the state dir. A missing key file is an error: the
/// daemon makes it at the start ([`ensure_key`]).
pub fn key(state_dir: &Path) -> ApiResult<MasterKey> {
    let path = crypto::key_path(state_dir);
    crypto::read_key(&path)?.ok_or_else(|| {
        ApiError::internal(format!(
            "the secret key {} is missing: restore it from a backup (without it no secret opens)",
            path.display()
        ))
    })
}

/// At the start: finish a key rotation that stopped before the rename,
/// make the key file when no sealed row exists, and set its mode.
/// `Ok(true)` when a new key was made. A missing key with sealed rows is
/// an error: a new key would not open them.
pub fn ensure_key(conn: &Connection, state_dir: &Path) -> anyhow::Result<bool> {
    let path = crypto::key_path(state_dir);
    migrate::finish_rotation(conn, state_dir)?;
    if path.exists() {
        crypto::fix_mode(&path).map_err(|e| anyhow::anyhow!(e.message))?;
        key(state_dir).map_err(|e| anyhow::anyhow!(e.message))?;
        return Ok(false);
    }
    let sealed: i64 = conn.query_row(
        "SELECT count(*) FROM secrets WHERE sealed IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    if sealed > 0 {
        anyhow::bail!(
            "the secret key {} is missing, and {sealed} secret(s) need it: restore the key file \
             from a backup",
            path.display()
        );
    }
    fs::create_dir_all(state_dir)?;
    crypto::write_key(&path, &MasterKey::generate()).map_err(|e| anyhow::anyhow!(e.message))?;
    tracing::info!(path = %path.display(), "made the secret key");
    Ok(true)
}

/// Check a scope: `global`, or a project that exists.
pub fn check_scope(conn: &Connection, scope: &str) -> ApiResult<()> {
    if scope == GLOBAL {
        return Ok(());
    }
    check_name("project", scope)?;
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM projects WHERE name = ?1",
        [scope],
        |r| r.get(0),
    )?;
    if n == 0 {
        return Err(ApiError::not_found(format!("no project `{scope}`")));
    }
    Ok(())
}

/// The project of a scope: `None` for `global`.
pub fn scope_project(scope: &str) -> Option<&str> {
    (scope != GLOBAL).then_some(scope)
}

const COLS: &str =
    "id, scope, name, kind, plain, secret_fields, created_at, updated_at, created_by";

fn row_info(r: &rusqlite::Row<'_>) -> rusqlite::Result<SecretInfo> {
    let kind: String = r.get(3)?;
    let plain: String = r.get(4)?;
    let names: String = r.get(5)?;
    let kind = Kind::parse(&kind).unwrap_or(Kind::Generic);
    let mut fields = serde_json::from_str(&plain).unwrap_or_default();
    // An ssh_key stored before the `fingerprint` field (PLAN §16b.6a).
    ssh::fill_fingerprint(kind, &mut fields);
    Ok(SecretInfo {
        id: r.get(0)?,
        scope: r.get(1)?,
        name: r.get(2)?,
        kind,
        fields,
        secret_fields: serde_json::from_str(&names).unwrap_or_default(),
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        created_by: r.get(8)?,
    })
}

/// How [`put`] treats a secret that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutMode {
    /// `409 conflict` when it exists.
    Create,
    /// Replace it (the ID and `created_*` stay), or make it.
    Upsert,
}

/// The fields of a put.
pub struct NewSecret<'a> {
    pub scope: &'a str,
    pub name: &'a str,
    pub kind: Kind,
    pub input: &'a SecretInput,
    /// The actor label.
    pub by: &'a str,
    /// False only for the migration of the old files ([`kinds::split`]).
    pub strict: bool,
}

/// Store a secret. The answer is the stored secret and `true` when it is
/// new.
pub fn put(
    conn: &Connection,
    key: &MasterKey,
    mode: PutMode,
    s: &NewSecret<'_>,
) -> ApiResult<(SecretInfo, bool)> {
    check_name("secret", s.name)?;
    check_scope(conn, s.scope)?;
    let split = kinds::split(s.kind, s.input, s.strict)?;
    let names = kinds::secret_names(&split.secret);
    let now = crate::db::now();
    let existing = get(conn, s.scope, s.name)?;
    if existing.is_some() && mode == PutMode::Create {
        return Err(ApiError::conflict(format!(
            "secret `{}/{}` exists; PUT /api/secrets/{}/{} replaces it",
            s.scope, s.name, s.scope, s.name
        )));
    }
    let id = existing
        .as_ref()
        .map_or_else(|| ulid::Ulid::new().to_string(), |e| e.id.clone());
    let plain_json = Zeroizing::new(serde_json::to_vec(
        &split
            .secret
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect::<std::collections::BTreeMap<_, _>>(),
    )?);
    let sealed = key.seal(
        &plain_json,
        &crypto::aad(&id, s.scope, s.name, s.kind.as_str()),
    )?;
    let plain = Value::Object(split.plain).to_string();
    let names_json = serde_json::to_string(&names)?;
    let created = existing.is_none();
    if created {
        conn.execute(
            "INSERT INTO secrets (id, scope, name, kind, plain, secret_fields, sealed,
                                  created_at, updated_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
            params![
                id,
                s.scope,
                s.name,
                s.kind.as_str(),
                plain,
                names_json,
                sealed,
                now,
                s.by
            ],
        )?;
    } else {
        conn.execute(
            "UPDATE secrets SET kind = ?2, plain = ?3, secret_fields = ?4, sealed = ?5,
                                updated_at = ?6
             WHERE id = ?1",
            params![id, s.kind.as_str(), plain, names_json, sealed, now],
        )?;
    }
    let info = get(conn, s.scope, s.name)?
        .ok_or_else(|| ApiError::internal("the secret is gone after the write"))?;
    Ok((info, created))
}

/// The secrets of one scope, or of all scopes, by scope and name.
pub fn list(conn: &Connection, scope: Option<&str>) -> ApiResult<Vec<SecretInfo>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM secrets WHERE ?1 IS NULL OR scope = ?1
         ORDER BY scope = 'global' DESC, scope, name"
    ))?;
    let rows = stmt
        .query_map([scope], row_info)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One secret, without its secret fields.
pub fn get(conn: &Connection, scope: &str, name: &str) -> ApiResult<Option<SecretInfo>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM secrets WHERE scope = ?1 AND name = ?2"),
            [scope, name],
            row_info,
        )
        .optional()?)
}

/// Delete a secret (its grants go with it). False when it does not exist.
pub fn delete(conn: &Connection, scope: &str, name: &str) -> ApiResult<bool> {
    let n = conn.execute(
        "DELETE FROM secrets WHERE scope = ?1 AND name = ?2",
        [scope, name],
    )?;
    Ok(n > 0)
}

/// Delete each secret of a project (the project is gone).
pub fn delete_project(conn: &Connection, project: &str) -> ApiResult<usize> {
    if project == GLOBAL {
        return Ok(0);
    }
    Ok(conn.execute("DELETE FROM secrets WHERE scope = ?1", [project])?)
}

/// Open one secret of one scope.
pub fn open(
    conn: &Connection,
    key: &MasterKey,
    scope: &str,
    name: &str,
) -> ApiResult<Option<Opened>> {
    let Some(info) = get(conn, scope, name)? else {
        return Ok(None);
    };
    let sealed: Option<Vec<u8>> = conn.query_row(
        "SELECT sealed FROM secrets WHERE id = ?1",
        [&info.id],
        |r| r.get(0),
    )?;
    let Some(sealed) = sealed else {
        return Err(ApiError::invalid(format!(
            "secret `{scope}/{name}` has no value yet; set it first"
        )));
    };
    let bytes = key
        .open(
            &sealed,
            &crypto::aad(&info.id, &info.scope, &info.name, info.kind.as_str()),
        )
        .ok_or_else(|| {
            tracing::error!(scope, name, "a secret does not open with the key");
            ApiError::internal(format!(
                "secret `{scope}/{name}` does not open with the key in secrets.key"
            ))
        })?;
    let raw: std::collections::BTreeMap<String, String> = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::internal(format!("secret `{scope}/{name}` is not valid JSON")))?;
    let values = raw
        .into_iter()
        .map(|(k, v)| (k, Zeroizing::new(v)))
        .collect();
    Ok(Some(Opened { info, values }))
}

/// Find a secret by name for an owner: the project's own secret first,
/// then the global one (a project secret hides a global secret of the
/// same name). `project` is `None` for a global owner.
pub fn resolve(
    conn: &Connection,
    key: &MasterKey,
    project: Option<&str>,
    name: &str,
) -> ApiResult<Option<Opened>> {
    if let Some(p) = project.filter(|p| *p != GLOBAL)
        && let Some(o) = open(conn, key, p, name)?
    {
        return Ok(Some(o));
    }
    open(conn, key, GLOBAL, name)
}

fn not_stored(name: &str, project: Option<&str>) -> ApiError {
    let wher = match project {
        Some(p) => format!("project `{p}` or global"),
        None => "global".to_owned(),
    };
    ApiError::invalid(format!(
        "secret `{name}` is not in the store ({wher}); add it with `terrarium secret set {name}`"
    ))
}

/// Open the secret named `name` for an owner in `project` (`None`:
/// global), on a connection. A name with no secret is `invalid`.
pub fn open_named(
    conn: &Connection,
    state_dir: &Path,
    project: Option<&str>,
    name: &str,
) -> ApiResult<Opened> {
    let key = key(state_dir)?;
    resolve(conn, &key, project, name)?.ok_or_else(|| not_stored(name, project))
}

/// [`open_named`], in the form that git, ssh, and the targets use.
pub fn require(
    conn: &Connection,
    state_dir: &Path,
    project: Option<&str>,
    name: &str,
) -> ApiResult<Secret> {
    open_named(conn, state_dir, project, name)?.to_secret()
}

/// [`require`] through the daemon's database. The read runs on the
/// blocking pool; the decrypt of a key with a passphrase (bcrypt, slow)
/// runs there too, after the database lock is free.
pub async fn load(state: &Shared, project: Option<&str>, name: &str) -> ApiResult<Secret> {
    let (dir, p, n) = (
        state.state_dir.clone(),
        project.map(str::to_owned),
        name.to_owned(),
    );
    let opened = state
        .db
        .read(move |c| open_named(c, &dir, p.as_deref(), &n))
        .await?;
    tokio::task::spawn_blocking(move || opened.to_secret())
        .await
        .map_err(|e| ApiError::internal(format!("the secret task failed: {e}")))?
}

/// [`require`] through the daemon's database, on the calling thread: for
/// the sync paths (an ssh exec start, a `display` frame). The read is one
/// row by its unique key; the decrypt runs after the database lock is
/// free.
pub fn load_now(state: &Shared, project: Option<&str>, name: &str) -> ApiResult<Secret> {
    state
        .db
        .read_now(|c| open_named(c, &state.state_dir, project, name))?
        .to_secret()
}

/// The secret of an optional name: `(name, secret)`.
pub async fn secret_of(
    state: &Shared,
    project: Option<&str>,
    name: Option<&str>,
) -> ApiResult<Option<(String, Secret)>> {
    match name {
        None => Ok(None),
        Some(n) => Ok(Some((n.to_owned(), load(state, project, n).await?))),
    }
}

/// A read-only connection to the daemon's database, for the code that has
/// no `Db` handle (the config repo, which `terrariumd config` also runs
/// with no daemon).
pub fn open_db(state_dir: &Path) -> ApiResult<Connection> {
    let path = state_dir.join("terrarium.db");
    let conn = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| ApiError::internal(format!("cannot open {}: {e}", path.display())))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        c.execute(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 'x', 'x')",
            [],
        )
        .unwrap();
        c
    }

    fn input(v: Value) -> SecretInput {
        serde_json::from_value(v).unwrap()
    }

    fn put_one(
        c: &Connection,
        k: &MasterKey,
        scope: &str,
        name: &str,
        kind: Kind,
        v: Value,
    ) -> ApiResult<(SecretInfo, bool)> {
        let i = input(v);
        put(
            c,
            k,
            PutMode::Upsert,
            &NewSecret {
                scope,
                name,
                kind,
                input: &i,
                by: "operator",
                strict: true,
            },
        )
    }

    #[test]
    fn put_seals_and_lists_hide_secret_fields() {
        let c = db();
        let k = MasterKey::generate();
        let (info, created) = put_one(
            &c,
            &k,
            GLOBAL,
            "deploy",
            Kind::Token,
            json!({"fields": {"value": "ghp_s3cret", "username": "bot"}}),
        )
        .unwrap();
        assert!(created);
        assert_eq!(info.secret_fields, ["value"]);
        assert_eq!(info.fields["username"], "bot");
        assert!(!serde_json::to_string(&info).unwrap().contains("ghp_s3cret"));
        // The row holds no plain value.
        let (plain, sealed): (String, Vec<u8>) = c
            .query_row(
                "SELECT plain, sealed FROM secrets WHERE name = 'deploy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(!plain.contains("ghp_s3cret"));
        assert!(!sealed.windows(10).any(|w| w == b"ghp_s3cret"));
        let list = serde_json::to_string(&list(&c, None).unwrap()).unwrap();
        assert!(!list.contains("ghp_s3cret"), "{list}");
        // Open gives it back.
        let o = open(&c, &k, GLOBAL, "deploy").unwrap().unwrap();
        assert_eq!(o.values["value"].as_str(), "ghp_s3cret");
        assert!(!format!("{o:?}").contains("ghp_s3cret"));
        let s = o.to_secret().unwrap();
        assert!(
            matches!(&s, Secret::Token { username, token } if username == "bot" && token.as_str() == "ghp_s3cret")
        );
        assert!(!format!("{s:?}").contains("ghp_s3cret"));
        // Another key does not open it.
        assert!(open(&c, &MasterKey::generate(), GLOBAL, "deploy").is_err());
        // A replace keeps the ID; a create of the same name is a conflict.
        let (again, created) = put_one(
            &c,
            &k,
            GLOBAL,
            "deploy",
            Kind::Token,
            json!({"fields": {"value": "new"}}),
        )
        .unwrap();
        assert!(!created);
        assert_eq!(again.id, info.id);
        let i = input(json!({"fields": {"value": "x"}}));
        let e = put(
            &c,
            &k,
            PutMode::Create,
            &NewSecret {
                scope: GLOBAL,
                name: "deploy",
                kind: Kind::Token,
                input: &i,
                by: "operator",
                strict: true,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, crate::error::Code::Conflict);
        assert!(delete(&c, GLOBAL, "deploy").unwrap());
        assert!(!delete(&c, GLOBAL, "deploy").unwrap());
    }

    #[test]
    fn a_row_copied_to_another_name_does_not_open() {
        let c = db();
        let k = MasterKey::generate();
        put_one(
            &c,
            &k,
            GLOBAL,
            "a",
            Kind::ApiKey,
            json!({"fields": {"value": "sk-a"}}),
        )
        .unwrap();
        put_one(
            &c,
            &k,
            GLOBAL,
            "b",
            Kind::ApiKey,
            json!({"fields": {"value": "sk-b"}}),
        )
        .unwrap();
        c.execute(
            "UPDATE secrets SET sealed = (SELECT sealed FROM secrets WHERE name = 'a')
             WHERE name = 'b'",
            [],
        )
        .unwrap();
        assert!(open(&c, &k, GLOBAL, "b").is_err());
        assert!(open(&c, &k, GLOBAL, "a").is_ok());
    }

    #[test]
    fn a_project_secret_hides_the_global_one() {
        let c = db();
        let k = MasterKey::generate();
        put_one(
            &c,
            &k,
            GLOBAL,
            "t",
            Kind::ApiKey,
            json!({"fields": {"value": "global"}}),
        )
        .unwrap();
        put_one(
            &c,
            &k,
            "p",
            "t",
            Kind::ApiKey,
            json!({"fields": {"value": "project"}}),
        )
        .unwrap();
        let v =
            |p: Option<&str>| resolve(&c, &k, p, "t").unwrap().unwrap().values["value"].to_string();
        assert_eq!(v(Some("p")), "project");
        assert_eq!(v(Some("other")), "global");
        assert_eq!(v(None), "global");
        assert!(resolve(&c, &k, Some("p"), "none").unwrap().is_none());
        // A scope must be global or a project that exists.
        let e = put_one(
            &c,
            &k,
            "nope",
            "t",
            Kind::ApiKey,
            json!({"fields": {"value": "x"}}),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::error::Code::NotFound);
        assert_eq!(list(&c, Some("p")).unwrap().len(), 1);
        assert_eq!(delete_project(&c, "p").unwrap(), 1);
        assert_eq!(v(Some("p")), "global");
    }

    #[test]
    fn ssh_keys_store_the_derived_public_key() {
        let c = db();
        let k = MasterKey::generate();
        let key = ssh::tests::ed25519_key();
        let (info, _) = put_one(
            &c,
            &k,
            GLOBAL,
            "box",
            Kind::SshKey,
            json!({"fields": {"private_key": key}}),
        )
        .unwrap();
        assert!(
            info.fields["public_key"]
                .as_str()
                .unwrap()
                .starts_with("ssh-ed25519 ")
        );
        assert_eq!(info.secret_fields, ["private_key"]);
        let s = open(&c, &k, GLOBAL, "box")
            .unwrap()
            .unwrap()
            .to_secret()
            .unwrap();
        assert!(matches!(s, Secret::SshKey(t) if t.contains("OPENSSH PRIVATE KEY")));
        // A caller cannot set the public key.
        assert!(
            put_one(
                &c,
                &k,
                GLOBAL,
                "box",
                Kind::SshKey,
                json!({"fields": {"private_key": ssh::tests::ed25519_key(), "public_key": "x"}}),
            )
            .is_err()
        );
    }

    #[test]
    fn the_key_is_made_once_and_never_over_sealed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let c = db();
        assert!(ensure_key(&c, dir.path()).unwrap());
        let mode = fs::metadata(crypto::key_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!ensure_key(&c, dir.path()).unwrap());
        let k = key(dir.path()).unwrap();
        put_one(
            &c,
            &k,
            GLOBAL,
            "a",
            Kind::ApiKey,
            json!({"fields": {"value": "v"}}),
        )
        .unwrap();
        fs::remove_file(crypto::key_path(dir.path())).unwrap();
        assert!(ensure_key(&c, dir.path()).is_err());
        assert!(key(dir.path()).is_err());
    }

    #[test]
    fn run_dirs_are_private_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let runs = RunDirs::new(dir.path());
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let path = {
            let run = runs.run_dir().unwrap();
            assert_eq!(mode(run.path()), 0o700);
            assert_eq!(mode(&credentials_dir(dir.path())), 0o700);
            let f = run.write_private("answer", b"x").unwrap();
            assert_eq!(mode(&f), 0o600);
            run.path().to_owned()
        };
        assert!(!path.exists());
    }
}
