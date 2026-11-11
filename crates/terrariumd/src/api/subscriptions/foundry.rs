//! A bounded paid test against a canonical Azure resource host only.
use serde_json::json;
use terrarium_core::cloud::{self, CloudSettings};

use crate::error::{ApiError, ApiResult};
use crate::login_session::ApiKey;

fn failure(hint: &str) -> ApiError {
    ApiError::invalid(format!("Foundry test failed. {hint} Nothing was saved."))
}

fn endpoint(settings: &CloudSettings) -> ApiResult<String> {
    if settings.base_url.is_some() {
        return Err(failure("Test calls do not support a custom Foundry base URL. Save through the API without test_connection."));
    }
    cloud::check(cloud::Kind::Foundry, settings).map_err(ApiError::invalid)?;
    let resource = settings.resource.as_deref().ok_or_else(|| failure("Set the Foundry resource name."))?;
    if !cloud::foundry_resource_ok(resource) {
        return Err(failure("Use a Foundry resource name, not a host or URL."));
    }
    Ok(format!("https://{resource}.services.ai.azure.com/anthropic/v1/messages"))
}

pub(super) async fn test(key: &ApiKey, settings: &CloudSettings) -> ApiResult<()> {
    let url = endpoint(settings)?;
    #[cfg(test)]
    let url = super::FOUNDRY_MOCK_URL.try_with(Clone::clone).unwrap_or(url);
    let model = settings.models.get("haiku").ok_or_else(|| failure("Set the haiku deployment name."))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build().map_err(|_| failure("Cannot create the test client."))?;
    // Only inspect the status. Never read or return an upstream response body.
    let response = client.post(url)
        .header("x-api-key", key.key.trim())
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": model, "max_tokens": 1, "messages": [{"role": "user", "content": "Hi"}]}))
        .send().await.map_err(|_| failure("Cannot reach Foundry. Check the resource name and network access."))?;
    if response.status().is_success() {
        return Ok(());
    }
    let hint = match response.status().as_u16() {
        401 | 403 => "Check the API key and resource permissions.",
        404 => "Check the resource and haiku deployment name.",
        429 => "Foundry throttled the request. Try later.",
        _ => "Check the resource and model deployment access.",
    };
    Err(failure(&format!("HTTP {}. {hint}", response.status().as_u16())))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn canonical_resource_host_only() {
        let mut settings = CloudSettings { resource: Some("my-resource".into()), ..Default::default() };
        assert_eq!(endpoint(&settings).unwrap(), "https://my-resource.services.ai.azure.com/anthropic/v1/messages");
        for resource in ["a.b", "a/b", "a@b", "a?b", "a#b", "a%b", "-bad", "bad-", "a_b", "a\\b"] {
            settings.resource = Some(resource.into());
            assert!(endpoint(&settings).is_err());
        }
    }

    #[tokio::test]
    async fn custom_base_url_receives_no_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let settings = CloudSettings { base_url: Some(format!("https://{}", listener.local_addr().unwrap())), ..Default::default() };
        let key = serde_json::from_value(json!({"provider": "foundry", "key": "FAKE-KEY"})).unwrap();
        let error = test(&key, &settings).await.unwrap_err().to_string();
        assert!(error.contains("custom Foundry base URL"));
        assert!(!error.contains("FAKE-KEY"));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept()).await.is_err());
    }
}
