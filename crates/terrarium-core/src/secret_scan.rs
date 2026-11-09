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
//! | `high-entropy` | a run of 32 or more base64 or hex characters with a Shannon entropy over 4.0 bits per character, unless the run is a path of words ([`is_word_path`]) or sits inside the quoted value of a non-secret repo field of the daemon-written export ([`exempt_export_value_span`], issue #27); a run that overlaps the user part of a URL keeps the rule (`ssh://<token>@host/r.git`, the scp form `<token>@host:r.git`, [`url_user_parts`]) |
//! | `url-userinfo` | a repo URL of the export with a credential in its user part (`https://token@x/y`, `https://user:pass@x/y`, `ssh://user:pass@x/y`), on the same exempt lines (issue #27) |
//! | `credential-file` | a file name that holds credentials: `.env`, `auth.json`, `.credentials.json`, `id_rsa`, `*.pem`, `*.key`, ... |
//!
//! A line with the marker `terrarium-scan: allow` skips the
//! `credential-key` and `high-entropy` rules (a false hit), but not a
//! run in the user part of a URL of an exempt line: like
//! `url-userinfo` it is not a rule for a false hit there. The token
//! patterns, `url-userinfo`, and the file names have no such marker.
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
//! a repo URL before it writes the export, so one there is a hand
//! edit. A user part with no password (`ssh://<token>@host/r.git`, the
//! scp form `<token>@host:r.git`) is not one, so a run that overlaps
//! the user part keeps `high-entropy` ([`url_user_parts`]), whatever
//! byte sits between the run and the `@`. A table header inside a
//! multi-line string of the file is text of that string and opens no
//! exemption ([`multiline_open_after`]): a `"""` or `'''` in a
//! comment, in a one-line string, or escaped in a basic string opens
//! nothing. The export writer does write multi-line strings (a
//! prompt with newlines), but it escapes the quotes inside them, so
//! the tracked state of its output is exact and no daemon-written
//! commit takes a false hit on a repo URL after one
//! (`the_export_writer_output_keeps_the_exemption` proves it).

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

/// The spans of the user parts of the URLs of `line`: after the `//`
/// of a `scheme://` URL up to the first `@` of its authority
/// (`ssh://<token>@host/r.git`, `https://user:<token>@host`), and from
/// the start of an scp value up to its `@` (`<token>@host:r.git`).
/// Those forms carry the credential in the user part, where
/// [`url_userinfo`] sees only a `user:password@`, so a run that
/// overlaps one keeps the `high-entropy` rule even on an exempt line,
/// whatever byte sits between the run and the `@` (`<token>.@`,
/// `<token>~x@`, `<token>%2B@`). The user part of a repo URL that
/// the daemon writes is a word (`git`, `oauth2`), never a
/// high-entropy run.
fn url_user_parts(line: &str) -> Vec<std::ops::Range<usize>> {
    let b = line.as_bytes();
    let mut spans = Vec::new();
    // `scheme://user@host`: the user part runs from the `//` of the
    // authority to its first `@`. The authority ends at the first
    // path, query, fragment, quote, or space.
    let mut from = 0;
    while let Some(p) = line[from..].find("://") {
        let p = from + p;
        let start = p + 3;
        let end = start
            + b[start..]
                .iter()
                .position(|&c| matches!(c, b'/' | b'?' | b'#' | b'"' | b'\'' | b' ' | b'\t'))
                .unwrap_or(b.len() - start);
        if let Some(at) = b[start..end].iter().position(|&c| c == b'@') {
            spans.push(start..start + at);
        }
        from = start;
    }
    // The scp form `user@host:path`: the host holds a `:` (the path
    // separator) before a `/`, a quote, or a space, and the user part
    // runs back from the `@` to the first byte that is not part of it.
    for (at, c) in b.iter().enumerate() {
        if *c != b'@'
            || b[at + 1..]
                .iter()
                .find(|&&c| matches!(c, b'/' | b':' | b'"' | b'\'' | b' ' | b'\t' | b'?' | b'#'))
                != Some(&b':')
        {
            continue;
        }
        let start = b[..at]
            .iter()
            .rposition(|&c| {
                !matches!(
                    c,
                    b'A'..=b'Z'
                        | b'a'..=b'z'
                        | b'0'..=b'9'
                        | b'+'
                        | b'/'
                        | b'='
                        | b'.'
                        | b'_'
                        | b'~'
                        | b'-'
                        | b'%'
                )
            })
            .map_or(0, |p| p + 1);
        spans.push(start..at);
    }
    spans
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

/// The multi-line TOML string (`"""` or `'''`) that is open at the end
/// of `line`, from the one open before it: a character scan of the
/// line, not a count of its delimiters. A `"""` or `'''` inside a
/// comment, inside a one-line `'...'` or `"..."` string, or escaped
/// in a basic string (`\"""`) opens and closes nothing, and a run of
/// four or more quotes closes at its last three (the TOML spec holds
/// up to two of them as content). A `[[projects.repos]]` line inside
/// such a string is text of the string, not a table header, so
/// [`scan`] takes no header from it: a hand edit with example fleet
/// TOML in a multi-line string must not exempt the lines in or after
/// it. The export writer does write multi-line strings (a prompt
/// with newlines), but it escapes the quotes inside them, so the
/// tracked state of its output is exact
/// (`the_export_writer_output_keeps_the_exemption`).
fn multiline_open_after(line: &str, open: Option<char>) -> Option<char> {
    let b = line.as_bytes();
    let mut open = open;
    let mut i = 0;
    while i < b.len() {
        match open {
            // A basic multi-line string: an escape holds its char, and
            // a run of 3 or more quotes closes it at its last three.
            Some('"') if b[i] == b'\\' => i += 2,
            Some('"') if b[i] == b'"' => {
                let run = b[i..].iter().take_while(|&&c| c == b'"').count();
                if run >= 3 {
                    open = None;
                }
                i += run;
            }
            // A literal multi-line string has no escapes; a run of 3
            // or more quotes closes it at its last three.
            Some('\'') if b[i] == b'\'' => {
                let run = b[i..].iter().take_while(|&&c| c == b'\'').count();
                if run >= 3 {
                    open = None;
                }
                i += run;
            }
            // Any other byte of an open multi-line string is content.
            Some(_) => i += 1,
            None => match b[i] {
                // A comment holds no string delimiter.
                b'#' => break,
                b'"' if b[i..].starts_with(b"\"\"\"") => {
                    open = Some('"');
                    i += 3;
                }
                b'\'' if b[i..].starts_with(b"'''") => {
                    open = Some('\'');
                    i += 3;
                }
                // A one-line basic string: an escape holds its char, a
                // quote closes it, and one that never closes holds the
                // rest of the line.
                b'"' => {
                    i += 1;
                    while i < b.len() {
                        if b[i] == b'\\' {
                            i += 2;
                        } else {
                            i += 1;
                            if b[i - 1] == b'"' {
                                break;
                            }
                        }
                    }
                }
                // A one-line literal string: to its closing quote, or
                // to the end of the line when there is none.
                b'\'' => {
                    i += 1;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                }
                _ => i += 1,
            },
        }
    }
    open
}

/// The span of the quoted value of `line`, when `line` is one of the
/// [`EXPORT_EXEMPT_FIELDS`] of a repo table of the daemon-written
/// export (issue #27): the key stands before the first `=`, and the
/// value is a quoted string. Only the runs inside that span skip the
/// `high-entropy` rule, so a trailing comment on the line keeps it. The
/// daemon writes the export, so the [`ALLOW_MARKER`] cannot stay on
/// its lines. The scan is line-based: a `[[projects.repos]]` line
/// inside a multi-line string is text of that string, not a header
/// ([`multiline_open_after`]), and the token patterns still run on
/// every line.
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
    // The table header that the last `[...]` or `[[...]]` line outside a
    // multi-line string opened: the export exemption below is scoped to
    // the repos table. A header line inside a `"""` or `'''` string is
    // text of that string, so while one is open the header is `None`, and
    // on its close the table before the string is back (round 3).
    let mut header: Option<String> = None;
    let mut open: Option<char> = None;
    let mut before_string: Option<String> = None;
    for (i, line) in text.lines().enumerate() {
        let next = multiline_open_after(line, open);
        if open.is_none() && next.is_some() {
            // The line opens one: the table above it is back after the
            // close, and the lines of the string get no header.
            before_string = header.take();
        } else if open.is_some() && next.is_none() {
            header = before_string.take();
        } else if open.is_none()
            && next.is_none()
            && let Some(h) = table_header(line)
        {
            header = Some(h.to_owned());
        }
        open = next;
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
            if p.re.is_match(line) {
                hit(p.rule);
            }
        }
        // Issue #27: only the runs inside the quoted value of a
        // non-secret repo field of the export fleet files skip the
        // high-entropy rule; the token patterns above, the
        // credential-key rule below, and `url-userinfo` still run on
        // the line. `url-userinfo` and the user-part runs of
        // `high-entropy` run before the marker check: like the token
        // patterns they are not rules for a false hit on an exempt
        // line, and the daemon writes the export, so a credential in
        // the user part of a repo URL, and a marker on that line, are
        // hand edits.
        let exempt = exempt_export_value_span(path, header.as_deref(), line);
        let user_parts = url_user_parts(line);
        if exempt.is_some()
            && let Some(re) = url_userinfo()
            && re.is_match(line)
        {
            hit("url-userinfo");
        }
        let marked = line.contains(ALLOW_MARKER);
        if let Some(re) = key_value() {
            for c in re.captures_iter(line) {
                if let Some(v) = c.get(1)
                    && v.as_str().len() >= KEY_VALUE_MIN_LEN
                    && !is_reference(v.as_str())
                    && !marked
                {
                    hit("credential-key");
                }
            }
        }
        if let Some(re) = long_runs() {
            for m in re.find_iter(line) {
                if m.as_str().len() >= ENTROPY_MIN_LEN
                    && shannon_entropy(m.as_str()) > ENTROPY_THRESHOLD
                    && !is_word_path(m.as_str())
                    && (exempt.is_some()
                        && user_parts
                            .iter()
                            .any(|u| m.start() < u.end && u.start < m.end())
                        || !marked
                            && !exempt
                                .as_ref()
                                .is_some_and(|e| m.start() >= e.start && m.end() <= e.end))
                {
                    hit("high-entropy");
                }
            }
        }
    }
    out
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
        // The token sits inside the quotes: `url-userinfo` and the
        // user-part run of `high-entropy` (round 3) block it.
        let found = at(&format!("https://bot:{random}@git.example/o/r.git"));
        assert_eq!(found, ["url-userinfo", "high-entropy"].map(str::to_owned));
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

    /// A table header inside a multi-line string is text of that string,
    /// not a header (round 3): a multi-line prompt that holds example
    /// fleet TOML must not exempt the `url` line inside it, and the
    /// header that only the string names must not exempt the lines
    /// after the string closes.
    #[test]
    fn a_table_header_inside_a_multiline_string_does_not_exempt() {
        let he = "high-entropy".to_owned();
        let export = "export/projects/p1.fleet.toml";
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        // A multi-line prompt with example fleet TOML: the `url` line
        // inside the string keeps the rule, in both quote forms.
        for q in ["\"\"\"", "'''"] {
            let prompt = format!(
                "[[projects]]\nname = \"p1\"\nsystem_prompt = {q}\nExample fleet TOML:\n\n[[projects.repos]]\nurl = \"https://x/{random}\"\n{q}\n"
            );
            assert!(rules(export, &prompt).contains(&he), "{prompt}");
        }
        // The header that only the string names exempts nothing after
        // the close: the line is under `projects`.
        let spoof = format!(
            "[[projects]]\nname = \"p1\"\ndescription = \"\"\"\n[[projects.repos]]\n\"\"\"\nurl = \"https://x/{random}\"\n"
        );
        assert!(rules(export, &spoof).contains(&he), "{spoof}");
        // A real repos table after a multi-line string still exempts
        // its own lines.
        let url = "https://github.com/wiltaylor-daf/ExampleReportingService.git";
        let real = format!(
            "[[projects]]\nname = \"p1\"\ndescription = '''\nsee [[projects.repos]] below\n'''\n\n[[projects.repos]]\nname = \"app\"\nurl = \"{url}\"\n"
        );
        assert!(!rules(export, &real).contains(&he), "{real}");
        // Round 4, the reviewer's spoofs: a `\"\"\"` or `'''` that is
        // escaped in a basic string, inside a one-line string, or
        // inside a comment opens and closes nothing, so the header
        // line inside the string exempts nothing (round 3 counted
        // every three-quote run on the line and took each of these for
        // the delimiter).
        for text in [
            // `\"""` on its own line inside the open basic string: the
            // escape holds its quote, so the string stays open.
            format!(
                "d = \"\"\"\n\\\"\"\"\n[[projects.repos]]\nurl = \"https://x/{random}\"\n\"\"\"\n"
            ),
            // A one-line literal string before it holds three quotes.
            format!(
                "name = 'p\"\"\"'\nd = \"\"\"\n[[projects.repos]]\nurl = \"https://x/{random}\"\n\"\"\"\n"
            ),
            // A one-line basic string with three escaped quotes.
            format!(
                "name = \"p\\\"\\\"\\\"\"\nd = \"\"\"\n[[projects.repos]]\nurl = \"https://x/{random}\"\n\"\"\"\n"
            ),
            // A comment holds three quotes, of both kinds.
            format!(
                "name = \"p\" # \"\"\"\nd = \"\"\"\n[[projects.repos]]\nurl = \"https://x/{random}\"\n\"\"\"\n"
            ),
            format!(
                "name = \"p\" # '''\nd = '''\n[[projects.repos]]\nurl = \"https://x/{random}\"\n'''\n"\n            ),
        ] {
            let text = format!("[[projects]]\n{text}");
            assert!(rules(export, &text).contains(&he), "{text}");
        }
    }

    /// A high-entropy user part of a repo URL keeps the rule on an
    /// exempt line (round 3): `ssh://<token>@host/r.git` and the scp
    /// form `<token>@host:r.git` have no password, so `url-userinfo`
    /// alone misses them.
    #[test]
    fn a_high_entropy_user_of_an_ssh_or_scp_url_keeps_the_rule() {
        let he = "high-entropy".to_owned();
        let export = "export/projects/p1.fleet.toml";
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        let at = |url: &str| {
            let text = format!("[[projects.repos]]\nurl = \"{url}\"\n");
            rules(export, &text)
        };
        for url in [
            format!("ssh://{random}@git.example/o/r.git"),
            format!("{random}@git.example:o/r.git"),
            format!("ssh://git:{random}@git.example/o/r.git"),
            format!("https://user:{random}@git.example/o/r.git"),
        ] {
            assert!(at(&url).contains(&he), "{url}: {:?}", at(&url));
        }
        // Round 4: whatever byte sits between the token and the `@`,
        // the whole user part keeps the rule, not only a run that ends
        // at the `@`.
        for url in [
            format!("ssh://{random}.@git.example/o/r.git"),
            format!("ssh://{random}%2B@git.example/o/r.git"),
            format!("{random}~x@git.example:o/r.git"),
        ] {
            assert!(at(&url).contains(&he), "{url}: {:?}", at(&url));
        }
        // A word user part stays a plain URL.
        for url in [
            "ssh://git@git.example/o/r.git",
            "git@github.com:wiltaylor-daf/terrarium.git",
            "https://github.com/wiltaylor-daf/ExampleReportingService.git",
        ] {
            assert!(!at(url).contains(&he), "{url}: {:?}", at(url));
        }
    }

    /// The allow marker does not skip `url-userinfo` (round 3) nor a
    /// high-entropy run in the user part of a URL (round 4): like the
    /// token patterns they are not rules for a false hit on an exempt
    /// line, and the daemon writes the export lines, so a marker on
    /// one is a hand edit too. In a file that a user writes the marker
    /// keeps its meaning for the soft rules.
    #[test]
    fn the_allow_marker_does_not_skip_url_userinfo() {
        let he = "high-entropy".to_owned();
        let export = "export/projects/p1.fleet.toml";
        let text = format!(
            "[[projects.repos]]\nurl = \"https://bot:password123@git.example/o/r.git\" # {ALLOW_MARKER}\n"
        );
        assert_eq!(rules(export, &text), ["url-userinfo"].map(str::to_owned));
        // The scp form carries the credential with no password, so the
        // run itself is the rule that blocks it, marker or not.
        let random = "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe";
        let text = format!(
            "[[projects.repos]]\nurl = \"ssh://{random}@git.example/o/r.git\" # {ALLOW_MARKER}\n"
        );
        assert_eq!(rules(export, &text), [he]);
        // In a file that a user writes the marker keeps its meaning for
        // the soft rules, and `url-userinfo` never runs there.
        let text = format!("url = \"https://bot:{random}@x/y\" # {ALLOW_MARKER}\n");
        assert!(rules("fleets/prod.fleet.toml", &text).is_empty());
        let text = format!("url = \"ssh://{random}@x/y\" # {ALLOW_MARKER}\n");
        assert!(rules("fleets/prod.fleet.toml", &text).is_empty());
    }

    /// The scan keeps the repo-table exemption on the export writer's
    /// own output (round 4): the writer writes a value with newlines
    /// (a prompt) as a multi-line string, escaping its quotes, so the
    /// string-state scan tracks it exactly: a `[[projects.repos]]`
    /// line inside the prompt exempts nothing, and the repo URL after
    /// the prompt keeps the exemption of issue #27. A commit that the
    /// daemon writes takes no false `high-entropy` hit on it.
    #[test]
    fn the_export_writer_output_keeps_the_exemption() {
        let text = r#"version = 1
name = "p1"

[defaults]
system_prompt = """
Example fleet TOML:

[[projects.repos]]
url = "https://x/example-service"
Escaped quotes: "\"\"\" and literal ''' stay in the prompt.
"""

[[projects]]
name = "p1"

[[projects.repos]]
name = "app"
url = "https://github.com/wiltaylor-daf/ExampleReportingService.git"
"#;
        let parsed = crate::fleet::parse(text, None);
        assert!(
            parsed
                .issues
                .iter()
                .all(|i| i.level != crate::fleet::Level::Error),
            "{:?}",
            parsed.issues
        );
        let fleet = parsed.fleet.as_ref().expect("a fleet");
        let out = fleet.to_toml();
        // The prompt is written as a multi-line string: its own `"""`
        // becomes `""\"`, which no run of three quotes closes early.
        assert!(out.contains("system_prompt = \"\"\"\n"), "{out}");
        assert!(out.contains("\"\"\\\""), "{out}");
        // The repo URL after it stays exempt: no false hit on a commit
        // that the daemon writes.
        let findings = scan("export/projects/p1.fleet.toml", out.as_bytes());
        assert!(findings.is_empty(), "{out}\n{findings:?}");
        assert_eq!(crate::fleet::parse(&out, None).fleet, parsed.fleet);
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
}
