//! The refresh rule (PLAN §16.3, `docs/security-review.md` F16): the
//! daemon never refreshes an OAuth token and never calls a token endpoint.
//! Each harness refreshes its own token with its own code. This test scans
//! the Rust sources of every crate for the token endpoints and the refresh
//! request body, so that nobody adds one later. It runs in `just
//! ci::check` (`cargo test --workspace`).
//!
//! The probes call usage endpoints with an access token that a harness
//! wrote; those paths (`/api/oauth/usage`) are not token endpoints.

use std::path::{Path, PathBuf};

/// The token endpoints of the plan providers, and the body of a refresh
/// request. Lower case; the scan is case-insensitive.
const FORBIDDEN: &[&str] = &[
    "oauth/token",
    "auth.openai.com/oauth",
    "platform.claude.com/v1/oauth",
    "console.anthropic.com/v1/oauth",
    "login/oauth/access_token",
    "copilot_internal/v2/token",
    "grant_type",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name == "target" || name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Every `.rs` file under a `src/` dir of each crate.
fn sources() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf();
    let mut roots = Vec::new();
    for e in std::fs::read_dir(&crates).expect("read crates").flatten() {
        let p = e.path();
        if p.join("src").is_dir() {
            roots.push(p.join("src"));
        }
        // crates/adapters/<harness>/src
        if let Ok(inner) = std::fs::read_dir(&p) {
            for i in inner.flatten() {
                if i.path().join("src").is_dir() && i.path().join("Cargo.toml").is_file() {
                    roots.push(i.path().join("src"));
                }
            }
        }
    }
    let mut files = Vec::new();
    for r in roots {
        rust_files(&r, &mut files);
    }
    files
}

#[test]
fn no_crate_calls_a_token_endpoint() {
    let files = sources();
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("terrariumd/src/accounts.rs")),
        "the scan must cover crates/terrariumd/src ({} files)",
        files.len()
    );
    let mut hits = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f)
            .expect("read source")
            .to_lowercase();
        for (n, line) in text.lines().enumerate() {
            for pat in FORBIDDEN {
                if line.contains(pat) {
                    hits.push(format!("{}:{}: `{pat}`", f.display(), n + 1));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "the daemon must never call a token endpoint or refresh a token (PLAN §16.3, \
         \"The refresh rule\"); found:\n{}",
        hits.join("\n")
    );
}
