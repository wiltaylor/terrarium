//! The build version of the Terrarium binaries (docs/CONVENTIONS.md,
//! "Build version"): the daemon, the CLI, the runner, and the host agent
//! report `<cargo version>+<git ref>[-dirty]`, for example `0.1.0+6e31754`.
//!
//! `build.rs` makes the constants. This crate is small and only the
//! binaries depend on it, so a new commit does not build `terrarium-core`
//! or the adapters again.

mod parse;

pub use parse::{UNKNOWN, full_version, normalize_ref, rfc3339};

include!(concat!(env!("OUT_DIR"), "/build_info.rs"));

/// The crate version from `Cargo.toml` (CI stamps the release version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The parts of a full version string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts<'a> {
    /// The crate version, for example `0.1.0`.
    pub version: &'a str,
    /// The commit, or `None` when the build did not know it (`+unknown`,
    /// or an older binary that sends only `0.1.0`).
    pub git_ref: Option<&'a str>,
    /// The build tree had changes.
    pub dirty: bool,
}

/// Split a full version (`0.1.0+6e31754-dirty`) into its parts.
pub fn parts(full: &str) -> Parts<'_> {
    let full = full.trim();
    let (version, meta) = match full.split_once('+') {
        Some((v, m)) => (v, Some(m)),
        None => (full, None),
    };
    let (meta, dirty) = match meta {
        Some(m) => match m.strip_suffix("-dirty") {
            Some(b) => (Some(b), true),
            None => (Some(m), false),
        },
        None => (None, false),
    };
    Parts {
        version,
        git_ref: meta.filter(|m| !m.is_empty() && *m != UNKNOWN),
        dirty,
    }
}

/// Whether a binary of version `other` is built from another commit than
/// `reference` (the daemon). `false` when the reference ref is unknown:
/// then nothing can be compared. An `other` with no ref (an older binary)
/// is stale. The `-dirty` mark does not count.
pub fn is_stale(other: &str, reference: &str) -> bool {
    let r = parts(reference);
    let Some(want) = r.git_ref else {
        return false;
    };
    let o = parts(other);
    o.git_ref != Some(want) || o.version != r.version
}

/// The parts of this build.
pub fn this() -> Parts<'static> {
    parts(FULL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_shortens_a_full_hash_and_reads_dirty() {
        assert_eq!(
            normalize_ref("6E31754ABCDEF0123456789abcdef0123456789a\n"),
            Some(("6e31754".into(), false))
        );
        assert_eq!(
            normalize_ref("6e31754abcdef-dirty"),
            Some(("6e31754".into(), true))
        );
        assert_eq!(normalize_ref("6e31754"), Some(("6e31754".into(), false)));
        assert_eq!(normalize_ref("v0.2.0"), Some(("v0.2.0".into(), false)));
        assert_eq!(normalize_ref(""), None);
        assert_eq!(normalize_ref("  "), None);
        assert_eq!(normalize_ref("unknown"), None);
        assert_eq!(normalize_ref("-dirty"), None);
        assert_eq!(normalize_ref("a b"), None);
        assert_eq!(normalize_ref("abc$(rm)"), None);
    }

    #[test]
    fn full_version_formats() {
        assert_eq!(
            full_version("0.1.0", Some("6e31754"), false),
            "0.1.0+6e31754"
        );
        assert_eq!(
            full_version("0.1.0", Some("6e31754"), true),
            "0.1.0+6e31754-dirty"
        );
        assert_eq!(full_version("0.1.0", None, true), "0.1.0+unknown");
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_787_630), "2026-11-04T10:20:30Z");
    }

    #[test]
    fn parts_split_the_full_version() {
        assert_eq!(
            parts("0.1.0+6e31754-dirty"),
            Parts {
                version: "0.1.0",
                git_ref: Some("6e31754"),
                dirty: true
            }
        );
        assert_eq!(
            parts("0.2.0-alpha+6e31754"),
            Parts {
                version: "0.2.0-alpha",
                git_ref: Some("6e31754"),
                dirty: false
            }
        );
        assert_eq!(parts("0.1.0").git_ref, None);
        assert_eq!(parts("0.1.0+unknown").git_ref, None);
    }

    #[test]
    fn stale_compares_the_ref_and_the_version() {
        let d = "0.1.0+6e31754";
        assert!(!is_stale("0.1.0+6e31754", d));
        assert!(!is_stale("0.1.0+6e31754-dirty", d));
        assert!(is_stale("0.1.0+1234567", d));
        assert!(is_stale("0.1.0", d), "an older binary has no ref");
        assert!(is_stale("0.1.0+unknown", d));
        assert!(is_stale("0.2.0+6e31754", d));
        assert!(!is_stale("0.1.0", "0.1.0+unknown"), "no daemon ref");
        assert!(!is_stale("0.1.0+1234567", "0.1.0"));
    }

    #[test]
    fn this_build_is_consistent() {
        assert!(FULL.starts_with(VERSION));
        assert!(LONG.starts_with(FULL));
        assert_eq!(this().git_ref.unwrap_or(UNKNOWN), GIT_REF);
        assert_eq!(this().dirty, DIRTY);
        assert!(BUILD_TIME.ends_with('Z'));
    }
}
