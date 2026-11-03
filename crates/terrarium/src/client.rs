//! The HTTP transport: HTTP/1.1 to the daemon, over the Unix socket or TCP.
//! It also opens the two streams that the runner tunnel cannot carry:
//! Server-Sent Events (`--follow`) and the PTY WebSocket (`agent attach`).

use std::path::PathBuf;

use anyhow::Context;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, Uri, header};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::transport::{ApiRequest, ApiResponse, BoxFuture, Transport, decode_body};

/// Where the daemon is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp {
        /// `host:port`, for the connect and the Host header.
        authority: String,
        /// A path prefix, without a trailing `/`.
        base: String,
        /// `https://`: TLS, with the system roots and `TERRARIUM_CA_FILE`.
        tls: bool,
    },
}

impl Endpoint {
    /// Parse an `http://` or `https://host[:port][/base]` URL.
    pub fn from_url(url: &str) -> anyhow::Result<Self> {
        let bad = || {
            anyhow::anyhow!("bad daemon URL `{url}`: use https://host[:port] or http://host:port")
        };
        let uri: Uri = url.parse().map_err(|_| bad())?;
        let tls = match uri.scheme_str() {
            Some("http") => false,
            Some("https") => true,
            _ => return Err(bad()),
        };
        let host = uri.host().ok_or_else(bad)?;
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        Ok(Self::Tcp {
            authority: format!("{host}:{port}"),
            base: uri.path().trim_end_matches('/').to_owned(),
            tls,
        })
    }

    /// The host name of a TCP endpoint, without the port.
    pub fn host_name(&self) -> Option<&str> {
        match self {
            Self::Unix(_) => None,
            Self::Tcp { authority, .. } => Some(
                authority
                    .rsplit_once(':')
                    .map_or(authority.as_str(), |(h, _)| h),
            ),
        }
    }

    /// Does a request to this endpoint stay on this machine, or go over
    /// TLS? A plain `http://` URL to another host sends the password and
    /// the session token in clear.
    pub fn is_private(&self) -> bool {
        match self {
            Self::Unix(_) | Self::Tcp { tls: true, .. } => true,
            Self::Tcp { .. } => self.host_name().is_some_and(is_loopback_host),
        }
    }

    fn host(&self) -> &str {
        match self {
            Self::Unix(_) => "localhost",
            Self::Tcp { authority, .. } => authority,
        }
    }

    fn base(&self) -> &str {
        match self {
            Self::Unix(_) => "",
            Self::Tcp { base, .. } => base,
        }
    }
}

/// `localhost`, or a loopback IP address (`127.0.0.0/8`, `::1`).
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']);
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
            Self::Tcp {
                authority,
                base,
                tls,
            } => {
                let scheme = if *tls { "https" } else { "http" };
                write!(f, "{scheme}://{authority}{base}")
            }
        }
    }
}

/// A connected byte stream to the daemon.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

#[derive(Debug, Clone)]
pub struct HttpTransport {
    pub endpoint: Endpoint,
    token: Option<String>,
}

impl HttpTransport {
    pub fn new(endpoint: Endpoint, token: Option<String>) -> Self {
        Self { endpoint, token }
    }

    async fn connect(&self) -> anyhow::Result<Box<dyn Stream>> {
        let ctx = || format!("cannot reach the daemon at {}", self.endpoint);
        Ok(match &self.endpoint {
            Endpoint::Unix(socket) => Box::new(
                tokio::net::UnixStream::connect(socket)
                    .await
                    .with_context(ctx)?,
            ),
            Endpoint::Tcp { authority, tls, .. } => {
                let tcp = tokio::net::TcpStream::connect(authority.as_str())
                    .await
                    .with_context(ctx)?;
                if *tls {
                    let host = self.endpoint.host_name().unwrap_or(authority);
                    crate::tls::connect(tcp, host).await.with_context(ctx)?
                } else {
                    Box::new(tcp)
                }
            }
        })
    }

    /// Send one request and give the raw response, with its body unread.
    async fn raw(&self, req: &ApiRequest, accept: &str) -> anyhow::Result<Response<Incoming>> {
        let stream = self.connect().await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .context("HTTP handshake with the daemon failed")?;
        tokio::spawn(async move {
            // The connection ends when the response is read; errors surface
            // through the request.
            let _ = conn.await;
        });
        let uri = format!("{}{}", self.endpoint.base(), req.path_and_query());
        let mut builder = Request::builder()
            .method(req.method.as_str())
            .uri(uri)
            .header(header::HOST, self.endpoint.host())
            .header(header::ACCEPT, accept);
        if let Some(token) = &self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match &req.body {
            Some(v) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Bytes::from(serde_json::to_vec(v)?)
            }
            None => Bytes::new(),
        };
        let resp = sender
            .send_request(builder.body(Full::new(body))?)
            .await
            .context("the daemon did not answer")?;
        Ok(resp)
    }

    async fn round_trip(&self, req: ApiRequest) -> anyhow::Result<ApiResponse> {
        let resp = self.raw(&req, "application/json").await?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let body = resp
            .into_body()
            .collect()
            .await
            .context("cannot read the daemon's answer")?
            .to_bytes();
        Ok(ApiResponse {
            status,
            headers,
            body: decode_body(&body),
        })
    }

    /// Read Server-Sent Events. `on_event` gets each `data:` JSON. It
    /// returns `false` to stop. A non-2xx answer comes back as a response.
    pub async fn sse(
        &self,
        req: &ApiRequest,
        mut on_event: impl FnMut(Value) -> anyhow::Result<bool>,
    ) -> anyhow::Result<Option<ApiResponse>> {
        let resp = self.raw(req, "text/event-stream").await?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.into_body().collect().await?.to_bytes();
            return Ok(Some(ApiResponse {
                status,
                headers: Default::default(),
                body: decode_body(&body),
            }));
        }
        let mut body = resp.into_body();
        let mut parser = SseParser::default();
        while let Some(frame) = body.frame().await {
            let frame = frame.context("the event stream broke")?;
            let Some(chunk) = frame.data_ref() else {
                continue;
            };
            for data in parser.push(chunk) {
                let event = serde_json::from_str(&data).unwrap_or(Value::String(data));
                if !on_event(event)? {
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    /// Write the body of a GET to `out`, for a file download. A non-2xx
    /// answer comes back as a response, and `out` stays empty. Answers the
    /// bytes written.
    pub async fn download(
        &self,
        req: &ApiRequest,
        out: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<Result<u64, ApiResponse>> {
        use tokio::io::AsyncWriteExt;
        let resp = self.raw(req, "application/octet-stream").await?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.into_body().collect().await?.to_bytes();
            return Ok(Err(ApiResponse {
                status,
                headers: Default::default(),
                body: decode_body(&body),
            }));
        }
        let mut body = resp.into_body();
        let mut n = 0u64;
        while let Some(frame) = body.frame().await {
            let frame = frame.context("the download broke")?;
            if let Some(chunk) = frame.data_ref() {
                out.write_all(chunk).await?;
                n += chunk.len() as u64;
            }
        }
        out.flush().await?;
        Ok(Ok(n))
    }

    /// Open a WebSocket on `path`, for `agent attach`.
    pub async fn websocket(
        &self,
        path: &str,
    ) -> anyhow::Result<tokio_tungstenite::WebSocketStream<Box<dyn Stream>>> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = format!(
            "ws://{}{}{}",
            self.endpoint.host(),
            self.endpoint.base(),
            path
        );
        let mut req = url.into_client_request()?;
        if let Some(token) = &self.token {
            req.headers_mut()
                .insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
        }
        let stream = self.connect().await?;
        let (ws, _) = tokio_tungstenite::client_async(req, stream)
            .await
            .map_err(|e| match e {
                tokio_tungstenite::tungstenite::Error::Http(resp) => {
                    let body = resp.body().as_deref().map(decode_body).unwrap_or_default();
                    let msg = body["error"]["message"]
                        .as_str()
                        .map_or_else(|| body.to_string(), str::to_owned);
                    anyhow::anyhow!("the daemon refused the attach ({}): {msg}", resp.status())
                }
                e => anyhow::Error::new(e).context("the WebSocket handshake failed"),
            })?;
        Ok(ws)
    }
}

impl Transport for HttpTransport {
    fn send(&self, req: ApiRequest) -> BoxFuture<'_, anyhow::Result<ApiResponse>> {
        Box::pin(self.round_trip(req))
    }

    fn describe(&self) -> String {
        self.endpoint.to_string()
    }
}

/// An incremental parser for `text/event-stream`. It gives the `data` of
/// each complete event.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                self.data.push(d.strip_prefix(' ').unwrap_or(d).to_owned());
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::transport::Method;

    #[test]
    fn urls_parse() {
        assert_eq!(
            Endpoint::from_url("http://localhost:8080").unwrap(),
            Endpoint::Tcp {
                authority: "localhost:8080".into(),
                base: String::new(),
                tls: false,
            }
        );
        assert_eq!(
            Endpoint::from_url("http://box/terrarium/").unwrap(),
            Endpoint::Tcp {
                authority: "box:80".into(),
                base: "/terrarium".into(),
                tls: false,
            }
        );
        let https = Endpoint::from_url("https://t.example.com/").unwrap();
        assert_eq!(
            https,
            Endpoint::Tcp {
                authority: "t.example.com:443".into(),
                base: String::new(),
                tls: true,
            }
        );
        assert_eq!(https.to_string(), "https://t.example.com:443");
        assert_eq!(https.host_name(), Some("t.example.com"));
        assert!(Endpoint::from_url("ftp://box").is_err());
        assert!(Endpoint::from_url("not a url").is_err());
    }

    /// Only TLS or loopback is private: a plain URL to another host is not.
    #[test]
    fn private_endpoints() {
        let p = |u: &str| Endpoint::from_url(u).unwrap().is_private();
        assert!(p("http://localhost:8080"));
        assert!(p("http://127.0.0.1:8080"));
        assert!(p("http://127.9.9.9:8080"));
        assert!(p("http://[::1]:8080"));
        assert!(p("https://t.example.com"));
        assert!(!p("http://t.lan:8080"));
        assert!(!p("http://box.tail1234.ts.net:8080"));
        assert!(!p("http://10.0.0.5:8080"));
        assert!(Endpoint::Unix("/x.sock".into()).is_private());
    }

    /// An `https://` daemon with a certificate that no root trusts: the
    /// handshake fails, and nothing is sent in clear.
    #[tokio::test]
    async fn https_checks_the_certificate() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // A server that speaks no TLS: the handshake must fail.
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
            }
        });
        let t = HttpTransport::new(
            Endpoint::from_url(&format!("https://localhost:{}", addr.port())).unwrap(),
            Some("secret".into()),
        );
        let err = t
            .send(ApiRequest::new(Method::Get, "/api/health"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("TLS"), "{err:#}");
    }

    #[test]
    fn sse_parses_split_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(b"id: 1\nevent: status\nda").is_empty());
        assert_eq!(
            p.push(b"ta: {\"seq\":1}\n\n: keepalive\n\n"),
            vec!["{\"seq\":1}"]
        );
        assert_eq!(p.push(b"data: a\r\ndata: b\r\n\r\n"), vec!["a\nb"]);
    }

    #[tokio::test]
    async fn a_missing_socket_is_a_connect_error() {
        let t = HttpTransport::new(Endpoint::Unix("/nonexistent/api.sock".into()), None);
        let err = t
            .send(ApiRequest::new(Method::Get, "/api/health"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("cannot reach the daemon"));
    }
}
