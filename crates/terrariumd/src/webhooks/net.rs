//! The network side of a webhook delivery: the URL rules, the address
//! rules (`TERRARIUM_WEBHOOK_PRIVATE`), and one HTTP request.
//!
//! The SSRF stance (docs/security-review.md, "Webhooks"):
//!
//! - Only a project_admin of the project, a sysadmin, or the operator sets
//!   a webhook URL (the rights of a trigger write).
//! - `http` and `https` only; no user or password in the URL.
//! - The host is resolved once for each attempt, and the request connects
//!   to that address ([`resolve`], `reqwest::ClientBuilder::resolve`), so
//!   a DNS answer that changes between the check and the connect (DNS
//!   rebinding) cannot move the request. No proxy from the environment.
//! - No redirect is followed: a `3xx` answer is a failed attempt.
//! - [`PrivateMode::Deny`] refuses loopback, link-local, private (RFC
//!   1918, CGNAT), unique-local (ULA), unspecified, multicast, and the
//!   ports of the daemon's own listeners. The default is
//!   [`PrivateMode::Allow`]: a self-hosted receiver on the LAN is common.
//! - A time limit for the whole request, at most [`READ_MAX`] bytes of the
//!   answer are read.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use reqwest::Url;

use crate::store::webhooks::{Attempt, Class};

/// `TERRARIUM_WEBHOOK_PRIVATE`: `allow` (the default) or `deny`.
pub const ENV_PRIVATE: &str = "TERRARIUM_WEBHOOK_PRIVATE";
/// The most bytes of an answer that an attempt reads.
pub const READ_MAX: usize = 64 * 1024;

/// May a webhook reach a private address?
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PrivateMode {
    /// Any address (the default).
    #[default]
    Allow,
    /// Public addresses only, and not the daemon's own ports.
    Deny,
}

impl PrivateMode {
    /// The mode of [`ENV_PRIVATE`]. A bad value is `deny`, the safe one.
    pub fn from_env() -> Self {
        match std::env::var(ENV_PRIVATE) {
            Err(_) => Self::Allow,
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" | "allow" => Self::Allow,
                "deny" => Self::Deny,
                other => {
                    tracing::warn!("{ENV_PRIVATE}={other} is not allow or deny: deny is used");
                    Self::Deny
                }
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Parse and check a webhook URL: `http` or `https`, a host, no user or
/// password.
pub fn check_url(raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    let u = Url::parse(raw).map_err(|e| format!("bad URL: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!(
            "the scheme `{}` is not allowed: use http or https",
            u.scheme()
        ));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(
            "a URL with a user or a password is not allowed: put the credential in a header \
             from a secret"
                .into(),
        );
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err("the URL has no host".into());
    }
    Ok(u)
}

/// The URL for the log: scheme, host, port, and path. A query shows as
/// `?…`, so a token in it stays out of the log.
pub fn log_url(u: &Url) -> String {
    let mut s = format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default());
    if let Some(p) = u.port() {
        s.push_str(&format!(":{p}"));
    }
    s.push_str(u.path());
    if u.query().is_some() {
        s.push_str("?…");
    }
    s
}

/// The log URL of a URL from a secret: the host only.
pub fn log_url_secret(u: &Url, secret: &str) -> String {
    format!(
        "{}://{}/… (secret {secret})",
        u.scheme(),
        u.host_str().unwrap_or_default()
    )
}

fn v4_private(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_unspecified() || o[0] == 0 {
        Some("unspecified")
    } else if ip.is_link_local() {
        Some("link-local")
    } else if ip.is_private() {
        Some("private (RFC 1918)")
    } else if o[0] == 100 && (64..128).contains(&o[1]) {
        Some("shared (CGNAT)")
    } else if ip.is_multicast() || ip.is_broadcast() || o[0] >= 240 {
        Some("multicast or reserved")
    } else {
        None
    }
}

/// Why `ip` is not a public address, or `None` when it is.
pub fn private_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => v4_private(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_private(v4);
            }
            let seg = v6.segments();
            if v6.is_loopback() {
                Some("loopback")
            } else if v6.is_unspecified() {
                Some("unspecified")
            } else if seg[0] & 0xffc0 == 0xfe80 {
                Some("link-local")
            } else if seg[0] & 0xfe00 == 0xfc00 {
                Some("unique-local (ULA)")
            } else if v6.is_multicast() {
                Some("multicast or reserved")
            } else if seg[0] == 0x2001 && seg[1] == 0x0db8 {
                Some("documentation")
            } else {
                // NAT64 and 6to4 hide an IPv4 address.
                match (seg[0], seg[1]) {
                    (0x0064, 0xff9b) => v4_private(Ipv4Addr::new(
                        (seg[6] >> 8) as u8,
                        seg[6] as u8,
                        (seg[7] >> 8) as u8,
                        seg[7] as u8,
                    )),
                    (0x2002, _) => v4_private(Ipv4Addr::new(
                        (seg[1] >> 8) as u8,
                        seg[1] as u8,
                        (seg[2] >> 8) as u8,
                        seg[2] as u8,
                    )),
                    _ => None,
                }
            }
        }
    }
}

/// A DNS resolver: the host and the port to the addresses. The tests put
/// a fake one in place (`crate::webhooks::Runtime::set_resolver`).
pub type Resolver = std::sync::Arc<dyn Fn(&str, u16) -> Vec<IpAddr> + Send + Sync>;

/// Resolve the host of `u` once and pick the address to connect to: the
/// first one that `mode` allows. An IP literal is its own address.
pub async fn resolve(
    u: &Url,
    mode: PrivateMode,
    own_ports: &[u16],
    resolver: Option<Resolver>,
) -> Result<SocketAddr, String> {
    let port = u
        .port_or_known_default()
        .ok_or_else(|| "the URL has no port".to_owned())?;
    let host = u.host().ok_or_else(|| "the URL has no host".to_owned())?;
    let ips: Vec<IpAddr> = match host {
        url::Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
        url::Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
        url::Host::Domain(d) => match resolver {
            Some(r) => r(d, port),
            None => tokio::net::lookup_host((d, port))
                .await
                .map_err(|e| format!("cannot resolve `{d}`: {e}"))?
                .map(|a| a.ip())
                .collect(),
        },
    };
    if ips.is_empty() {
        return Err(format!(
            "`{}` has no address",
            u.host_str().unwrap_or_default()
        ));
    }
    if mode == PrivateMode::Allow {
        return Ok(SocketAddr::new(ips[0], port));
    }
    if own_ports.contains(&port) {
        return Err(format!(
            "port {port} is a port of the daemon's own listeners, and {ENV_PRIVATE}=deny"
        ));
    }
    let mut why = Vec::new();
    for ip in &ips {
        match private_reason(*ip) {
            None => return Ok(SocketAddr::new(*ip, port)),
            Some(r) => why.push(format!("{ip} ({r})")),
        }
    }
    Err(format!(
        "`{}` resolves only to addresses that {ENV_PRIVATE}=deny blocks: {}",
        u.host_str().unwrap_or_default(),
        why.join(", ")
    ))
}

/// One request of a delivery, ready to send.
#[derive(Clone)]
pub struct Request {
    pub method: reqwest::Method,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

/// The `Retry-After` of an answer, in seconds: a number of seconds or an
/// HTTP date.
pub fn retry_after(v: &str, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let v = v.trim();
    if let Ok(n) = v.parse::<i64>() {
        return Some(n.max(0));
    }
    let t = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    Some((t.with_timezone(&chrono::Utc) - now).num_seconds().max(0))
}

/// Send `req` to `addr`. The answer class: `2xx` is ok; a `5xx`, a `429`,
/// a time-out, or a network error is a retry; any other answer (a `3xx`
/// too: no redirect is followed) is a failure.
pub async fn send(req: Request, addr: SocketAddr) -> Attempt {
    let started = Instant::now();
    let ms = |s: Instant| i64::try_from(s.elapsed().as_millis()).unwrap_or(i64::MAX);
    let mut b = reqwest::Client::builder()
        .timeout(req.timeout)
        .connect_timeout(req.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("terrarium/", env!("CARGO_PKG_VERSION")));
    if let Some(url::Host::Domain(d)) = req.url.host() {
        b = b.resolve(d, SocketAddr::new(addr.ip(), 0));
    }
    let client = match b.build() {
        Ok(c) => c,
        Err(e) => {
            return Attempt {
                class: Class::Fail,
                http_status: None,
                duration_ms: 0,
                response: None,
                error: Some(format!("cannot make the HTTP client: {e}")),
                retry_after: None,
            };
        }
    };
    let mut r = client.request(req.method.clone(), req.url.clone());
    for (k, v) in &req.headers {
        r = r.header(k.as_str(), v.as_str());
    }
    let sent = r.body(req.body.clone()).send().await;
    let mut resp = match sent {
        Ok(r) => r,
        Err(e) => {
            let what = if e.is_timeout() {
                format!("timed out after {} s", req.timeout.as_secs())
            } else if e.is_connect() {
                format!("cannot connect to {addr}: {}", source_text(&e))
            } else {
                format!("request failed: {}", source_text(&e))
            };
            return Attempt {
                class: Class::Retry,
                http_status: None,
                duration_ms: ms(started),
                response: None,
                error: Some(what),
                retry_after: None,
            };
        }
    };
    let status = resp.status();
    let after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| retry_after(v, chrono::Utc::now()));
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut body: Vec<u8> = Vec::new();
    let mut read_error = None;
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                let room = READ_MAX - body.len();
                body.extend_from_slice(&c[..c.len().min(room)]);
                if body.len() >= READ_MAX {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                read_error = Some(if e.is_timeout() {
                    format!(
                        "timed out after {} s while reading the answer",
                        req.timeout.as_secs()
                    )
                } else {
                    format!("reading the answer failed: {}", source_text(&e))
                });
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&body[..body.len().min(4096)]).into_owned();
    let response = Some(text).filter(|t| !t.is_empty());
    let code = status.as_u16();
    let (class, error) = if status.is_success() {
        (Class::Ok, read_error)
    } else if status.is_redirection() {
        (
            Class::Fail,
            Some(format!(
                "status {code} (a redirect{}); Terrarium does not follow redirects, so give the \
                 final URL",
                location
                    .map(|l| format!(" to {}", crate::store::webhooks::cut(&l, 200)))
                    .unwrap_or_default()
            )),
        )
    } else if status.is_server_error() || code == 429 {
        (Class::Retry, Some(format!("status {code}")))
    } else {
        (Class::Fail, Some(format!("status {code}")))
    };
    Attempt {
        class,
        http_status: Some(code),
        duration_ms: ms(started),
        response,
        error,
        retry_after: after,
    }
}

/// The deepest cause of a reqwest error: the useful part ("connection
/// refused"), with no URL.
fn source_text(e: &reqwest::Error) -> String {
    let mut s: &dyn std::error::Error = e;
    while let Some(next) = s.source() {
        s = next;
    }
    s.to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert!(check_url("https://hooks.example/x?a=1").is_ok());
        assert!(check_url("http://10.0.0.5:8080/hook").is_ok());
        assert!(check_url("ftp://x/y").unwrap_err().contains("scheme"));
        assert!(
            check_url("file:///etc/passwd")
                .unwrap_err()
                .contains("scheme")
        );
        assert!(check_url("gopher://x").is_err());
        assert!(
            check_url("https://user:pw@x/")
                .unwrap_err()
                .contains("password")
        );
        assert!(check_url("https://user@x/").is_err());
        assert!(check_url("not a url").is_err());
        let u = check_url("https://hooks.example:8443/services/T/B?token=abc").unwrap();
        assert_eq!(log_url(&u), "https://hooks.example:8443/services/T/B?…");
        assert_eq!(
            log_url_secret(&u, "slack-url"),
            "https://hooks.example/… (secret slack-url)"
        );
    }

    #[test]
    fn private_addresses() {
        let p = |s: &str| private_reason(s.parse().unwrap());
        for a in [
            "127.0.0.1",
            "127.9.9.9",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2002:7f00:1::",
            "ff02::1",
        ] {
            assert!(p(a).is_some(), "{a} is private");
        }
        for a in [
            "93.184.216.34",
            "172.32.0.1",
            "1.1.1.1",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(p(a).is_none(), "{a} is public");
        }
    }

    #[tokio::test]
    async fn resolve_picks_an_allowed_address_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let r: Resolver = Arc::new(move |_h: &str, _p: u16| {
            c.fetch_add(1, Ordering::SeqCst);
            vec![
                "10.0.0.5".parse().unwrap(),
                "93.184.216.34".parse().unwrap(),
            ]
        });
        let u = check_url("https://hook.test/x").unwrap();
        let a = resolve(&u, PrivateMode::Deny, &[], Some(r.clone()))
            .await
            .unwrap();
        assert_eq!(a, "93.184.216.34:443".parse().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let a = resolve(&u, PrivateMode::Allow, &[], Some(r)).await.unwrap();
        assert_eq!(a.ip().to_string(), "10.0.0.5");
        let only_private: Resolver = Arc::new(|_h: &str, _p: u16| vec!["::1".parse().unwrap()]);
        let e = resolve(&u, PrivateMode::Deny, &[], Some(only_private))
            .await
            .unwrap_err();
        assert!(e.contains("loopback"), "{e}");
        let lit = check_url("http://[::1]:9/x").unwrap();
        assert!(resolve(&lit, PrivateMode::Deny, &[], None).await.is_err());
        assert!(resolve(&lit, PrivateMode::Allow, &[], None).await.is_ok());
        let own = check_url("http://93.184.216.34:8080/").unwrap();
        let e = resolve(&own, PrivateMode::Deny, &[8080], None)
            .await
            .unwrap_err();
        assert!(e.contains("own listeners"), "{e}");
    }

    #[test]
    fn retry_after_values() {
        let now = chrono::DateTime::parse_from_rfc3339("2015-10-21T07:28:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(retry_after("120", now), Some(120));
        assert_eq!(retry_after("Wed, 21 Oct 2015 07:30:00 GMT", now), Some(120));
        assert_eq!(retry_after("soon", now), None);
    }
}
