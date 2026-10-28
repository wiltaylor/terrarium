//! The master key and the seal of the secret fields (PLAN §16b.2).
//!
//! The key is 32 random bytes in `<state>/secrets.key`, mode 0600, outside
//! the database. A sealed value is the 24-byte nonce, then the
//! XChaCha20-Poly1305 ciphertext with its tag. The associated data is
//! `id|scope|name|kind`, so a row copied to another name does not open.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, Generate, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::error::{ApiError, ApiResult};

/// The key file in the state dir.
pub const KEY_FILE: &str = "secrets.key";
/// The new key of a rotation, before it replaces [`KEY_FILE`].
pub const NEW_KEY_FILE: &str = "secrets.key.new";
/// The length of the key, in bytes.
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// The master key. `Debug` does not print it; drop zeroes it.
#[derive(Clone)]
pub struct MasterKey(Zeroizing<[u8; KEY_LEN]>);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(***)")
    }
}

impl MasterKey {
    /// A new random key.
    pub fn generate() -> Self {
        let key = Key::generate();
        let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
        bytes.copy_from_slice(&key);
        Self(bytes)
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&Key::from(*self.0))
    }

    /// Seal `plain` with the associated data `aad`.
    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> ApiResult<Vec<u8>> {
        let nonce = XNonce::generate();
        let ct = self
            .cipher()
            .encrypt(&nonce, Payload { msg: plain, aad })
            .map_err(|_| ApiError::internal("cannot seal the secret"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open a sealed value. `None` when the key, the data, or the
    /// associated data do not match.
    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        if sealed.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce).ok()?;
        self.cipher()
            .decrypt(&nonce, Payload { msg: ct, aad })
            .ok()
            .map(Zeroizing::new)
    }
}

/// The associated data of a row.
pub fn aad(id: &str, scope: &str, name: &str, kind: &str) -> Vec<u8> {
    format!("{id}|{scope}|{name}|{kind}").into_bytes()
}

pub fn key_path(state_dir: &Path) -> PathBuf {
    state_dir.join(KEY_FILE)
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> ApiError {
    tracing::error!(path = %path.display(), "secrets: {what}: {e}");
    ApiError::internal(format!("cannot {what} {}: {e}", path.display()))
}

/// Read a key file. `Ok(None)` when it does not exist.
pub fn read_key(path: &Path) -> ApiResult<Option<MasterKey>> {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("read", path, &e)),
    };
    let mut buf = Zeroizing::new(Vec::with_capacity(KEY_LEN + 1));
    Read::by_ref(&mut f)
        .take(KEY_LEN as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| io_err("read", path, &e))?;
    if buf.len() != KEY_LEN {
        return Err(ApiError::internal(format!(
            "{} is not a secret key: it must hold {KEY_LEN} bytes",
            path.display()
        )));
    }
    let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
    bytes.copy_from_slice(&buf);
    Ok(Some(MasterKey(bytes)))
}

/// Write `key` to `path` with mode 0600: a temp file, fsync, and a
/// rename, so the file is the old key or the new one, never a part.
pub fn write_key(path: &Path, key: &MasterKey) -> ApiResult<()> {
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&key.0[..])?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent() {
            fs::File::open(dir)?.sync_all()?;
        }
        Ok::<_, std::io::Error>(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_err("write", path, &e));
    }
    Ok(())
}

/// Set the mode of an existing key file to 0600.
pub fn fix_mode(path: &Path) -> ApiResult<()> {
    let mode = fs::metadata(path)
        .map_err(|e| io_err("read", path, &e))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        tracing::warn!(path = %path.display(), "the secret key file had mode {mode:o}; it is 0600 now");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| io_err("set the mode of", path, &e))?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn seal_opens_with_the_same_key_and_aad_only() {
        let k = MasterKey::generate();
        let a = aad("01X", "global", "deploy", "token");
        let sealed = k.seal(b"{\"value\":\"ghp_x\"}", &a).unwrap();
        assert!(!sealed.windows(5).any(|w| w == b"ghp_x"));
        assert_eq!(&k.open(&sealed, &a).unwrap()[..], b"{\"value\":\"ghp_x\"}");
        // Another name, scope, kind, or id: no open.
        for other in [
            aad("01X", "global", "other", "token"),
            aad("01X", "proj", "deploy", "token"),
            aad("01X", "global", "deploy", "api_key"),
            aad("01Y", "global", "deploy", "token"),
        ] {
            assert!(k.open(&sealed, &other).is_none());
        }
        // Another key: no open.
        assert!(MasterKey::generate().open(&sealed, &a).is_none());
        // A changed byte: no open.
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(k.open(&bad, &a).is_none());
        assert!(k.open(&sealed[..10], &a).is_none());
        // Each seal has a new nonce.
        assert_ne!(sealed, k.seal(b"{\"value\":\"ghp_x\"}", &a).unwrap());
    }

    #[test]
    fn the_key_file_is_0600_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = key_path(dir.path());
        assert!(read_key(&p).unwrap().is_none());
        let k = MasterKey::generate();
        write_key(&p, &k).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::metadata(&p).unwrap().len(), KEY_LEN as u64);
        let back = read_key(&p).unwrap().unwrap();
        let a = aad("i", "global", "n", "token");
        assert!(back.open(&k.seal(b"x", &a).unwrap(), &a).is_some());
        assert!(!format!("{back:?}").contains(&format!("{:?}", &back.0[..4])));
        // A short file is an error, not a key.
        fs::write(&p, b"short").unwrap();
        assert!(read_key(&p).is_err());
        // A loose mode is set to 0600.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        fix_mode(&p).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
