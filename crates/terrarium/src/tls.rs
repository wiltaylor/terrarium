//! HTTPS to the daemon: rustls (ring) with the system root certificates,
//! and the certificates of `TERRARIUM_CA_FILE` (a PEM file) when it is set,
//! for a daemon behind a proxy with a private CA.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};

use crate::client::Stream;

/// `TERRARIUM_CA_FILE`: more trusted CA certificates (PEM) for `https://`.
pub const ENV_CA_FILE: &str = "TERRARIUM_CA_FILE";

fn roots(ca_file: Option<&Path>) -> anyhow::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let found = rustls_native_certs::load_native_certs();
    for cert in found.certs {
        // A system certificate that rustls cannot use is skipped.
        let _ = roots.add(cert);
    }
    if let Some(path) = ca_file {
        let mut added = 0;
        for cert in CertificateDer::pem_file_iter(path)
            .with_context(|| format!("cannot read {ENV_CA_FILE} {}", path.display()))?
        {
            let cert = cert.with_context(|| format!("{} is not PEM", path.display()))?;
            roots
                .add(cert)
                .with_context(|| format!("a certificate in {} is not valid", path.display()))?;
            added += 1;
        }
        if added == 0 {
            anyhow::bail!("{ENV_CA_FILE} {} holds no certificate", path.display());
        }
    }
    if roots.is_empty() {
        anyhow::bail!("no root certificates: install the system CA bundle, or set {ENV_CA_FILE}");
    }
    Ok(roots)
}

/// The client config, made once, from the env.
fn config() -> anyhow::Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(c) = CONFIG.get() {
        return Ok(c.clone());
    }
    let ca = std::env::var_os(ENV_CA_FILE).filter(|v| !v.is_empty());
    let c = config_with(ca.as_deref().map(Path::new))?;
    Ok(CONFIG.get_or_init(|| c).clone())
}

/// A client config with the system roots and the certificates of `ca_file`.
fn config_with(ca_file: Option<&Path>) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let roots = roots(ca_file)?;
    let mut c = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("cannot set up TLS")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

/// Wrap a TCP stream in TLS to `host`, and check the certificate.
pub async fn connect(tcp: tokio::net::TcpStream, host: &str) -> anyhow::Result<Box<dyn Stream>> {
    connect_with(tcp, host, config()?).await
}

async fn connect_with(
    tcp: tokio::net::TcpStream,
    host: &str,
    config: Arc<rustls::ClientConfig>,
) -> anyhow::Result<Box<dyn Stream>> {
    let name = ServerName::try_from(host.trim_matches(['[', ']']).to_owned())
        .with_context(|| format!("`{host}` is not a TLS server name"))?;
    let tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .with_context(|| format!("the TLS handshake with {host} failed"))?;
    Ok(Box::new(tls))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A TLS server with a certificate of its own CA: the client trusts it
    /// through the CA file, checks the name, and talks over the stream.
    #[tokio::test]
    async fn https_with_a_ca_file() {
        use rustls::pki_types::PrivateKeyDer;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!("terrarium-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, cert.pem()).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                if let Ok(mut s) = acceptor.accept(tcp).await {
                    let mut buf = [0u8; 4];
                    s.read_exact(&mut buf).await.unwrap();
                    s.write_all(b"pong").await.unwrap();
                }
            }
        });
        let cfg = config_with(Some(&ca)).unwrap();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut s = connect_with(tcp, "localhost", cfg.clone()).await.unwrap();
        s.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
        // Another name than the certificate's: refused.
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(connect_with(tcp, "127.0.0.1", cfg).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_ca_file_must_hold_certificates() {
        let dir = std::env::temp_dir().join(format!("terrarium-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        let err = roots(Some(&empty)).unwrap_err().to_string();
        assert!(err.contains("holds no certificate"), "{err}");
        assert!(roots(Some(&dir.join("missing.pem"))).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
