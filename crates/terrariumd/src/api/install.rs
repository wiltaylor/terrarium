//! `GET /install/...`: the host installer (docs/api.md §17.9). The script
//! `scripts/install-host.sh` with this daemon's URLs filled in, and the
//! static `terrarium-host` binary with its SHA-256.
//!
//! These routes are outside `/api`, so they need no token, like the
//! dashboard assets. Nothing in them is secret: the script holds the
//! public URLs and the SHA-256 of the host listener certificate (a public
//! value: every TLS client sees the certificate). The enrol token never
//! comes from here; the operator gets it from `POST /api/hosts`.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use sha2::Digest;
use terrarium_host::tls::{is_loopback_host, url_host};
use tracing::warn;

use super::Shared;
use crate::settings::WebConfig;

pub const SCRIPT_PATH: &str = "/install/host.sh";
pub const BINARY_PATH: &str = "/install/terrarium-host";
pub const CHECKSUM_PATH: &str = "/install/terrarium-host.sha256";
/// The daemon version, on each answer of these routes.
pub const VERSION_HEADER: &str = "x-terrarium-version";
/// The arches that have a build. To add one, build `terrarium-host` for it
/// into the dist dir of the image (docker/Dockerfile, `host-build`) as
/// `terrarium-host-<arch>-linux`, and add it here. The script maps
/// `uname -m` to these names (`arm64` is `aarch64`).
pub const ARCHES: &[&str] = &["x86_64"];

const SCRIPT: &str = include_str!("../../../../scripts/install-host.sh");
const UNIT: &str = include_str!("../../../terrarium-host/terrarium-host.service");
const VERSION: &str = terrarium_version::FULL;

pub fn routes() -> Router<Shared> {
    registered_routes().into_parts().0
}

pub(super) fn registered_routes() -> super::link_registry::RegisteredRouter {
    use super::link_registry::{Access, RegisteredRouter};
    use axum::http::Method;
    RegisteredRouter::new()
        .route(
            SCRIPT_PATH,
            get(script),
            &[(Method::GET, Access::PublicDenied)],
        )
        .route(
            BINARY_PATH,
            get(binary),
            &[(Method::GET, Access::PublicDenied)],
        )
        .route(
            CHECKSUM_PATH,
            get(checksum),
            &[(Method::GET, Access::PublicDenied)],
        )
}

/// The binary and its SHA-256 only. The host listener serves these too:
/// a host agent downloads its self-update from there, with the TLS
/// settings of its link (docs/host-protocol.md, "Self-update").
pub fn binary_routes() -> Router<Shared> {
    Router::new()
        .route(BINARY_PATH, get(binary))
        .route(CHECKSUM_PATH, get(checksum))
}

/// The file of the release checksums in the dist dir. The image does not
/// carry it (the binary of the image is the release binary; CI checks
/// that). For a daemon outside the image, put the `SHA256SUMS` of the
/// release here, and host agents check it too.
pub const RELEASE_SUMS: &str = "SHA256SUMS";

fn hex64(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_lowercase();
    (s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(s)
}

/// The SHA-256 of the binary at `path`: the `.sha256` file next to it,
/// else the hash of the binary now.
async fn binary_sha256(path: &Path) -> std::io::Result<String> {
    let mut side = path.to_owned().into_os_string();
    side.push(".sha256");
    let stored = tokio::fs::read_to_string(&side)
        .await
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(hex64));
    if let Some(h) = stored {
        return Ok(h);
    }
    let d = tokio::fs::read(path).await?;
    Ok(sha2::Sha256::digest(&d)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The hash of the `arch` binary in a release `SHA256SUMS`: the line of
/// `terrarium-host-<version>-linux-<arch>`. The version starts with a
/// digit, so the line of `terrarium-host-helper-<version>-linux-<arch>`
/// (the same release) is not taken.
pub fn release_sum(sums: &str, arch: &str) -> Option<String> {
    let suffix = format!("-linux-{arch}");
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        let file = file.trim().trim_start_matches('*');
        let version = file.strip_prefix("terrarium-host-")?;
        (version.starts_with(|c: char| c.is_ascii_digit()) && file.ends_with(&suffix))
            .then(|| hex64(hash))
            .flatten()
    })
}

/// The self-update offer for a host agent of `arch`: this daemon's
/// version and the binary of the dist dir. `None` when the dist dir has
/// no binary for the arch.
pub async fn update_offer(
    dist: &Path,
    arch: &str,
) -> Option<terrarium_host::protocol::UpdateOffer> {
    if !ARCHES.contains(&arch) {
        return None;
    }
    let path = binary_path(dist, arch)?;
    let sha256 = match binary_sha256(&path).await {
        Ok(h) => h,
        Err(e) => {
            warn!(path = %path.display(), "no self-update offer: cannot read the host agent binary: {e}");
            return None;
        }
    };
    let release_sha256 = tokio::fs::read_to_string(dist.join(RELEASE_SUMS))
        .await
        .ok()
        .and_then(|s| release_sum(&s, arch));
    if let Some(r) = &release_sha256
        && *r != sha256
    {
        warn!(
            dist = %dist.display(),
            "no self-update offer: the host agent binary does not match the release {RELEASE_SUMS}"
        );
        return None;
    }
    Some(terrarium_host::protocol::UpdateOffer {
        version: VERSION.to_owned(),
        path: format!("{BINARY_PATH}?arch={arch}"),
        sha256,
        release_sha256,
    })
}

/// The defaults that the daemon writes into the script. `None`: the
/// script asks for the flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Defaults {
    /// The dashboard URL, for the downloads (`--url`).
    pub url: Option<String>,
    /// The host listener as a host agent dials it (`--host-url`).
    pub host_url: Option<String>,
    /// The SHA-256 of the host listener certificate (`--fingerprint`).
    pub pin: Option<String>,
}

/// Only these characters go into the script: no quote, no `$`, no space.
fn safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || ":/._-[]%~".contains(c))
}

fn loopback_url(url: &str) -> bool {
    url_host(url).is_some_and(|h| is_loopback_host(&h))
}

/// The daemon URL for the downloads: `TERRARIUM_PUBLIC_URL`, unless it
/// names a loopback host and the request came to another name; then
/// `http://` and the `Host` of the request.
pub fn base_url(web: &WebConfig, request_host: Option<&str>) -> Option<String> {
    let public = web.public_url.clone().filter(|u| safe(u));
    let from_request = request_host
        .filter(|h| safe(h) && !h.contains('/'))
        .map(|h| format!("http://{h}"));
    match public {
        Some(p) if !loopback_url(&p) => Some(p),
        p => match from_request {
            Some(r) if !loopback_url(&r) => Some(r),
            r => p.or(r),
        },
    }
}

/// The defaults for a request: [`base_url`], then the host listener URL
/// (`TERRARIUM_HOST_DAEMON_URL`, else `wss://<host of the base>:<port of
/// the listener>` with TLS, else `ws://` for a loopback base on a
/// plaintext listener), then the pin of the listener certificate for a
/// `wss://` URL.
pub fn defaults(web: &WebConfig, request_host: Option<&str>) -> Defaults {
    let url = base_url(web, request_host);
    let install = &web.install;
    let host_url = install
        .host_daemon_url
        .clone()
        .filter(|u| safe(u))
        .or_else(|| {
            let listen = install.host_listen.as_ref()?;
            let port: u16 = listen.addr.rsplit_once(':')?.1.parse().ok()?;
            let host = url_host(url.as_deref()?)?;
            let host = if host.contains(':') {
                format!("[{host}]")
            } else {
                host
            };
            if listen.tls.is_some() {
                Some(format!("wss://{host}:{port}"))
            } else if loopback_url(&format!("ws://{host}")) {
                Some(format!("ws://{host}:{port}"))
            } else {
                None
            }
        });
    let tls_url = host_url
        .as_deref()
        .is_some_and(|u| u.starts_with("wss://") || u.starts_with("https://"));
    let pin = match install.host_listen.as_ref().and_then(|l| l.tls.as_ref()) {
        Some((cert, _)) if tls_url => match terrarium_host::tls::cert_file_fingerprint(cert) {
            Ok(fp) => Some(fp),
            Err(e) => {
                warn!("the host installer has no pin: {e}");
                None
            }
        },
        _ => None,
    };
    Defaults { url, host_url, pin }
}

/// The script with the defaults and the unit filled in.
pub fn render_script(d: &Defaults) -> String {
    let opt = |v: &Option<String>| v.clone().filter(|s| safe(s)).unwrap_or_default();
    // The unit goes into a quoted here-document: its end marker must not
    // be a line of it.
    let unit = UNIT.trim_end_matches('\n');
    SCRIPT
        .replace("@TERRARIUM_URL@", &opt(&d.url))
        .replace("@TERRARIUM_HOST_URL@", &opt(&d.host_url))
        .replace("@TERRARIUM_HOST_PIN@", &opt(&d.pin))
        .replace("@TERRARIUM_VERSION@", VERSION)
        .replace("@TERRARIUM_HOST_UNIT@", unit)
}

/// The line that installs a host with an enrol token. `base`: the daemon
/// URL ([`base_url`]).
pub fn install_command(base: &str, token: &str, name: &str) -> String {
    format!(
        "curl -fsSL {}{SCRIPT_PATH} | sudo sh -s -- --token {token} --name {name}",
        base.trim_end_matches('/')
    )
}

fn request_host(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

fn text(status: StatusCode, body: String) -> Response {
    let mut r = (status, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r.headers_mut()
        .insert(VERSION_HEADER, HeaderValue::from_static(VERSION));
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// `GET /install/host.sh`.
async fn script(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let d = defaults(&state.web, request_host(&headers));
    text(StatusCode::OK, render_script(&d))
}

#[derive(Debug, Deserialize)]
struct ArchQuery {
    arch: Option<String>,
}

/// The binary of an arch in the dist dir: `terrarium-host-<arch>-linux`,
/// else `terrarium-host` when the arch is the daemon's own (the output of
/// `just host::build`, for a daemon outside the image).
fn binary_path(dist: &Path, arch: &str) -> Option<PathBuf> {
    let named = dist.join(format!("terrarium-host-{arch}-linux"));
    if named.is_file() {
        return Some(named);
    }
    let plain = dist.join("terrarium-host");
    (arch == std::env::consts::ARCH && plain.is_file()).then_some(plain)
}

/// The arch of the query, checked, or the refusal.
fn pick(state: &Shared, q: &ArchQuery) -> Result<(String, PathBuf), Box<Response>> {
    let have = ARCHES.join(", ");
    let Some(arch) = q.arch.as_deref().filter(|a| !a.is_empty()) else {
        return Err(Box::new(text(
            StatusCode::BAD_REQUEST,
            format!("pass ?arch=<arch>; this daemon serves terrarium-host for: {have}\n"),
        )));
    };
    if !ARCHES.contains(&arch) {
        let shown =
            if arch.len() <= 32 && arch.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                arch
            } else {
                "that arch"
            };
        return Err(Box::new(text(
            StatusCode::NOT_FOUND,
            format!("no terrarium-host for arch `{shown}`: this daemon serves {have}\n"),
        )));
    }
    match binary_path(&state.web.install.dist_dir, arch) {
        Some(p) => Ok((arch.to_owned(), p)),
        None => {
            warn!(
                dist = %state.web.install.dist_dir.display(),
                arch,
                "GET /install/terrarium-host: no binary in the dist dir"
            );
            Err(Box::new(text(
                StatusCode::NOT_FOUND,
                format!(
                    "this daemon has no terrarium-host binary for {arch}: the image carries it; \
                     for a daemon outside the image, run `just host::build` and set \
                     TERRARIUM_HOST_DIST_DIR=target/host\n"
                ),
            )))
        }
    }
}

/// `GET /install/terrarium-host?arch=<arch>`.
async fn binary(State(state): State<Shared>, Query(q): Query<ArchQuery>) -> Response {
    let (_, path) = match pick(&state, &q) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(e) => {
            warn!(path = %path.display(), "cannot read the host agent binary: {e}");
            return text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot read the terrarium-host binary\n".into(),
            );
        }
    };
    let mut r = (StatusCode::OK, data).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"terrarium-host\""),
    );
    h.insert(VERSION_HEADER, HeaderValue::from_static(VERSION));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// `GET /install/terrarium-host.sha256?arch=<arch>`: `<hex>  <file>`. The
/// `.sha256` file next to the binary, else the hash of the binary now.
async fn checksum(State(state): State<Shared>, Query(q): Query<ArchQuery>) -> Response {
    let (arch, path) = match pick(&state, &q) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let hex = match binary_sha256(&path).await {
        Ok(h) => h,
        Err(e) => {
            warn!(path = %path.display(), "cannot read the host agent binary: {e}");
            return text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot read the terrarium-host binary\n".into(),
            );
        }
    };
    text(
        StatusCode::OK,
        format!("{hex}  terrarium-host-{arch}-linux\n"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hosts::HostListen;
    use crate::settings::InstallConfig;

    fn web(public: Option<&str>, listen: Option<HostListen>) -> WebConfig {
        WebConfig {
            public_url: public.map(str::to_owned),
            install: InstallConfig {
                host_listen: listen,
                ..InstallConfig::default()
            },
            ..WebConfig::default()
        }
    }

    fn plain(addr: &str) -> HostListen {
        HostListen {
            addr: addr.into(),
            tls: None,
        }
    }

    #[test]
    fn base_url_prefers_a_public_url_that_a_remote_host_can_reach() {
        let w = web(Some("https://t.example.com"), None);
        assert_eq!(
            base_url(&w, Some("10.0.0.5:8080")).as_deref(),
            Some("https://t.example.com")
        );
        // A localhost public URL gives way to the name the request used.
        let w = web(Some("http://localhost:8080"), None);
        assert_eq!(
            base_url(&w, Some("terrarium.lan:8080")).as_deref(),
            Some("http://terrarium.lan:8080")
        );
        assert_eq!(
            base_url(&w, Some("localhost:8080")).as_deref(),
            Some("http://localhost:8080")
        );
        assert_eq!(base_url(&w, None).as_deref(), Some("http://localhost:8080"));
        // No public URL: the request host.
        let w = web(None, None);
        assert_eq!(
            base_url(&w, Some("t.lan:8080")).as_deref(),
            Some("http://t.lan:8080")
        );
        assert_eq!(base_url(&w, None), None);
        // A Host header with shell characters is not used.
        assert_eq!(base_url(&w, Some("t.lan';id;'")), None);
    }

    #[test]
    fn host_url_and_pin() {
        // TLS: wss to the public host, the listener port, and the pin.
        let dir = tempfile::tempdir().unwrap();
        let ss = terrarium_host::tls::self_signed(&["t.lan".into()]).unwrap();
        let (c, k) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        std::fs::write(&c, &ss.cert_pem).unwrap();
        std::fs::write(&k, &ss.key_pem).unwrap();
        let tls = HostListen {
            addr: "0.0.0.0:8082".into(),
            tls: Some((c, k)),
        };
        let d = defaults(&web(Some("https://t.lan"), Some(tls.clone())), None);
        assert_eq!(d.host_url.as_deref(), Some("wss://t.lan:8082"));
        assert_eq!(d.pin.as_deref(), Some(ss.fingerprint.as_str()));
        // TERRARIUM_HOST_DAEMON_URL wins.
        let mut w = web(Some("https://t.lan"), Some(tls));
        w.install.host_daemon_url = Some("wss://proxy.lan:443".into());
        assert_eq!(
            defaults(&w, None).host_url.as_deref(),
            Some("wss://proxy.lan:443")
        );
        // Plaintext: only for a loopback base, as ws://, with no pin.
        let d = defaults(
            &web(Some("http://localhost:8080"), Some(plain("0.0.0.0:8082"))),
            None,
        );
        assert_eq!(d.host_url.as_deref(), Some("ws://localhost:8082"));
        assert_eq!(d.pin, None);
        let d = defaults(
            &web(Some("http://t.lan:8080"), Some(plain("0.0.0.0:8082"))),
            None,
        );
        assert_eq!(d.host_url, None);
        // No listener: no URL.
        assert_eq!(
            defaults(&web(Some("https://t.lan"), None), None).host_url,
            None
        );
    }

    #[test]
    fn script_has_the_defaults_and_the_unit() {
        let s = render_script(&Defaults {
            url: Some("http://t.lan:8080".into()),
            host_url: Some("wss://t.lan:8082".into()),
            pin: Some("AA:BB".into()),
        });
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("DEFAULT_URL='http://t.lan:8080'\n"));
        assert!(s.contains("DEFAULT_HOST_URL='wss://t.lan:8082'\n"));
        assert!(s.contains("DEFAULT_PIN='AA:BB'\n"));
        assert!(s.contains(&format!("SERVED_VERSION='{VERSION}'\n")));
        assert!(s.contains(
            "ExecStart=/var/lib/terrarium-host/bin/terrarium-host --config /etc/terrarium-host/config.toml run"
        ));
        assert!(!s.contains("@TERRARIUM_"));
        // An unsafe value is left out, not quoted.
        let s = render_script(&Defaults {
            url: Some("http://x/'$(id)'".into()),
            ..Defaults::default()
        });
        assert!(s.contains("DEFAULT_URL=''\n"));
        assert!(!UNIT.lines().any(|l| l == "TERRARIUM_UNIT_END"));
    }

    /// The served script parses as POSIX sh (scripts/test_install_host.py
    /// runs it).
    #[test]
    fn rendered_script_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.sh");
        std::fs::write(
            &path,
            render_script(&Defaults {
                url: Some("http://t.lan:8080".into()),
                host_url: Some("wss://t.lan:8082".into()),
                pin: None,
            }),
        )
        .unwrap();
        let out = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn release_sums_pick_the_arch() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  terrarium-host-0.3.0-linux-x86_64\n{b} *terrarium-host-0.3.0-linux-aarch64\n"
        );
        assert_eq!(release_sum(&sums, "x86_64"), Some(a));
        assert_eq!(release_sum(&sums, "aarch64"), Some("b".repeat(64)));
        assert_eq!(release_sum(&sums, "riscv64"), None);
        assert_eq!(
            release_sum("zz  terrarium-host-0.3.0-linux-x86_64", "x86_64"),
            None
        );
    }

    #[test]
    fn release_sums_skip_the_host_helper() {
        let a = "a".repeat(64);
        let h = "c".repeat(64);
        let sums = format!(
            "{h}  terrarium-host-helper-0.3.0-linux-x86_64\n{a}  terrarium-host-0.3.0-linux-x86_64\n"
        );
        assert_eq!(release_sum(&sums, "x86_64"), Some(a.clone()));
        // The order of the release CI: the host agent first, then the helper.
        let sums = format!(
            "{a}  terrarium-host-0.3.0-linux-x86_64\n{h}  terrarium-host-helper-0.3.0-linux-x86_64\n"
        );
        assert_eq!(release_sum(&sums, "x86_64"), Some(a));
        assert_eq!(
            release_sum(
                &format!("{h}  terrarium-host-helper-0.3.0-linux-x86_64\n"),
                "x86_64"
            ),
            None
        );
    }

    #[tokio::test]
    async fn the_offer_is_the_dist_binary_of_the_arch() {
        let dist = tempfile::tempdir().unwrap();
        assert!(update_offer(dist.path(), "x86_64").await.is_none());
        let bin = dist.path().join("terrarium-host-x86_64-linux");
        std::fs::write(&bin, b"the binary").unwrap();
        let sha: String = sha2::Sha256::digest(b"the binary")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let o = update_offer(dist.path(), "x86_64").await.unwrap();
        assert_eq!(o.version, VERSION);
        assert_eq!(o.path, "/install/terrarium-host?arch=x86_64");
        assert_eq!(o.sha256, sha);
        assert_eq!(o.release_sha256, None);
        assert!(update_offer(dist.path(), "aarch64").await.is_none());
        // With the release sums: they go into the offer, and a binary that
        // does not match them is not offered.
        let sums = dist.path().join(RELEASE_SUMS);
        std::fs::write(&sums, format!("{sha}  terrarium-host-0.3.0-linux-x86_64\n")).unwrap();
        let o = update_offer(dist.path(), "x86_64").await.unwrap();
        assert_eq!(o.release_sha256.as_deref(), Some(sha.as_str()));
        std::fs::write(
            &sums,
            format!("{}  terrarium-host-0.3.0-linux-x86_64\n", "0".repeat(64)),
        )
        .unwrap();
        assert!(update_offer(dist.path(), "x86_64").await.is_none());
    }

    #[test]
    fn install_line() {
        assert_eq!(
            install_command("http://t.lan:8080/", "the_x", "lab1"),
            "curl -fsSL http://t.lan:8080/install/host.sh | sudo sh -s -- --token the_x --name lab1"
        );
    }
}
