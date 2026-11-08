//! Stamp the build version: `<cargo version>+<git ref>[-dirty]` and the
//! build time (docs/CONVENTIONS.md, "Build version").
//!
//! The ref comes from `TERRARIUM_GIT_REF` when it is set (the Dockerfile
//! and CI set it: a `git archive` build has no `.git`), else from git. When
//! neither gives a ref, the ref is `unknown`. The build time is
//! `SOURCE_DATE_EPOCH` when it is set, else the time this script runs.
//!
//! The script runs again only when the env changes, or when git moves HEAD,
//! the branch, or the index. So `-dirty` shows the state of the tree at the
//! last of those, not at each edit. Only the crates that depend on this
//! one (the binaries) build again; `terrarium-core` and the adapters do not.

#[path = "src/parse.rs"]
#[allow(dead_code)]
mod parse;

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // Never write the index (a write would start this script again).
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The ref from git, and the git files whose change starts this script again.
fn from_git(dir: &Path) -> Option<(String, bool)> {
    let git_dir = PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"])?);
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from)
    .unwrap_or_else(|| git_dir.clone());
    let mut watch = vec![
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common.join("packed-refs"),
    ];
    if let Some(branch) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        watch.push(common.join(branch));
    }
    for f in watch {
        if f.exists() {
            println!("cargo::rerun-if-changed={}", f.display());
        }
    }
    let commit = git(
        dir,
        &["rev-parse", &format!("--short={}", parse::SHORT), "HEAD"],
    )?;
    let dirty =
        git(dir, &["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    parse::normalize_ref(&commit).map(|(r, _)| (r, dirty))
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/parse.rs");
    println!("cargo::rerun-if-env-changed=TERRARIUM_GIT_REF");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let cargo = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let found = match std::env::var("TERRARIUM_GIT_REF") {
        Ok(v) if !v.trim().is_empty() => parse::normalize_ref(&v),
        _ => from_git(&manifest),
    };
    let (git_ref, dirty) = match &found {
        Some((r, d)) => (Some(r.as_str()), *d),
        None => (None, false),
    };
    let full = parse::full_version(&cargo, git_ref, dirty);
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    let built = parse::rfc3339(secs);
    let code = format!(
        "/// The full build version, for example `0.1.0+6e31754`.\n\
         pub const FULL: &str = {full:?};\n\
         /// The short commit of the build, or `unknown`.\n\
         pub const GIT_REF: &str = {r:?};\n\
         /// The tree had changes to tracked files.\n\
         pub const DIRTY: bool = {dirty};\n\
         /// The build time, RFC 3339 UTC.\n\
         pub const BUILD_TIME: &str = {built:?};\n\
         /// `--version` text: the full version, then `built <time>` on its own line.\n\
         pub const LONG: &str = {long:?};\n",
        r = git_ref.unwrap_or(parse::UNKNOWN),
        // The first line must end with the version: host agents from releases before TER-195
        // take the last word of the first line of `terrarium-host --version`
        // as the version of a staged update (TER-195).
        long = format!("{full}\nbuilt {built}"),
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("build_info.rs");
    if let Err(e) = std::fs::write(&out, code) {
        panic!("cannot write {}: {e}", out.display());
    }
}
