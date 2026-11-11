//! The move of the old credential files into the store (PLAN §16b.7), and
//! the key rotation (`terrariumd secrets rotate-key`, §16b.2).
//!
//! The old store was `<state>/credentials/<name>` (the secret) and
//! `<name>.json` (`{kind, username?, created_at}`). At each start, the
//! daemon moves each pair into a global secret: `token` -> `token`,
//! `ssh_key` -> `ssh_key`, `password` -> `login`. It copies the two files
//! to `<state>/credentials/backup/` (0600) first, stores the secret and a
//! `secret.migrated` audit row in one transaction, and removes the files
//! after the commit. A pair that a crash left behind finds its secret
//! stored, and is only removed. A pair whose value the store refuses
//! stays, with a warning.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::crypto::{self, MasterKey, NEW_KEY_FILE};
use super::{GLOBAL, Kind, NewSecret, PutMode, RESERVED_NAMES, SecretInput, credentials_dir};
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};

/// One old credential file pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldFile {
    pub name: String,
    pub secret: PathBuf,
    pub meta: PathBuf,
}

/// The old credential pairs in `<state>/credentials/`.
pub fn old_files(state_dir: &Path) -> Vec<OldFile> {
    let dir = credentials_dir(state_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<OldFile> = entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let name = file.strip_suffix(".json")?.to_owned();
            if RESERVED_NAMES.contains(&name.as_str()) || !crate::store::valid_name(&name) {
                return None;
            }
            let secret = dir.join(&name);
            secret.is_file().then(|| OldFile {
                meta: dir.join(&file),
                secret,
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn io(what: &str, p: &Path, e: &std::io::Error) -> ApiError {
    ApiError::internal(format!("cannot {what} {}: {e}", p.display()))
}

/// Copy one file to `<state>/credentials/backup/<stamp>-credential-<file>`,
/// mode 0600.
fn backup(state_dir: &Path, path: &Path) -> ApiResult<PathBuf> {
    let dir = credentials_dir(state_dir).join(crate::accounts::BACKUP_DIR);
    super::private_dir(&credentials_dir(state_dir))?;
    super::private_dir(&dir)?;
    let bytes = zeroize::Zeroizing::new(fs::read(path).map_err(|e| io("read", path, &e))?);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let file = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let to = dir.join(format!("{stamp}-credential-{file}"));
    super::write_private(&to, &bytes)?;
    Ok(to)
}

/// The kind and the fields of an old pair.
fn convert(f: &OldFile) -> ApiResult<(Kind, &'static str, SecretInput)> {
    let meta: Value =
        serde_json::from_slice(&fs::read(&f.meta).map_err(|e| io("read", &f.meta, &e))?)
            .map_err(|_| ApiError::invalid(format!("{} is not JSON", f.meta.display())))?;
    let value = zeroize::Zeroizing::new(
        fs::read_to_string(&f.secret).map_err(|e| io("read", &f.secret, &e))?,
    );
    let username = meta["username"].as_str().map(str::to_owned);
    let mut fields = serde_json::Map::new();
    let (kind, from) = match meta["kind"].as_str() {
        Some("token") => {
            fields.insert("value".into(), value.trim().into());
            if let Some(u) = username {
                fields.insert("username".into(), u.into());
            }
            (Kind::Token, "token")
        }
        Some("ssh_key") => {
            fields.insert("private_key".into(), value.as_str().into());
            (Kind::SshKey, "ssh_key")
        }
        Some("password") => {
            fields.insert("password".into(), value.as_str().into());
            if let Some(u) = username {
                fields.insert("username".into(), u.into());
            }
            (Kind::Login, "password")
        }
        other => {
            return Err(ApiError::invalid(format!(
                "{} has the unknown kind {other:?}",
                f.meta.display()
            )));
        }
    };
    Ok((
        kind,
        from,
        SecretInput {
            fields,
            secret_fields: Vec::new(),
        },
    ))
}

/// What happened to one pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Stored as a global secret; the files can go.
    Moved { name: String, kind: Kind },
    /// The global secret exists already (an earlier start stored it); the
    /// files can go.
    Stored { name: String },
    /// The store refused it; the files stay.
    Refused { name: String, error: String },
}

/// Move the old pairs into the store, in `t`. The files stay until the
/// caller removes them after the commit ([`remove_files`]).
pub fn migrate_files(
    t: &Tx<'_>,
    state_dir: &Path,
    key: &MasterKey,
    files: &[OldFile],
) -> ApiResult<Vec<Outcome>> {
    let mut out = Vec::new();
    for f in files {
        if super::get(t, GLOBAL, &f.name)?.is_some() {
            out.push(Outcome::Stored {
                name: f.name.clone(),
            });
            continue;
        }
        let converted = convert(f);
        let (kind, from, input) = match converted {
            Ok(c) => c,
            Err(e) => {
                out.push(Outcome::Refused {
                    name: f.name.clone(),
                    error: e.message,
                });
                continue;
            }
        };
        let backups = [backup(state_dir, &f.secret)?, backup(state_dir, &f.meta)?];
        let stored = super::put(
            t,
            key,
            PutMode::Create,
            &NewSecret {
                scope: GLOBAL,
                name: &f.name,
                kind,
                input: &input,
                by: "system",
                strict: false,
            },
        );
        let info = match stored {
            Ok((info, _)) => info,
            Err(e) => {
                for b in &backups {
                    let _ = fs::remove_file(b);
                }
                out.push(Outcome::Refused {
                    name: f.name.clone(),
                    error: e.message,
                });
                continue;
            }
        };
        // Keep the age of the old credential.
        let meta: Value = fs::read(&f.meta)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if let Some(at) = meta["created_at"].as_str() {
            t.execute(
                "UPDATE secrets SET created_at = ?2 WHERE id = ?1",
                params![info.id, at],
            )?;
        }
        t.audit(
            "secret.migrated",
            &format!("{GLOBAL}/{}", f.name),
            None,
            json!({
                "scope": GLOBAL, "name": f.name, "kind": kind, "from_kind": from,
                "backups": backups.iter().map(|b| b.display().to_string()).collect::<Vec<_>>(),
            }),
        )?;
        out.push(Outcome::Moved {
            name: f.name.clone(),
            kind,
        });
    }
    Ok(out)
}

/// Remove the files of the pairs that are in the store now.
pub fn remove_files(files: &[OldFile], outcomes: &[Outcome]) {
    for o in outcomes {
        let name = match o {
            Outcome::Moved { name, .. } | Outcome::Stored { name } => name,
            Outcome::Refused { name, error } => {
                tracing::warn!(name, "the old credential stays in its file: {error}");
                continue;
            }
        };
        if let Some(f) = files.iter().find(|f| &f.name == name) {
            for p in [&f.secret, &f.meta] {
                if let Err(e) = fs::remove_file(p)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %p.display(), "cannot remove an old credential file: {e}");
                }
            }
        }
    }
}

/// The marker in the name of a backup of a moved credential
/// (`<stamp>-credential-<file>`).
const BACKUP_MARK: &str = "-credential-";

/// Remove each backup of a moved credential
/// (`<state>/credentials/backup/*-credential-*`) whose mtime is more than
/// `max_age` before `now` (PLAN §16b.7: the trash retention,
/// `TERRARIUM_TRASH_DAYS`, 7 by default). The backups hold the plain
/// values, so they do not stay. Other backups in the dir stay. A symlink
/// is removed, not followed. The answer: the removed paths.
pub fn prune_backups(
    state_dir: &Path,
    now: std::time::SystemTime,
    max_age: std::time::Duration,
) -> Vec<PathBuf> {
    let dir = credentials_dir(state_dir).join(crate::accounts::BACKUP_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().contains(BACKUP_MARK) {
            continue;
        }
        let path = e.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if !old || meta.is_dir() {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(e) => tracing::warn!(path = %path.display(), "cannot remove an old backup: {e}"),
        }
    }
    removed.sort();
    removed
}

/// [`prune_backups`] with the trash retention, now.
pub fn prune_backups_now(state_dir: &Path) {
    let max_age = std::time::Duration::from_secs(crate::trash::trash_days() * 86_400);
    let removed = prune_backups(state_dir, std::time::SystemTime::now(), max_age);
    if !removed.is_empty() {
        tracing::info!(files = removed.len(), "removed old credential backups");
    }
}

/// The start-up move of the old files (the daemon calls it once).
pub async fn run_at_start(state: &crate::api::Shared) {
    prune_backups_now(&state.state_dir);
    let files = old_files(&state.state_dir);
    if files.is_empty() {
        return;
    }
    let key = match super::key(&state.state_dir) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("the old credentials cannot move into the secret store: {e}");
            return;
        }
    };
    let dir = state.state_dir.clone();
    let list = files.clone();
    let res = state
        .db
        .write("system", move |t| migrate_files(t, &dir, &key, &list))
        .await;
    match res {
        Ok(outcomes) => {
            remove_files(&files, &outcomes);
            let moved: Vec<&str> = outcomes
                .iter()
                .filter_map(|o| match o {
                    Outcome::Moved { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            if !moved.is_empty() {
                tracing::info!(moved = ?moved, "moved the old credentials into the secret store");
            }
        }
        Err(e) => tracing::error!("the move of the old credentials failed: {e}"),
    }
}

// ---- Key rotation ----

/// One sealed row.
struct Row {
    id: String,
    scope: String,
    name: String,
    kind: String,
    sealed: Vec<u8>,
    remote: bool,
}

fn sealed_rows(conn: &Connection) -> ApiResult<Vec<Row>> {
    let mut stmt = conn.prepare(
        "SELECT id, scope, name, kind, sealed FROM secrets WHERE sealed IS NOT NULL ORDER BY id",
    )?;
    let mut rows = stmt
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                scope: r.get(1)?,
                name: r.get(2)?,
                kind: r.get(3)?,
                sealed: r.get(4)?,
                remote: false,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut remotes = conn.prepare("SELECT id,sealed FROM remotes ORDER BY id")?;
    rows.extend(
        remotes
            .query_map([], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    sealed: r.get(1)?,
                    scope: String::new(),
                    name: String::new(),
                    kind: String::new(),
                    remote: true,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    );
    Ok(rows)
}

fn row_aad(r: &Row) -> Vec<u8> {
    if r.remote {
        crate::store::remotes::aad(&r.id)
    } else {
        crypto::aad(&r.id, &r.scope, &r.name, &r.kind)
    }
}

fn opens(key: &MasterKey, r: &Row) -> bool {
    key.open(&r.sealed, &row_aad(r)).is_some()
}

/// A rotation that stopped after its commit and before the rename left
/// `secrets.key.new`. When the rows open with it, it is the key now;
/// else it goes.
pub fn finish_rotation(conn: &Connection, state_dir: &Path) -> anyhow::Result<()> {
    let new_path = state_dir.join(NEW_KEY_FILE);
    let Some(new) = crypto::read_key(&new_path).ok().flatten() else {
        let _ = fs::remove_file(&new_path);
        return Ok(());
    };
    let rows = sealed_rows(conn).map_err(|e| anyhow::anyhow!(e.message))?;
    match rows.first() {
        Some(r) if opens(&new, r) => {
            fs::rename(&new_path, crypto::key_path(state_dir))?;
            tracing::warn!("finished a key rotation that stopped before the key file moved");
        }
        _ => {
            fs::remove_file(&new_path)?;
        }
    }
    Ok(())
}

/// Make a new master key and seal each row again with it, in one
/// transaction. The new key goes to `secrets.key.new` first; after the
/// commit it replaces `secrets.key` with a rename. The answer is the
/// count of rows.
pub fn rotate_key(conn: &mut Connection, state_dir: &Path) -> anyhow::Result<usize> {
    finish_rotation(conn, state_dir)?;
    let old = super::key(state_dir).map_err(|e| anyhow::anyhow!(e.message))?;
    let new = MasterKey::generate();
    let new_path = state_dir.join(NEW_KEY_FILE);
    crypto::write_key(&new_path, &new).map_err(|e| anyhow::anyhow!(e.message))?;
    let result = (|| {
        let tx = conn.transaction()?;
        let rows = sealed_rows(&tx).map_err(|e| anyhow::anyhow!(e.message))?;
        for r in &rows {
            let aad = row_aad(r);
            let plain = old.open(&r.sealed, &aad).ok_or_else(|| {
                anyhow::anyhow!(
                    "secret `{}/{}` does not open with the current key; nothing changed",
                    r.scope,
                    r.name
                )
            })?;
            let sealed = new
                .seal(&plain, &aad)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            tx.execute(
                if r.remote {
                    "UPDATE remotes SET sealed = ?2 WHERE id = ?1"
                } else {
                    "UPDATE secrets SET sealed = ?2 WHERE id = ?1"
                },
                params![r.id, sealed],
            )?;
        }
        tx.execute(
            "INSERT INTO audit (id, actor, action, target, project, detail, at)
             VALUES (?1, 'system', 'secret.rotate_key', 'secrets', NULL, ?2, ?3)",
            params![
                ulid::Ulid::new().to_string(),
                json!({"rows": rows.len()}).to_string(),
                crate::db::now()
            ],
        )?;
        tx.commit()?;
        Ok::<_, anyhow::Error>(rows.len())
    })();
    match result {
        Ok(n) => {
            let path = crypto::key_path(state_dir);
            fs::rename(&new_path, &path)?;
            fs::File::open(state_dir)?.sync_all()?;
            Ok(n)
        }
        Err(e) => {
            let _ = fs::remove_file(&new_path);
            Err(e)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::secrets::{self, Secret};

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn old(dir: &Path, name: &str, meta: Value, value: &str) {
        let c = credentials_dir(dir);
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join(name), value).unwrap();
        fs::write(c.join(format!("{name}.json")), meta.to_string()).unwrap();
    }

    /// One start: the move in a committed transaction, then the removal.
    fn start(c: &mut Connection, dir: &Path) -> Vec<Outcome> {
        let key = secrets::key(dir).unwrap();
        let files = old_files(dir);
        let t = Tx::for_test(c.transaction().unwrap(), "system");
        let out = migrate_files(&t, dir, &key, &files).unwrap();
        t.commit_for_test().unwrap();
        remove_files(&files, &out);
        out
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// S6 (secret manager review, 2026-10-29): the plain backups of the
    /// moved credentials go after the retention time. The other backups
    /// in the dir (the accounts) stay.
    #[test]
    fn old_credential_backups_are_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = credentials_dir(tmp.path()).join(crate::accounts::BACKUP_DIR);
        fs::create_dir_all(&dir).unwrap();
        let day = std::time::Duration::from_secs(86_400);
        let now = std::time::SystemTime::now();
        let old = dir.join("20261001T000000.000Z-credential-gh");
        let young = dir.join("20261028T000000.000Z-credential-gh.json");
        let other = dir.join("20261001T000000.000Z-account-max");
        for p in [&old, &young, &other] {
            fs::write(p, "ghp_PLAIN").unwrap();
        }
        let f = fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(now - 8 * day).unwrap();
        let f = fs::File::options().write(true).open(&other).unwrap();
        f.set_modified(now - 8 * day).unwrap();
        let removed = prune_backups(tmp.path(), now, 7 * day);
        assert_eq!(removed, vec![old.clone()]);
        assert!(!old.exists());
        assert!(young.exists());
        assert!(other.exists());
        // No backup dir: nothing to do.
        let empty = tempfile::tempdir().unwrap();
        assert!(prune_backups(empty.path(), now, 7 * day).is_empty());
    }

    #[test]
    fn old_files_move_into_global_secrets_once() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mut c = secrets::tests::db();
        secrets::ensure_key(&c, d).unwrap();
        let key = secrets::key(d).unwrap();
        old(
            d,
            "gh",
            json!({"name": "gh", "kind": "token", "username": "bot",
                              "created_at": "2026-01-01T00:00:00.000Z"}),
            "ghp_old\n",
        );
        old(
            d,
            "vnc",
            json!({"name": "vnc", "kind": "password"}),
            " pass word ",
        );
        // The old store took any text with the PEM marker as a key.
        old(
            d,
            "box",
            json!({"name": "box", "kind": "ssh_key"}),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n",
        );
        old(d, "odd", json!({"name": "odd", "kind": "weird"}), "x");
        // The other stores stay as they are.
        let creds = credentials_dir(d);
        fs::create_dir_all(creds.join("subscriptions")).unwrap();
        fs::write(creds.join("subscriptions").join("s.json"), "{}").unwrap();
        fs::create_dir_all(creds.join("accounts")).unwrap();
        fs::write(creds.join("accounts.json"), "{}").unwrap();

        let out = start(&mut c, d);
        assert_eq!(out.len(), 4);
        assert!(out.contains(&Outcome::Moved {
            name: "gh".into(),
            kind: Kind::Token
        }));
        assert!(out.contains(&Outcome::Moved {
            name: "vnc".into(),
            kind: Kind::Login
        }));
        assert!(out.contains(&Outcome::Moved {
            name: "box".into(),
            kind: Kind::SshKey
        }));
        assert!(matches!(&out[2], Outcome::Refused { name, .. } if name == "odd"));

        // The secrets open, with the old values and the old age.
        let gh = secrets::resolve(&c, &key, Some("p"), "gh")
            .unwrap()
            .unwrap();
        assert_eq!(gh.info.scope, GLOBAL);
        assert_eq!(gh.info.created_at, "2026-01-01T00:00:00.000Z");
        assert_eq!(gh.info.created_by, "system");
        assert!(matches!(gh.to_secret().unwrap(),
            Secret::Token { username, token } if username == "bot" && token.as_str() == "ghp_old"));
        let vnc = secrets::open(&c, &key, GLOBAL, "vnc").unwrap().unwrap();
        assert!(matches!(vnc.to_secret().unwrap(),
            Secret::Login { username: None, password } if password.as_str() == " pass word "));
        let bx = secrets::open(&c, &key, GLOBAL, "box").unwrap().unwrap();
        assert!(!bx.info.fields.contains_key("public_key"));
        assert!(matches!(bx.to_secret().unwrap(), Secret::SshKey(k) if k.contains("abc")));

        // The files are gone, with 0600 backups; the refused one stays.
        for n in ["gh", "gh.json", "vnc", "vnc.json", "box", "box.json"] {
            assert!(!creds.join(n).exists(), "{n}");
        }
        assert!(creds.join("odd").exists() && creds.join("odd.json").exists());
        let backups: Vec<_> = fs::read_dir(creds.join("backup"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(backups.len(), 6);
        assert!(backups.iter().all(|b| mode(b) == 0o600));
        assert_eq!(mode(&creds.join("backup")), 0o700);
        assert!(creds.join("subscriptions").join("s.json").exists());
        assert!(creds.join("accounts.json").exists());

        // The audit rows hold no value.
        assert_eq!(
            count(
                &c,
                "SELECT count(*) FROM audit WHERE action = 'secret.migrated'"
            ),
            3
        );
        let detail: String = c
            .query_row("SELECT group_concat(detail) FROM audit", [], |r| r.get(0))
            .unwrap();
        assert!(
            !detail.contains("ghp_old") && !detail.contains("pass word"),
            "{detail}"
        );

        // A second start changes nothing.
        let again = start(&mut c, d);
        assert_eq!(again.len(), 1);
        assert_eq!(count(&c, "SELECT count(*) FROM secrets"), 3);
        assert_eq!(count(&c, "SELECT count(*) FROM audit"), 3);

        // A crash after the commit left the files: the next start only
        // removes them.
        old(d, "gh", json!({"name": "gh", "kind": "token"}), "ghp_other");
        let again = start(&mut c, d);
        assert!(again.contains(&Outcome::Stored { name: "gh".into() }));
        assert!(!creds.join("gh").exists());
        assert_eq!(count(&c, "SELECT count(*) FROM audit"), 3);
    }

    #[test]
    fn rotate_key_reseals_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let path = d.join("t.db");
        let mut c = crate::db::open(&path).unwrap();
        c.execute(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 'x', 'x')",
            [],
        )
        .unwrap();
        secrets::ensure_key(&c, d).unwrap();
        let old_key = secrets::key(d).unwrap();
        for (scope, name) in [(GLOBAL, "a"), ("p", "b")] {
            let i: SecretInput =
                serde_json::from_value(json!({"fields": {"value": format!("v-{name}")}})).unwrap();
            secrets::put(
                &c,
                &old_key,
                PutMode::Create,
                &NewSecret {
                    scope,
                    name,
                    kind: Kind::ApiKey,
                    input: &i,
                    by: "operator",
                    strict: true,
                },
            )
            .unwrap();
        }
        assert_eq!(rotate_key(&mut c, d).unwrap(), 2);
        let new_key = secrets::key(d).unwrap();
        assert_eq!(mode(&crypto::key_path(d)), 0o600);
        assert!(!d.join(NEW_KEY_FILE).exists());
        assert!(secrets::open(&c, &old_key, GLOBAL, "a").is_err());
        let b = secrets::open(&c, &new_key, "p", "b").unwrap().unwrap();
        assert_eq!(b.values["value"].as_str(), "v-b");
        assert_eq!(
            count(
                &c,
                "SELECT count(*) FROM audit WHERE action = 'secret.rotate_key'"
            ),
            1
        );

        // A rotation that stopped after the commit: the next start puts
        // the new key in place.
        let next = MasterKey::generate();
        crypto::write_key(&d.join(NEW_KEY_FILE), &next).unwrap();
        for r in sealed_rows(&c).unwrap() {
            let aad = crypto::aad(&r.id, &r.scope, &r.name, &r.kind);
            let plain = new_key.open(&r.sealed, &aad).unwrap();
            c.execute(
                "UPDATE secrets SET sealed = ?2 WHERE id = ?1",
                params![r.id, next.seal(&plain, &aad).unwrap()],
            )
            .unwrap();
        }
        finish_rotation(&c, d).unwrap();
        assert!(!d.join(NEW_KEY_FILE).exists());
        let k = secrets::key(d).unwrap();
        assert!(secrets::open(&c, &k, GLOBAL, "a").unwrap().is_some());
        // A stale new key that opens nothing goes.
        crypto::write_key(&d.join(NEW_KEY_FILE), &MasterKey::generate()).unwrap();
        finish_rotation(&c, d).unwrap();
        assert!(!d.join(NEW_KEY_FILE).exists());
        assert!(
            secrets::open(&c, &secrets::key(d).unwrap(), GLOBAL, "a")
                .unwrap()
                .is_some()
        );
    }
}
