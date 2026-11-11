//! Bounded Vertex check. Credentials go only to Google's canonical hosts.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rsa::{RsaPrivateKey, pkcs8::DecodePrivateKey, traits::PublicKeyParts};
use serde::Deserialize;
use serde_json::json;
use signature::{SignatureEncoding, RandomizedSigner};
use terrarium_core::cloud::CloudSettings;

use crate::error::{ApiError, ApiResult};
use crate::login_session::ApiKey;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

fn failure(hint: &str) -> ApiError {
    ApiError::invalid(format!("Vertex test failed. {hint} Nothing was saved."))
}

#[derive(Deserialize)]
struct ServiceAccount {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key: String,
}

fn assertion(key: &str) -> ApiResult<String> {
    let account: ServiceAccount = serde_json::from_str(key)
        .map_err(|_| failure("Use a service-account JSON key with client_email and private_key."))?;
    if account.kind != "service_account" || account.client_email.is_empty() {
        return Err(failure("Use a service-account JSON key."));
    }
    if account.private_key.len() > 8192 {
        return Err(failure("The service-account private key is too large."));
    }
    let private = RsaPrivateKey::from_pkcs8_pem(&account.private_key)
        .map_err(|_| failure("The service-account private key is not a valid RSA PEM key."))?;
    if !(2048..=4096).contains(&private.n().bits()) {
        return Err(failure("Use a 2048 to 4096 bit service-account RSA key."));
    }
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": account.client_email,
        "scope": "https://www.googleapis.com/auth/cloud-platform",
        "aud": TOKEN_URL,
        "iat": now,
        "exp": now + 300,
    });
    let unsigned = format!("{}.{}", URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#), URL_SAFE_NO_PAD.encode(claims.to_string()));
    let signing = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(private);
    let signature = signing.try_sign_with_rng(&mut rand::rng(), unsigned.as_bytes())
        .map_err(|_| failure("Cannot sign with this service-account key."))?;
    Ok(format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())))
}

fn endpoint(settings: &CloudSettings) -> ApiResult<String> {
    // Do not send a newly entered key or token to a user-controlled endpoint.
    if settings.base_url.is_some() {
        return Err(failure("Test calls do not support a custom Vertex base URL."));
    }
    terrarium_core::cloud::check(terrarium_core::cloud::Kind::Gcp, settings)
        .map_err(ApiError::invalid)?;
    let region = settings.region.as_deref().unwrap_or("us-east5");
    let host = match region {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{region}.rep.googleapis.com"),
        _ => format!("{region}-aiplatform.googleapis.com"),
    };
    let project = settings.project_id.as_deref().ok_or_else(|| failure("Set the Google Cloud project ID."))?;
    // word_ok allows slashes in model IDs, but URL path components must not.
    let component = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b)) && !s.is_empty() && s != "." && s != "..";
    let model = settings.models.get("haiku").ok_or_else(|| failure("Set the haiku Vertex model ID."))?;
    if !component(project) || !component(model) {
        return Err(failure("Project and model IDs must be single path components."));
    }
    Ok(format!("https://{host}/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict"))
}

pub(super) async fn test(key: &ApiKey, settings: &CloudSettings) -> ApiResult<()> {
    let url = endpoint(settings)?;
    let jwt = assertion(key.key.trim())?;
    tokio::time::timeout(std::time::Duration::from_secs(20), invoke(&jwt, TOKEN_URL, &url))
        .await.map_err(|_| failure("The request timed out."))?
}

async fn invoke(jwt: &str, token_url: &str, url: &str) -> ApiResult<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build().map_err(|_| failure("Cannot create the test client."))?;
    let mut response = client.post(token_url)
        .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", jwt)])
        .send().await.map_err(|_| failure("Cannot reach Google authentication."))?;
    if !response.status().is_success() {
        return Err(failure("Google authentication refused the service-account key."));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failure("Cannot read Google authentication response."))? {
        if bytes.len() + chunk.len() > 16384 {
            return Err(failure("Google authentication response was too large."));
        }
        bytes.extend_from_slice(&chunk);
    }
    #[derive(Deserialize)]
    struct Token { access_token: String }
    let token: Token = serde_json::from_slice(&bytes).map_err(|_| failure("Invalid Google authentication response."))?;
    let response = client.post(url).bearer_auth(token.access_token)
        .header("content-type", "application/json")
        .body(json!({"anthropic_version": "vertex-2023-10-16", "max_tokens": 1, "messages": [{"role": "user", "content": "Hi"}]}).to_string())
        .send().await.map_err(|_| failure("Cannot reach Vertex. Check the region."))?;
    if response.status().is_success() { return Ok(()); }
    let hint = match response.status().as_u16() {
        401 | 403 => "Check the Vertex AI User role (roles/aiplatform.user), API enablement, and model access.",
        404 => "Check the project, region, and haiku model ID.",
        429 => "Vertex throttled the request. Try later.",
        _ => "Check the project, region, and model access.",
    };
    Err(failure(&format!("HTTP {}. {hint}", response.status().as_u16())))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn service_account_jwt_has_valid_signature_and_fixed_google_audience() {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        use signature::Verifier;
        let private = RsaPrivateKey::new(&mut rand::rng(), 2048).unwrap();
        let public = private.to_public_key();
        let account = json!({
            "type": "service_account", "client_email": "test@example.invalid",
            "private_key": private.to_pkcs8_pem(LineEnding::LF).unwrap().as_str(),
            "token_uri": "https://attacker.invalid/token",
        });
        let jwt = assertion(&account.to_string()).unwrap();
        let parts: Vec<_> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], TOKEN_URL);
        assert_eq!(claims["iss"], "test@example.invalid");
        assert_eq!(claims["scope"], "https://www.googleapis.com/auth/cloud-platform");
        assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 300);
        let sig = rsa::pkcs1v15::Signature::try_from(URL_SAFE_NO_PAD.decode(parts[2]).unwrap().as_slice()).unwrap();
        rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }

    #[test]
    fn canonical_hosts_and_path_components_only() {
        let mut s = CloudSettings {
            project_id: Some("my-project".into()),
            region: Some("global".into()),
            models: terrarium_core::cloud::vertex_models(),
            ..Default::default()
        };
        assert!(endpoint(&s).unwrap().starts_with("https://aiplatform.googleapis.com/"));
        s.region = Some("us-east5".into());
        assert!(endpoint(&s).unwrap().starts_with("https://us-east5-aiplatform.googleapis.com/"));
        for region in ["us", "eu"] {
            s.region = Some(region.into());
            assert!(endpoint(&s).unwrap().starts_with(&format!("https://aiplatform.{region}.rep.googleapis.com/")));
        }
        s.base_url = Some("https://attacker.invalid".into());
        assert!(endpoint(&s).is_err());
        s.base_url = None;
        for id in ["../other", "a?x", "a#b", ".."] {
            s.models.insert("haiku".into(), id.into());
            assert!(endpoint(&s).is_err());
        }
        assert!(assertion(r#"{"type":"authorized_user"}"#).is_err());
    }

    #[tokio::test]
    async fn mocked_auth_and_vertex_success_and_safe_error() {
        for status in [200, 403, 404, 429, 500] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                for auth in [true, false] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = vec![0; 8192];
                    let n = stream.read(&mut bytes).await.unwrap();
                    let request = String::from_utf8_lossy(&bytes[..n]);
                    let (code, body) = if auth {
                        assert!(request.contains("assertion=FAKE-JWT"));
                        (200, r#"{"access_token":"FAKE-TOKEN"}"#)
                    } else {
                        assert!(request.contains("Bearer FAKE-TOKEN"));
                        assert!(request.contains("vertex-2023-10-16"));
                        assert!(request.contains("\"max_tokens\":1"));
                        (status, "FAKE-TOKEN upstream secret")
                    };
                    stream.write_all(format!("HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            });
            let result = invoke("FAKE-JWT", &url, &url).await;
            if status == 200 {
                assert!(result.is_ok());
            } else {
                let e = result.unwrap_err().to_string();
                assert!(e.contains(&status.to_string()));
                assert!(!e.contains("FAKE") && !e.contains(&url));
                assert!(e.contains("Nothing was saved"));
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreign_base_url_is_refused_without_network() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let settings = CloudSettings {
            project_id: Some("my-project".into()),
            base_url: Some(format!("https://{}", listener.local_addr().unwrap())),
            models: terrarium_core::cloud::vertex_models(),
            ..Default::default()
        };
        let key: ApiKey = serde_json::from_value(json!({"provider": "gcp", "key": "FAKE-PRIVATE-KEY"})).unwrap();
        let error = test(&key, &settings).await.unwrap_err().to_string();
        assert!(error.contains("custom Vertex base URL"));
        assert!(!error.contains("FAKE"));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept()).await.is_err());
    }
}
