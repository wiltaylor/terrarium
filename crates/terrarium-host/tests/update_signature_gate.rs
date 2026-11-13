//! The shared pre-staging gate used by both Windows and Linux self_update.
//! This target also cross-compiles as a native Windows test executable.
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use terrarium_host::update_signature::download_and_verify;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn signed(data: &[u8]) -> (String, Vec<u8>) {
    let rng = SystemRandom::new();
    let private = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, private.as_ref(), &rng).unwrap();
    let mut der = vec![
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    der.extend_from_slice(pair.public_key().as_ref());
    let key = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----",
        STANDARD.encode(der)
    );
    let signature = pair.sign(&rng, data).unwrap();
    let bundle = serde_json::to_vec(
        &serde_json::json!({"base64Signature": STANDARD.encode(signature.as_ref())}),
    )
    .unwrap();
    (key, bundle)
}

async fn gate(data: &[u8], key: &str, bundle: Vec<u8>, status: u16) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/asset.sigstore.json",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut bytes = [0; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut bytes).await.unwrap();
            assert!(n > 0 && request.len() < 16384);
            request.extend_from_slice(&bytes[..n]);
        }
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bundle.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(&bundle).await.unwrap();
    });
    let result = download_and_verify(data, &url, None, Some(key))
        .await
        .map_err(|e| e.to_string());
    server.await.unwrap();
    result
}

#[tokio::test]
async fn signed_asset_passes_the_shared_windows_update_gate() {
    let (key, bundle) = signed(b"Windows asset");
    gate(b"Windows asset", &key, bundle, 200).await.unwrap();
}

#[tokio::test]
async fn missing_bundle_is_refused_before_windows_staging() {
    let (key, _) = signed(b"Windows asset");
    assert!(
        gate(b"Windows asset", &key, vec![], 404)
            .await
            .unwrap_err()
            .contains("404")
    );
}

#[tokio::test]
async fn malformed_bundle_is_refused_before_windows_staging() {
    let (key, _) = signed(b"Windows asset");
    assert!(
        gate(b"Windows asset", &key, b"not JSON".to_vec(), 200)
            .await
            .unwrap_err()
            .contains("invalid release signature bundle")
    );
}

#[tokio::test]
async fn signature_for_other_bytes_is_refused_before_windows_staging() {
    let (key, bundle) = signed(b"other asset");
    assert!(
        gate(b"Windows asset", &key, bundle, 200)
            .await
            .unwrap_err()
            .contains("release signature verification failed")
    );
}

#[tokio::test]
async fn signature_from_other_key_is_refused_before_windows_staging() {
    let (key, _) = signed(b"Windows asset");
    let (_, bundle) = signed(b"Windows asset");
    assert!(
        gate(b"Windows asset", &key, bundle, 200)
            .await
            .unwrap_err()
            .contains("release signature verification failed")
    );
}
