//! Offline release-key verification. The bundle is an untrusted carrier, not a trust root.
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

pub const MAX_BUNDLE_BYTES: usize = 1 << 20;
pub const RELEASE_KEY: Option<&str> = option_env!("TERRARIUM_RELEASE_PUBKEY");

// DER SubjectPublicKeyInfo header for id-ecPublicKey / prime256v1 and a 65-byte point.
const P256_SPKI: &[u8] = &[
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01,
    0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bundle {
    media_type: Option<String>,
    message_signature: Option<MessageSignature>,
    // cosign's legacy sign-blob bundle. Certificates and log entries are not trusted.
    base64_signature: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageSignature {
    message_digest: MessageDigest,
    signature: String,
}

#[derive(Deserialize)]
struct MessageDigest {
    algorithm: String,
    digest: String,
}

/// Verify an ECDSA P-256/SHA-256 cosign sign-blob signature over the asset bytes.
/// No certificate, embedded key, transparency-log entry, or daemon hash can replace the key.
pub fn verify(data: &[u8], bundle: &[u8], key: Option<&str>) -> Result<(), String> {
    use sha2::Digest;
    let key = key.ok_or("this dev build has no release verification key; install manually")?;
    if bundle.is_empty() || bundle.len() > MAX_BUNDLE_BYTES {
        return Err("missing or oversized release signature bundle".into());
    }
    let pem = key.trim().strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|s| s.strip_suffix("-----END PUBLIC KEY-----"))
        .ok_or("invalid built-in release public key")?;
    let der = STANDARD.decode(pem.split_whitespace().collect::<String>())
        .map_err(|_| "invalid built-in release public key")?;
    let point = der.strip_prefix(P256_SPKI).filter(|p| p.len() == 65 && p[0] == 4)
        .ok_or("release public key is not ECDSA P-256")?;
    let bundle: Bundle = serde_json::from_slice(bundle).map_err(|_| "invalid release signature bundle")?;
    let signature = match (bundle.message_signature, bundle.base64_signature) {
        (Some(message), None) => {
            if !matches!(bundle.media_type.as_deref(),
                Some("application/vnd.dev.sigstore.bundle.v0.3+json") |
                Some("application/vnd.dev.sigstore.bundle+json;version=0.3")) {
                return Err("unsupported release signature bundle format".into());
            }
            if message.message_digest.algorithm != "SHA2_256"
                || STANDARD.decode(message.message_digest.digest).ok().as_deref()
                    != Some(sha2::Sha256::digest(data).as_slice()) {
                return Err("release signature digest does not match the asset".into());
            }
            message.signature
        }
        (None, Some(signature)) if bundle.media_type.is_none() => signature,
        _ => return Err("missing or ambiguous release signature".into()),
    };
    let signature = STANDARD.decode(signature).map_err(|_| "invalid release signature encoding")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point)
        .verify(data, &signature).map_err(|_| "release signature verification failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::{rand::SystemRandom, signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING}};

    fn fixture(data: &[u8]) -> (String, Vec<u8>) {
        let rng = SystemRandom::new();
        let private = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, private.as_ref(), &rng).unwrap();
        let mut der = P256_SPKI.to_vec();
        der.extend_from_slice(pair.public_key().as_ref());
        let key = format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----", STANDARD.encode(der));
        let signature = pair.sign(&rng, data).unwrap();
        use sha2::Digest;
        let bundle = serde_json::json!({
            "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
            "messageSignature": {
                "messageDigest": {"algorithm": "SHA2_256", "digest": STANDARD.encode(sha2::Sha256::digest(data))},
                "signature": STANDARD.encode(signature.as_ref())
            }
        });
        (key, serde_json::to_vec(&bundle).unwrap())
    }

    #[test]
    fn valid_bundle_and_legacy_signature() {
        let (key, bundle) = fixture(b"asset");
        verify(b"asset", &bundle, Some(&key)).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
        let legacy = serde_json::to_vec(&serde_json::json!({"base64Signature": value["messageSignature"]["signature"]})).unwrap();
        verify(b"asset", &legacy, Some(&key)).unwrap();
    }

    #[test]
    fn missing_tampered_wrong_key_and_dev_build_are_refused() {
        let (key, bundle) = fixture(b"asset");
        assert!(verify(b"asset", b"", Some(&key)).is_err());
        assert!(verify(b"other bytes", &bundle, Some(&key)).is_err());
        let (other, _) = fixture(b"asset");
        assert!(verify(b"asset", &bundle, Some(&other)).is_err());
        assert!(verify(b"asset", &bundle, None).unwrap_err().contains("install manually"));
        // Replacing the unsigned digest cannot make a signature for other bytes valid.
        let mut value: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
        use sha2::Digest;
        value["messageSignature"]["messageDigest"]["digest"] = STANDARD.encode(sha2::Sha256::digest(b"other bytes")).into();
        assert!(verify(b"other bytes", &serde_json::to_vec(&value).unwrap(), Some(&key)).is_err());
    }
}
