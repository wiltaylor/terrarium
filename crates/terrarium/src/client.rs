//! An HTTP client for the daemon API, over the Unix socket or TCP.

use std::path::PathBuf;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::{Request, StatusCode, Uri, header};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the daemon at {endpoint}: {source}")]
    Connect {
        endpoint: String,
        #[source]
        source: std::io::Error,
    },
    #[error("bad daemon URL `{0}`: use http://host:port")]
    BadUrl(String),
    #[error("HTTP error: {0}")]
    Http(#[from] hyper::Error),
    #[error("cannot build the request: {0}")]
    Request(#[from] hyper::http::Error),
    #[error("the daemon answered {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("cannot decode the daemon's answer: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Where the daemon is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp {
        /// `host:port`, for the connect and the Host header.
        authority: String,
        /// A path prefix, without a trailing `/`.
        base: String,
    },
}

impl Endpoint {
    /// Parse an `http://host[:port][/base]` URL.
    pub fn from_url(url: &str) -> Result<Self, ClientError> {
        let uri: Uri = url
            .parse()
            .map_err(|_| ClientError::BadUrl(url.to_owned()))?;
        if uri.scheme_str() != Some("http") {
            return Err(ClientError::BadUrl(url.to_owned()));
        }
        let host = uri
            .host()
            .ok_or_else(|| ClientError::BadUrl(url.to_owned()))?;
        let port = uri.port_u16().unwrap_or(80);
        Ok(Self::Tcp {
            authority: format!("{host}:{port}"),
            base: uri.path().trim_end_matches('/').to_owned(),
        })
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
            Self::Tcp { authority, base } => write!(f, "http://{authority}{base}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    pub endpoint: Endpoint,
    token: Option<String>,
}

impl Client {
    pub fn new(endpoint: Endpoint, token: Option<String>) -> Self {
        Self { endpoint, token }
    }

    /// `GET path` and decode the JSON body.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let body = self.get(path).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// `GET path`. A non-2xx answer is an error.
    pub async fn get(&self, path: &str) -> Result<Bytes, ClientError> {
        let connect_err = |source| ClientError::Connect {
            endpoint: self.endpoint.to_string(),
            source,
        };
        match &self.endpoint {
            Endpoint::Unix(socket) => {
                let stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .map_err(connect_err)?;
                self.send(stream, "localhost", path).await
            }
            Endpoint::Tcp { authority, base } => {
                let stream = tokio::net::TcpStream::connect(authority.as_str())
                    .await
                    .map_err(connect_err)?;
                self.send(stream, authority, &format!("{base}{path}")).await
            }
        }
    }

    async fn send<S>(&self, stream: S, host: &str, path: &str) -> Result<Bytes, ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            // The connection ends when the response is read; errors surface
            // through the request.
            let _ = conn.await;
        });
        let mut req = Request::get(path)
            .header(header::HOST, host)
            .header(header::ACCEPT, "application/json");
        if let Some(token) = &self.token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let resp = sender
            .send_request(req.body(Empty::<Bytes>::new())?)
            .await?;
        let status = resp.status();
        let body = resp.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            return Err(ClientError::Status {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(body)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::routing::get;

    #[test]
    fn urls_parse() {
        assert_eq!(
            Endpoint::from_url("http://localhost:8080").unwrap(),
            Endpoint::Tcp {
                authority: "localhost:8080".into(),
                base: String::new()
            }
        );
        assert_eq!(
            Endpoint::from_url("http://box/terrarium/").unwrap(),
            Endpoint::Tcp {
                authority: "box:80".into(),
                base: "/terrarium".into()
            }
        );
        assert!(Endpoint::from_url("https://box").is_err());
        assert!(Endpoint::from_url("not a url").is_err());
    }

    fn app() -> axum::Router {
        axum::Router::new()
            .route("/api/health", get(|| async { r#"{"ok":true}"# }))
            .route(
                "/api/auth",
                get(|headers: axum::http::HeaderMap| async move {
                    headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap_or_default().to_owned())
                        .unwrap_or_default()
                }),
            )
    }

    #[tokio::test]
    async fn talks_over_a_unix_socket() {
        let dir = std::env::temp_dir().join(format!("terrarium-cli-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move { axum::serve(listener, app()).await });

        let client = Client::new(Endpoint::Unix(socket), None);
        let v: serde_json::Value = client.get_json("/api/health").await.unwrap();
        assert_eq!(v["ok"], true);
        assert!(matches!(
            client.get("/missing").await,
            Err(ClientError::Status { status, .. }) if status == StatusCode::NOT_FOUND
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn talks_over_tcp_with_a_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app()).await });

        let endpoint = Endpoint::from_url(&format!("http://{addr}")).unwrap();
        let client = Client::new(endpoint, Some("t0ken".into()));
        let body = client.get("/api/auth").await.unwrap();
        assert_eq!(&body[..], b"Bearer t0ken");
    }

    #[tokio::test]
    async fn a_missing_socket_is_a_connect_error() {
        let client = Client::new(Endpoint::Unix("/nonexistent/api.sock".into()), None);
        assert!(matches!(
            client.get("/api/health").await,
            Err(ClientError::Connect { .. })
        ));
    }
}
