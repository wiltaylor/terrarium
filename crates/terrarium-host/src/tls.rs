//! TLS on the host agent link (docs/host-protocol.md "TLS").
//!
//! - [`transport`] picks the [`TlsMode`] for a daemon URL and the config,
//!   and refuses `ws://` to a host that is not loopback unless the config
//!   sets `allow_insecure`.
//! - [`PinnedVerifier`] accepts a server certificate by its SHA-256
//!   fingerprint, for a daemon with a self-signed certificate.
//! - [`probe`] does the TCP connect and the TLS handshake only, for
//!   `terrarium-host check --probe`.
//! - [`self_signed`] makes a certificate and key for the daemon's host
//!   listener (`terrarium-host cert`, `just host::cert`).
//! - [`server_config`] loads the daemon's PEM files
//!   (`TERRARIUM_HOST_TLS_CERT`, `TERRARIUM_HOST_TLS_KEY`).
//!
//! Everything uses rustls with the ring provider. No OpenSSL.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, OtherError, RootCertStore, SignatureScheme};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The TLS errors of the host agent link.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error(
        "refusing plaintext ws:// to {host}: the token and all exec traffic would cross the \
         network in clear text; use wss:// (see docs/host-protocol.md \"TLS\"), or set \
         allow_insecure = true in the config to accept the risk"
    )]
    Insecure { host: String },
    #[error("{0}")]
    Config(String),
    #[error("cannot read {path}: {message}")]
    File { path: PathBuf, message: String },
    #[error("tls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("certificate: {0}")]
    Generate(#[from] rcgen::Error),
}

/// How the host agent protects its link to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TlsMode {
    /// `ws://` to a loopback address. No TLS; the bytes stay on this machine.
    PlainLoopback,
    /// `ws://` to another machine, allowed by `allow_insecure = true`.
    PlainInsecure,
    /// `wss://`, verified against the system roots.
    SystemRoots,
    /// `wss://`, verified against the CA certificates in `ca_file`.
    CaFile { ca_file: PathBuf },
    /// `wss://`, the server certificate pinned by its SHA-256 fingerprint.
    /// With a `ca_file`, the chain must also verify against it.
    Pinned {
        fingerprint: String,
        ca_file: Option<PathBuf>,
    },
}

impl TlsMode {
    pub fn is_tls(&self) -> bool {
        !matches!(self, Self::PlainLoopback | Self::PlainInsecure)
    }

    /// A line for `check` and the start log.
    pub fn describe(&self) -> String {
        match self {
            Self::PlainLoopback => "plaintext ws:// to loopback".into(),
            Self::PlainInsecure => {
                "PLAINTEXT ws:// to a remote host (allow_insecure = true)".into()
            }
            Self::SystemRoots => "wss://, system root certificates".into(),
            Self::CaFile { ca_file } => format!("wss://, CA file {}", ca_file.display()),
            Self::Pinned {
                fingerprint,
                ca_file: None,
            } => format!("wss://, pinned SHA-256 {fingerprint}"),
            Self::Pinned {
                fingerprint,
                ca_file: Some(ca),
            } => format!(
                "wss://, CA file {} and pinned SHA-256 {fingerprint}",
                ca.display()
            ),
        }
    }
}

/// The transport to dial: the URL, the mode, and the rustls config for
/// `wss://` (`None` for `ws://`).
#[derive(Debug, Clone)]
pub struct Transport {
    pub url: String,
    pub mode: TlsMode,
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

/// The TLS settings of the config file.
#[derive(Debug, Clone, Default)]
pub struct TlsSettings<'a> {
    pub allow_insecure: bool,
    pub ca_file: Option<&'a Path>,
    pub server_fingerprint_sha256: Option<&'a str>,
}

/// The SHA-256 of a DER certificate, as upper-case hex pairs with colons.
pub fn fingerprint_sha256(der: &[u8]) -> String {
    format_fingerprint(&Sha256::digest(der))
}

fn format_fingerprint(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Parse a fingerprint: 64 hex digits, with or without colons or spaces,
/// with an optional `sha256:` prefix. Any case.
pub fn parse_fingerprint(s: &str) -> Result<[u8; 32], TlsError> {
    let t = s.trim();
    let t = t
        .strip_prefix("sha256:")
        .or_else(|| t.strip_prefix("SHA256:"))
        .or_else(|| t.strip_prefix("sha256="))
        .or_else(|| t.strip_prefix("SHA256="))
        .unwrap_or(t);
    let hex: String = t
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect();
    let bad = || {
        TlsError::Config(format!(
            "server_fingerprint_sha256 {s:?} is not 64 hex digits (a SHA-256)"
        ))
    };
    if hex.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

/// The host part of a URL authority, without brackets, port, or user info.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// The port of a URL: the explicit one, else 80 for `ws`/`http` and 443
/// for `wss`/`https`.
fn url_port(url: &str) -> Option<u16> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let port = match authority.strip_prefix('[') {
        Some(v6) => v6.split_once("]:").map(|(_, p)| p),
        None => authority.split_once(':').map(|(_, p)| p),
    };
    match port {
        Some(p) => p.parse().ok(),
        None => match scheme.to_ascii_lowercase().as_str() {
            "wss" | "https" => Some(443),
            _ => Some(80),
        },
    }
}

/// Is `host` a loopback name or address? `localhost`, `*.localhost`,
/// `127.0.0.0/8`, and `::1`. A DNS name that resolves to loopback does
/// not count: the check does not trust the resolver.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    h.parse::<IpAddr>().is_ok_and(|ip| match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    })
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn load_roots(ca_file: &Path) -> Result<RootCertStore, TlsError> {
    let file_err = |message: String| TlsError::File {
        path: ca_file.to_owned(),
        message,
    };
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca_file).map_err(|e| file_err(e.to_string()))? {
        let cert = cert.map_err(|e| file_err(e.to_string()))?;
        roots.add(cert)?;
    }
    if roots.is_empty() {
        return Err(file_err("the file holds no certificates".into()));
    }
    Ok(roots)
}

fn system_roots() -> Result<RootCertStore, TlsError> {
    let found = rustls_native_certs::load_native_certs();
    for e in &found.errors {
        tracing::debug!(error = %e, "a system root certificate did not load");
    }
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(found.certs);
    if added == 0 {
        return Err(TlsError::Config(
            "no system root certificates found; set ca_file or server_fingerprint_sha256".into(),
        ));
    }
    Ok(roots)
}

fn client_config(
    verifier: Arc<dyn ServerCertVerifier>,
) -> Result<Arc<rustls::ClientConfig>, TlsError> {
    Ok(Arc::new(
        rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    ))
}

fn roots_config(roots: RootCertStore) -> Result<Arc<rustls::ClientConfig>, TlsError> {
    Ok(Arc::new(
        rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// Pick the transport for `url` (a WebSocket URL from
/// [`crate::config::connect_url`]).
pub fn transport(url: &str, s: &TlsSettings<'_>) -> Result<Transport, TlsError> {
    let scheme = url
        .split_once("://")
        .map(|(sc, _)| sc.to_ascii_lowercase())
        .unwrap_or_default();
    let host =
        url_host(url).ok_or_else(|| TlsError::Config(format!("daemon_url {url:?} has no host")))?;
    match scheme.as_str() {
        "ws" | "http" => {
            if s.server_fingerprint_sha256.is_some() || s.ca_file.is_some() {
                return Err(TlsError::Config(
                    "ca_file or server_fingerprint_sha256 is set, but daemon_url is ws://; \
                     use wss://"
                        .into(),
                ));
            }
            let mode = if is_loopback_host(&host) {
                TlsMode::PlainLoopback
            } else if s.allow_insecure {
                TlsMode::PlainInsecure
            } else {
                return Err(TlsError::Insecure { host });
            };
            Ok(Transport {
                url: url.to_owned(),
                mode,
                tls: None,
            })
        }
        "wss" | "https" => {
            let roots = s.ca_file.map(load_roots).transpose()?;
            let (mode, tls) = match s.server_fingerprint_sha256 {
                Some(fp) => {
                    let want = parse_fingerprint(fp)?;
                    let inner = match roots {
                        Some(r) => Some(
                            WebPkiServerVerifier::builder_with_provider(Arc::new(r), provider())
                                .build()
                                .map_err(|e| TlsError::Config(e.to_string()))?,
                        ),
                        None => None,
                    };
                    let v = PinnedVerifier {
                        want,
                        inner,
                        provider: provider(),
                    };
                    (
                        TlsMode::Pinned {
                            fingerprint: format_fingerprint(&want),
                            ca_file: s.ca_file.map(Path::to_owned),
                        },
                        client_config(Arc::new(v))?,
                    )
                }
                None => match roots {
                    Some(r) => (
                        TlsMode::CaFile {
                            ca_file: s.ca_file.map(Path::to_owned).unwrap_or_default(),
                        },
                        roots_config(r)?,
                    ),
                    None => (TlsMode::SystemRoots, roots_config(system_roots()?)?),
                },
            };
            Ok(Transport {
                url: url.to_owned(),
                mode,
                tls: Some(tls),
            })
        }
        other => Err(TlsError::Config(format!(
            "daemon_url scheme {other:?} is not ws or wss"
        ))),
    }
}

/// The error of a pinned certificate that does not match.
#[derive(Debug, thiserror::Error)]
#[error("the server certificate has SHA-256 {got}, not the pinned {want}")]
pub struct FingerprintMismatch {
    pub got: String,
    pub want: String,
}

/// Accept the server certificate whose SHA-256 is the pin. The host name
/// and the expiry do not matter: the pin names the one certificate. With
/// an `inner` verifier the chain must also verify.
#[derive(Debug)]
pub struct PinnedVerifier {
    want: [u8; 32],
    inner: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    pub fn new(fingerprint: &str) -> Result<Self, TlsError> {
        Ok(Self {
            want: parse_fingerprint(fingerprint)?,
            inner: None,
            provider: provider(),
        })
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(inner) = &self.inner {
            inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
        }
        let got = Sha256::digest(end_entity.as_ref());
        if got.as_slice() == self.want {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(CertificateError::Other(
                OtherError(Arc::new(FingerprintMismatch {
                    got: format_fingerprint(&got),
                    want: format_fingerprint(&self.want),
                })),
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// What `check --probe` found.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub ok: bool,
    /// `host:port` that the probe dialled.
    pub address: String,
    #[serde(flatten)]
    pub mode: TlsMode,
    pub tls_version: Option<String>,
    pub cipher_suite: Option<String>,
    pub server_fingerprint_sha256: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// Dial the daemon and do the TLS handshake (for `wss://`), then close.
/// No WebSocket upgrade and no `hello`: the probe sends no token.
pub async fn probe(t: &Transport, timeout: Duration) -> ProbeReport {
    let start = Instant::now();
    let host = url_host(&t.url).unwrap_or_default();
    let port = url_port(&t.url).unwrap_or(0);
    let address = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut report = ProbeReport {
        ok: false,
        address,
        mode: t.mode.clone(),
        tls_version: None,
        cipher_suite: None,
        server_fingerprint_sha256: None,
        error: None,
        elapsed_ms: 0,
    };
    let result = tokio::time::timeout(timeout, async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let Some(tls) = &t.tls else {
            return Ok(None);
        };
        let name =
            ServerName::try_from(host.clone()).map_err(|e| format!("server name {host}: {e}"))?;
        let stream = tokio_rustls::TlsConnector::from(tls.clone())
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS handshake: {e}"))?;
        let (_, conn) = stream.get_ref();
        Ok(Some((
            conn.protocol_version().map(|v| format!("{v:?}")),
            conn.negotiated_cipher_suite()
                .map(|c| format!("{:?}", c.suite())),
            conn.peer_certificates()
                .and_then(|c| c.first())
                .map(|c| fingerprint_sha256(c.as_ref())),
        )))
    })
    .await;
    match result {
        Ok(Ok(info)) => {
            report.ok = true;
            if let Some((version, suite, fp)) = info {
                report.tls_version = version;
                report.cipher_suite = suite;
                report.server_fingerprint_sha256 = fp;
            }
        }
        Ok(Err(e)) => report.error = Some(e),
        Err(_) => report.error = Some(format!("timed out after {timeout:?}")),
    }
    report.elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    report
}

/// A self-signed certificate and its key, in PEM.
#[derive(Debug, Clone)]
pub struct SelfSigned {
    pub cert_pem: String,
    pub key_pem: String,
    /// The SHA-256 of the certificate, for `server_fingerprint_sha256`.
    pub fingerprint: String,
}

/// Make a self-signed certificate for `names` (DNS names or IP
/// addresses; the first is the common name). The key is ECDSA P-256.
pub fn self_signed(names: &[String]) -> Result<SelfSigned, TlsError> {
    if names.is_empty() {
        return Err(TlsError::Config(
            "name at least one host name or address".into(),
        ));
    }
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(names.to_vec())?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, names[0].clone());
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "Terrarium host link");
    let cert = params.self_signed(&key)?;
    Ok(SelfSigned {
        fingerprint: fingerprint_sha256(cert.der().as_ref()),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// The daemon's server config from PEM files, and the SHA-256 of the
/// first (leaf) certificate.
pub fn server_config(
    cert: &Path,
    key: &Path,
) -> Result<(Arc<rustls::ServerConfig>, String), TlsError> {
    let file_err = |path: &Path, message: String| TlsError::File {
        path: path.to_owned(),
        message,
    };
    let chain = CertificateDer::pem_file_iter(cert)
        .map_err(|e| file_err(cert, e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| file_err(cert, e.to_string()))?;
    let Some(leaf) = chain.first() else {
        return Err(file_err(cert, "the file holds no certificates".into()));
    };
    let fingerprint = fingerprint_sha256(leaf.as_ref());
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| file_err(key, e.to_string()))?;
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    Ok((Arc::new(config), fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost",
            "LOCALHOST",
            "a.localhost",
            "127.0.0.1",
            "127.9.9.9",
            "::1",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "10.0.0.1",
            "terrarium.lan",
            "localhost.evil.com",
            "0.0.0.0",
            "::",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn hosts_and_ports_of_urls() {
        assert_eq!(url_host("ws://d:8082/x").as_deref(), Some("d"));
        assert_eq!(url_host("wss://[::1]:8082/x").as_deref(), Some("::1"));
        assert_eq!(url_host("wss://u:p@h/x").as_deref(), Some("h"));
        assert_eq!(url_port("wss://h/x"), Some(443));
        assert_eq!(url_port("ws://h/x"), Some(80));
        assert_eq!(url_port("ws://[::1]:9/x"), Some(9));
    }

    #[test]
    fn fingerprints_parse_in_any_form() {
        let fp = "AB:".repeat(31) + "AB";
        let a = parse_fingerprint(&fp).unwrap();
        assert_eq!(
            parse_fingerprint(&fp.replace(':', "").to_lowercase()).unwrap(),
            a
        );
        assert_eq!(parse_fingerprint(&format!("sha256:{fp}")).unwrap(), a);
        assert_eq!(format_fingerprint(&a), fp);
        assert!(parse_fingerprint("abcd").is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn plaintext_to_a_remote_host_needs_allow_insecure() {
        let s = TlsSettings::default();
        let t = transport("ws://127.0.0.1:8082/hosts/connect", &s).unwrap();
        assert_eq!(t.mode, TlsMode::PlainLoopback);
        assert!(t.tls.is_none());
        let e = transport("ws://terrarium.lan:8082/hosts/connect", &s).unwrap_err();
        assert!(matches!(e, TlsError::Insecure { .. }), "{e}");
        let s = TlsSettings {
            allow_insecure: true,
            ..Default::default()
        };
        let t = transport("ws://terrarium.lan:8082/hosts/connect", &s).unwrap();
        assert_eq!(t.mode, TlsMode::PlainInsecure);
        // A pin with ws:// is a mistake, not a downgrade.
        let fp = "00".repeat(32);
        let s = TlsSettings {
            allow_insecure: true,
            server_fingerprint_sha256: Some(&fp),
            ..Default::default()
        };
        assert!(transport("ws://terrarium.lan/hosts/connect", &s).is_err());
    }

    #[test]
    fn wss_modes() {
        let fp = "00".repeat(32);
        let s = TlsSettings {
            server_fingerprint_sha256: Some(&fp),
            ..Default::default()
        };
        let t = transport("wss://terrarium.lan:8082/hosts/connect", &s).unwrap();
        assert!(matches!(t.mode, TlsMode::Pinned { ca_file: None, .. }));
        assert!(t.tls.is_some());

        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, self_signed(&["ca.lan".into()]).unwrap().cert_pem).unwrap();
        let s = TlsSettings {
            ca_file: Some(&ca),
            ..Default::default()
        };
        let t = transport("wss://terrarium.lan/hosts/connect", &s).unwrap();
        assert_eq!(
            t.mode,
            TlsMode::CaFile {
                ca_file: ca.clone()
            }
        );
    }

    #[test]
    fn self_signed_round_trips_through_server_config() {
        let dir = tempfile::tempdir().unwrap();
        let ss = self_signed(&["terrarium.lan".into(), "10.0.0.5".into()]).unwrap();
        let (c, k) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        std::fs::write(&c, &ss.cert_pem).unwrap();
        std::fs::write(&k, &ss.key_pem).unwrap();
        let (_, fp) = server_config(&c, &k).unwrap();
        assert_eq!(fp, ss.fingerprint);
        assert_eq!(parse_fingerprint(&fp).unwrap().len(), 32);
    }

    /// A one-shot TLS server on loopback with a fresh self-signed cert.
    async fn tls_server() -> (u16, String) {
        let dir = tempfile::tempdir().unwrap();
        let ss = self_signed(&["localhost".into()]).unwrap();
        let (c, k) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        std::fs::write(&c, &ss.cert_pem).unwrap();
        std::fs::write(&k, &ss.key_pem).unwrap();
        let (cfg, _) = server_config(&c, &k).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
            while let Ok((tcp, _)) = listener.accept().await {
                let a = acceptor.clone();
                tokio::spawn(async move {
                    let _ = a.accept(tcp).await;
                });
            }
        });
        (port, ss.fingerprint)
    }

    #[tokio::test]
    async fn probe_checks_the_pin() {
        let (port, fp) = tls_server().await;
        let url = format!("wss://localhost:{port}/hosts/connect");
        let good = transport(
            &url,
            &TlsSettings {
                server_fingerprint_sha256: Some(&fp),
                ..Default::default()
            },
        )
        .unwrap();
        let r = probe(&good, Duration::from_secs(5)).await;
        assert!(r.ok, "{r:?}");
        assert_eq!(r.server_fingerprint_sha256.as_deref(), Some(fp.as_str()));
        assert!(r.tls_version.is_some());

        let wrong = "11".repeat(32);
        let bad = transport(
            &url,
            &TlsSettings {
                server_fingerprint_sha256: Some(&wrong),
                ..Default::default()
            },
        )
        .unwrap();
        let r = probe(&bad, Duration::from_secs(5)).await;
        assert!(!r.ok);
        let e = r.error.unwrap();
        assert!(e.contains(&fp), "the error names the seen fingerprint: {e}");
    }

    #[tokio::test]
    async fn probe_of_plaintext_is_a_tcp_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let t = transport(
            &format!("ws://127.0.0.1:{port}/hosts/connect"),
            &TlsSettings::default(),
        )
        .unwrap();
        let r = probe(&t, Duration::from_secs(5)).await;
        assert!(r.ok, "{r:?}");
        assert!(r.tls_version.is_none());
        drop(listener);
        // Port 1 is privileged, so no parallel test can bind it. A probe of
        // the dropped ephemeral port is a race: another test may take it.
        let closed = transport("ws://127.0.0.1:1/hosts/connect", &TlsSettings::default()).unwrap();
        let r = probe(&closed, Duration::from_secs(5)).await;
        assert!(!r.ok, "{r:?}");
    }
}
