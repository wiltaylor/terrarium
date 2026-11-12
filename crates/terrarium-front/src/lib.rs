//! Byte-streaming front. No credentials, request logging, or writable state.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(2);

// Signal completion without buffering or setting a deadline on an upload.
struct UploadBody {
    body: Body,
    sent: Option<tokio::sync::oneshot::Sender<()>>,
}
impl axum::body::HttpBody for UploadBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let result = std::pin::Pin::new(&mut this.body).poll_frame(cx);
        if (matches!(result, std::task::Poll::Ready(None)) || this.body.is_end_stream())
            && let Some(sent) = this.sent.take()
        {
            let _ = sent.send(());
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

pub struct Front {
    pub upstream: String,
    pub status: PathBuf,
    pub sessions: PathBuf,
    pub webhook: bool,
    outage: Mutex<Option<Instant>>,
}

impl Front {
    pub fn new(upstream: String, status: PathBuf, sessions: PathBuf, webhook: bool) -> Self {
        Self {
            upstream,
            status,
            sessions,
            webhook,
            outage: Mutex::new(None),
        }
    }
    pub fn router(self) -> axum::Router {
        axum::Router::new()
            .fallback(proxy)
            .with_state(Arc::new(self))
    }
}

async fn proxy(State(front): State<Arc<Front>>, mut req: Request) -> Response {
    let browser = !front.webhook
        && req.method() == axum::http::Method::GET
        && !req.uri().path().starts_with("/api/")
        && req
            .headers()
            .get(header::ACCEPT)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.contains("text/html"));
    let cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let upgrade = hyper::upgrade::on(&mut req);
    let response = async {
        let stream = timeout(DEADLINE, TcpStream::connect(&front.upstream)).await??;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
        });
        use axum::body::HttpBody;
        let (sent, uploaded) = tokio::sync::oneshot::channel();
        let (parts, body) = req.into_parts();
        let sent = if body.is_end_stream() {
            let _ = sent.send(());
            None
        } else {
            Some(sent)
        };
        let req = Request::from_parts(parts, Body::new(UploadBody { body, sent }));
        let response = sender.send_request(req);
        tokio::pin!(response);
        let response = tokio::select! {
            response = &mut response => response?,
            _ = uploaded => timeout(DEADLINE, &mut response).await??,
        };
        Ok::<_, anyhow::Error>(response)
    }
    .await;
    match response {
        Ok(mut response) => {
            if let Ok(mut outage) = front.outage.lock() {
                *outage = None;
            }
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                let upstream = hyper::upgrade::on(&mut response);
                tokio::spawn(async move {
                    if let (Ok(client), Ok(server)) = tokio::join!(upgrade, upstream) {
                        let _ = tokio::io::copy_bidirectional(
                            &mut TokioIo::new(client),
                            &mut TokioIo::new(server),
                        )
                        .await;
                    }
                });
            }
            response.map(Body::new)
        }
        _ => front.maintenance(browser, &cookie).await,
    }
}

#[derive(Deserialize)]
struct Export {
    at: i64,
    sessions: Vec<SessionHash>,
}
#[derive(Deserialize)]
struct SessionHash {
    hash: String,
    exp: i64,
}

fn authorized(cookie: &str, data: &[u8], now: i64) -> bool {
    use hmac::digest::CtOutput;
    let Ok(export) = serde_json::from_slice::<Export>(data) else {
        return false;
    };
    if now < export.at || now - export.at > 300 {
        return false;
    }
    let Some(token) = cookie
        .split(';')
        .find_map(|s| s.trim().strip_prefix("terrarium_session="))
    else {
        return false;
    };
    if token.len() > 8192 {
        return false;
    }
    let actual = CtOutput::<Sha256>::new(Sha256::digest(token.as_bytes()));
    export.sessions.iter().any(|s| {
        if s.exp <= now || s.hash.len() != 64 {
            return false;
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let Some(pair) = s.hash.get(i * 2..i * 2 + 2) else {
                return false;
            };
            let Ok(n) = u8::from_str_radix(pair, 16) else {
                return false;
            };
            *byte = n;
        }
        actual == CtOutput::<Sha256>::new(bytes.into())
    })
}

impl Front {
    async fn maintenance(&self, browser: bool, cookie: &str) -> Response {
        let now = chrono::Utc::now().timestamp();
        let private = !self.webhook
            && authorized(
                cookie,
                &tokio::fs::read(&self.sessions).await.unwrap_or_default(),
                now,
            );
        let current = tokio::fs::read(&self.status).await.unwrap_or_default();
        let current: Value = serde_json::from_slice(&current).unwrap_or(Value::Null);
        let run = current.is_object().then_some(&current);
        let running = run.is_some_and(|r| r["outcome"] == "running");
        let elapsed = self
            .outage
            .lock()
            .map(|mut o| o.get_or_insert_with(Instant::now).elapsed().as_secs())
            .unwrap_or(0);
        let mut value = json!({"maintenance": true, "message": if running { "Terrarium is updating" } else { "Terrarium is restarting" },
            "step": null, "elapsed_seconds": elapsed});
        if let Some(run) = run {
            if running {
                let step = run["steps"]
                    .as_array()
                    .and_then(|s| s.last())
                    .and_then(|s| s["name"].as_str());
                // Never publish arbitrary detail, versions, or error text as a step.
                if let Some(step) = step.filter(|s| PUBLIC_STEPS.contains(s)) {
                    value["step"] = json!(step);
                }
                if let Some(start) = run["started_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                {
                    value["elapsed_seconds"] = json!((now - start.timestamp()).max(0));
                }
            }
            if private {
                value["update"] = json!({"steps": run["steps"], "from": run["from"], "to": run["to"], "error": run["error"]});
            }
        }
        let (content_type, body) = if browser {
            ("text/html; charset=utf-8", PAGE.to_owned())
        } else {
            ("application/json", value.to_string())
        };
        (StatusCode::SERVICE_UNAVAILABLE, [
            (header::CONTENT_TYPE, content_type), (header::RETRY_AFTER, "2"),
            (header::CACHE_CONTROL, "no-store"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'")
        ], body).into_response()
    }
}

const PUBLIC_STEPS: &[&str] = &[
    "inspect",
    "resolve",
    "verify",
    "pull",
    "drain",
    "backup",
    "stop",
    "restore",
    "start",
    "health",
    "rollback",
    "pause",
    "snapshot",
    "snapshot-database",
    "switch",
    "start-daemon",
    "resume",
    "activation",
    "commit",
    "drain-jobs",
    "environment",
    "hot-swap-eligibility",
    "pause-daemon",
    "remove",
    "stage",
];
const PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Terrarium maintenance</title></head>
<body><h1>Terrarium is restarting</h1><p id="progress"></p><pre id="details"></pre>
<p>Details require an existing operator login. New logins are available when Terrarium returns.</p>
<script>async function poll(){try{const r=await fetch(location.href,{headers:{Accept:'application/json'},cache:'no-store'});
if(r.status!==503){location.reload();return}const p=await r.json();if(!p.maintenance)return;
document.querySelector('h1').textContent=p.message;
document.querySelector('#progress').textContent=(p.step||'Waiting for daemon')+' · '+p.elapsed_seconds+' seconds';
document.querySelector('#details').textContent=p.update?JSON.stringify(p.update,null,2):'';
}catch{}}poll();setInterval(poll,2000);</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_fails_closed() {
        let hash: String = Sha256::digest(b"test-session")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let export = json!({"at": 1000, "sessions": [{"hash": hash, "exp": 1500}]}).to_string();
        assert!(authorized(
            "terrarium_session=test-session",
            export.as_bytes(),
            1100
        ));
        assert!(!authorized(
            "terrarium_session=wrong",
            export.as_bytes(),
            1100
        ));
        assert!(!authorized(
            "terrarium_session=test-session",
            export.as_bytes(),
            1301
        ));
        assert!(!authorized(
            "terrarium_session=test-session",
            export.as_bytes(),
            999
        ));
        assert!(!authorized(
            "terrarium_session=test-session",
            b"bad json",
            1100
        ));
        assert!(!authorized(
            "terrarium_session=test-session",
            export.as_bytes(),
            1500
        ));
    }

    #[tokio::test]
    async fn public_projection_hides_details_and_private_projection_shows_them() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current-run.json");
        let sessions = dir.path().join("sessions.json");
        let now = chrono::Utc::now();
        tokio::fs::write(
            &current,
            json!({"outcome":"running", "started_at": now.to_rfc3339(),
            "steps":[{"name":"backup","at":now.to_rfc3339(),"detail":"secret-step-detail"}],
            "from":"private-version", "to":"next-version", "error":"private-error"})
            .to_string(),
        )
        .await
        .unwrap();
        let hash: String = Sha256::digest(b"test-session")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        tokio::fs::write(
            &sessions,
            json!({"at":now.timestamp(), "sessions":[{"hash":hash,"exp":now.timestamp()+600}]})
                .to_string(),
        )
        .await
        .unwrap();
        let front = Front::new("unused".into(), current, sessions, false);
        let body = |r: Response| async {
            String::from_utf8(
                axum::body::to_bytes(r.into_body(), 100000)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap()
        };
        let public = body(front.maintenance(false, "").await).await;
        assert!(public.contains("backup"));
        assert!(public.contains("Terrarium is updating"));
        for secret in [
            "secret-step-detail",
            "private-version",
            "next-version",
            "private-error",
            "steps",
        ] {
            assert!(!public.contains(secret));
        }
        let private = body(
            front
                .maintenance(false, "terrarium_session=test-session")
                .await,
        )
        .await;
        for secret in [
            "secret-step-detail",
            "private-version",
            "next-version",
            "private-error",
        ] {
            assert!(private.contains(secret));
        }
    }
}
