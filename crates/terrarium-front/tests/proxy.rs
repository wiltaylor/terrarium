use axum::{Router, routing::get};
use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::time::Duration;
use terrarium_front::Front;
use tokio::net::TcpListener;

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, task)
}
fn front(upstream: String, webhook: bool) -> Router {
    Front::new(
        upstream,
        PathBuf::from("/nonexistent/history"),
        PathBuf::from("/nonexistent/sessions"),
        webhook,
    )
    .router()
}

#[tokio::test]
async fn outage_browser_api_webhook_and_recovery() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap().to_string();
    drop(listener);
    let (address, task) = serve(front(upstream.clone(), false)).await;
    let client = reqwest::Client::new();
    let url = format!("http://{address}");
    let page = client
        .get(&url)
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 503);
    assert_eq!(page.headers()["retry-after"], "2");
    let page = page.text().await.unwrap();
    assert!(page.contains("location.reload()"));
    assert!(page.contains("setInterval(poll,2000)"));
    let api = client
        .get(format!("{url}/api/status"))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), 503);
    assert!(api.text().await.unwrap().contains("\"maintenance\":true"));
    let (hooks, hook_task) = serve(front(upstream.clone(), true)).await;
    let hook = client
        .post(format!("http://{hooks}/hooks/test"))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(hook.status(), 503);
    assert_eq!(hook.headers()["retry-after"], "2");
    assert!(
        hook.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let listener = TcpListener::bind(&upstream).await.unwrap();
    let daemon = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/", get(|| async { "dashboard" })),
        )
        .await
        .unwrap();
    });
    assert_eq!(
        client.get(&url).send().await.unwrap().text().await.unwrap(),
        "dashboard"
    );
    daemon.abort();
    let _ = daemon.await;
    // A replacement daemon can take arbitrarily long. The same front stays up.
    for _ in 0..3 {
        let waiting = client
            .get(&url)
            .header("accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(waiting.status(), 503);
        assert!(
            waiting
                .text()
                .await
                .unwrap()
                .contains("Terrarium is restarting")
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let listener = TcpListener::bind(&upstream).await.unwrap();
    let replacement = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/", get(|| async { "replacement dashboard" })),
        )
        .await
        .unwrap();
    });
    assert_eq!(
        client.get(&url).send().await.unwrap().text().await.unwrap(),
        "replacement dashboard"
    );
    task.abort();
    hook_task.abort();
    replacement.abort();
}

#[tokio::test]
async fn headers_streams_and_websocket_survive() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    let upstream = Router::new()
        .route(
            "/headers",
            get(|headers: axum::http::HeaderMap| async move {
                format!(
                    "{}|{}|{}|{}",
                    headers["authorization"].to_str().unwrap(),
                    headers["cookie"].to_str().unwrap(),
                    headers["x-terrarium-link-key"].to_str().unwrap(),
                    headers["host"].to_str().unwrap()
                )
            }),
        )
        .route(
            "/events",
            get(|| async {
                let body = axum::body::Body::from_stream(futures_util::stream::unfold(
                    0,
                    |n| async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Some((Ok::<_, std::io::Error>(format!("data: {n}\n\n")), n + 1))
                    },
                ));
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(body)
                    .unwrap()
            }),
        )
        .route(
            "/ws",
            get(|ws: WebSocketUpgrade| async {
                ws.protocols(["test-protocol"])
                    .on_upgrade(|mut socket| async move {
                        if let Some(Ok(Message::Text(text))) = socket.recv().await {
                            socket.send(Message::Text(text)).await.unwrap();
                        }
                    })
            }),
        );
    let (upstream, daemon) = serve(upstream).await;
    let (address, task) = serve(front(upstream, false)).await;
    let client = reqwest::Client::new();
    let text = client
        .get(format!("http://{address}/headers"))
        .header("authorization", "Bearer test-only")
        .header("cookie", "terrarium_session=test-only")
        .header("x-terrarium-link-key", "link-test")
        .header("host", "original.example")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        text,
        "Bearer test-only|terrarium_session=test-only|link-test|original.example"
    );
    let mut stream = client
        .get(format!("http://{address}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(stream.chunk().await.unwrap().is_some());
    let mut req = format!("ws://{address}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("sec-websocket-protocol", "test-protocol".parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "test-protocol"
    );
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "hello".into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap().into_text().unwrap(),
        "hello"
    );
    task.abort();
    daemon.abort();
}

#[tokio::test]
async fn slow_headers_timeout_but_streaming_body_does_not() {
    let upstream = Router::new().route(
        "/",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            "late"
        }),
    );
    let (upstream, daemon) = serve(upstream).await;
    let (address, task) = serve(front(upstream, false)).await;
    let start = std::time::Instant::now();
    assert_eq!(
        reqwest::get(format!("http://{address}/"))
            .await
            .unwrap()
            .status(),
        503
    );
    assert!(start.elapsed() < Duration::from_secs(4));
    task.abort();
    daemon.abort();
}

#[tokio::test]
async fn paced_upload_is_not_an_outage_and_receives_the_daemon_response() {
    let upstream = Router::new().route(
        "/upload",
        axum::routing::post(|body: axum::body::Bytes| async move {
            assert_eq!(body.as_ref(), b"chunk0chunk1chunk2chunk3chunk4");
            "complete upload"
        }),
    );
    let (upstream, daemon) = serve(upstream).await;
    let (address, task) = serve(front(upstream, false)).await;
    let chunks = futures_util::stream::unfold(0, |n| async move {
        if n == 5 {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(550)).await;
        Some((Ok::<_, std::io::Error>(format!("chunk{n}")), n + 1))
    });
    let start = std::time::Instant::now();
    let response = reqwest::Client::new()
        .post(format!("http://{address}/upload"))
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "complete upload");
    assert!(start.elapsed() > Duration::from_secs(2));
    task.abort();
    daemon.abort();
}
