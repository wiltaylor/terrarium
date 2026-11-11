//! The refresh rule (PLAN §16.3, `docs/security-review.md` F16): the
//! daemon never refreshes a harness OAuth token or calls its token endpoint.
//! TER-632 permits only Vertex's fixed-endpoint service-account JWT grant.
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
    "oauth2.googleapis.com/token",
    "urn:ietf:params:oauth:grant-type:jwt-bearer",
];

fn violations(file: &Path, text: &str) -> Vec<String> {
    let vertex = file.ends_with("crates/terrariumd/src/api/subscriptions/vertex.rs");
    let mut hits = Vec::new();
    for (n, line) in text.to_lowercase().lines().enumerate() {
        // Exact lines, not a whole-file exemption. No refresh or other grant,
        // endpoint, or module is permitted by this service-account exception.
        let allowed = vertex
            && matches!(
                line.trim(),
                "const token_url: &str = \"https://oauth2.googleapis.com/token\";"
                    | "(\"grant_type\", \"urn:ietf:params:oauth:grant-type:jwt-bearer\"),"
            );
        for pat in FORBIDDEN {
            if line.contains(pat) && !allowed {
                hits.push(format!("{}:{}: `{pat}`", file.display(), n + 1));
            }
        }
    }
    hits
}

#[test]
fn vertex_exception_is_exact_module_endpoint_and_grant_only() {
    let vertex = Path::new("crates/terrariumd/src/api/subscriptions/vertex.rs");
    let other = Path::new("crates/terrariumd/src/accounts.rs");
    let grant = r#"("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),"#;
    let endpoint = r#"const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";"#;
    assert!(violations(vertex, grant).is_empty());
    assert!(violations(vertex, endpoint).is_empty());
    assert!(!violations(other, grant).is_empty());
    assert!(!violations(other, endpoint).is_empty());
    for bad in [
        r#"("grant_type", "refresh_token"),"#,
        r#"("grant_type", "client_credentials"),"#,
        r#"("grant_type", "authorization_code"),"#,
        r#"const TOKEN_URL: &str = "https://other.invalid/oauth/token";"#,
        r#"let token_url = "https://oauth2.googleapis.com/token";"#,
    ] {
        assert!(!violations(vertex, bad).is_empty(), "{bad}");
    }
}

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
        let text = std::fs::read_to_string(f).expect("read source");
        hits.extend(violations(f, &text));
    }
    assert!(
        hits.is_empty(),
        "the daemon must never call a harness token endpoint or refresh a harness token \
         (only the exact Vertex service-account JWT exchange is exempt, TER-632) (PLAN §16.3, \
         \"The refresh rule\"); found:\n{}",
        hits.join("\n")
    );
}
