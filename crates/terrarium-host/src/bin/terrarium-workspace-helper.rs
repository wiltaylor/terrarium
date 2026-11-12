//! Separately installed root-owned helper. Never the self-updating agent path.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .init();
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args == ["--capabilities"] {
        println!(
            "{}",
            serde_json::to_string(&terrarium_host::workspace_helper::service::capability())?
        );
        return Ok(());
    }
    if !args.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only --capabilities is supported",
        ));
    }
    terrarium_host::workspace_helper::service::run().await
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn main() {
    eprintln!("workspace helper requires supported Linux GNU mount APIs");
    std::process::exit(1);
}
