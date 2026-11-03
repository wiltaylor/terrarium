//! The passphrase seal of a backup (`terrariumd backup create --encrypt`).
//!
//! The key is Argon2id of the passphrase and a random 16-byte salt, with
//! the parameters in the header (`backup.json`). Each sealed zip entry is
//! a stream of chunks:
//!
//! ```text
//! prefix (16 random bytes)
//! { len (u32 LE; the top bit marks the last chunk) | XChaCha20-Poly1305 ciphertext }*
//! ```
//!
//! The nonce of a chunk is the prefix, then the chunk counter as a u64 BE
//! with the top bit set on the last chunk. The associated data is
//! `<backup id>|<entry name>`, so an entry copied to another name or to
//! another backup does not open. A stream that ends before its last chunk
//! is an error (a cut file does not restore as a short file).

use std::io::{self, Read, Write};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, Generate, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The plain bytes in one chunk.
pub const CHUNK: usize = 64 * 1024;
const PREFIX_LEN: usize = 16;
const LAST: u32 = 1 << 31;
const LAST_NONCE: u64 = 1 << 63;
/// The plain text of the passphrase check.
const CHECK: &[u8] = b"terrarium-backup";

/// The seal parameters, in the header.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Encryption {
    /// `xchacha20poly1305`.
    pub cipher: String,
    /// `argon2id`.
    pub kdf: String,
    /// The salt, base64.
    pub salt: String,
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    /// A sealed constant: a wrong passphrase fails at once, with a clear
    /// error, not at the first file.
    pub check: String,
}

/// The key of one backup. Drop zeroes it.
#[derive(Clone)]
pub struct SealKey {
    key: Zeroizing<[u8; 32]>,
    /// The backup ID, the first part of the associated data.
    id: String,
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealKey(***)")
    }
}

fn derive(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> anyhow::Result<[u8; 32]> {
    let params =
        Params::new(m, t, p, Some(32)).map_err(|e| anyhow::anyhow!("bad key parameters: {e}"))?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| anyhow::anyhow!("cannot derive the key: {e}"))?;
    Ok(out)
}

impl SealKey {
    /// A new key for the backup `id`, with a new salt. Slow (Argon2).
    pub fn create(passphrase: &str, id: &str) -> anyhow::Result<(Self, Encryption)> {
        if passphrase.is_empty() {
            anyhow::bail!("the passphrase is empty");
        }
        let nonce = XNonce::generate();
        let salt = &nonce[..16];
        let (m, t, p) = (
            Params::DEFAULT_M_COST,
            Params::DEFAULT_T_COST,
            Params::DEFAULT_P_COST,
        );
        let key = Self {
            key: Zeroizing::new(derive(passphrase, salt, m, t, p)?),
            id: id.to_owned(),
        };
        let check = key.seal_small(CHECK, "check")?;
        Ok((
            key,
            Encryption {
                cipher: "xchacha20poly1305".into(),
                kdf: "argon2id".into(),
                salt: B64.encode(salt),
                m_cost: m,
                t_cost: t,
                p_cost: p,
                check: B64.encode(check),
            },
        ))
    }

    /// The key of an existing backup. A wrong passphrase is an error.
    pub fn open(passphrase: &str, id: &str, enc: &Encryption) -> anyhow::Result<Self> {
        if enc.cipher != "xchacha20poly1305" || enc.kdf != "argon2id" {
            anyhow::bail!("unknown backup cipher {} / {}", enc.cipher, enc.kdf);
        }
        let salt = B64
            .decode(&enc.salt)
            .map_err(|_| anyhow::anyhow!("bad salt in the backup header"))?;
        let key = Self {
            key: Zeroizing::new(derive(
                passphrase, &salt, enc.m_cost, enc.t_cost, enc.p_cost,
            )?),
            id: id.to_owned(),
        };
        let check = B64
            .decode(&enc.check)
            .map_err(|_| anyhow::anyhow!("bad check in the backup header"))?;
        match key.open_small(&check, "check") {
            Some(v) if v.as_slice() == CHECK => Ok(key),
            _ => anyhow::bail!("wrong passphrase for backup {id}"),
        }
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&Key::from(*self.key))
    }

    fn aad(&self, entry: &str) -> Vec<u8> {
        format!("{}|{entry}", self.id).into_bytes()
    }

    fn seal_small(&self, plain: &[u8], entry: &str) -> anyhow::Result<Vec<u8>> {
        let nonce = XNonce::generate();
        let aad = self.aad(entry);
        let ct = self
            .cipher()
            .encrypt(
                &nonce,
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot seal"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn open_small(&self, sealed: &[u8], entry: &str) -> Option<Vec<u8>> {
        if sealed.len() < 24 {
            return None;
        }
        let (nonce, ct) = sealed.split_at(24);
        let nonce = XNonce::try_from(nonce).ok()?;
        let aad = self.aad(entry);
        self.cipher()
            .decrypt(&nonce, Payload { msg: ct, aad: &aad })
            .ok()
    }

    /// A writer that seals what it gets into `inner`, as the entry
    /// `entry`. Call [`SealWriter::finish`] at the end.
    pub fn writer<W: Write>(&self, inner: W, entry: &str) -> io::Result<SealWriter<W>> {
        SealWriter::new(inner, self.cipher(), self.aad(entry))
    }

    /// A reader of the sealed entry `entry`.
    pub fn reader<R: Read>(&self, inner: R, entry: &str) -> OpenReader<R> {
        OpenReader {
            inner,
            cipher: self.cipher(),
            aad: self.aad(entry),
            prefix: None,
            counter: 0,
            buf: Zeroizing::new(Vec::new()),
            pos: 0,
            done: false,
        }
    }
}

fn nonce(prefix: &[u8; PREFIX_LEN], counter: u64, last: bool) -> XNonce {
    let mut n = [0u8; 24];
    n[..PREFIX_LEN].copy_from_slice(prefix);
    let c = if last { counter | LAST_NONCE } else { counter };
    n[PREFIX_LEN..].copy_from_slice(&c.to_be_bytes());
    XNonce::from(n)
}

/// See [`SealKey::writer`].
pub struct SealWriter<W: Write> {
    inner: W,
    cipher: XChaCha20Poly1305,
    aad: Vec<u8>,
    prefix: [u8; PREFIX_LEN],
    counter: u64,
    buf: Zeroizing<Vec<u8>>,
}

impl<W: Write> SealWriter<W> {
    fn new(mut inner: W, cipher: XChaCha20Poly1305, aad: Vec<u8>) -> io::Result<Self> {
        let n = XNonce::generate();
        let mut prefix = [0u8; PREFIX_LEN];
        prefix.copy_from_slice(&n[..PREFIX_LEN]);
        inner.write_all(&prefix)?;
        Ok(Self {
            inner,
            cipher,
            aad,
            prefix,
            counter: 0,
            buf: Zeroizing::new(Vec::with_capacity(CHUNK)),
        })
    }

    fn seal_chunk(&mut self, last: bool) -> io::Result<()> {
        let nonce = nonce(&self.prefix, self.counter, last);
        let ct = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &self.buf,
                    aad: &self.aad,
                },
            )
            .map_err(|_| io::Error::other("cannot seal a chunk"))?;
        let len = u32::try_from(ct.len()).map_err(|_| io::Error::other("chunk too large"))?;
        let len = if last { len | LAST } else { len };
        self.inner.write_all(&len.to_le_bytes())?;
        self.inner.write_all(&ct)?;
        self.buf.clear();
        self.counter += 1;
        Ok(())
    }

    /// Seal the last chunk and give the inner writer back.
    pub fn finish(mut self) -> io::Result<W> {
        self.seal_chunk(true)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for SealWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = CHUNK - self.buf.len();
        let n = room.min(data.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == CHUNK {
            self.seal_chunk(false)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// See [`SealKey::reader`].
pub struct OpenReader<R: Read> {
    inner: R,
    cipher: XChaCha20Poly1305,
    aad: Vec<u8>,
    prefix: Option<[u8; PREFIX_LEN]>,
    counter: u64,
    buf: Zeroizing<Vec<u8>>,
    pos: usize,
    done: bool,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

impl<R: Read> OpenReader<R> {
    fn next_chunk(&mut self) -> io::Result<()> {
        let prefix = match self.prefix {
            Some(p) => p,
            None => {
                let mut p = [0u8; PREFIX_LEN];
                self.inner
                    .read_exact(&mut p)
                    .map_err(|_| bad("the sealed entry is cut"))?;
                self.prefix = Some(p);
                p
            }
        };
        let mut len = [0u8; 4];
        self.inner
            .read_exact(&mut len)
            .map_err(|_| bad("the sealed entry is cut"))?;
        let len = u32::from_le_bytes(len);
        let last = len & LAST != 0;
        let len = (len & !LAST) as usize;
        if len > CHUNK + 16 {
            return Err(bad("a sealed chunk is too large"));
        }
        let mut ct = vec![0u8; len];
        self.inner
            .read_exact(&mut ct)
            .map_err(|_| bad("the sealed entry is cut"))?;
        let nonce = nonce(&prefix, self.counter, last);
        let plain = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &ct,
                    aad: &self.aad,
                },
            )
            .map_err(|_| bad("a sealed chunk does not open (changed data or a wrong key)"))?;
        self.buf = Zeroizing::new(plain);
        self.pos = 0;
        self.counter += 1;
        if last {
            self.done = true;
            let mut extra = [0u8; 1];
            if self.inner.read(&mut extra)? != 0 {
                return Err(bad("data after the last sealed chunk"));
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for OpenReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.buf.len() {
            if self.done {
                return Ok(0);
            }
            self.next_chunk()?;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn round_trip(len: usize) {
        let (key, enc) = SealKey::create("correct horse", "B1").unwrap();
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut w = key.writer(Vec::new(), "files/a").unwrap();
        w.write_all(&data).unwrap();
        let sealed = w.finish().unwrap();
        let key2 = SealKey::open("correct horse", "B1", &enc).unwrap();
        let mut out = Vec::new();
        key2.reader(sealed.as_slice(), "files/a")
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn seals_and_opens_any_length() {
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 7] {
            round_trip(len);
        }
    }

    #[test]
    fn a_wrong_passphrase_fails_at_once() {
        let (_, enc) = SealKey::create("right", "B1").unwrap();
        let err = SealKey::open("wrong", "B1", &enc).unwrap_err();
        assert!(err.to_string().contains("wrong passphrase"));
        // The check is bound to the backup ID too.
        assert!(SealKey::open("right", "B2", &enc).is_err());
    }

    #[test]
    fn a_cut_a_changed_or_a_moved_entry_does_not_open() {
        let (key, _) = SealKey::create("p", "B1").unwrap();
        let mut w = key.writer(Vec::new(), "files/a").unwrap();
        w.write_all(&vec![7u8; CHUNK * 2]).unwrap();
        let sealed = w.finish().unwrap();
        let read = |bytes: &[u8], entry: &str| {
            let mut out = Vec::new();
            key.reader(bytes, entry).read_to_end(&mut out).map(|_| out)
        };
        assert!(read(&sealed, "files/a").is_ok());
        // Cut after the first chunk: no last chunk.
        let first = PREFIX_LEN + 4 + CHUNK + 16;
        assert!(read(&sealed[..first], "files/a").is_err());
        let mut changed = sealed.clone();
        changed[PREFIX_LEN + 10] ^= 1;
        assert!(read(&changed, "files/a").is_err());
        assert!(read(&sealed, "files/b").is_err());
        let mut extra = sealed.clone();
        extra.push(0);
        assert!(read(&extra, "files/a").is_err());
    }
}
