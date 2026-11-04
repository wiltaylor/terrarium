//! Self-update: the host agent follows the daemon's build
//! (docs/host-protocol.md, "Self-update").
//!
//! The daemon offers its own `terrarium-host` binary in `welcome.update`
//! when the host agent is of another build (`stale`). The host agent then:
//!
//! 1. checks that it may update ([`decide`]) and that it can write the
//!    directory of its binary ([`check_writable`]);
//! 2. downloads the binary from the host listener ([`download`]) and
//!    checks its SHA-256 against the offer and, when the offer has it, the
//!    release `SHA256SUMS` ([`verify`]);
//! 3. writes it as `<binary>.new` and runs `<binary>.new --version`: the
//!    new binary must start and report the offered version ([`stage`]);
//! 4. waits until it holds no session, then keeps the running binary as
//!    `<binary>.prev`, writes `<binary>.pending`, and renames `.new` over
//!    the binary ([`swap`]); the rename is atomic;
//! 5. exits, and systemd (`Restart=always`) starts the new binary.
//!
//! The unit's `ExecStartPre` guard counts the starts in `.pending`. After
//! three starts that did not reach the daemon, it puts `.prev` back and
//! renames `.pending` to `.failed`. The new binary removes `.pending` when
//! it gets its first `welcome` ([`confirm`]). The host agent does not take
//! the version in `.failed` again ([`failed_version`]) and reports it in
//! `hello.update_failed`.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::UpdateOffer;
use crate::transfer::sha256_hex;

/// The largest binary the host agent takes.
pub const MAX_BINARY_BYTES: usize = 256 << 20;
/// The limit for the download.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// The limit for `<binary>.new --version`.
pub const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The suffixes of the files next to the binary.
pub const NEW: &str = "new";
pub const PREV: &str = "prev";
pub const PENDING: &str = "pending";
pub const FAILED: &str = "failed";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("download {url}: {message}")]
    Download { url: String, message: String },
    #[error("the SHA-256 of the download is {got}, but the {what} says {want}")]
    Checksum {
        got: String,
        want: String,
        what: &'static str,
    },
    #[error("the new binary {0}")]
    Smoke(String),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Offer(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> UpdateError + '_ {
    move |source| UpdateError::Io {
        path: path.to_owned(),
        source,
    }
}

/// `<binary>.<ext>`.
pub fn sidecar(bin: &Path, ext: &str) -> PathBuf {
    let mut s = bin.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
}

/// The version of the last update that did not start (`<binary>.failed`).
pub fn failed_version(bin: &Path) -> Option<String> {
    std::fs::read_to_string(sidecar(bin, FAILED))
        .ok()
        .and_then(|t| first_line(&t))
}

/// The new binary reached the daemon: drop `<binary>.pending` (and an old
/// `<binary>.failed`). Returns the version that was pending, if any.
/// `<binary>.prev` stays, for a manual roll back.
pub fn confirm(bin: &Path) -> Option<String> {
    let pending = sidecar(bin, PENDING);
    let text = std::fs::read_to_string(&pending).ok()?;
    let _ = std::fs::remove_file(&pending);
    let _ = std::fs::remove_file(sidecar(bin, FAILED));
    Some(first_line(&text).unwrap_or_default())
}

/// Whether to take an offer. `Err` holds the reason not to.
pub fn decide(
    offer: &UpdateOffer,
    own: &str,
    enabled: bool,
    supervised: bool,
    failed: Option<&str>,
) -> Result<(), String> {
    if !enabled {
        return Err("auto_update = false in the config".into());
    }
    if same_build(own, &offer.version) {
        return Err("this binary is that build already".into());
    }
    if failed == Some(offer.version.as_str()) {
        return Err(format!(
            "version {} did not start here before (see {FAILED} next to the binary); \
             install it by hand with `install-host.sh --upgrade`",
            offer.version
        ));
    }
    if !supervised {
        return Err(
            "not a systemd service (no INVOCATION_ID): nothing would start the new \
                    binary; install it by hand"
                .into(),
        );
    }
    Ok(())
}

/// The same crate version and the same commit (`-dirty` does not count).
fn same_build(a: &str, b: &str) -> bool {
    let (a, b) = (terrarium_version::parts(a), terrarium_version::parts(b));
    a.version == b.version && a.git_ref.is_some() && a.git_ref == b.git_ref
}

/// Can the host agent make files next to its binary? An install that
/// predates self-update has the binary in a directory of root's.
pub fn check_writable(bin: &Path) -> Result<(), UpdateError> {
    let probe = sidecar(bin, "probe");
    let _ = std::fs::remove_file(&probe);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(io_err(&probe))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// The download URL: the scheme and the authority of `daemon_url`
/// (`ws` becomes `http`, `wss` becomes `https`) and the offer's path.
pub fn http_url(daemon_url: &str, path: &str) -> Result<String, UpdateError> {
    let bad =
        |m: &str| UpdateError::Offer(format!("{m}: daemon_url {daemon_url:?}, path {path:?}"));
    let (scheme, rest) = daemon_url
        .split_once("://")
        .ok_or_else(|| bad("no scheme"))?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return Err(bad("not ws or wss")),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(bad("no host"));
    }
    let clean = path.starts_with('/')
        && !path.starts_with("//")
        && path.chars().all(|c| c.is_ascii_graphic());
    if !clean {
        return Err(bad("the offer's path is not an absolute path"));
    }
    Ok(format!("{scheme}://{authority}{path}"))
}

/// `GET url` with HTTP/1.1 over the transport of the link: TLS with `tls`
/// for `https://`, plain TCP for `http://`. At most `limit` bytes of body.
pub async fn download(
    url: &str,
    tls: Option<Arc<rustls::ClientConfig>>,
    limit: usize,
) -> Result<Vec<u8>, UpdateError> {
    let err = |message: String| UpdateError::Download {
        url: url.to_owned(),
        message,
    };
    let host = crate::tls::url_host(url).ok_or_else(|| err("no host".into()))?;
    let port = crate::tls::url_port(url).ok_or_else(|| err("no port".into()))?;
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let path = rest
        .get(authority.len()..)
        .filter(|p| !p.is_empty())
        .unwrap_or("/");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: terrarium-host/{}\r\n\
         Accept: application/octet-stream\r\nConnection: close\r\n\r\n",
        terrarium_version::FULL
    );
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| err(format!("connect: {e}")))?;
    let raw = if url.to_ascii_lowercase().starts_with("https://") {
        let tls = tls.ok_or_else(|| err("no TLS settings for https".into()))?;
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| err(format!("server name {host}: {e}")))?;
        let stream = tokio_rustls::TlsConnector::from(tls)
            .connect(name, tcp)
            .await
            .map_err(|e| err(format!("TLS handshake: {e}")))?;
        exchange(stream, &req, limit).await
    } else {
        exchange(tcp, &req, limit).await
    }
    .map_err(err)?;
    parse_response(&raw).map_err(err)
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    req: &str,
    limit: usize,
) -> Result<Vec<u8>, String> {
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("send: {e}"))?;
    s.flush().await.map_err(|e| format!("send: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let n = match s.read(&mut chunk).await {
            Ok(n) => n,
            // A server that closes with no TLS close_notify.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !buf.is_empty() => 0,
            Err(e) => return Err(format!("read: {e}")),
        };
        if n == 0 {
            return Ok(buf);
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        if buf.len() > limit.saturating_add(64 << 10) {
            return Err(format!("the answer is larger than {limit} bytes"));
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The body of a `200` HTTP/1.1 answer (`Content-Length` or chunked). A
/// refusal gives its status and the start of its text.
pub fn parse_response(raw: &[u8]) -> Result<Vec<u8>, String> {
    let end = find(raw, b"\r\n\r\n").ok_or("no HTTP header in the answer")?;
    let head = std::str::from_utf8(raw.get(..end).unwrap_or_default())
        .map_err(|_| "the HTTP header is not UTF-8")?;
    let body = raw.get(end + 4..).unwrap_or_default();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("bad status line {status:?}"))?;
    let mut length = None;
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                length = v.parse::<usize>().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    if code != 200 {
        let shown = String::from_utf8_lossy(body.get(..body.len().min(400)).unwrap_or_default())
            .trim()
            .to_owned();
        return Err(format!("the daemon answered {code}: {shown}"));
    }
    if let Some(n) = length
        && !chunked
        && body.len() != n
    {
        return Err(format!("got {} of {n} bytes", body.len()));
    }
    Ok(body)
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let i = find(b, b"\r\n").ok_or("a chunk has no size line")?;
        let line = std::str::from_utf8(b.get(..i).unwrap_or_default())
            .map_err(|_| "a chunk size is not UTF-8")?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or_default().trim(), 16)
            .map_err(|_| format!("bad chunk size {line:?}"))?;
        b = b.get(i + 2..).unwrap_or_default();
        if size == 0 {
            return Ok(out);
        }
        let data = b.get(..size).ok_or("a chunk is cut off")?;
        out.extend_from_slice(data);
        b = b.get(size + 2..).ok_or("a chunk is cut off")?;
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Check the download against the offer. Returns its SHA-256.
pub fn verify(data: &[u8], offer: &UpdateOffer) -> Result<String, UpdateError> {
    let want = offer.sha256.trim().to_ascii_lowercase();
    if !is_hex64(&want) {
        return Err(UpdateError::Offer(
            "the offer has no valid SHA-256; not updating".into(),
        ));
    }
    let got = sha256_hex(data);
    if got != want {
        return Err(UpdateError::Checksum {
            got,
            want,
            what: "daemon",
        });
    }
    if let Some(r) = &offer.release_sha256 {
        let r = r.trim().to_ascii_lowercase();
        if got != r {
            return Err(UpdateError::Checksum {
                got,
                want: r,
                what: "release SHA256SUMS",
            });
        }
    }
    Ok(got)
}

/// Write `data` as `<binary>.new` (mode 0755) and run it with
/// `--version`. Returns the path of `.new`. On a failure `.new` is gone.
pub async fn stage(bin: &Path, data: &[u8], want_version: &str) -> Result<PathBuf, UpdateError> {
    let new = sidecar(bin, NEW);
    let _ = std::fs::remove_file(&new);
    write_file(&new, data, 0o755)?;
    if let Err(e) = smoke_test(&new, want_version, SMOKE_TIMEOUT).await {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    Ok(new)
}

fn write_file(path: &Path, data: &[u8], mode: u32) -> Result<(), UpdateError> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(io_err(path))?;
    f.write_all(data).map_err(io_err(path))?;
    f.sync_all().map_err(io_err(path))?;
    drop(f);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(io_err(path))
}

/// Run `<path> --version`: it must exit 0 and print the version `want`
/// (`terrarium-host <version>` on its first line; `-dirty` does not count).
pub async fn smoke_test(path: &Path, want: &str, timeout: Duration) -> Result<(), UpdateError> {
    let out = tokio::time::timeout(timeout, run_version(path))
        .await
        .map_err(|_| UpdateError::Smoke(format!("did not answer --version in {timeout:?}")))?
        .map_err(|e| UpdateError::Smoke(format!("does not run: {e}")))?;
    if !out.status.success() {
        return Err(UpdateError::Smoke(format!(
            "exited with {} for --version",
            out.status
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let got = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().last())
        .unwrap_or_default();
    if !same_build(got, want) {
        return Err(UpdateError::Smoke(format!(
            "reports version {got:?}, not {want}"
        )));
    }
    Ok(())
}

/// How long [`run_version`] tries again a start that fails with
/// `ETXTBSY`, and the pause between two tries.
const BUSY_RETRY: Duration = Duration::from_secs(1);
const BUSY_PAUSE: Duration = Duration::from_millis(50);

/// Start `<path> --version` and give its output. A start that fails with
/// `ETXTBSY` ("Text file busy") is tried again for up to [`BUSY_RETRY`]:
/// when another thread forks while `path` is open for writing, the child
/// holds a copy of the write fd until its own exec, and the kernel refuses
/// to run a file that is open for writing. `O_CLOEXEC` does not help, and
/// the host starts session processes while it stages an update. Any other
/// error comes back at once.
async fn run_version(path: &Path) -> std::io::Result<std::process::Output> {
    let began = tokio::time::Instant::now();
    loop {
        let r = tokio::process::Command::new(path)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        match r {
            Err(e)
                if e.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32)
                    && began.elapsed() < BUSY_RETRY =>
            {
                tokio::time::sleep(BUSY_PAUSE).await;
            }
            r => return r,
        }
    }
}

/// Put `<binary>.new` in place: keep the running binary as `.prev`, write
/// `.pending` with the new version, and rename `.new` over the binary.
pub fn swap(bin: &Path, version: &str) -> Result<(), UpdateError> {
    let new = sidecar(bin, NEW);
    let prev = sidecar(bin, PREV);
    let pending = sidecar(bin, PENDING);
    if !new.is_file() {
        return Err(UpdateError::Io {
            path: new,
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
    }
    // The running binary stays as `.prev`: a hard link (no copy of the
    // bytes), else a copy.
    let tmp = sidecar(bin, "prev.tmp");
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(bin, &tmp).is_err() {
        std::fs::copy(bin, &tmp).map_err(io_err(&tmp))?;
    }
    std::fs::rename(&tmp, &prev).map_err(io_err(&prev))?;
    // The guard in the unit counts the starts of the new binary here.
    let pending_tmp = sidecar(bin, "pending.tmp");
    write_file(&pending_tmp, format!("{version}\n").as_bytes(), 0o644)?;
    std::fs::rename(&pending_tmp, &pending).map_err(io_err(&pending))?;
    if let Err(e) = std::fs::rename(&new, bin) {
        let _ = std::fs::remove_file(&pending);
        return Err(io_err(bin)(e));
    }
    if let Some(dir) = bin.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const UNIT: &str = include_str!("../terrarium-host.service");

    fn offer(version: &str, data: &[u8]) -> UpdateOffer {
        UpdateOffer {
            version: version.into(),
            path: "/install/terrarium-host?arch=x86_64".into(),
            sha256: sha256_hex(data),
            release_sha256: None,
        }
    }

    /// A fake binary: a script that prints `terrarium-host <version>`.
    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho 'terrarium-host {version}'\n").into_bytes()
    }

    fn install(dir: &Path, data: &[u8]) -> PathBuf {
        let bin = dir.join("terrarium-host");
        write_file(&bin, data, 0o755).unwrap();
        bin
    }

    #[test]
    fn decide_says_why_not() {
        let o = offer("0.3.0+bbbbbbb", b"x");
        assert!(decide(&o, "0.2.0+aaaaaaa", true, true, None).is_ok());
        assert!(
            decide(&o, "0.2.0+aaaaaaa", false, true, None)
                .unwrap_err()
                .contains("auto_update = false")
        );
        assert!(
            decide(&o, "0.3.0+bbbbbbb-dirty", true, true, None)
                .unwrap_err()
                .contains("already")
        );
        assert!(
            decide(&o, "0.2.0+aaaaaaa", true, true, Some("0.3.0+bbbbbbb"))
                .unwrap_err()
                .contains("did not start")
        );
        // Another failed version does not block this one.
        assert!(decide(&o, "0.2.0+aaaaaaa", true, true, Some("0.2.5+ccccccc")).is_ok());
        assert!(
            decide(&o, "0.2.0+aaaaaaa", true, false, None)
                .unwrap_err()
                .contains("systemd")
        );
    }

    #[test]
    fn http_url_keeps_the_authority_of_the_link() {
        let p = "/install/terrarium-host?arch=x86_64";
        assert_eq!(
            http_url("wss://t.lan:8082/hosts/connect", p).unwrap(),
            "https://t.lan:8082/install/terrarium-host?arch=x86_64"
        );
        assert_eq!(
            http_url("ws://127.0.0.1:8082/hosts/connect", p).unwrap(),
            "http://127.0.0.1:8082/install/terrarium-host?arch=x86_64"
        );
        assert_eq!(
            http_url("wss://[::1]:8082", p).unwrap(),
            "https://[::1]:8082/install/terrarium-host?arch=x86_64"
        );
        // The offer cannot send the host agent to another host.
        assert!(http_url("wss://t.lan:8082", "//evil.example/x").is_err());
        assert!(http_url("wss://t.lan:8082", "https://evil.example/x").is_err());
        assert!(http_url("wss://t.lan:8082", "/a b").is_err());
        assert!(http_url("ftp://t.lan", p).is_err());
    }

    #[test]
    fn responses_parse() {
        let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
        assert_eq!(parse_response(ok).unwrap(), b"hello");
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), b"hello");
        let short = b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhello";
        assert!(parse_response(short).unwrap_err().contains("5 of 9"));
        let refused = b"HTTP/1.1 404 Not Found\r\ncontent-length: 7\r\n\r\nno arch";
        assert_eq!(
            parse_response(refused).unwrap_err(),
            "the daemon answered 404: no arch"
        );
        assert!(parse_response(b"garbage").is_err());
    }

    #[tokio::test]
    async fn download_over_http() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut req = vec![0u8; 4096];
            let n = s.read(&mut req).await.unwrap();
            let req = String::from_utf8_lossy(&req[..n]).to_string();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nbin")
                .await
                .unwrap();
            req
        });
        let url = format!("http://{addr}/install/terrarium-host?arch=x86_64");
        assert_eq!(download(&url, None, 1024).await.unwrap(), b"bin");
        let req = server.await.unwrap();
        assert!(req.starts_with("GET /install/terrarium-host?arch=x86_64 HTTP/1.1\r\n"));
        assert!(req.contains(&format!("Host: {addr}\r\n")));
    }

    #[tokio::test]
    async fn download_refuses_an_answer_over_the_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut req = vec![0u8; 4096];
            let _ = s.read(&mut req).await;
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 200000\r\n\r\n")
                .await;
            let _ = s.write_all(&vec![b'x'; 200_000]).await;
        });
        let e = download(&format!("http://{addr}/x"), None, 10)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("larger than 10 bytes"), "{e}");
    }

    #[test]
    fn a_checksum_mismatch_is_refused() {
        let data = script("0.3.0+bbbbbbb");
        let mut o = offer("0.3.0+bbbbbbb", &data);
        assert_eq!(verify(&data, &o).unwrap(), sha256_hex(&data));
        // The daemon's hash in upper case is the same hash.
        o.sha256 = o.sha256.to_ascii_uppercase();
        assert!(verify(&data, &o).is_ok());
        let e = verify(b"tampered", &o).unwrap_err();
        assert!(
            matches!(e, UpdateError::Checksum { what: "daemon", .. }),
            "{e}"
        );
        // The release sum must match too.
        o.release_sha256 = Some("00".repeat(32));
        let e = verify(&data, &o).unwrap_err();
        assert!(
            matches!(
                e,
                UpdateError::Checksum {
                    what: "release SHA256SUMS",
                    ..
                }
            ),
            "{e}"
        );
        o.release_sha256 = None;
        o.sha256 = "not a hash".into();
        assert!(matches!(verify(&data, &o), Err(UpdateError::Offer(_))));
    }

    #[tokio::test]
    async fn stage_runs_the_new_binary_and_swap_keeps_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let old = script("0.2.0+aaaaaaa");
        let bin = install(dir.path(), &old);
        check_writable(&bin).unwrap();
        let new = script("0.3.0+bbbbbbb");
        let staged = stage(&bin, &new, "0.3.0+bbbbbbb").await.unwrap();
        assert_eq!(staged, sidecar(&bin, NEW));
        // Nothing changed for the running binary yet.
        assert_eq!(std::fs::read(&bin).unwrap(), old);
        swap(&bin, "0.3.0+bbbbbbb").unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), new);
        assert_eq!(std::fs::read(sidecar(&bin, PREV)).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(sidecar(&bin, PENDING)).unwrap(),
            "0.3.0+bbbbbbb\n"
        );
        assert!(!sidecar(&bin, NEW).exists());
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // The first welcome confirms it.
        assert_eq!(confirm(&bin).as_deref(), Some("0.3.0+bbbbbbb"));
        assert!(!sidecar(&bin, PENDING).exists());
        assert!(sidecar(&bin, PREV).exists());
        assert_eq!(confirm(&bin), None);
    }

    #[tokio::test]
    async fn a_binary_that_does_not_run_is_not_staged() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install(dir.path(), &script("0.2.0+aaaaaaa"));
        // It exits at once.
        let e = stage(&bin, b"#!/bin/sh\nexit 3\n", "0.3.0+bbbbbbb")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("exited"), "{e}");
        assert!(!sidecar(&bin, NEW).exists());
        // It reports another build.
        let e = stage(&bin, &script("0.3.0+ccccccc"), "0.3.0+bbbbbbb")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not 0.3.0+bbbbbbb"), "{e}");
        // It is not a program at all.
        let e = stage(&bin, b"\x7fELF garbage", "0.3.0+bbbbbbb")
            .await
            .unwrap_err();
        assert!(matches!(e, UpdateError::Smoke(_)), "{e}");
        assert!(!sidecar(&bin, NEW).exists());
        // And swap with nothing staged changes nothing.
        assert!(swap(&bin, "0.3.0+bbbbbbb").is_err());
        assert!(!sidecar(&bin, PENDING).exists());
    }

    /// A file with a write fd open, as a child forked by another thread
    /// holds it: the exec fails with ETXTBSY until the fd closes.
    fn busy(dir: &Path) -> (PathBuf, std::fs::File) {
        let path = install(dir, &script("0.3.0+bbbbbbb"));
        let fd = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        (path, fd)
    }

    #[tokio::test]
    async fn the_smoke_test_waits_out_a_short_text_file_busy() {
        let dir = tempfile::tempdir().unwrap();
        let (path, fd) = busy(dir.path());
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(fd);
        });
        smoke_test(&path, "0.3.0+bbbbbbb", SMOKE_TIMEOUT)
            .await
            .unwrap();
        closer.join().unwrap();
    }

    #[tokio::test]
    async fn the_smoke_test_gives_up_on_a_long_text_file_busy() {
        let dir = tempfile::tempdir().unwrap();
        let (path, fd) = busy(dir.path());
        let began = std::time::Instant::now();
        let e = smoke_test(&path, "0.3.0+bbbbbbb", SMOKE_TIMEOUT)
            .await
            .unwrap_err();
        let took = began.elapsed();
        drop(fd);
        assert!(e.to_string().contains("Text file busy"), "{e}");
        assert!(took >= BUSY_RETRY, "it tried again for {took:?}");
        assert!(took < SMOKE_TIMEOUT, "{took:?}");
    }

    #[test]
    fn a_read_only_directory_is_reported() {
        if nix::unistd::geteuid().is_root() {
            return; // root writes anywhere
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = install(dir.path(), b"x");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = check_writable(&bin);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(r, Err(UpdateError::Io { .. })));
    }

    /// The `ExecStartPre=` guard of the unit, as systemd runs it: `$$` is
    /// `$`, and the binary path is the last argument.
    fn guard() -> String {
        let line = UNIT
            .lines()
            .find(|l| l.starts_with("ExecStartPre="))
            .expect("the unit has the guard");
        let rest = line
            .strip_prefix("ExecStartPre=-/bin/sh -c '")
            .expect("the guard is -/bin/sh -c '...'");
        let (script, args) = rest.rsplit_once("' ").unwrap();
        assert_eq!(
            args, "guard /var/lib/terrarium-host/bin/terrarium-host",
            "the guard names the binary of ExecStart"
        );
        assert!(!script.contains('%'), "no systemd specifiers in the guard");
        script.replace("$$", "$")
    }

    fn run_guard(bin: &Path) -> std::process::Output {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(guard())
            .arg("guard")
            .arg(bin)
            .output()
            .unwrap()
    }

    #[test]
    fn the_unit_runs_the_guarded_binary() {
        assert!(UNIT.contains(
            "\nExecStart=/var/lib/terrarium-host/bin/terrarium-host --config \
             /etc/terrarium-host/config.toml run\n"
        ));
        assert!(UNIT.contains("\nRestart=always\n"));
    }

    #[tokio::test]
    async fn the_guard_rolls_back_a_binary_that_does_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let old = script("0.2.0+aaaaaaa");
        let bin = install(dir.path(), &old);
        // No update pending: the guard does nothing.
        assert!(run_guard(&bin).status.success());
        assert!(!sidecar(&bin, PENDING).exists());

        let new = script("0.3.0+bbbbbbb");
        stage(&bin, &new, "0.3.0+bbbbbbb").await.unwrap();
        swap(&bin, "0.3.0+bbbbbbb").unwrap();
        // Three starts of the new binary that never reach the daemon.
        for _ in 0..3 {
            let out = run_guard(&bin);
            assert!(out.status.success());
            assert_eq!(std::fs::read(&bin).unwrap(), new);
        }
        // The fourth start runs the old binary.
        let out = run_guard(&bin);
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("restored the previous binary"),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&bin).unwrap(), old);
        assert!(!sidecar(&bin, PENDING).exists());
        assert!(!sidecar(&bin, PREV).exists());
        assert_eq!(failed_version(&bin).as_deref(), Some("0.3.0+bbbbbbb"));
        // The old binary does not take that version again.
        let o = offer("0.3.0+bbbbbbb", &new);
        assert!(
            decide(
                &o,
                "0.2.0+aaaaaaa",
                true,
                true,
                failed_version(&bin).as_deref()
            )
            .is_err()
        );
        // The next start is a normal one.
        assert!(run_guard(&bin).status.success());
        assert_eq!(std::fs::read(&bin).unwrap(), old);
    }

    #[tokio::test]
    async fn the_guard_leaves_a_confirmed_update_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install(dir.path(), &script("0.2.0+aaaaaaa"));
        let new = script("0.3.0+bbbbbbb");
        stage(&bin, &new, "0.3.0+bbbbbbb").await.unwrap();
        swap(&bin, "0.3.0+bbbbbbb").unwrap();
        assert!(run_guard(&bin).status.success());
        confirm(&bin);
        for _ in 0..5 {
            assert!(run_guard(&bin).status.success());
        }
        assert_eq!(std::fs::read(&bin).unwrap(), new);
        assert_eq!(failed_version(&bin), None);
    }

    #[test]
    fn the_guard_without_a_prev_keeps_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install(dir.path(), b"new");
        std::fs::write(sidecar(&bin, PENDING), "0.3.0+bbbbbbb\n").unwrap();
        for _ in 0..5 {
            assert!(run_guard(&bin).status.success());
        }
        assert_eq!(std::fs::read(&bin).unwrap(), b"new");
    }
}
