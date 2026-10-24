//! `rust-embed` needs the dashboard folder at compile time. The dashboard is
//! built separately (`just dashboard::build`), so make an empty folder when it
//! is absent. The daemon then serves a placeholder page.

use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/dist");
    if !dist.exists()
        && let Err(e) = std::fs::create_dir_all(&dist)
    {
        println!("cargo::warning=cannot create {}: {e}", dist.display());
    }
    println!("cargo::rerun-if-changed=../../dashboard/dist");
}
