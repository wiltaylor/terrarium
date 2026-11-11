//! Build-script inputs used by reusable CI slot trees, in the shared test binary.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

fn compile(source: &Path, binary: &Path) {
    let result = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(binary)
        .arg(source)
        .env("CARGO_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
}

#[test]
fn dashboard_test_mode_has_no_checkout_directory_input() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("build");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    compile(&manifest.join("build.rs"), &binary);
    let run = |test_build: &str| {
        // Cargo gives a build script its CARGO_MANIFEST_DIR at run time
        // (TER-751); a test binary run directly (the uid 1001 pass) has none.
        let result = Command::new(&binary)
            .env("CARGO_MANIFEST_DIR", manifest)
            .env("OUT_DIR", temp.path())
            .env("TERRARIUM_TEST_BUILD", test_build)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        String::from_utf8(result.stdout).unwrap()
    };
    let first = run("1");
    assert_eq!(first, run("1"));
    assert!(first.contains("cargo::rerun-if-env-changed=TERRARIUM_TEST_BUILD"));
    assert!(!first.contains("cargo::rerun-if-changed=.."));
    assert!(temp.path().join("empty-dashboard").is_dir());
    let generated = std::fs::read_to_string(temp.path().join("dashboard.rs")).unwrap();
    assert!(generated.contains("empty-dashboard"));
    // Ordinary builds still watch and embed the real dashboard.
    assert!(run("0").contains("dashboard/dist"));
    let generated = std::fs::read_to_string(temp.path().join("dashboard.rs")).unwrap();
    assert!(generated.contains("dashboard/dist"));
}

#[test]
fn explicit_test_version_does_not_watch_git_state() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("build");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../terrarium-version");
    compile(&manifest.join("build.rs"), &binary);
    let result = Command::new(binary)
        .env("CARGO_MANIFEST_DIR", manifest)
        .env("CARGO_PKG_VERSION", "1.2.3")
        .env("OUT_DIR", temp.path())
        .env("TERRARIUM_GIT_REF", "ci-test")
        .env("SOURCE_DATE_EPOCH", "0")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(!stdout.contains(".git"), "{stdout}");
    let generated = std::fs::read_to_string(temp.path().join("build_info.rs")).unwrap();
    assert!(generated.contains("1.2.3+ci-test"));
    assert!(generated.contains("1970-01-01T00:00:00Z"));
}
