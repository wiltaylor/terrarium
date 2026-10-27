//! Drive a real VNC or RDP server with the host agent's display drivers,
//! for the live checks in docs/compat.md ("Target displays: VNC and RDP").
//!
//! ```sh
//! DISPLAY_LIVE_PASSWORD=... [DISPLAY_LIVE_USER=lab] [DISPLAY_LIVE_DOMAIN=LAB] \
//!   cargo run -p terrarium-host --example display_live -- \
//!   vnc 127.0.0.1:5900 /tmp/out '{"op":"info"}' '{"op":"screenshot"}' \
//!   '{"op":"click","x":300,"y":200}' '{"op":"type","text":"echo hi\n"}' sleep:500 '{"op":"ocr"}'
//! ```
//!
//! Each operation prints its `result` value; each screenshot goes to
//! `<out>/shot-<n>.png`. `sleep:<ms>` waits. All operations share one
//! connection, as in the host agent. The password comes from the
//! environment, never from the command line. RDP certificate pins go to
//! `<out>/rdp-known-hosts`.

use std::sync::Arc;
use std::time::Duration;

use terrarium_core::display::DisplayOp;
use terrarium_host::config::{DisplayConfig, PathPolicy};
use terrarium_host::display::{DisplayCtx, dispatch, on_path, remote::Remotes, runner};
use terrarium_host::protocol::DisplayTarget;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [kind, address, out, ops @ ..] = args.as_slice() else {
        anyhow::bail!("usage: display_live <vnc|rdp> <address> <out-dir> <op-json|sleep:ms>...");
    };
    let out = std::path::PathBuf::from(out);
    std::fs::create_dir_all(&out)?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let password = env("DISPLAY_LIVE_PASSWORD");
    let target = match kind.as_str() {
        "vnc" => DisplayTarget::Vnc {
            address: address.clone(),
            username: None,
            password,
        },
        "rdp" => DisplayTarget::Rdp {
            address: address.clone(),
            username: env("DISPLAY_LIVE_USER"),
            domain: env("DISPLAY_LIVE_DOMAIN"),
            password,
        },
        k => anyhow::bail!("unknown kind {k}: vnc or rdp"),
    };
    let config = DisplayConfig::default();
    let ctx = DisplayCtx {
        runner: Arc::new(runner::SystemRunner),
        docker_bin: "docker".into(),
        docker_ok: false,
        vmlab_bin: "vmlab".into(),
        vmlab_ok: false,
        policy: PathPolicy::default(),
        tesseract: on_path(&config.tesseract_bin),
        config,
        timeout: Duration::from_secs(30),
        remotes: Remotes::new(out.join("rdp-known-hosts")),
    };
    let mut shots = 0;
    for op in ops {
        if let Some(ms) = op.strip_prefix("sleep:") {
            tokio::time::sleep(Duration::from_millis(ms.parse()?)).await;
            continue;
        }
        let op: DisplayOp = serde_json::from_str(op)?;
        let start = std::time::Instant::now();
        match dispatch(&ctx, &target, &op).await {
            Ok(o) => {
                if let Some(png) = o.png {
                    shots += 1;
                    let p = out.join(format!("shot-{shots}.png"));
                    std::fs::write(&p, png)?;
                    println!(
                        "{} ({} ms) -> {}",
                        o.value,
                        start.elapsed().as_millis(),
                        p.display()
                    );
                } else {
                    println!("{} ({} ms)", o.value, start.elapsed().as_millis());
                }
            }
            Err(e) => println!(
                "error {:?}: {e} ({} ms)",
                e.code(),
                start.elapsed().as_millis()
            ),
        }
    }
    Ok(())
}
