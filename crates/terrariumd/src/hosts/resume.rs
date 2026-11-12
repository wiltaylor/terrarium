//! Encrypted, transactional host reconnect journal. No protocol ACK may
//! precede the commit that saves its corresponding output offset.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::error::{ApiError, ApiResult};

fn journal_error(error: rusqlite::Error) -> ApiError {
    if matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ) {
        ApiError::new(crate::error::Code::ResourceBusy, "host journal is busy")
    } else {
        error.into()
    }
}

/// Shares the daemon writer. Registry users never wait for its mutex:
/// contention returns ResourceBusy for retry outside their own lock.
pub struct Journal {
    state_dir: PathBuf,
    db: crate::db::Db,
    wait: bool,
}

impl Journal {
    pub fn with_db(state_dir: &Path, db: crate::db::Db) -> Self {
        Self {
            state_dir: state_dir.to_owned(),
            db,
            wait: true,
        }
    }

    /// Only registry callers require fail-fast locking. Other synchronous
    /// users may queue behind normal writes, as they do for Db::read_now.
    pub(crate) fn for_registry(state_dir: &Path, db: crate::db::Db) -> Self {
        Self { wait: false, ..Self::with_db(state_dir, db) }
    }

    pub fn load<T: DeserializeOwned>(&self, id: &str) -> ApiResult<Option<T>> {
        // The writer lock also serializes us with key rotation. Finish a
        // committed rotation before reading the current key from disk.
        let (key, sealed) = self.db.journal_now(self.wait, |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            crate::secrets::migrate::finish_rotation(&tx, &self.state_dir)
                .map_err(|_| ApiError::internal("cannot finish host journal key rotation"))?;
            let sealed: Option<Vec<u8>> = tx
                .query_row("SELECT sealed FROM host_resume WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            let key = crate::secrets::key(&self.state_dir)?;
            tx.commit()?;
            Ok((key, sealed))
        })?;
        let value =
            match sealed {
                None => None,
                Some(sealed) => {
                    let plain = key.open(&sealed, &super::resume_aad(id)).ok_or_else(|| {
                        ApiError::internal("cannot decrypt the host reconnect journal")
                    })?;
                    Some(serde_json::from_slice(&plain).map_err(|_| {
                        ApiError::internal("cannot decode the host reconnect journal")
                    })?)
                }
            };
        Ok(value)
    }

    pub fn save(&self, id: &str, value: &impl Serialize) -> ApiResult<()> {
        self.save_many(&[(
            id.to_owned(),
            serde_json::to_value(value)
                .map_err(|_| ApiError::internal("cannot encode the host reconnect journal"))?,
        )])
    }

    /// Commit a session's event and received offset together.
    pub fn save_many(&self, values: &[(String, serde_json::Value)]) -> ApiResult<()> {
        self.replace(values, &[], &[])
    }

    /// Replace owner metadata and delete its obsolete replay rows atomically.
    pub fn replace(
        &self,
        values: &[(String, serde_json::Value)],
        ids: &[String],
        prefixes: &[String],
    ) -> ApiResult<()> {
        // Encoding is independent of rotation and stays outside the writer.
        let values = values
            .iter()
            .map(|(id, value)| {
                let plain = serde_json::to_vec(value)
                    .map_err(|_| ApiError::internal("cannot encode the host reconnect journal"))?;
                Ok((id, zeroize::Zeroizing::new(plain)))
            })
            .collect::<ApiResult<Vec<_>>>()?;
        self.db.journal_now(self.wait, |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(journal_error)?;
            crate::secrets::migrate::finish_rotation(&tx, &self.state_dir)
                .map_err(|_| ApiError::internal("cannot finish host journal key rotation"))?;
            let key = crate::secrets::key(&self.state_dir)?;
            for id in ids {
                tx.execute("DELETE FROM host_resume WHERE id=?1", [id])
                    .map_err(journal_error)?;
            }
            for prefix in prefixes {
                tx.execute(
                    "DELETE FROM host_resume WHERE id>=?1 AND id<?2",
                    params![prefix, prefix_end(prefix)?],
                )
                .map_err(journal_error)?;
            }
            for (id, plain) in values {
                let sealed = key.seal(&plain, &super::resume_aad(id))?;
                tx.execute(
                    "INSERT INTO host_resume(id,sealed) VALUES (?1,?2)
             ON CONFLICT(id) DO UPDATE SET sealed=excluded.sealed",
                    params![id, sealed],
                )
                .map_err(journal_error)?;
            }
            tx.commit().map_err(journal_error)?;
            Ok(())
        })
    }

    pub fn scan<T: DeserializeOwned>(&self, prefix: &str) -> ApiResult<Vec<(String, T)>> {
        let end = prefix_end(prefix)?;
        let mut cursor = String::new();
        let mut out = Vec::new();
        loop {
            // Capture the key and one sealed page under the rotation lock,
            // then release the writer before decrypting or decoding anything.
            let (key, rows) = self.db.journal_now(self.wait, |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                crate::secrets::migrate::finish_rotation(&tx, &self.state_dir)
                    .map_err(|_| ApiError::internal("cannot finish host journal key rotation"))?;
                let key = crate::secrets::key(&self.state_dir)?;
                let rows = {
                    let mut stmt = tx.prepare(SCAN_PAGE)?;
                    stmt.query_map(params![prefix, end, cursor], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                };
                tx.commit()?;
                Ok((key, rows))
            })?;
            let count = rows.len();
            for (id, sealed) in rows {
                cursor.clone_from(&id);
                let plain = key.open(&sealed, &super::resume_aad(&id)).ok_or_else(|| {
                    ApiError::internal("cannot decrypt the host reconnect journal")
                })?;
                let value = serde_json::from_slice(&plain)
                    .map_err(|_| ApiError::internal("cannot decode the host reconnect journal"))?;
                out.push((id, value));
            }
            if count < 256 {
                break;
            }
        }
        Ok(out)
    }
}

const SCAN_PAGE: &str = "SELECT id,sealed FROM host_resume
    WHERE id>=?1 AND id<?2 AND id>?3 ORDER BY id LIMIT 256";

#[cfg(test)]
impl Journal {
    #[allow(clippy::expect_used)]
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self::with_db(
            state_dir,
            crate::db::Db::new(
                crate::db::open(&state_dir.join("terrarium.db")).expect("test journal DB"),
                crate::events::EventHub::default(),
            ),
        )
    }
}

// Journal namespaces are nonempty ASCII paths. Reject other prefixes rather
// than constructing an invalid UTF-8 bound or silently scanning the table.
fn prefix_end(prefix: &str) -> ApiResult<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    if !prefix.is_ascii() || bytes.is_empty() || bytes.last() == Some(&127) {
        return Err(ApiError::internal("invalid host journal prefix"));
    }
    if let Some(last) = bytes.last_mut() {
        *last += 1;
    }
    String::from_utf8(bytes).map_err(|_| ApiError::internal("invalid host journal prefix"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn file_connections_are_the_main_writer_or_read_only() {
        fn check(dir: &Path) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path.file_name().unwrap() != "tests" {
                        check(&path);
                    }
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs")
                    || path.file_name().unwrap() == "tests.rs"
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                // Inline test modules are conventionally gated this way.
                let production = source.split("#[cfg(test)]").next().unwrap();
                let direct = ["Connection::", "open("].concat();
                for (_, _) in production.match_indices(&direct) {
                    assert!(
                        path.ends_with("db.rs"),
                        "second writer in {}",
                        path.display()
                    );
                }
                let flagged = ["Connection::", "open_with_flags("].concat();
                for (offset, _) in production.match_indices(&flagged) {
                    let call = &production[offset..];
                    let args = call.split(")?").next().unwrap();
                    assert!(
                        args.contains("SQLITE_OPEN_READ_ONLY")
                            && !args.contains("SQLITE_OPEN_READ_WRITE"),
                        "writable file connection in {}",
                        path.display()
                    );
                }
            }
        }
        check(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    }

    #[test]
    fn journal_prefix_pages_use_primary_key_range_at_scale() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let journal = Journal::new(dir.path());
        let rows: Vec<_> = (0..32)
            .flat_map(|host| {
                (0..600).map(move |seq| {
                    (
                        format!("event/host-{host:03}/1/{seq:020}"),
                        serde_json::json!(seq),
                    )
                })
            })
            .collect();
        journal.save_many(&rows).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {SCAN_PAGE}"))
            .unwrap()
            .query_map(params!["event/host-016/", "event/host-0160", ""], |r| {
                r.get(3)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|s| s.contains("SEARCH host_resume")
                && s.contains("id>?")
                && s.contains("id<?")),
            "{plan:?}"
        );
        for host in 0..32 {
            let found = journal
                .scan::<u32>(&format!("event/host-{host:03}/"))
                .unwrap();
            assert_eq!(found.len(), 600);
            assert_eq!(found[599].1, 599);
        }
        journal
            .replace(&[], &[], &["event/host-016/".into()])
            .unwrap();
        assert!(journal.scan::<u32>("event/host-016/").unwrap().is_empty());
        assert_eq!(journal.scan::<u32>("event/host-017/").unwrap().len(), 600);
    }

    #[test]
    fn journal_survives_reopen_and_master_key_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let value = serde_json::json!({"token": "private-resume-token", "received": 42});
        Journal::new(dir.path()).save("host-a", &value).unwrap();
        let sealed: Vec<u8> = conn
            .query_row("SELECT sealed FROM host_resume", [], |r| r.get(0))
            .unwrap();
        assert!(!sealed.windows(20).any(|w| w == b"private-resume-token"));
        crate::secrets::migrate::rotate_key(&mut conn, dir.path()).unwrap();
        let restored: serde_json::Value = Journal::new(dir.path()).load("host-a").unwrap().unwrap();
        assert_eq!(restored, value);
        assert!(
            Journal::new(dir.path())
                .load::<serde_json::Value>("missing")
                .unwrap()
                .is_none()
        );
        conn.execute("UPDATE host_resume SET id='host-b'", [])
            .unwrap();
        assert!(
            Journal::new(dir.path())
                .load::<serde_json::Value>("host-b")
                .is_err()
        );
    }
}
