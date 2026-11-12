//! Loopback-only native service update fixture. Does not access enrollment.
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use terrarium_host::protocol::{DaemonFrame, HostFrame, UpdateOffer, Welcome, decode_control, encode_control};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(args.len() == 5, "usage: windows-update-stub PORT ASSET VERSION accept|reject");
    let port: u16 = args[1].parse()?;
    let asset = Arc::new(std::fs::read(&args[2])?);
    let version = args[3].clone();
    let reject = args[4] == "reject";
    let sha = terrarium_host::transfer::sha256_hex(&asset);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!("READY {port}");
    loop {
        let (mut stream, _) = listener.accept().await?;
        let asset = asset.clone();
        let version = version.clone();
        let sha = sha.clone();
        tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                let mut head = [0; 2048];
                let n = stream.peek(&mut head).await?;
                if head[..n].starts_with(b"GET /asset ") {
                    // Consume the request before replying; the client closes
                    // only after it has received the complete HTTP body.
                    let mut request = Vec::new();
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut head).await?;
                        anyhow::ensure!(n != 0 && request.len() < 16384, "bad HTTP request");
                        request.extend_from_slice(&head[..n]);
                    }
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", asset.len()).as_bytes()).await?;
                    stream.write_all(&asset).await?;
                    stream.shutdown().await?;
                    println!("DOWNLOAD {version}");
                    return Ok(());
                }
                let mut socket = tokio_tungstenite::accept_async(stream).await?;
                let message = socket.next().await.ok_or_else(|| anyhow::anyhow!("no hello"))??;
                let hello = match decode_control::<HostFrame>(message.to_text()?)? {
                    HostFrame::Hello(hello) => hello,
                    _ => anyhow::bail!("expected hello"),
                };
                println!("HELLO {}", hello.version);
                if reject && hello.version == version {
                    // The new image runs, but cannot finish its health check.
                    // Keep the socket open so SCM's bounded recovery is tested.
                    tokio::time::sleep(std::time::Duration::from_secs(180)).await;
                    return Ok(());
                }
                let update = (hello.version != version).then(|| UpdateOffer {
                    os: Some("windows".into()), arch: Some("x86_64".into()),
                    version: version.clone(), path: "/asset".into(),
                    sha256: sha.clone(), release_sha256: Some(sha),
                });
                let welcome = DaemonFrame::Welcome(Welcome {
                    host_id: "local-update-test".into(), heartbeat_secs: 1,
                    resume_token: "local-test-resume".into(), resume: vec![],
                    token: None, metrics: false, update,
                });
                socket.send(Message::Text(encode_control(&welcome)?.into())).await?;
                println!("WELCOME {}", hello.version);
                while let Some(message) = socket.next().await {
                    match message? {
                        Message::Text(text) => {
                            if let HostFrame::Ping { nonce } = decode_control::<HostFrame>(&text)? {
                                socket.send(Message::Text(encode_control(&DaemonFrame::Pong { nonce })?.into())).await?;
                            }
                        }
                        Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await?,
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
                Ok(())
            }.await;
            if let Err(error) = result { eprintln!("fixture connection: {error}"); }
        });
    }
}
