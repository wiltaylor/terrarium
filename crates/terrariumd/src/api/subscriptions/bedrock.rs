//! A bounded, paid InvokeModel check before saving a Bedrock credential.
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use terrarium_core::cloud::CloudSettings;

use crate::error::{ApiError, ApiResult};
use crate::login_session::ApiKey;

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn sign(key: &[u8], text: &str) -> ApiResult<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| ApiError::internal("cannot sign Bedrock test"))?;
    mac.update(text.as_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}

/// AWS URI encoding, including the colons and slashes of a profile ARN.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub(super) async fn test(key: &ApiKey, settings: &CloudSettings) -> ApiResult<()> {
    let region = settings.region.as_deref().unwrap_or("us-east-1");
    let endpoint = settings
        .base_url
        .clone()
        .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"));
    invoke(key, settings, &endpoint).await
}

async fn invoke(key: &ApiKey, settings: &CloudSettings, endpoint: &str) -> ApiResult<()> {
    let region = settings.region.as_deref().unwrap_or("us-east-1");
    let model = settings
        .models
        .get("haiku")
        .ok_or_else(|| ApiError::invalid("Bedrock test needs cloud.models.haiku"))?;
    let url = reqwest::Url::parse(&format!(
        "{}/model/{}/invoke",
        endpoint.trim_end_matches('/'),
        encode(model)
    ))
    .map_err(|_| ApiError::invalid("invalid Bedrock test endpoint"))?;
    let host = url
        .host_str()
        .ok_or_else(|| ApiError::invalid("invalid Bedrock host"))?;
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let body = r#"{"anthropic_version":"bedrock-2023-05-31","max_tokens":1,"messages":[{"role":"user","content":"Hi"}]}"#;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ApiError::internal("cannot create Bedrock test client"))?;
    let mut request = client
        .post(url.clone())
        .header("content-type", "application/json");
    if let Some(id) = &key.key_id {
        let now = Utc::now();
        let date = now.format("%Y%m%d").to_string();
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let scope = format!("{date}/{region}/bedrock/aws4_request");
        let headers = "content-type;host;x-amz-date";
        // Bedrock is not S3: SigV4 double-encodes the path's percent escapes.
        let canonical_path = url.path().replace('%', "%25");
        let canonical = format!(
            "POST\n{canonical_path}\n\ncontent-type:application/json\nhost:{host}\nx-amz-date:{timestamp}\n\n{headers}\n{}",
            hex(Sha256::digest(body.as_bytes()))
        );
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            hex(Sha256::digest(canonical.as_bytes()))
        );
        let date_key = sign(format!("AWS4{}", key.key.trim()).as_bytes(), &date)?;
        let region_key = sign(&date_key, region)?;
        let service_key = sign(&region_key, "bedrock")?;
        let signing_key = sign(&service_key, "aws4_request")?;
        let signature = hex(sign(&signing_key, &to_sign)?);
        request = request.header("x-amz-date", timestamp).header("authorization", format!(
            "AWS4-HMAC-SHA256 Credential={id}/{scope}, SignedHeaders={headers}, Signature={signature}"
        ));
    } else {
        request = request.bearer_auth(key.key.trim());
    }
    let response = request.body(body).send().await.map_err(|_| {
        ApiError::invalid("Bedrock test could not connect or timed out. Check the region and endpoint. Nothing was saved.")
    })?;
    if response.status().is_success() {
        return Ok(());
    }
    let hint = match response.status().as_u16() {
        401 | 403 => "Check the IAM key pair, bedrock:InvokeModel permission, and model access.",
        404 => "Check the region and haiku inference-profile model ID.",
        429 => "Bedrock throttled the test. Try again later.",
        _ => "Check the region, model access, and inference-profile model ID.",
    };
    Err(ApiError::invalid(format!(
        "Bedrock test failed (HTTP {}). {hint} Nothing was saved.",
        response.status().as_u16()
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn mocked_save_check_success_and_safe_error() {
        for status in [200, 403, 404, 429, 500] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let n = stream.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]);
                assert!(
                    request.contains(
                        "POST /model/us.anthropic.claude-haiku-4-5-20251001-v1%3A0/invoke"
                    )
                );
                assert!(request.contains("Credential=FAKE-ID/"));
                assert!(!request.contains("FAKE-SECRET"));
                let body = "FAKE-SECRET FAKE-ID upstream error";
                stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let key: ApiKey = serde_json::from_value(serde_json::json!({
                "provider": "bedrock", "key": "FAKE-SECRET", "key_id": "FAKE-ID"
            }))
            .unwrap();
            let settings = CloudSettings {
                models: terrarium_core::cloud::bedrock_models(),
                ..Default::default()
            };
            let result = invoke(&key, &settings, &endpoint).await;
            if status == 200 {
                assert!(result.is_ok());
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(&status.to_string()));
                assert!(!error.contains("FAKE-") && !error.contains(&endpoint));
                assert!(error.contains("Nothing was saved"));
            }
            server.await.unwrap();
        }
    }
}
