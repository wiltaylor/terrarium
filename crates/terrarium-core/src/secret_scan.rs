//! The secret scan of the config repo (PLAN §8.3, `docs/security-review.md`,
//! "Config repo").
//!
//! The daemon runs it on the staged files before each commit that it
//! makes, and the `pre-commit` hook of the repo runs it for a commit by
//! hand (`terrarium admin config scan --staged`). A finding blocks the
//! commit. A finding names the file, the line, and the rule. It never
//! holds the value: the scan does not print, log, or return a secret.
//!
//! The rules:
//!
//! | Rule | Finds |
//! |---|---|
//! | `anthropic-key` | `sk-ant-...` |
//! | `openrouter-key` | `sk-or-...` |
//! | `openai-key` | `sk-proj-...`, `sk-svcacct-...` |
//! | `moonshot-key` | `sk-kimi-...` (a Kimi Code key; the format is not documented by Moonshot) |
//! | `github-token` | `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_` |
//! | `aws-access-key` | `AKIA...`, `ASIA...` |
//! | `slack-token` | `xoxa-`, `xoxb-`, `xoxp-`, `xoxo-`, `xoxr-`, `xoxs-` |
//! | `private-key` | `-----BEGIN ... PRIVATE KEY-----` |
//! | `jwt` | `eyJ...` `.` `eyJ...` `.` (a JWT, as the CLI session tokens) |
//! | `terrarium-host-token` | `thk_...` (a host agent token), `the_...` (an enrol token) |
//! | `credential-key` | a key named like `token`, `secret`, `password`, `api_key`, or `authorization`, with a value of 16 characters or more |
//! | `high-entropy` | a run of 32 or more base64 or hex characters with a Shannon entropy over 4.0 bits per character, unless the run is a path of words ([`is_word_path`]) or sits inside the quoted value of a non-secret repo field of the daemon-written export ([`exempt_export_value_span`], issue #27) |
//! | `url-userinfo` | a repo URL of the export with a credential in its user part (`https://token@x/y`, `https://user:pass@x/y`, `ssh://user:pass@x/y`), on the same exempt lines (issue #27) |
//! | `credential-file` | a file name that holds credentials: `.env`, `auth.json`, `.credentials.json`, `id_rsa`, `*.pem`, `*.key`, ... |
//!
//! A line with the marker `terrarium-scan: allow` skips the
//! `credential-key` and `high-entropy` rules (a false hit). The token
//! patterns and the file names have no such marker.
//!
//! A line with a backslash is also scanned in its decoded view
//! ([`decoded_line`], TER-459): the TOML and JSON string escapes
//! decoded, and a TOML line-ending backslash joined with the next line.
//! So an escape (`gh\u0070_...`) cannot hide a token. Every rule runs on
//! that view, and no run of it gets the export exemption below.
//!
//! The daemon writes the export of the config repo, so no marker can
//! stay on its lines (issue #27). Instead, only the runs inside the
//! quoted value of a repo field of the export fleet files that holds no
//! secret skip the `high-entropy` rule ([`EXPORT_EXEMPT_FIELDS`]):
//! the `url` of a repo, its `default_branch`, and a `credential` name,
//! under a `[[projects.repos]]` table header. Another table of the same
//! file (a schedule webhook action also writes a `url`, and its path
//! can hold the secret) and the rest of the line (a trailing comment)
//! keep the rule. A real secret in another field of the same export
//! file still hits, the token patterns still run on the line, and a
//! file that a user writes keeps the rule, with the marker as the way
//! out. A credential in the user part of such a URL hits as
//! `url-userinfo` on the same lines: the daemon drops the user part of
//! a repo URL before it writes the export, so one there is a hand edit.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The marker that skips the soft rules on its line.
pub const ALLOW_MARKER: &str = "terrarium-scan: allow";

/// One hit. It never holds the matched text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The path, relative to the repo root.
    pub path: String,
    /// The line, from 1. `0` for a rule on the file name.
    pub line: usize,
    /// The rule name (see the module table).
    pub rule: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "{}: {}", self.path, self.rule)
        } else {
            write!(f, "{}:{}: {}", self.path, self.line, self.rule)
        }
    }
}

struct Pattern {
    rule: &'static str,
    re: Regex,
}

fn patterns() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| {
        let table: &[(&str, &str)] = &[
            ("anthropic-key", r"sk-ant-[A-Za-z0-9_\-]{16,}"),
            ("openrouter-key", r"sk-or-[A-Za-z0-9_\-]{16,}"),
            // Kimi Code keys. Moonshot does not document the format; the
            // prefix is from third-party clients (hermes-agent issue
            // #5561). 20 or more characters keep a short example out.
            ("moonshot-key", r"\bsk-kimi-[A-Za-z0-9_\-]{20,}"),
            (
                "openai-key",
                r"sk-(?:proj|svcacct|admin)-[A-Za-z0-9_\-]{16,}",
            ),
            (
                "github-token",
                r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
            ),
            ("aws-access-key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            ("slack-token", r"\bxox[abposr]-[A-Za-z0-9\-]{10,}"),
            ("private-key", r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----"),
            (
                "jwt",
                r"eyJ[A-Za-z0-9_\-]{6,}\.eyJ[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]*",
            ),
            ("terrarium-host-token", r"\bth[ke]_[A-Za-z0-9_\-]{16,}"),
        ];
        table
            .iter()
            .filter_map(|(rule, re)| Regex::new(re).ok().map(|re| Pattern { rule, re }))
            .collect()
    })
}

/// `key = value`, `key: value`, `"key": "value"`, `KEY=value`, with a key
/// that names a credential.
fn key_value() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r#"(?i)["']?[A-Za-z0-9_.\-]*(?:token|secret|passw(?:or)?d|api[_\-]?key|authorization)[A-Za-z0-9_.\-]*["']?\s*[:=]\s*["']?(?:bearer\s+|basic\s+)?([^"'\s,;}\]]+)"#,
        )
        .ok()
    })
    .as_ref()
}

fn long_runs() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[A-Za-z0-9+/=_\-]{32,}").ok())
        .as_ref()
}

/// A URL with a credential in its user part: any user part of an
/// `http(s)` URL (`https://token@x/y`, `https://user:pass@x/y`), or a
/// `user:password@` of any other scheme (`ssh://git:pw@x/y`). A user
/// part without a password of another scheme (`ssh://git@x/y`) and the
/// scp form (`git@host:x/y`) are not credentials.
fn url_userinfo() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\bhttps?://[^\s/?#]*@|://[^\s/:@]+:[^\s/@]*@").ok())
        .as_ref()
}

/// A value that names a secret and is not one: an env reference, a
/// placeholder, or a path.
fn is_reference(value: &str) -> bool {
    value.starts_with('$')
        || value.starts_with('<')
        || value.starts_with('{')
        || value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("~/")
        || value.chars().all(|c| c == '*' || c == 'x' || c == 'X')
}

/// The Shannon entropy of `s` in bits per character.
pub fn shannon_entropy(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    let mut n = 0usize;
    for b in s.bytes() {
        counts[b as usize] += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let len = n as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = *c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// The entropy threshold of the `high-entropy` rule.
pub const ENTROPY_THRESHOLD: f64 = 4.0;
/// The shortest run that the `high-entropy` rule looks at.
pub const ENTROPY_MIN_LEN: usize = 32;
/// The shortest value of the `credential-key` rule.
pub const KEY_VALUE_MIN_LEN: usize = 16;

/// The longest part of a [`is_word_path`] run.
const WORD_MAX_LEN: usize = 24;

/// Is the run `run` a path or a name made of words, as the path of a URL
/// (`com/manage-profile/security/api-tokens`), and not an encoded value?
/// The run has no `+` or `=` (base64 has them, a URL path does not), and
/// it has at least two parts between `/`, `-`, and `_`. Each part is one
/// word: at most [`WORD_MAX_LEN`] characters, letters in one case form
/// (`api`, `API`, `Api`) and then at most 4 digits (`v2`), or only digits
/// (`2024`). A random base64 or hex value of 32 characters or more almost
/// never splits into such parts (mixed case and digits in a part), so the
/// `high-entropy` rule keeps it.
pub fn is_word_path(run: &str) -> bool {
    if run.contains(['+', '=']) {
        return false;
    }
    let parts: Vec<&str> = run
        .split(['/', '-', '_'])
        .filter(|p| !p.is_empty())
        .collect();
    parts.len() >= 2 && parts.iter().all(|p| is_word(p))
}

fn is_word(part: &str) -> bool {
    if part.len() > WORD_MAX_LEN {
        return false;
    }
    let letters = part.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = part.len() - letters.len();
    if letters.is_empty() {
        return digits <= 8;
    }
    if digits > 4 || !letters.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    // ASCII letters only, so `[1..]` is on a char boundary.
    letters[1..].chars().all(|c| c.is_ascii_lowercase())
        || letters.chars().all(|c| c.is_ascii_uppercase())
}

/// The dir of the daemon-written export in the config repo
/// (`terrariumd::config_repo::EXPORT_DIR`): the fleet files, the secrets
/// by name, and the board rules. This crate cannot see that constant, so
/// it holds the same string.
const EXPORT_PREFIX: &str = "export/";

/// The suffix of the fleet files of the export
/// (`export/projects/<name>.fleet.toml`, `export/global.fleet.toml`):
/// the only export files with repo tables.
const EXPORT_FLEET_SUFFIX: &str = ".fleet.toml";

/// The table header under which the fleet writer writes each repo table
/// (`[[projects.repos]]`, `crates/terrarium-core/src/fleet.rs`). Only
/// the fields of that table are exempt: another table of the same file
/// (a schedule webhook action) also writes a `url`, and its path can
/// hold the secret.
const REPOS_TABLE: &str = "projects.repos";

/// The fields of a repo table that hold no secret (issue #27): the
/// `url` of a repo, its `default_branch`, and a `credential` name (a
/// name in the secret store, never a value). Only the runs inside their
/// quoted value skip the `high-entropy` rule
/// ([`exempt_export_value_span`]).
const EXPORT_EXEMPT_FIELDS: &[&str] = &["url", "default_branch", "credential"];

/// The table header of `line` (`[[projects.repos]]`, `[defaults]`),
/// without its brackets, or `None` when the line is not one.
fn table_header(line: &str) -> Option<&str> {
    let t = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    let t = t.strip_prefix('[').unwrap_or(t);
    let t = t.strip_suffix(']').unwrap_or(t);
    Some(t.trim())
}

/// The span of the quoted value of `line`, when `line` is one of the
/// [`EXPORT_EXEMPT_FIELDS`] of a repo table of the daemon-written
/// export (issue #27): the key stands before the first `=`, and the
/// value is a quoted string. Only the runs inside that span skip the
/// `high-entropy` rule, so a trailing comment on the line keeps it. The
/// daemon writes the export, so the [`ALLOW_MARKER`] cannot stay on
/// its lines. The scan is line-based: a `[[projects.repos]]` line
/// inside a multi-line string counts as a header, and the token
/// patterns still run on every line.
fn exempt_export_value_span(
    path: &str,
    header: Option<&str>,
    line: &str,
) -> Option<std::ops::Range<usize>> {
    if !path.starts_with(EXPORT_PREFIX) || !path.ends_with(EXPORT_FLEET_SUFFIX) {
        return None;
    }
    if header != Some(REPOS_TABLE) {
        return None;
    }
    let eq = line.find('=')?;
    let key = line[..eq].trim().trim_matches(['"', '\'']);
    if !EXPORT_EXEMPT_FIELDS.contains(&key) {
        return None;
    }
    let value = line[eq + 1..].trim_start();
    let first = *value.as_bytes().first()?;
    if first != b'"' && first != b'\'' {
        return None;
    }
    let start = line.len() - value.len() + 1;
    let end = start + value[1..].find(char::from(first))?;
    Some(start..end)
}

/// The decoded view of line `i` of `lines` (TER-459), or `None` when the
/// line holds no backslash. The scan works on raw text, so an escape in a
/// TOML basic string or a JSON string (`gh\u0070_...` for `ghp_...`)
/// would hide a token from the patterns. This view decodes the escapes of
/// both formats: `\b \t \n \f \r \e \" \\ \/`, `\xHH`, `\uXXXX` (with a
/// JSON surrogate pair), and `\UXXXXXXXX`. A line that ends in an odd
/// number of backslashes before trailing whitespace (the line-ending
/// backslash of a TOML multi-line basic string) joins the next line
/// without its leading whitespace, as TOML reads it. An unknown or cut escape stays as written. The view is
/// for matching only: it does not need to be valid TOML or JSON, and it
/// never leaves the scan.
fn decoded_line(lines: &[&str], i: usize) -> Option<String> {
    let first = lines.get(i)?;
    if !first.contains('\\') {
        return None;
    }
    let mut joined = String::new();
    let mut j = i;
    let mut part: &str = first;
    loop {
        let t = part.trim_end();
        let trailing = t.len() - t.trim_end_matches('\\').len();
        if trailing % 2 == 1 && j + 1 < lines.len() {
            joined.push_str(&t[..t.len() - 1]);
            j += 1;
            part = lines[j].trim_start();
        } else {
            joined.push_str(part);
            break;
        }
    }
    Some(decode_escapes(&joined))
}

/// Decode the TOML and JSON string escapes of `s` (see [`decoded_line`]).
fn decode_escapes(s: &str) -> String {
    let hex = |h: &str| -> Option<u32> {
        if h.bytes().all(|b| b.is_ascii_hexdigit()) {
            u32::from_str_radix(h, 16).ok()
        } else {
            None
        }
    };
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let esc = &rest[at + 1..];
        let mut chars = esc.chars();
        let (decoded, used): (Option<char>, usize) = match chars.next() {
            Some('b') => (Some('\u{8}'), 1),
            Some('t') => (Some('\t'), 1),
            Some('n') => (Some('\n'), 1),
            Some('f') => (Some('\u{c}'), 1),
            Some('r') => (Some('\r'), 1),
            Some('e') => (Some('\u{1b}'), 1),
            Some(c @ ('"' | '\\' | '/')) => (Some(c), 1),
            Some(k @ ('x' | 'u' | 'U')) => {
                let n = match k {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                match esc.get(1..1 + n).and_then(hex) {
                    // A JSON surrogate pair: `😀`.
                    Some(hi @ 0xD800..=0xDBFF) if k == 'u' => {
                        let lo = esc
                            .get(5..7)
                            .filter(|p| *p == "\\u")
                            .and_then(|_| esc.get(7..11))
                            .and_then(hex)
                            .filter(|lo| (0xDC00..=0xDFFF).contains(lo));
                        match lo {
                            Some(lo) => (
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)),
                                11,
                            ),
                            None => (Some(char::REPLACEMENT_CHARACTER), 5),
                        }
                    }
                    Some(v) => (
                        Some(char::from_u32(v).unwrap_or(char::REPLACEMENT_CHARACTER)),
                        1 + n,
                    ),
                    None => (None, 0),
                }
            }
            _ => (None, 0),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &esc[used..];
            }
            None => {
                // An unknown or cut escape stays as written.
                out.push('\\');
                rest = esc;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Is `name` (the last part of a path) a file that holds credentials?
pub fn is_credential_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const EXACT: &[&str] = &[
        ".env",
        ".netrc",
        "_netrc",
        ".git-credentials",
        ".credentials.json",
        "credentials.json",
        "credentials",
        "auth.json",
        "hosts.yml",
        "session.key",
        "operator.password",
        "terrarium.db",
        ".pgpass",
        "known_hosts",
    ];
    const PREFIX: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", ".env."];
    const SUFFIX: &[&str] = &[
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".ppk",
        ".age",
        ".gpg",
        ".asc",
    ];
    EXACT.contains(&lower.as_str())
        || PREFIX.iter().any(|p| lower.starts_with(p))
        || SUFFIX.iter().any(|s| lower.ends_with(s))
}

/// Scan one file. `path` is the path in the repo (for the findings and
/// the file name rule); `text` is its content. A binary file (one with a
/// NUL byte) gets only the file name rule.
pub fn scan(path: &str, text: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    if is_credential_file(name) {
        out.push(Finding {
            path: path.to_owned(),
            line: 0,
            rule: "credential-file".into(),
        });
    }
    if text.contains(&0) {
        return out;
    }
    let text = String::from_utf8_lossy(text);
    // The table header that the last `[...]` or `[[...]]` line opened:
    // the export exemption below is scoped to the repos table.
    let mut header: Option<String> = None;
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().copied().enumerate() {
        // TER-459: the rules also run on the decoded view of a line with
        // an escape, so a TOML or JSON escape cannot hide a token.
        let decoded = decoded_line(&lines, i);
        if let Some(h) = table_header(line) {
            header = Some(h.to_owned());
        }
        let mut hit = |rule: &str| {
            if !out.iter().any(|f| f.line == i + 1 && f.rule == rule) {
                out.push(Finding {
                    path: path.to_owned(),
                    line: i + 1,
                    rule: rule.to_owned(),
                });
            }
        };
        for p in patterns() {
            if p.re.is_match(line) || decoded.as_deref().is_some_and(|d| p.re.is_match(d)) {
                hit(p.rule);
            }
        }
        // Only a marker in the raw line counts: an escape cannot write one.
        if line.contains(ALLOW_MARKER) {
            continue;
        }
        // Issue #27: only the runs inside the quoted value of a
        // non-secret repo field of the export fleet files skip the
        // high-entropy rule; the token patterns above and the
        // credential-key rule below still run on the line, and a
        // credential in the user part of the URL hits as `url-userinfo`.
        let exempt = exempt_export_value_span(path, header.as_deref(), line);
        if let Some(re) = key_value() {
            for c in re.captures_iter(line) {
                if let Some(v) = c.get(1)
                    && v.as_str().len() >= KEY_VALUE_MIN_LEN
                    && !is_reference(v.as_str())
                {
                    hit("credential-key");
                }
            }
        }
        if exempt.is_some()
            && let Some(re) = url_userinfo()
            && re.is_match(line)
        {
            hit("url-userinfo");
        }
        if let Some(re) = long_runs() {
            for m in re.find_iter(line) {
                if m.as_str().len() >= ENTROPY_MIN_LEN
                    && shannon_entropy(m.as_str()) > ENTROPY_THRESHOLD
                    && !is_word_path(m.as_str())
                    && !exempt
                        .as_ref()
                        .is_some_and(|e| m.start() >= e.start && m.end() <= e.end)
                {
                    hit("high-entropy");
                }
            }
        }
        if let Some(d) = decoded.as_deref() {
            scan_decoded_soft(path, header.as_deref(), exempt.is_some(), d, &mut hit);
        }
    }
    out
}

/// The soft rules on the decoded view `d` of a line (TER-459): the
/// `credential-key` and `high-entropy` rules, and `url-userinfo` when the
/// raw or the decoded line is an exempt repo field of the export. No run
/// of the decoded view gets the export exemption: the daemon writes no
/// escape into a repo value, so one there is a hand edit.
fn scan_decoded_soft(
    path: &str,
    header: Option<&str>,
    raw_exempt: bool,
    d: &str,
    hit: &mut dyn FnMut(&str),
) {
    if let Some(re) = key_value() {
        for c in re.captures_iter(d) {
            if let Some(v) = c.get(1)
                && v.as_str().len() >= KEY_VALUE_MIN_LEN
                && !is_reference(v.as_str())
            {
                hit("credential-key");
            }
        }
    }
    if (raw_exempt || exempt_export_value_span(path, header, d).is_some())
        && let Some(re) = url_userinfo()
        && re.is_match(d)
    {
        hit("url-userinfo");
    }
    if let Some(re) = long_runs() {
        for m in re.find_iter(d) {
            if m.as_str().len() >= ENTROPY_MIN_LEN
                && shannon_entropy(m.as_str()) > ENTROPY_THRESHOLD
                && !is_word_path(m.as_str())
            {
                hit("high-entropy");
            }
        }
    }
}

/// Scan what the next commit of the git repo at `repo` takes: each staged
/// file as the index holds it, not the work tree. The `pre-commit` hook of
/// the config repo runs this (`terrarium admin config scan --staged`). It
/// runs `git`: the one place in this crate that starts a process, so that
/// the CLI and the daemon share the hook path.
pub fn scan_staged(repo: &std::path::Path) -> Result<Vec<Finding>, String> {
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    let names = git(&[
        "diff",
        "--cached",
        "--name-only",
        "-z",
        "--diff-filter=ACMR",
    ])?;
    let mut out = Vec::new();
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let path = String::from_utf8_lossy(name).into_owned();
        let blob = git(&["show", &format!(":{path}")])?;
        out.extend(scan(&path, &blob));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn rules(path: &str, text: &str) -> Vec<String> {
        scan(path, text.as_bytes())
            .into_iter()
            .map(|f| f.rule)
            .collect()
    }

    /// Each pattern blocks an obviously fake value of its kind.
    #[test]
    fn each_pattern_finds_a_fake_secret() {
        let fake = |p: &str| format!("{p}{}", "FAKEfake0123456789".repeat(2));
        let cases = [
            ("anthropic-key", fake("sk-ant-api03-")),
            ("openrouter-key", fake("sk-or-v1-")),
            ("openai-key", fake("sk-proj-")),
            ("moonshot-key", fake("sk-kimi-")),
            ("github-token", fake("ghp_")),
            ("github-token", fake("gho_")),
            ("github-token", fake("github_pat_")),
            ("aws-access-key", "AKIAFAKEFAKEFAKEFAKE".to_owned()),
            ("slack-token", "xoxb-0000-FAKE-fake".to_owned()),
            (
                "private-key",
                "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
            ),
            ("private-key", "-----BEGIN PRIVATE KEY-----".to_owned()),
            (
                "jwt",
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJmYWtlIn0.c2ln".to_owned(),
            ),
            ("terrarium-host-token", fake("thk_")),
            ("terrarium-host-token", fake("the_")),
        ];
        for (rule, value) in cases {
            let found = rules("prompts/x.md", &format!("text {value} text\n"));
            assert!(found.contains(&rule.to_owned()), "{rule}: {found:?}");
        }
    }

    /// A Kimi Code key hits; a short example or a word that ends in
    /// `sk-kimi-` does not. The allow marker does not skip it.
    #[test]
    fn moonshot_keys_hit_and_near_misses_do_not() {
        let key = format!("sk-kimi-{}", "FAKEfake0123456789".repeat(2));
        let moonshot = "moonshot-key".to_owned();
        assert!(rules("prompts/a.md", &format!("key {key}\n")).contains(&moonshot));
        assert!(
            rules("harness/pi/models.json", &format!("\"k\": \"{key}\"\n")).contains(&moonshot)
        );
        for line in [
            "Kimi Code keys start with sk-kimi- and stay in the env.",
            "sk-kimi-mock-never-shown",
            "task-kimi-FAKEfake0123456789FAKE",
            "sk-kimi FAKEfake0123456789FAKEfake",
        ] {
            assert!(!rules("prompts/a.md", line).contains(&moonshot), "{line}");
        }
        // The marker skips the entropy rule, but not the token pattern.
        let marked = format!("key {key} # {ALLOW_MARKER}\n");
        assert_eq!(rules("prompts/a.md", &marked), [moonshot]);
    }

    #[test]
    fn credential_keys_need_a_long_value_that_is_not_a_reference() {
        for line in [
            "api_key = \"FAKEfakeFAKEfake1234\"",
            "\"apiKey\": \"FAKEfakeFAKEfake1234\"",
            "TERRARIUM_TOKEN=0123456789abcdef0123456789abcdef",
            "password: hunter2hunter2hunter2",
            "\"Authorization\": \"Bearer FAKEfakeFAKEfake1234\"",
            "client_secret='FAKEfakeFAKEfake1234'",
        ] {
            assert!(
                rules("harness/pi/settings.json", line).contains(&"credential-key".to_owned()),
                "{line}"
            );
        }
        for line in [
            "\"apiKey\": \"$ZAI_API_KEY\"",
            "\"apiKey\": \"$KIMI_API_KEY\"",
            "token = \"short\"",
            "password_file = \"/run/secrets/terrarium-password\"",
            "The bearer token for the API is in the env.",
            "max_tokens = 4096",
            "token: <your token here, never commit it>",
            "api_key = \"FAKEfakeFAKEfake1234\" # terrarium-scan: allow",
        ] {
            assert!(
                !rules("harness/pi/settings.json", line).contains(&"credential-key".to_owned()),
                "{line}"
            );
        }
    }

    #[test]
    fn high_entropy_runs_hit_and_plain_text_does_not() {
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        assert!(shannon_entropy(random) > ENTROPY_THRESHOLD);
        assert_eq!(rules("prompts/a.md", random), vec!["high-entropy"]);
        // Long, but low entropy: words, a repeated run, hex.
        for line in [
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "terrarium-project-lead-worker-template-name",
            "0123456789abcdef0123456789abcdef0123456789abcdef",
            "Run `terrarium exec <target> -- sh -c 'cd /work/p/a/repo && make'`.",
        ] {
            assert!(
                !rules("prompts/a.md", line).contains(&"high-entropy".to_owned()),
                "{line}"
            );
        }
    }

    /// A URL made of words is not a secret (issue #5); a secret in a URL
    /// still hits.
    #[test]
    fn urls_of_words_do_not_hit_and_secrets_in_urls_do() {
        let he = "high-entropy".to_owned();
        for line in [
            "Make an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>.",
            "See https://docs.example.com/en/v2/getting-started/install-on-linux-and-macos",
            "https://github.com/wiltaylor/terrarium/blob/main/docs/security-review/Config-Repo",
            "https://example.com/Users/Wil/Projects/2024/release-notes/README",
            "scripts/references/templates/component-kit/button-group",
        ] {
            assert!(!rules("skills/a/SKILL.md", line).contains(&he), "{line}");
        }
        // A random value in a URL path, a query, or a webhook URL still hits.
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        for line in [
            format!("https://hooks.example.com/services/T0001/B0001/{random}"),
            format!("https://example.com/download?sig={random}"),
            format!("https://example.com/api/{random}/items"),
            "https://example.com/x/Zx8Qp2Lm/Vt4Rn9Ks/1Wd6Hy3F/j5Bc0GaU/eKq7".to_owned(),
            "https://example.com/a-Zx8Qp2Lm7Vt4-Rn9Ks1Wd6Hy3-Fj5Bc0GaUe".to_owned(),
        ] {
            assert!(rules("skills/a/SKILL.md", &line).contains(&he), "{line}");
        }
        // A hex token in a query hits through its key name.
        let line = "https://example.com/feed?token=0123456789abcdef0123456789abcdef";
        assert!(rules("skills/a/SKILL.md", line).contains(&"credential-key".to_owned()));
        // A token pattern in a URL still hits.
        let line = format!("https://x.com/?k=ghp_{}", "FAKEfake0123456789".repeat(2));
        assert!(rules("skills/a/SKILL.md", &line).contains(&"github-token".to_owned()));
        assert!(is_word_path("manage-profile/security/api-tokens"));
        assert!(!is_word_path("Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe"));
        assert!(!is_word_path("abc/def+ghi=="));
    }

    /// The non-secret repo fields of the daemon-written export fleet
    /// files skip the `high-entropy` rule (issue #27): a repo URL with
    /// a long mixed-case path commits. The same value in a field that
    /// is not exempt, or in a file outside the export, still hits, and
    /// so does a token pattern on the exempt line.
    #[test]
    fn export_repo_fields_known_not_secret_skip_high_entropy() {
        let he = "high-entropy".to_owned();
        let export = "export/projects/p1.fleet.toml";
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        // The URL of the issue: the run under the host is 32 or more
        // mixed-case characters, not a word path.
        let url = "https://github.com/wiltaylor-daf/ExampleReportingService.git";
        let run = "com/wiltaylor-daf/ExampleReportingService";
        assert!(run.len() >= ENTROPY_MIN_LEN && shannon_entropy(run) > ENTROPY_THRESHOLD);
        let repos = format!(
            "[[projects]]\nname = \"p1\"\n\n[[projects.repos]]\nname = \"app\"\nurl = \"{url}\"\ndefault_branch = \"{random}\"\ncredential = \"{random}\"\n"
        );
        assert!(!rules(export, &repos).contains(&he), "{repos}");
        // By field, not by file: another field of the same table hits.
        for line in [
            format!("description = \"{random}\""),
            format!("host = \"{random}\""),
            format!("name = \"{random}\""),
        ] {
            let text = format!("[[projects.repos]]\n{line}\n");
            assert!(rules(export, &text).contains(&he), "{line}");
        }
        // Only in the export: a file that a user writes keeps the rule,
        // with the marker as the way out.
        for path in ["fleets/prod.fleet.toml", "prompts/a.md"] {
            let text = format!("[[projects.repos]]\nurl = \"{url}\"\n");
            assert!(rules(path, &text).contains(&he), "{path}");
            let text = format!("[[projects.repos]]\nurl = \"{url}\" # {ALLOW_MARKER}\n");
            assert!(!rules(path, &text).contains(&he), "{path}");
        }
        // The token patterns and the `credential-key` rule still run on
        // the exempt line.
        let ghp = format!("ghp_{}", "FAKEfake0123456789".repeat(2));
        let text = format!("[[projects.repos]]\nurl = \"https://x.example/{ghp}\"\n");
        assert!(rules(export, &text).contains(&"github-token".to_owned()));
        let feed = "https://example.com/feed?token=0123456789abcdef0123456789abcdef";
        let text = format!("[[projects.repos]]\nurl = \"{feed}\"\n");
        assert!(rules(export, &text).contains(&"credential-key".to_owned()));
    }

    /// Only the `[[projects.repos]]` table of an export fleet file is
    /// exempt (round 2): a schedule webhook action of the same file
    /// also writes a `url`, and its path can hold the secret. A repo
    /// line with no table above it, and an export file that is not a
    /// fleet file, keep the rule.
    #[test]
    fn only_the_repos_table_of_an_export_fleet_file_is_exempt() {
        let he = "high-entropy".to_owned();
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        // A webhook URL with a random path segment (a Slack webhook).
        let hook = format!("https://hooks.example.com/services/T0001/B0001/{random}");
        let export = "export/projects/p1.fleet.toml";
        let text = format!(
            "[[projects.schedules]]\nname = \"warn\"\n\n[projects.schedules.action]\ntype = \"webhook\"\nurl = \"{hook}\"\n"
        );
        assert!(rules(export, &text).contains(&he), "{text}");
        // The same line under a repos header is exempt, in both header
        // forms.
        for head in ["[[projects.repos]]", "[projects.repos]"] {
            let text = format!("{head}\nurl = \"{hook}\"\n");
            assert!(!rules(export, &text).contains(&he), "{text}");
        }
        // No table header above it: not a repo line.
        let text = format!("url = \"{hook}\"\n");
        assert!(rules(export, &text).contains(&he), "{text}");
        // An export file that is not a fleet file keeps the rule.
        let text = format!("[[projects.repos]]\nurl = \"{hook}\"\n");
        assert!(rules("export/secrets/p1.toml", &text).contains(&he));
        assert!(rules("export/rules/p1.rules.wcl", &text).contains(&he));
    }

    /// A credential in the user part of an exempt repo URL hits as
    /// `url-userinfo` (round 2): the daemon drops the user part of a
    /// repo URL before it writes the export, so one on an exempt line
    /// is a hand edit that `high-entropy` alone can miss. A user part
    /// without a password of another scheme and the scp form are not
    /// credentials.
    #[test]
    fn a_credential_in_the_user_part_of_an_exempt_repo_url_hits() {
        let rule = "url-userinfo".to_owned();
        let export = "export/projects/p1.fleet.toml";
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        let at = |url: &str| {
            let text = format!("[[projects.repos]]\nurl = \"{url}\"\n");
            rules(export, &text)
        };
        for url in [
            format!("https://bot:{random}@git.example/o/r.git"),
            "https://bot:password123@git.example/o/r.git".to_owned(),
            format!("https://{random}@git.example/o/r.git"),
            format!("ssh://git:{random}@git.example/o/r.git"),
        ] {
            assert!(at(&url).contains(&rule), "{url}: {:?}", at(&url));
        }
        // The token sits inside the quotes, so nothing else hits on the
        // line: this rule alone blocks it.
        let found = at(&format!("https://bot:{random}@git.example/o/r.git"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0], "url-userinfo");
        // A low-entropy user part (never a `high-entropy` hit) hits too.
        assert!(at("https://bot:password123@git.example/o/r.git").contains(&rule));
        for url in [
            "https://github.com/wiltaylor-daf/ExampleReportingService.git",
            "https://example.com:8080/o/r.git",
            "ssh://git@git.example/o/r.git",
            "git@github.com:wiltaylor-daf/terrarium.git",
        ] {
            assert!(!at(url).contains(&rule), "{url}: {:?}", at(url));
        }
    }

    /// Only the runs inside the quoted value of an exempt line skip the
    /// rule (round 2): a trailing comment on the line, and a value that
    /// is not a quoted string, keep `high-entropy`.
    #[test]
    fn only_the_quoted_value_of_an_exempt_repo_line_is_exempt() {
        let he = "high-entropy".to_owned();
        let export = "export/projects/p1.fleet.toml";
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        // A run in a trailing comment, outside the quotes, hits.
        let text = format!("[[projects.repos]]\nurl = \"https://x/o/r.git\" # {random}\n");
        assert!(rules(export, &text).contains(&he), "{text}");
        // A value that is not a quoted string is not exempt.
        let text = format!("[[projects.repos]]\nurl = {random}\n");
        assert!(rules(export, &text).contains(&he), "{text}");
        // Runs inside the quotes still skip the rule.
        let url = "https://github.com/wiltaylor-daf/ExampleReportingService.git";
        let text = format!("[[projects.repos]]\nurl = \"{url}\"\n");
        assert!(!rules(export, &text).contains(&he), "{text}");
    }

    #[test]
    fn credential_file_names_hit_by_name() {
        for name in [
            ".env",
            "x/.env.local",
            "auth.json",
            "harness/claude/.credentials.json",
            "id_ed25519",
            "id_rsa.pub",
            "tls/server.key",
            "a.pem",
        ] {
            assert_eq!(
                scan(name, b"nothing").first().map(|f| f.rule.as_str()),
                Some("credential-file"),
                "{name}"
            );
        }
        assert!(scan("harness/claude/settings.json", b"{}").is_empty());
        assert!(scan("prompts/leader.md", b"# Leader\n").is_empty());
    }

    /// A finding names the file, the line, and the rule, and never the value.
    #[test]
    fn findings_never_hold_the_value() {
        let secret = format!("sk-ant-api03-{}", "FAKEfake0123456789".repeat(2));
        let found = scan("prompts/a.md", format!("ok\nkey {secret}\n").as_bytes());
        assert_eq!(found[0].line, 2);
        let shown = format!("{found:?} {}", found[0]);
        assert!(!shown.contains(&secret[8..]), "{shown}");
        assert_eq!(found[0].to_string(), "prompts/a.md:2: anthropic-key");
    }

    /// TER-459: the TOML and JSON escapes decode, and an unknown or cut
    /// escape stays as written.
    #[test]
    fn decode_escapes_reads_toml_and_json_escapes() {
        let cases = [
            (r"gh\u0070_x", "ghp_x"),
            (r"gh\U00000070_x", "ghp_x"),
            (r"gh\x70_x", "ghp_x"),
            (r#"a\/b\\c\"d"#, "a/b\\c\"d"),
            (r"\b\t\n\f\r\e", "\u{8}\t\n\u{c}\r\u{1b}"),
            (r"\uD83D\uDE00", "\u{1F600}"),
            (r"\uD83Dx", "\u{FFFD}x"),
            (r"\q \u12 \x4 end\", r"\q \u12 \x4 end\"),
            (r"\u00e9\u0041", "\u{e9}A"),
        ];
        for (raw, want) in cases {
            assert_eq!(decode_escapes(raw), want, "{raw}");
        }
        // A real secret written in plain text still decodes to itself.
        let token = format!("ghp_{}", "FAKEfake0123456789".repeat(2));
        assert_eq!(
            decode_escapes(&format!(r"x\n{token}")),
            format!("x\n{token}")
        );
    }

    /// TER-459: a TOML or JSON escape inside a token does not hide it, in
    /// any file. Each case writes one character of a `ghp_` token (or of
    /// another token) as an escape. The allow marker does not skip it.
    #[test]
    fn escaped_tokens_hit_in_any_file() {
        let body = "FAKEfake0123456789".repeat(2);
        let gh = "github-token".to_owned();
        // A real secret in plain text still blocks.
        assert!(rules("prompts/a.md", &format!("ghp_{body}\n")).contains(&gh));
        for (path, line) in [
            ("prompts/a.md", format!(r"gh\u0070_{body}")),
            ("fleet.toml", format!(r#"note = "gh\u0070_{body}""#)),
            ("fleet.toml", format!(r#"note = "gh\U00000070_{body}""#)),
            ("fleet.toml", format!(r#"note = "ghp\u005f{body}""#)),
            (
                "fleet.toml",
                format!(r#"note = "ghp_{}\u0041{}""#, &body[..4], &body[4..]),
            ),
            (
                "harness/pi/models.json",
                format!(r#""k": "gh\u0070_{body}""#),
            ),
            (
                "harness/pi/models.json",
                format!(r#""k": "\u0067\u0068\u0070\u005f{body}""#),
            ),
            (
                "export/projects/p1.fleet.toml",
                format!("[[projects.repos]]\ncredential = \"gh\\u0070_{body}\""),
            ),
            (
                "fleet.toml",
                format!(r#"note = "gh\u0070_{body}" # {ALLOW_MARKER}"#),
            ),
        ] {
            let found = rules(path, &format!("{line}\n"));
            assert!(found.contains(&gh), "{path}: {line}: {found:?}");
        }
        // The other token patterns read the decoded view too.
        let key = format!(r#"k = "sk-ant\u002dapi03-{body}""#);
        assert!(rules("fleet.toml", &key).contains(&"anthropic-key".to_owned()));
        let pk = r#"k = "-----BEGIN\u0020PRIVATE KEY-----""#;
        assert!(rules("fleet.toml", pk).contains(&"private-key".to_owned()));
    }

    /// TER-459: a token split by the line-ending backslash of a TOML
    /// multi-line basic string hits on the line where it starts.
    #[test]
    fn a_token_split_by_a_line_ending_backslash_hits() {
        let body = "FAKEfake0123456789".repeat(2);
        let text = format!(
            "note = \"\"\"\nghp_{}\\  \n    {}\"\"\"\n",
            &body[..8],
            &body[8..]
        );
        let found = scan("fleet.toml", text.as_bytes());
        assert!(
            found
                .iter()
                .any(|f| f.rule == "github-token" && f.line == 2),
            "{found:?}"
        );
        // A real secret on one line still blocks on its line.
        let plain = scan("fleet.toml", format!("a\nghp_{body}\n").as_bytes());
        assert!(
            plain
                .iter()
                .any(|f| f.rule == "github-token" && f.line == 2)
        );
        // An even count of backslashes is an escaped backslash: no join.
        let even = format!(
            "note = \"\"\"ghp_{}\\\\\n{}\"\"\"\n",
            &body[..8],
            &body[8..]
        );
        assert!(!rules("fleet.toml", &even).contains(&"github-token".to_owned()));
    }

    /// TER-459: a credential in the user part of an exempt repo URL,
    /// written with escapes, still hits, and an escape gives a token in
    /// a repo value no exemption.
    #[test]
    fn an_escaped_url_credential_in_an_export_repo_hits() {
        let export = "export/projects/p1.fleet.toml";
        let userinfo = "url-userinfo".to_owned();
        let at = |line: &str| rules(export, &format!("[[projects.repos]]\n{line}\n"));
        // A real credential in plain text still blocks.
        assert!(at(r#"url = "https://bot:password123@git.example/o/r.git""#).contains(&userinfo));
        for line in [
            r#"url = "https://bot:password123\u0040git.example/o/r.git""#,
            r#"url = "https:\/\/bot:password123@git.example/o/r.git""#,
            r#"url = "https\u003a//bot:password123@git.example/o/r.git""#,
            r#""u\u0072l" = "https://bot:password123@git.example/o/r.git""#,
            r#"url = "https://bot\u003apassword123@git.example/o/r.git""#,
        ] {
            assert!(at(line).contains(&userinfo), "{line}: {:?}", at(line));
        }
        // A high-entropy run with an escape in a repo URL is not exempt.
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        let line = format!(
            r#"url = "https://git.example/o/{}\u0041{}""#,
            &random[..10],
            &random[10..]
        );
        assert!(
            at(&line).contains(&"high-entropy".to_owned()),
            "{:?}",
            at(&line)
        );
    }

    /// TER-459: a backslash in plain text (a Windows path, a Markdown
    /// escape, a short escaped string) takes no hit, and a real secret
    /// on such a line still does.
    #[test]
    fn plain_backslashes_take_no_hit() {
        for line in [
            r"Copy it to C:\Users\name\AppData\Local\Terrarium\cache.",
            r"Use \_ and \* in Markdown, and a\nb in a string.",
            r#"prompt = "Line one.\nLine two.\n\tIndented \"quote\".""#,
            r#""path": "C:\\Program Files\\Terrarium\\bin""#,
        ] {
            assert!(
                rules("prompts/a.md", line).is_empty(),
                "{line}: {:?}",
                rules("prompts/a.md", line)
            );
        }
        let token = format!("ghp_{}", "FAKEfake0123456789".repeat(2));
        let line = format!(r"C:\Users\name {token}");
        assert!(rules("prompts/a.md", &line).contains(&"github-token".to_owned()));
    }
}
