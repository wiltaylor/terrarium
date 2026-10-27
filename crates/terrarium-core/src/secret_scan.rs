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
//! | `github-token` | `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_` |
//! | `aws-access-key` | `AKIA...`, `ASIA...` |
//! | `slack-token` | `xoxa-`, `xoxb-`, `xoxp-`, `xoxo-`, `xoxr-`, `xoxs-` |
//! | `private-key` | `-----BEGIN ... PRIVATE KEY-----` |
//! | `jwt` | `eyJ...` `.` `eyJ...` `.` (a JWT, as the CLI session tokens) |
//! | `terrarium-host-token` | `thk_...` (a host agent token), `the_...` (an enrol token) |
//! | `credential-key` | a key named like `token`, `secret`, `password`, `api_key`, or `authorization`, with a value of 16 characters or more |
//! | `high-entropy` | a run of 32 or more base64 or hex characters with a Shannon entropy over 4.0 bits per character |
//! | `credential-file` | a file name that holds credentials: `.env`, `auth.json`, `.credentials.json`, `id_rsa`, `*.pem`, `*.key`, ... |
//!
//! A line with the marker `terrarium-scan: allow` skips the
//! `credential-key` and `high-entropy` rules (a false hit). The token
//! patterns and the file names have no such marker.

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
    for (i, line) in text.lines().enumerate() {
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
        if line.contains(ALLOW_MARKER) {
            continue;
        }
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
        if let Some(re) = long_runs() {
            for m in re.find_iter(line) {
                if m.as_str().len() >= ENTROPY_MIN_LEN
                    && shannon_entropy(m.as_str()) > ENTROPY_THRESHOLD
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
