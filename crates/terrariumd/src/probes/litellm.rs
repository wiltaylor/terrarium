//! LiteLLM virtual-key budget, from the subscription's own proxy only.
use std::path::Path;
use std::net::{SocketAddr, ToSocketAddrs};

use chrono::{DateTime, Utc};
use serde_json::json;
use terrarium_core::HarnessId;

use super::{Credentials, HttpRequest, ProbeError, QuotaProbe, QuotaWindow};

pub const NAME: &str = "litellm-key-spend";
const NO_SPEND: &str = "no spend from LiteLLM";

/// HTTP requires every resolved address to be private. Call again at connect
/// time and pin the result. This does not pin a harness's independent client.
pub fn check_base_url(base: &str) -> Result<(), String> {
    checked_address(base, None).map(|_| ())
}

fn checked_address(
    base: &str,
    resolver: Option<crate::webhooks::net::Resolver>,
) -> Result<Option<SocketAddr>, String> {
    let base = terrarium_core::selfhosted::check_http_url("base_url", base, false)?;
    let url = reqwest::Url::parse(&base).map_err(|_| "invalid LiteLLM base URL")?;
    if url.scheme() == "https" {
        return Ok(None);
    }
    let port = url.port_or_known_default().ok_or("missing port")?;
    let ips = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![ip.into()],
        Some(url::Host::Ipv6(ip)) => vec![ip.into()],
        Some(url::Host::Domain(host)) => match resolver {
            Some(resolve) => resolve(host, port),
            None => (host, port)
                .to_socket_addrs()
                .map_err(|_| "cannot resolve LiteLLM HTTP host")?
                .map(|addr| addr.ip())
                .collect(),
        },
        None => return Err("missing host".into()),
    };
    if !ips.is_empty() && ips.iter().all(|ip| private_ip(*ip)) {
        Ok(Some(SocketAddr::new(ips[0], port)))
    } else {
        Err("LiteLLM HTTP needs a private network address; public and link-local addresses require HTTPS".into())
    }
}

fn private_ip(ip: std::net::IpAddr) -> bool {
    // Reuse the network address classification, but allow only D1's ranges.
    // Mapped IPv6 is not one of those ranges.
    if let std::net::IpAddr::V6(v6) = ip {
        return v6.is_loopback() || v6.segments()[0] & 0xfe00 == 0xfc00;
    }
    matches!(
        crate::webhooks::net::private_reason(ip),
        Some("loopback" | "private (RFC 1918)" | "shared (CGNAT)" | "unique-local (ULA)")
    )
}

pub struct LiteLlmKeySpend {
    url: String,
}

pub fn registered() -> LiteLlmKeySpend {
    LiteLlmKeySpend { url: String::new() }
}

impl LiteLlmKeySpend {
    pub fn new(base: &str) -> Result<Self, ProbeError> {
        let base = terrarium_core::selfhosted::check_http_url("base_url", base, false)
            .map_err(|_| unavailable())?;
        check_base_url(&base).map_err(|_| unavailable())?;
        Ok(Self {
            url: format!("{}/key/info", base.trim_end_matches('/')),
        })
    }
}

pub fn unavailable() -> ProbeError {
    ProbeError::Parse(NO_SPEND.into())
}

impl QuotaProbe for LiteLlmKeySpend {
    fn name(&self) -> &'static str {
        NAME
    }
    fn harness(&self) -> HarnessId {
        HarnessId::Pi
    }
    fn credentials(&self, _home: &Path) -> Result<Credentials, ProbeError> {
        Err(ProbeError::NoCredential(NO_SPEND.into()))
    }
    fn accepts_api_key(&self) -> bool {
        true
    }
    fn request(&self, creds: &Credentials) -> HttpRequest {
        HttpRequest {
            url: self.url.clone(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", creds.token())),
                ("Accept".into(), "application/json".into()),
            ],
        }
    }
    fn parse(&self, body: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
        let v: serde_json::Value = serde_json::from_str(body).map_err(|_| unavailable())?;
        let info = &v["info"];
        let spend = info["spend"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(unavailable)?;
        let budget = info["max_budget"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(unavailable)?;
        let duration = info["budget_duration"].as_str();
        let resets_at = if duration.is_some_and(|s| !s.is_empty()) {
            Some(
                DateTime::parse_from_rfc3339(
                    info["budget_reset_at"].as_str().ok_or_else(unavailable)?,
                )
                .map_err(|_| unavailable())?
                .with_timezone(&Utc),
            )
        } else {
            None
        };
        let used_pct = if budget > 0.0 {
            (spend / budget * 100.0).clamp(0.0, 100.0)
        } else if spend > 0.0 {
            100.0
        } else {
            0.0
        };
        Ok(vec![QuotaWindow {
            window: "key-spend".into(),
            used_pct,
            resets_at,
            raw: json!({"spend": spend, "max_budget": budget,
                "budget_duration": duration, "currency": "USD"}),
        }])
    }
}

/// A separate client: no redirects or proxies, and a 1 MiB response cap.
pub async fn fetch(
    probe: &LiteLlmKeySpend,
    creds: &Credentials,
) -> Result<Vec<QuotaWindow>, ProbeError> {
    fetch_with_resolver(probe, creds, None).await
}

async fn fetch_with_resolver(
    probe: &LiteLlmKeySpend,
    creds: &Credentials,
    resolver: Option<crate::webhooks::net::Resolver>,
) -> Result<Vec<QuotaWindow>, ProbeError> {
    creds.check_fresh(Utc::now())?;
    let base = probe.url.clone();
    let addr = tokio::task::spawn_blocking(move || checked_address(&base, resolver))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    let url = reqwest::Url::parse(&probe.url).map_err(|_| unavailable())?;
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20));
    if let (Some(addr), Some(url::Host::Domain(host))) = (addr, url.host()) {
        builder = builder.resolve(host, addr);
    }
    let client = builder
        .build()
        .map_err(|_| unavailable())?;
    let mut response = client
        .get(&probe.url)
        .bearer_auth(creds.token())
        .send()
        .await
        .map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if body.len() + chunk.len() > 1024 * 1024 {
            return Err(unavailable());
        }
        body.extend_from_slice(&chunk);
    }
    probe.parse(std::str::from_utf8(&body).map_err(|_| unavailable())?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn private_http_ranges_and_edges() {
        for ip in [
            "10.0.0.0",
            "10.255.255.255",
            "172.16.0.0",
            "172.31.255.255",
            "192.168.0.0",
            "192.168.255.255",
            "127.0.0.0",
            "127.255.255.255",
            "100.64.0.0",
            "100.127.255.255",
            "[::1]",
            "[fc00::]",
            "[fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]",
        ] {
            assert!(check_base_url(&format!("http://{ip}:8000")).is_ok(), "{ip}");
        }
        for ip in [
            "9.255.255.255",
            "11.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "169.254.1.1",
            "0.0.0.0",
            "224.0.0.1",
            "[::]",
            "[fbff::]",
            "[fe00::]",
            "[fe80::1]",
            "[::ffff:127.0.0.1]",
            "[64:ff9b::a00:1]",
            "[2002:a00:1::]",
            "private.lan",
            "public.example",
        ] {
            assert!(check_base_url(&format!("http://{ip}")).is_err(), "{ip}");
        }
        assert!(check_base_url("https://public.example").is_ok());
        assert!(check_base_url("http://localhost").is_ok());
    }

    #[tokio::test]
    async fn probe_private_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // This name cannot resolve in real DNS. A successful request proves
        // the client uses the checked override, retaining the original Host.
        let base = format!("http://private.invalid:{}", listener.local_addr().unwrap().port());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let resolver: crate::webhooks::net::Resolver = std::sync::Arc::new(move |_, _| {
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![if n < 2 { "127.0.0.1" } else { "8.8.8.8" }.parse().unwrap()]
        });
        assert!(checked_address(&base, Some(resolver.clone())).is_ok());
        let probe = LiteLlmKeySpend { url: format!("{base}/key/info") };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.starts_with("GET /key/info "));
            assert!(request.contains("Bearer test-key"));
            assert!(request.contains("host: private.invalid:"));
            let body = r#"{"info":{"spend":2,"max_budget":10}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let windows = fetch_with_resolver(&probe, &Credentials::new("test-key"), Some(resolver.clone())).await.unwrap();
        assert_eq!(windows[0].used_pct, 20.0);
        server.await.unwrap();
        // A subsequent connection rechecks DNS and refuses the rebound answer.
        assert!(fetch_with_resolver(&probe, &Credentials::new("test-key"), Some(resolver)).await.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn all_dns_answers_must_be_private() {
        for answers in [
            vec![],
            vec!["127.0.0.1", "8.8.8.8"],
            vec!["8.8.8.8", "127.0.0.1"],
            vec!["10.0.0.1", "fe80::1"],
            vec!["10.0.0.1", "::ffff:127.0.0.1"],
        ] {
            let resolver: crate::webhooks::net::Resolver = std::sync::Arc::new(move |_, _| {
                answers.iter().map(|ip| ip.parse().unwrap()).collect()
            });
            assert!(checked_address("http://private.invalid", Some(resolver)).is_err());
        }
        let resolver: crate::webhooks::net::Resolver = std::sync::Arc::new(|_, _| {
            vec!["10.0.0.1".parse().unwrap(), "fd00::1".parse().unwrap()]
        });
        assert_eq!(checked_address("http://private.invalid:8000", Some(resolver)).unwrap().unwrap(), "10.0.0.1:8000".parse().unwrap());
    }
    #[test]
    fn budget_and_reset() {
        let p = LiteLlmKeySpend::new("https://proxy.example/prefix/").unwrap();
        let w = p.parse(r#"{"info":{"spend":2.5,"max_budget":10,"budget_duration":"1d","budget_reset_at":"2026-11-12T02:00:00+02:00","key":"must-not-store"}}"#).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].used_pct, 25.0);
        assert_eq!(
            w[0].resets_at.unwrap().to_rfc3339(),
            "2026-11-12T00:00:00+00:00"
        );
        assert_eq!(w[0].raw["spend"], 2.5);
        assert!(!w[0].raw.to_string().contains("must-not-store"));
        let req = p.request(&Credentials::new("test-key"));
        assert_eq!(req.url, "https://proxy.example/prefix/key/info");
        assert_eq!(req.headers[0].1, "Bearer test-key");
    }
    #[test]
    fn incomplete_and_invalid_answers() {
        let p = LiteLlmKeySpend::new("https://proxy.example").unwrap();
        for body in [
            "not JSON",
            "{}",
            r#"{"info":{"spend":null,"max_budget":10}}"#,
            r#"{"info":{"spend":-1,"max_budget":10}}"#,
            r#"{"info":{"spend":1,"max_budget":10,"budget_duration":"1d","budget_reset_at":"bad"}}"#,
        ] {
            assert_eq!(p.parse(body).unwrap_err(), unavailable());
        }
        for (spend, budget, pct) in [(0, 0, 0.0), (1, 0, 100.0), (12, 10, 100.0)] {
            let w = p
                .parse(&json!({"info":{"spend":spend,"max_budget":budget}}).to_string())
                .unwrap();
            assert_eq!(w[0].used_pct, pct);
            assert_eq!(w[0].resets_at, None);
        }
    }
    #[test]
    fn rejects_unsafe_urls() {
        for base in [
            "http://proxy.example",
            "https://user:password@proxy.example",
            "https://proxy.example?key=secret",
            "https://proxy.example/#secret",
        ] {
            assert!(LiteLlmKeySpend::new(base).is_err());
        }
    }
}
