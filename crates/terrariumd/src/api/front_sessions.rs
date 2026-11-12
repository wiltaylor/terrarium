//! Read-only maintenance-view capabilities. No signing keys or tokens on disk.
use super::session::{Session, TokenKind};
use crate::password::OperatorAuth;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct Cache {
    pv: u64,
    written_at: i64,
    entries: HashMap<String, Entry>,
    revoked: HashMap<String, i64>,
}
#[derive(Serialize)]
struct Entry {
    hash: String,
    exp: i64,
    #[serde(skip)]
    sid: String,
}
fn hash(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
static CACHES: OnceLock<Mutex<HashMap<PathBuf, Cache>>> = OnceLock::new();

// Serialize export updates with logout tombstones and password-version checks.
fn change(op: &OperatorAuth, f: impl FnOnce(&mut Cache, i64) -> bool) {
    let Some(parent) = op.store.path().parent() else {
        return;
    };
    let dir = parent.join("front");
    let Ok(mut caches) = CACHES.get_or_init(Default::default).lock() else {
        return;
    };
    let Ok(password) = op.store.current() else {
        let _ = std::fs::remove_file(dir.join("sessions.json"));
        return;
    };
    let cache = caches.entry(dir.clone()).or_default();
    if cache.pv != password.version {
        *cache = Cache {
            pv: password.version,
            ..Cache::default()
        };
    }
    let now = chrono::Utc::now().timestamp();
    cache.entries.retain(|_, e| e.exp > now);
    cache.revoked.retain(|_, exp| *exp > now);
    let changed = f(cache, now);
    if !changed && now >= cache.written_at && now - cache.written_at < 5 {
        return;
    }
    if write(&dir, cache, now).is_err() {
        let _ = std::fs::remove_file(dir.join("sessions.json"));
        tracing::warn!("cannot export front session hashes");
    } else {
        cache.written_at = now;
    }
}
fn write(dir: &std::path::Path, cache: &Cache, now: i64) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(
        &mut tmp,
        &serde_json::json!({
            "at": now, "sessions": cache.entries.values().collect::<Vec<_>>()
        }),
    )?;
    tmp.as_file().sync_all()?;
    tmp.persist(dir.join("sessions.json"))?;
    Ok(())
}
pub(super) fn observe(op: &OperatorAuth, s: &Session) {
    if s.claims.kind != TokenKind::Browser {
        return;
    }
    change(op, |c, _| {
        let sid = hash(&s.claims.sid);
        if s.claims.pv == c.pv && !c.revoked.contains_key(&sid) && c.entries.len() < 1024 {
            let h = hash(&s.token);
            if c.entries.contains_key(&h) {
                return false;
            }
            c.entries.insert(
                h.clone(),
                Entry {
                    hash: h,
                    exp: s.claims.exp,
                    sid,
                },
            );
            return true;
        }
        false
    });
}
pub(super) fn revoke(op: &OperatorAuth, s: &Session, exp: i64) {
    change(op, |c, _| {
        let sid = hash(&s.claims.sid);
        c.entries.retain(|_, e| e.sid != sid);
        c.revoked.insert(sid, exp);
        true
    });
}
pub(super) fn clear(op: &OperatorAuth) {
    change(op, |c, _| {
        c.entries.clear();
        true
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::session::SessionKey;
    use super::*;
    #[test]
    fn export_is_hash_only_private_and_revocation_cannot_be_readded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let op = OperatorAuth::new(dir.path());
        let pv = op.store.current().unwrap().version;
        let now = chrono::Utc::now().timestamp();
        let key = SessionKey::from_bytes(vec![7; 32]);
        let s = key.mint(TokenKind::Browser, None, now, 600, pv).unwrap();
        observe(&op, &s);
        let path = dir.path().join("front/sessions.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&s.token));
        assert!(!text.contains(&s.claims.sid));
        assert!(text.contains(&hash(&s.token)));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        revoke(&op, &s, now + 600);
        observe(&op, &s);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains(&hash(&s.token))
        );
        let other = key.mint(TokenKind::Browser, None, now, 600, pv).unwrap();
        observe(&op, &other);
        op.store.bump().unwrap();
        observe(&op, &other);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains(&hash(&other.token))
        );
    }
}
