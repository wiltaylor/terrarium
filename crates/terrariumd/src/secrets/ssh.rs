//! SSH private keys of `ssh_key` secrets: derive the public key, and give
//! a key that `ssh -i` reads with no prompt.
//!
//! Formats: the OpenSSH format (each algorithm, with or without a
//! passphrase), and PEM with no passphrase: PKCS#1 RSA, PKCS#8 (RSA,
//! Ed25519, P-256), and SEC1 P-256. A PEM key with a passphrase is
//! refused with a hint: `ssh-keygen -p -f <file>` writes it again in the
//! OpenSSH format.

use ssh_key::public::{EcdsaPublicKey, Ed25519PublicKey, KeyData, RsaPublicKey};
use ssh_key::{LineEnding, PrivateKey, PublicKey};
use zeroize::Zeroizing;

use crate::error::{ApiError, ApiResult};

const OPENSSH: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const PKCS1_RSA: &str = "-----BEGIN RSA PRIVATE KEY-----";
const PKCS8: &str = "-----BEGIN PRIVATE KEY-----";
const SEC1_EC: &str = "-----BEGIN EC PRIVATE KEY-----";
const PKCS8_ENCRYPTED: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError::invalid(m)
}

fn pem_passphrase() -> ApiError {
    invalid(
        "a PEM key with a passphrase is not supported; write it in the OpenSSH format with \
         `ssh-keygen -p -f <file>`, then store that",
    )
}

/// The OpenSSH key of `pem`. Its passphrase is checked when it has one.
fn openssh(pem: &str, passphrase: Option<&str>) -> ApiResult<PrivateKey> {
    let key = PrivateKey::from_openssh(pem.trim())
        .map_err(|e| invalid(format!("the OpenSSH private key does not parse: {e}")))?;
    match (key.is_encrypted(), passphrase) {
        (true, Some(p)) => key
            .decrypt(p)
            .map_err(|_| invalid("the passphrase does not open the private key")),
        (true, None) => Err(invalid(
            "the private key has a passphrase; give it as the `passphrase` field",
        )),
        (false, _) => Ok(key),
    }
}

fn pem_public(pem: &str) -> ApiResult<KeyData> {
    use ed25519_dalek::pkcs8::DecodePrivateKey as _;
    use rsa::pkcs1::DecodeRsaPrivateKey as _;
    let s = pem.trim();
    let bad = |what: &str| invalid(format!("the {what} private key does not parse"));
    let rsa_data = |k: rsa::RsaPrivateKey| -> ApiResult<KeyData> {
        RsaPublicKey::try_from(rsa::RsaPublicKey::from(&k))
            .map(KeyData::Rsa)
            .map_err(|e| invalid(format!("the RSA key is not usable: {e}")))
    };
    let p256_data = |k: p256::SecretKey| -> ApiResult<KeyData> {
        EcdsaPublicKey::from_sec1_bytes(&k.public_key().to_sec1_bytes())
            .map(KeyData::Ecdsa)
            .map_err(|e| invalid(format!("the P-256 key is not usable: {e}")))
    };
    if s.starts_with(PKCS1_RSA) {
        if s.contains("ENCRYPTED") {
            return Err(pem_passphrase());
        }
        let k = rsa::RsaPrivateKey::from_pkcs1_pem(s).map_err(|_| bad("PKCS#1 RSA"))?;
        return rsa_data(k);
    }
    if s.starts_with(SEC1_EC) {
        if s.contains("ENCRYPTED") {
            return Err(pem_passphrase());
        }
        let k = p256::SecretKey::from_sec1_pem(s)
            .map_err(|_| bad("SEC1 EC (only P-256 in PEM; use the OpenSSH format for others)"))?;
        return p256_data(k);
    }
    if s.starts_with(PKCS8) {
        if let Ok(k) = rsa::RsaPrivateKey::from_pkcs8_pem(s) {
            return rsa_data(k);
        }
        if let Ok(k) = ed25519_dalek::SigningKey::from_pkcs8_pem(s) {
            return Ok(KeyData::Ed25519(Ed25519PublicKey(
                k.verifying_key().to_bytes(),
            )));
        }
        if let Ok(k) = p256::SecretKey::from_pkcs8_pem(s) {
            return p256_data(k);
        }
        return Err(bad("PKCS#8 (RSA, Ed25519, or P-256 in PEM)"));
    }
    if s.starts_with(PKCS8_ENCRYPTED) {
        return Err(pem_passphrase());
    }
    Err(invalid(
        "an ssh_key `private_key` is a private key in the OpenSSH or PEM format",
    ))
}

/// The public key of a private key, in the OpenSSH `authorized_keys`
/// format.
pub fn public_key(pem: &str, passphrase: Option<&str>) -> ApiResult<String> {
    let s = pem.trim_start();
    if s.starts_with(OPENSSH) {
        let k = openssh(s, passphrase)?;
        return k
            .public_key()
            .to_openssh()
            .map_err(|e| invalid(format!("cannot write the public key: {e}")));
    }
    if passphrase.is_some() {
        return Err(pem_passphrase());
    }
    PublicKey::new(pem_public(s)?, "")
        .to_openssh()
        .map_err(|e| invalid(format!("cannot write the public key: {e}")))
}

/// A key that `ssh -i` reads with no prompt: the key as stored, or the
/// OpenSSH key without its passphrase. It goes only to a 0600 file of one
/// run (`super::RunDir`).
pub fn usable_key(pem: &str, passphrase: Option<&str>) -> ApiResult<Zeroizing<String>> {
    let s = pem.trim_start();
    let encrypted = s.starts_with(OPENSSH)
        && PrivateKey::from_openssh(s.trim()).is_ok_and(|k| k.is_encrypted());
    if !encrypted {
        let mut out = Zeroizing::new(pem.to_owned());
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return Ok(out);
    }
    let k = openssh(s, passphrase)?;
    k.to_openssh(LineEnding::LF)
        .map_err(|e| invalid(format!("cannot write the private key: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;

    /// An unencrypted Ed25519 key in the OpenSSH format, from a fixed seed.
    pub fn ed25519_key() -> String {
        let pair = ssh_key::private::Ed25519Keypair::from_seed(&[3u8; 32]);
        PrivateKey::new(pair.into(), "test")
            .unwrap()
            .to_openssh(LineEnding::LF)
            .unwrap()
            .to_string()
    }

    #[test]
    fn openssh_keys_give_their_public_key() {
        let key = ed25519_key();
        let p = public_key(&key, None).unwrap();
        assert!(p.starts_with("ssh-ed25519 AAAA"), "{p}");
        assert!(!p.contains("PRIVATE"));
        assert!(
            public_key(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n",
                None
            )
            .is_err()
        );
        assert!(public_key("not a key", None).is_err());
        assert_eq!(*usable_key(&key, None).unwrap(), key);
    }

    /// A fixed xorshift for the test encryption of a key (salt and IV).
    struct TestRng(u64);

    impl ssh_key::rand_core::TryRng for TestRng {
        type Error = std::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            Ok((self.try_next_u64()? >> 32) as u32)
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            Ok(self.0)
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
            for b in dst {
                *b = self.try_next_u64()?.to_le_bytes()[0];
            }
            Ok(())
        }
    }

    impl ssh_key::rand_core::TryCryptoRng for TestRng {}

    #[test]
    fn a_passphrase_opens_and_strips_an_openssh_key() {
        let text = ed25519_key();
        let key = PrivateKey::from_openssh(text.trim()).unwrap();
        let enc = key
            .encrypt(&mut TestRng(0x9e37_79b9_7f4a_7c15), "s3cret")
            .unwrap()
            .to_openssh(LineEnding::LF)
            .unwrap();
        let want = public_key(&text, None).unwrap();
        assert_eq!(public_key(&enc, Some("s3cret")).unwrap(), want);
        assert!(public_key(&enc, None).is_err());
        assert!(public_key(&enc, Some("wrong")).is_err());
        let plain = usable_key(&enc, Some("s3cret")).unwrap();
        assert!(
            !PrivateKey::from_openssh(plain.trim())
                .unwrap()
                .is_encrypted()
        );
        assert!(usable_key(&enc, Some("wrong")).is_err());
    }

    #[test]
    fn pem_keys_give_their_public_key() {
        use ed25519_dalek::pkcs8::EncodePrivateKey as _;
        // Ed25519 in PKCS#8.
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let pem = sk.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let p = public_key(&pem, None).unwrap();
        assert!(p.starts_with("ssh-ed25519 "), "{p}");
        // P-256 in SEC1 and PKCS#8.
        let ec = p256::SecretKey::from_slice(&[9u8; 32]).unwrap();
        let sec1 = ec.to_sec1_pem(p256::pkcs8::LineEnding::LF).unwrap();
        let p = public_key(&sec1, None).unwrap();
        assert!(p.starts_with("ecdsa-sha2-nistp256 "), "{p}");
        let p8 = ec.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
        assert_eq!(public_key(&p8, None).unwrap(), p);
        // A passphrase on a PEM key, and an encrypted PEM key.
        assert!(public_key(&pem, Some("x")).is_err());
        assert!(
            public_key(
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\nabc\n-----END ENCRYPTED PRIVATE KEY-----\n",
                None
            )
            .is_err()
        );
        assert!(
            public_key(
                "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nabc\n-----END RSA PRIVATE KEY-----\n",
                None
            )
            .is_err()
        );
    }
}
