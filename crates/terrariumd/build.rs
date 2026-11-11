//! `rust-embed` needs the dashboard folder at compile time. The dashboard is
//! built separately (`just dashboard::build`), so make an empty folder when it
//! is absent. The daemon then serves a placeholder page.
//!
//! The named schema migrations are compiled in with `include_str!`, also in
//! debug builds: `rust-embed` reads a debug build's files from the source
//! tree at run time, so a test binary run on another machine had no
//! migrations (TER-492).

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
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
            .join("../../dashboard/dist")
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
    write_named_migrations(&out);
}

/// Write `named_migrations.rs`: each `migrations/*.sql` file, sorted by name,
/// as `(name, include_str!(path))`.
fn write_named_migrations(out: &std::path::Path) {
    // Cargo can reuse this build-script executable across checkouts sharing
    // a target directory. Resolve the active checkout when the script runs.
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("migrations");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read migrations/")
        .map(|e| e.expect("read migrations/ entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .collect();
    files.sort();
    let mut code = String::from("const NAMED_MIGRATIONS: &[(&str, &str)] = &[\n");
    for path in &files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("migration name is UTF-8");
        let path = path.to_str().expect("migration path is UTF-8");
        println!("cargo::rerun-if-changed={path}");
        code.push_str(&format!("    ({name:?}, include_str!({path:?})),\n"));
    }
    code.push_str("];\n");
    std::fs::write(out.join("named_migrations.rs"), code).expect("write named_migrations.rs");
}
