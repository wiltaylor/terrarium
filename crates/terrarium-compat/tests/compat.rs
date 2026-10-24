//! The compatibility suite. See `docs/compat-suite.md`.
//!
//! - `dry_run` runs in every `cargo test`: it builds each launch spec,
//!   materializes into a temp home, and checks the versions. It starts no
//!   harness and spends nothing.
//! - `real_harnesses` is ignored: it spends real subscription turns (at
//!   most three per harness). Run it with `just ci::compat [harness]`.
#![allow(clippy::print_stdout)]

use std::path::Path;

use terrarium_compat::{ALL, Settings, run_suite, target_dir_of};

fn settings() -> Settings {
    let target = target_dir_of(Path::new(env!("CARGO_TARGET_TMPDIR")));
    Settings::from_env(target, &|k| std::env::var(k).ok())
}

async fn run(settings: Settings) {
    let report = run_suite(&settings).await;
    let md = report.to_markdown();
    println!("\n{md}");
    let path = settings.report_path();
    match report.write(&path) {
        Ok(()) => println!("report: {}", path.display()),
        Err(e) => println!("cannot write {}: {e}", path.display()),
    }
    assert!(
        !report.failed(),
        "a compatibility step failed; see {}",
        path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dry_run() {
    let mut s = settings();
    s.dry_run = true;
    s.harnesses = ALL.to_vec();
    run(s).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts the real harnesses on the real homes and spends up to 3 model turns each"]
async fn real_harnesses() {
    run(settings()).await;
}
