use std::path::PathBuf;
use terrarium_front::Front;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let web = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    let hooks = tokio::net::TcpListener::bind("0.0.0.0:8083").await?;
    let make = |port, webhook| {
        Front::new(
            format!("terrarium:{port}"),
            PathBuf::from("/progress/current-run.json"),
            PathBuf::from("/front/sessions.json"),
            webhook,
        )
        .router()
    };
    tokio::try_join!(
        axum::serve(web, make(8080, false)),
        axum::serve(hooks, make(8083, true))
    )?;
    Ok(())
}
