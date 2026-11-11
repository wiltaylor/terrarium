//! Encrypted, transactional host reconnect journal. No protocol ACK may
//! precede the commit that saves its corresponding output offset.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::error::{ApiError, ApiResult};

/// Uses a separate connection, never the shared DB lock. Registry callers
/// may hold their own lock while saving without inverting the shared lock.
pub struct Journal {
    state_dir: PathBuf,
}

impl Journal {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_owned(),
        }
    }

    fn connect(&self) -> ApiResult<Connection> {
        let conn = Connection::open(self.state_dir.join("terrarium.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(conn)
    }

    pub fn load<T: DeserializeOwned>(&self, id: &str) -> ApiResult<Option<T>> {
        let mut conn = self.connect()?;
        // The writer lock also serializes us with key rotation. Finish a
        // committed rotation before reading the current key from disk.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::secrets::migrate::finish_rotation(&tx, &self.state_dir)
            .map_err(|_| ApiError::internal("cannot finish host journal key rotation"))?;
        let sealed: Option<Vec<u8>> = tx
            .query_row("SELECT sealed FROM host_resume WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let value =
            match sealed {
                None => None,
                Some(sealed) => {
                    let key = crate::secrets::key(&self.state_dir)?;
                    let plain = key.open(&sealed, &super::resume_aad(id)).ok_or_else(|| {
                        ApiError::internal("cannot decrypt the host reconnect journal")
                    })?;
                    Some(serde_json::from_slice(&plain).map_err(|_| {
                        ApiError::internal("cannot decode the host reconnect journal")
                    })?)
                }
            };
        tx.commit()?;
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
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::secrets::migrate::finish_rotation(&tx, &self.state_dir)
            .map_err(|_| ApiError::internal("cannot finish host journal key rotation"))?;
        let key = crate::secrets::key(&self.state_dir)?;
        for id in ids {
            tx.execute("DELETE FROM host_resume WHERE id=?1", [id])?;
        }
        for prefix in prefixes {
            tx.execute(
                "DELETE FROM host_resume WHERE substr(id,1,length(?1))=?1",
                [prefix],
            )?;
        }
        for (id, value) in values {
            let plain = zeroize::Zeroizing::new(
                serde_json::to_vec(value)
                    .map_err(|_| ApiError::internal("cannot encode the host reconnect journal"))?,
            );
            let sealed = key.seal(&plain, &super::resume_aad(id))?;
            tx.execute(
                "INSERT INTO host_resume(id,sealed) VALUES (?1,?2)
             ON CONFLICT(id) DO UPDATE SET sealed=excluded.sealed",
                params![id, sealed],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn scan<T: DeserializeOwned>(&self, prefix: &str) -> ApiResult<Vec<(String, T)>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::secrets::migrate::finish_rotation(&tx, &self.state_dir)
            .map_err(|_| ApiError::internal("cannot finish host journal key rotation"))?;
        let key = crate::secrets::key(&self.state_dir)?;
        let mut out = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT id,sealed FROM host_resume WHERE substr(id,1,length(?1))=?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([prefix], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (id, sealed) = row?;
                let plain = key.open(&sealed, &super::resume_aad(&id)).ok_or_else(|| {
                    ApiError::internal("cannot decrypt the host reconnect journal")
                })?;
                let value = serde_json::from_slice(&plain)
                    .map_err(|_| ApiError::internal("cannot decode the host reconnect journal"))?;
                out.push((id, value));
            }
        }
        tx.commit()?;
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
