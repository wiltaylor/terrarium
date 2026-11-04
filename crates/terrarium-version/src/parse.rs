//! The pure helpers of the build version. `build.rs` includes this file
//! (`#[path]`), so it uses only `std`.

/// The ref when nothing is known.
pub const UNKNOWN: &str = "unknown";

/// The length of a short commit hash.
pub const SHORT: usize = 7;

/// A git ref as the build gets it: `<commit>` or `<commit>-dirty`, from
/// `TERRARIUM_GIT_REF` or from git. A full hex hash becomes a short one.
/// Returns `None` for an empty value, `unknown`, or a value with
/// characters that are not `[0-9A-Za-z._-]`.
pub fn normalize_ref(raw: &str) -> Option<(String, bool)> {
    let raw = raw.trim();
    let (base, dirty) = match raw.strip_suffix("-dirty") {
        Some(b) => (b, true),
        None => (raw, false),
    };
    if base.is_empty()
        || base == UNKNOWN
        || !base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    let base = if base.len() > SHORT && base.chars().all(|c| c.is_ascii_hexdigit()) {
        base[..SHORT].to_ascii_lowercase()
    } else {
        base.to_owned()
    };
    Some((base, dirty))
}

/// The full version: `<cargo version>+<ref>`, `+<ref>-dirty`, or
/// `+unknown`.
pub fn full_version(cargo: &str, git_ref: Option<&str>, dirty: bool) -> String {
    match git_ref {
        Some(r) if dirty => format!("{cargo}+{r}-dirty"),
        Some(r) => format!("{cargo}+{r}"),
        None => format!("{cargo}+{UNKNOWN}"),
    }
}

/// Seconds since the Unix epoch as RFC 3339 UTC (`2026-11-04T10:20:30Z`).
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
