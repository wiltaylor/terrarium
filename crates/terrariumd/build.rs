//! `rust-embed` needs the dashboard folder at compile time. The dashboard is
//! built separately (`just dashboard::build`), so make an empty folder when it
//! is absent. The daemon then serves a placeholder page.

use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=TERRARIUM_TEST_BUILD");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    // CI tests the placeholder, not a packaged dashboard. Keep its empty
    // directory with Cargo's output, outside the slot's checkout cleanup.
    let test_build = std::env::var("TERRARIUM_TEST_BUILD").as_deref() == Ok("1");
    let dist = if test_build {
        out.join("empty-dashboard")
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/dist")
    };
    if !dist.exists()
        && let Err(e) = std::fs::create_dir_all(&dist)
    {
        println!("cargo::warning=cannot create {}: {e}", dist.display());
    }
    // Watch the directory, not just existing embedded files: new SQL files
    // must cause the daemon to be rebuilt too.
    println!("cargo::rerun-if-changed=migrations");
    if !test_build {
        println!("cargo::rerun-if-changed={}", dist.display());
    }
    std::fs::write(
        out.join("dashboard.rs"),
        format!(
            "#[derive(rust_embed::RustEmbed)]\n#[folder = {:?}]\nstruct Dist;\n",
            dist.to_str().expect("dashboard path is UTF-8")
        ),
    )
    .expect("write dashboard embed declaration");
}
