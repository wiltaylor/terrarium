//! The redaction matcher (PLAN §16b.6).
//!
//! A [`Redactor`] holds the known secret values and replaces each match in a
//! text or a byte stream with `[secret:<label>]`. The daemon builds one from
//! the secret store and the subscription API keys, and keeps it in a
//! [`RedactorHandle`] that it swaps after each change of a secret.
//!
//! The matcher is an Aho-Corasick automaton with leftmost-longest semantics:
//! where two secrets overlap, the longer match at the leftmost position wins.
//!
//! For each value the matcher holds these patterns, all with the label of the
//! value:
//!
//! - the value itself;
//! - the JSON-escaped form of the value (newlines as `\n`, quotes as `\"`),
//!   because transcripts and events are JSON;
//! - for a value with more than one line (an SSH key, a PEM), each line of
//!   [`MIN_LEN`] or more bytes that is not a `-----BEGIN`/`-----END` armor
//!   line, so a partial paste of a key is caught.
//!
//! A pattern shorter than [`MIN_LEN`] bytes is skipped: a short value matches
//! too much ordinary text.
//!
//! # Whole records only
//!
//! The matcher has no state between calls. A secret that is split across two
//! calls (for example, a log line split across two reads) is not found.
//! Callers redact whole records: a full log line, a full transcript entry, a
//! full mail body. A byte stream that leaves in chunks (the output of a
//! tool run) goes through a [`StreamRedactor`], which holds back the end of
//! each chunk that could still start a secret.
//!
//! Redaction is a guard against accidents. It does not find a value that is
//! changed (base64, reversed, split with spaces).
//!
//! # Labels and views
//!
//! Each entry has a scope (`None`: the label is public). A [`View`] says
//! which labels a caller may see. A match of an entry that the view does
//! not show becomes [`HIDDEN`] (`[secret]`), with no name (PLAN §16b.6).
//!
//! # Weak values
//!
//! [`judge`] marks a value that looks like an identifier (a ULID, a word
//! of the daemon's records) or that has low entropy. Such a value matches
//! too much ordinary text: a matcher with it blinds records. The daemon
//! refuses some of these values and leaves the others out of the matcher
//! ([`Redactor::build`] with a `keep` filter).

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use aho_corasick::{AhoCorasick, MatchKind};

/// The minimum length in bytes of a pattern. Shorter values are skipped.
pub const MIN_LEN: usize = 8;

/// The replacement of a match that the [`View`] does not show: no name.
pub const HIDDEN: &str = "[secret]";

/// A value of this length in bytes or more is judged only as an
/// identifier, not by its shape or its entropy ([`judge`]).
pub const STRONG_LEN: usize = 16;

/// The lowest entropy, in bits, of a value shorter than [`STRONG_LEN`].
/// The estimate is the Shannon entropy of the characters of the value
/// times its length.
pub const MIN_BITS: f64 = 28.0;

/// Words of the daemon's records (roles, actors, kinds, field names). A
/// value equal to one of them (case is not significant) is an identifier
/// ([`judge`]).
pub const RECORD_WORDS: &[&str] = &[
    "operator",
    "sysadmin",
    "project_admin",
    "system",
    "global",
    "terrarium",
    "terrariumd",
    "password",
    "passphrase",
    "secret",
    "secrets",
    "secret_fields",
    "plain_fields",
    "private_key",
    "public_key",
    "authorized_keys",
    "fingerprint",
    "grantee",
    "grantee_label",
    "username",
    "localhost",
    "github_token",
    "api_key",
    "ssh_key",
    "generic",
    "token",
    "login",
    "agent",
    "project",
    "created_by",
    "created_at",
    "updated_at",
];

/// Which labels a caller sees (PLAN §16b.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    /// Each label in full: the operator, a sysadmin, the system.
    All,
    /// The labels of this scope and the public labels. The others are
    /// [`HIDDEN`].
    Scope(String),
    /// The public labels only.
    Hidden,
}

impl View {
    fn shows(&self, scope: Option<&str>) -> bool {
        match (self, scope) {
            (_, None) | (Self::All, _) => true,
            (Self::Scope(own), Some(s)) => own == s,
            (Self::Hidden, Some(_)) => false,
        }
    }
}

/// One value of a [`Redactor::build`].
#[derive(Debug, Clone)]
pub struct Entry<V> {
    /// The text in `[secret:<label>]`.
    pub label: String,
    /// The scope of the value. `None`: each view sees the label.
    pub scope: Option<String>,
    pub value: V,
}

/// What [`judge`] finds in a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgment {
    /// Safe to match.
    Ok,
    /// Looks like an identifier of the records (a ULID, an actor label, a
    /// word of [`RECORD_WORDS`]). The text is the reason.
    Identifier(&'static str),
    /// Shorter than [`STRONG_LEN`] bytes, and one word or name, or low
    /// entropy. The text is the reason.
    Weak(&'static str),
}

/// True for a ULID: 26 characters of Crockford base32 (no I, L, O, U),
/// in either case.
pub fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes().all(|b| {
            let u = b.to_ascii_uppercase();
            u.is_ascii_digit()
                || (u.is_ascii_uppercase() && !matches!(u, b'I' | b'L' | b'O' | b'U'))
        })
}

/// The Shannon entropy of the characters of `s`, times its length in
/// characters: an estimate of the bits of a value.
#[allow(clippy::cast_precision_loss)]
pub fn entropy_bits(s: &str) -> f64 {
    let mut counts: std::collections::HashMap<char, usize> = std::collections::HashMap::new();
    let mut n = 0usize;
    for c in s.chars() {
        *counts.entry(c).or_default() += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let total = n as f64;
    let per_char: f64 = counts
        .values()
        .map(|&k| {
            let p = k as f64 / total;
            -p * p.log2()
        })
        .sum();
    per_char * total
}

/// Judge one pattern (a value, or one line of a value). A pattern shorter
/// than [`MIN_LEN`] bytes is `Ok`: the matcher skips it anyway.
pub fn judge(pattern: &[u8]) -> Judgment {
    let Ok(s) = std::str::from_utf8(pattern) else {
        return Judgment::Ok;
    };
    let s = s.trim();
    if s.len() < MIN_LEN {
        return Judgment::Ok;
    }
    // An actor label (`agent:<ULID>`, `schedule:<ULID>`) or a bare ULID.
    let id = match s.rsplit_once(':') {
        Some((prefix, id)) if prefix.chars().all(|c| c.is_ascii_lowercase() || c == '_') => id,
        _ => s,
    };
    if is_ulid(id) {
        return Judgment::Identifier("it is a ULID, the form of the IDs in the records");
    }
    if RECORD_WORDS.iter().any(|w| w.eq_ignore_ascii_case(s)) {
        return Judgment::Identifier("it is a word of the daemon's records");
    }
    if s.len() >= STRONG_LEN {
        return Judgment::Ok;
    }
    if s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._:/-".contains(c))
    {
        return Judgment::Weak("it is a lower-case word, name, or number");
    }
    if s.chars().all(|c| c.is_ascii_alphabetic()) {
        return Judgment::Weak("it is one word of letters only");
    }
    if entropy_bits(s) < MIN_BITS {
        return Judgment::Weak("it has low entropy");
    }
    Judgment::Ok
}

/// The patterns of a value before the JSON escape: the value, and for a
/// value with more than one line, each trimmed line of [`MIN_LEN`] bytes
/// or more that is not a PEM armor line. A value shorter than [`MIN_LEN`]
/// bytes gives no pattern of its own.
pub fn raw_patterns(value: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if value.len() >= MIN_LEN {
        out.push(value);
    }
    if value.contains(&b'\n') {
        for line in value.split(|&b| b == b'\n') {
            let line = line.trim_ascii();
            if line.len() >= MIN_LEN && !is_armor(line) {
                out.push(line);
            }
        }
    }
    out
}

/// The replacement of one pattern.
#[derive(Clone)]
struct Replacement {
    /// `[secret:<label>]`.
    full: Arc<str>,
    scope: Option<Arc<str>>,
}

/// An error from [`Redactor::new`].
#[derive(Debug, thiserror::Error)]
#[error("cannot build the redaction matcher: {0}")]
pub struct RedactorError(String);

/// A matcher of known secret values. Immutable; build a new one after a
/// change and swap it in a [`RedactorHandle`].
pub struct Redactor {
    /// `None` when there is no pattern: the fast path.
    matcher: Option<AhoCorasick>,
    /// The replacement of each pattern, by pattern index.
    replacements: Vec<Replacement>,
    /// The length in bytes of the longest pattern. 0 with no pattern.
    max_len: usize,
}

impl Redactor {
    /// Builds a matcher from `(label, value)` pairs. Each label is public
    /// (no scope), and each pattern is kept.
    ///
    /// A value shorter than [`MIN_LEN`] bytes is skipped. When two entries
    /// make the same pattern, the first entry gives the label.
    pub fn new<V: AsRef<[u8]>>(
        entries: impl IntoIterator<Item = (String, V)>,
    ) -> Result<Self, RedactorError> {
        Self::build(
            entries.into_iter().map(|(label, value)| Entry {
                label,
                scope: None,
                value,
            }),
            |_| true,
        )
    }

    /// Builds a matcher from entries. `keep` gets each raw pattern
    /// ([`raw_patterns`]). A pattern that `keep` refuses is not matched,
    /// and its JSON-escaped form is not matched either.
    pub fn build<V: AsRef<[u8]>>(
        entries: impl IntoIterator<Item = Entry<V>>,
        keep: impl Fn(&[u8]) -> bool,
    ) -> Result<Self, RedactorError> {
        let mut patterns: Vec<Vec<u8>> = Vec::new();
        let mut replacements: Vec<Replacement> = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();

        for entry in entries {
            let value = entry.value.as_ref();
            let replacement = Replacement {
                full: Arc::from(format!("[secret:{}]", entry.label)),
                scope: entry.scope.map(Arc::from),
            };
            for raw in raw_patterns(value) {
                if !keep(raw) {
                    continue;
                }
                let escaped = json_escaped(raw);
                for pattern in std::iter::once(raw.to_vec()).chain(escaped) {
                    if pattern.len() >= MIN_LEN && seen.insert(pattern.clone()) {
                        patterns.push(pattern);
                        replacements.push(replacement.clone());
                    }
                }
            }
        }

        if patterns.is_empty() {
            return Ok(Self::empty());
        }
        let max_len = patterns.iter().map(Vec::len).max().unwrap_or(0);
        let matcher = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .map_err(|e| RedactorError(e.to_string()))?;
        Ok(Self {
            matcher: Some(matcher),
            replacements,
            max_len,
        })
    }

    /// A matcher with no pattern. Each call returns its input unchanged and
    /// does not allocate.
    pub fn empty() -> Self {
        Self {
            matcher: None,
            replacements: Vec::new(),
            max_len: 0,
        }
    }

    /// True when the matcher has no pattern.
    pub fn is_empty(&self) -> bool {
        self.matcher.is_none()
    }

    /// The number of patterns (values, escaped forms, and lines).
    pub fn pattern_count(&self) -> usize {
        self.replacements.len()
    }

    /// The length in bytes of the longest pattern. 0 with no pattern.
    pub fn max_pattern_len(&self) -> usize {
        self.max_len
    }

    /// The replacement of pattern `i` for `view`.
    fn replacement(&self, i: usize, view: &View) -> &str {
        let r = &self.replacements[i];
        if view.shows(r.scope.as_deref()) {
            &r.full
        } else {
            HIDDEN
        }
    }

    /// Replaces each secret in `s` with `[secret:<label>]`, each label in
    /// full ([`View::All`]).
    ///
    /// Returns the input borrowed (no allocation) when nothing matches. A
    /// match that starts or ends inside a UTF-8 character (possible only for
    /// a value that is not valid UTF-8) is widened to the whole character, so
    /// the result is always valid UTF-8 and no part of the secret stays.
    pub fn redact_str<'a>(&self, s: &'a str) -> Cow<'a, str> {
        self.redact_str_as(s, &View::All).0
    }

    /// [`Self::redact_str`] for a view, with the number of matches.
    pub fn redact_str_as<'a>(&self, s: &'a str, view: &View) -> (Cow<'a, str>, usize) {
        let Some(matcher) = &self.matcher else {
            return (Cow::Borrowed(s), 0);
        };
        let mut out: Option<String> = None;
        let mut last = 0;
        let mut n = 0;
        for m in matcher.find_iter(s) {
            let mut start = m.start();
            while !s.is_char_boundary(start) {
                start -= 1;
            }
            let mut end = m.end();
            while !s.is_char_boundary(end) {
                end += 1;
            }
            let start = start.max(last);
            if end <= last {
                continue;
            }
            let buf = out.get_or_insert_with(|| String::with_capacity(s.len()));
            buf.push_str(&s[last..start]);
            buf.push_str(self.replacement(m.pattern().as_usize(), view));
            last = end;
            n += 1;
        }
        match out {
            None => (Cow::Borrowed(s), 0),
            Some(mut buf) => {
                buf.push_str(&s[last..]);
                (Cow::Owned(buf), n)
            }
        }
    }

    /// Replaces each secret in `b` with `[secret:<label>]`. Returns the
    /// input borrowed (no allocation) when nothing matches.
    pub fn redact_bytes<'a>(&self, b: &'a [u8]) -> Cow<'a, [u8]> {
        self.redact_bytes_as(b, &View::All)
    }

    /// [`Self::redact_bytes`] for a view.
    pub fn redact_bytes_as<'a>(&self, b: &'a [u8], view: &View) -> Cow<'a, [u8]> {
        let Some(matcher) = &self.matcher else {
            return Cow::Borrowed(b);
        };
        let mut out: Option<Vec<u8>> = None;
        let mut last = 0;
        for m in matcher.find_iter(b) {
            let buf = out.get_or_insert_with(|| Vec::with_capacity(b.len()));
            buf.extend_from_slice(&b[last..m.start()]);
            buf.extend_from_slice(self.replacement(m.pattern().as_usize(), view).as_bytes());
            last = m.end();
        }
        match out {
            None => Cow::Borrowed(b),
            Some(mut buf) => {
                buf.extend_from_slice(&b[last..]);
                Cow::Owned(buf)
            }
        }
    }

    /// Redacts an owned string. Returns the same allocation when nothing
    /// matches.
    pub fn redact_string(&self, s: String) -> String {
        self.redact_string_as(s, &View::All).0
    }

    /// [`Self::redact_string`] for a view, with the number of matches.
    /// Returns the same allocation when nothing matches.
    pub fn redact_string_as(&self, s: String, view: &View) -> (String, usize) {
        match self.redact_str_as(&s, view) {
            (Cow::Borrowed(_), _) => (s, 0),
            (Cow::Owned(r), n) => (r, n),
        }
    }
}

/// Redacts a byte stream that arrives in chunks (the output of a tool run),
/// so that a secret split across two chunks is found too.
///
/// [`Self::push`] returns the bytes that are safe to send: each byte before
/// the last `max - 1` bytes of what it has (`max` is the longest pattern),
/// with each match in that part replaced. A match that starts there fits in
/// the buffer whole, so the leftmost-longest choice is the same as on the
/// whole stream. The rest waits for the next chunk; [`Self::finish`] gives
/// it at the end. The held part never grows past `max - 1 + 3` bytes: the
/// cut moves back to the start of a UTF-8 character, so each part of a
/// valid UTF-8 stream stays valid. With no pattern, nothing is held.
pub struct StreamRedactor {
    redactor: Arc<Redactor>,
    view: View,
    held: Vec<u8>,
}

/// The most bytes that a cut moves back to the start of a UTF-8
/// character.
const UTF8_BACK: usize = 3;

impl StreamRedactor {
    pub fn new(redactor: Arc<Redactor>, view: View) -> Self {
        Self {
            redactor,
            view,
            held: Vec::new(),
        }
    }

    /// The most bytes that the redactor holds back between two calls.
    pub fn hold_limit(&self) -> usize {
        match self.redactor.max_pattern_len() {
            0 => 0,
            n => n - 1 + UTF8_BACK,
        }
    }

    /// The bytes that it holds back now.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Take the next chunk; return the redacted bytes that are safe to
    /// send now.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let Some(matcher) = &self.redactor.matcher else {
            // No pattern: a held part is left only by a matcher swap,
            // which does not happen here.
            let mut out = std::mem::take(&mut self.held);
            out.extend_from_slice(chunk);
            return out;
        };
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(chunk);
        let keep = self.redactor.max_len - 1;
        let mut safe = buf.len().saturating_sub(keep);
        let mut out = Vec::with_capacity(buf.len());
        let mut last = 0;
        for m in matcher.find_iter(&buf) {
            if m.start() >= safe {
                break;
            }
            out.extend_from_slice(&buf[last..m.start()]);
            out.extend_from_slice(
                self.redactor
                    .replacement(m.pattern().as_usize(), &self.view)
                    .as_bytes(),
            );
            last = m.end();
        }
        // The cut: not inside a match that went out, and at the start of a
        // UTF-8 character (a continuation byte is 0b10xx_xxxx).
        safe = safe.max(last);
        let floor = safe.saturating_sub(UTF8_BACK).max(last);
        while safe > floor && safe < buf.len() && buf[safe] & 0xC0 == 0x80 {
            safe -= 1;
        }
        out.extend_from_slice(&buf[last..safe]);
        self.held = buf.split_off(safe);
        out
    }

    /// The end of the stream: the held part, redacted.
    pub fn finish(&mut self) -> Vec<u8> {
        let rest = std::mem::take(&mut self.held);
        self.redactor
            .redact_bytes_as(&rest, &self.view)
            .into_owned()
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::empty()
    }
}

/// The patterns are secrets: the debug form shows the count only.
impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("patterns", &self.pattern_count())
            .finish()
    }
}

/// The JSON string body of `value` (no surrounding quotes), when it differs
/// from `value`. `None` for a value that is not UTF-8 or needs no escape.
fn json_escaped(value: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(value).ok()?;
    let quoted = serde_json::to_string(s).ok()?;
    let body = &quoted[1..quoted.len() - 1];
    (body.as_bytes() != value).then(|| body.as_bytes().to_vec())
}

/// True for a PEM armor line (`-----BEGIN ...`, `-----END ...`).
fn is_armor(line: &[u8]) -> bool {
    line.starts_with(b"-----BEGIN ") || line.starts_with(b"-----END ")
}

/// A shared, swappable [`Redactor`]. Clones share the same slot.
///
/// Readers take a snapshot with [`get`](Self::get) and use it without a
/// lock; [`replace`](Self::replace) swaps in a new matcher after a change of a
/// secret. A reader that holds an old snapshot finishes with the old matcher.
#[derive(Clone, Default)]
pub struct RedactorHandle {
    slot: Arc<RwLock<Arc<Redactor>>>,
}

impl RedactorHandle {
    /// A handle that holds `redactor`.
    pub fn new(redactor: Redactor) -> Self {
        Self {
            slot: Arc::new(RwLock::new(Arc::new(redactor))),
        }
    }

    /// The current matcher.
    pub fn get(&self) -> Arc<Redactor> {
        Arc::clone(&self.slot.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Swaps in a new matcher. Returns the old one.
    pub fn replace(&self, redactor: Redactor) -> Arc<Redactor> {
        let mut slot = self.slot.write().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *slot, Arc::new(redactor))
    }
}

impl fmt::Debug for RedactorHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RedactorHandle").field(&self.get()).finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn r(entries: &[(&str, &str)]) -> Redactor {
        Redactor::new(entries.iter().map(|(l, v)| (l.to_string(), *v))).unwrap()
    }

    #[test]
    fn replaces_a_value_with_its_label() {
        let red = r(&[("gh", "ghp_abcdefghijklmnop")]);
        assert_eq!(
            red.redact_str("token=ghp_abcdefghijklmnop end"),
            "token=[secret:gh] end"
        );
    }

    #[test]
    fn replaces_each_occurrence_and_each_secret() {
        let red = r(&[("a", "alpha-secret-1"), ("b", "bravo-secret-2")]);
        assert_eq!(
            red.redact_str("alpha-secret-1 bravo-secret-2 alpha-secret-1"),
            "[secret:a] [secret:b] [secret:a]"
        );
    }

    #[test]
    fn no_match_borrows() {
        let red = r(&[("a", "alpha-secret-1")]);
        assert!(matches!(red.redact_str("nothing here"), Cow::Borrowed(_)));
        assert!(matches!(
            red.redact_bytes(b"nothing here"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn empty_redactor_is_a_fast_path_with_no_allocation() {
        let red = Redactor::new(Vec::<(String, &str)>::new()).unwrap();
        assert!(red.is_empty());
        assert_eq!(red.pattern_count(), 0);
        let text = "anything at all";
        match red.redact_str(text) {
            Cow::Borrowed(b) => assert!(std::ptr::eq(b, text)),
            Cow::Owned(_) => panic!("allocated"),
        }
        assert!(matches!(red.redact_bytes(b"x"), Cow::Borrowed(_)));
        let owned = String::from("keep me");
        let ptr = owned.as_ptr();
        let back = red.redact_string(owned);
        assert_eq!(back.as_ptr(), ptr);
        assert!(Redactor::default().is_empty());
    }

    #[test]
    fn short_values_are_skipped() {
        let red = r(&[("short", "1234567"), ("ok", "12345678")]);
        assert_eq!(red.pattern_count(), 1);
        assert_eq!(red.redact_str("1234567 12345678"), "1234567 [secret:ok]");
        let only_short = r(&[("s", "abc"), ("e", "")]);
        assert!(only_short.is_empty());
    }

    #[test]
    fn longest_match_wins_at_the_same_start() {
        let red = r(&[("short", "secret-abc"), ("long", "secret-abc-def-ghi")]);
        assert_eq!(
            red.redact_str("x secret-abc-def-ghi y"),
            "x [secret:long] y"
        );
        assert_eq!(red.redact_str("x secret-abc y"), "x [secret:short] y");
        // The order of the entries does not matter.
        let red = r(&[("long", "secret-abc-def-ghi"), ("short", "secret-abc")]);
        assert_eq!(red.redact_str("secret-abc-def-ghi"), "[secret:long]");
    }

    #[test]
    fn overlapping_secrets_leftmost_wins_and_nothing_leaks_twice() {
        // "AAAABBBBCCCC" and "BBBBCCCCDDDD" overlap in "BBBBCCCC".
        let red = r(&[("one", "AAAABBBBCCCC"), ("two", "BBBBCCCCDDDD")]);
        // The leftmost match takes the text; the rest of the second value
        // ("DDDD") is not a secret by itself.
        assert_eq!(red.redact_str("AAAABBBBCCCCDDDD"), "[secret:one]DDDD");
        assert_eq!(red.redact_str("BBBBCCCCDDDD"), "[secret:two]");
        // A value inside another value: the outer one wins.
        let red = r(&[("inner", "inner-secret"), ("outer", "xx-inner-secret-xx")]);
        assert_eq!(red.redact_str("xx-inner-secret-xx"), "[secret:outer]");
    }

    #[test]
    fn duplicate_values_take_the_first_label() {
        let red = r(&[("first", "same-value-here"), ("second", "same-value-here")]);
        assert_eq!(red.pattern_count(), 1);
        assert_eq!(red.redact_str("same-value-here"), "[secret:first]");
    }

    const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACDw2nGq7NPNcbrqEOOqDyfn0NpY2eKCBGy4Bn9Gz7dvGQAAAJCRWwYHkVsG\n\
AAAEC8KfV0e1t3aM\n\
Bw==\n\
-----END OPENSSH PRIVATE KEY-----\n";

    #[test]
    fn multi_line_key_is_matched_whole() {
        let red = r(&[("deploy", KEY)]);
        let text = format!("here:\n{KEY}done");
        assert_eq!(red.redact_str(&text), "here:\n[secret:deploy]done");
    }

    #[test]
    fn multi_line_key_is_matched_line_by_line() {
        let red = r(&[("deploy", KEY)]);
        // A partial paste: the second body line alone.
        let text =
            "oops QyNTUxOQAAACDw2nGq7NPNcbrqEOOqDyfn0NpY2eKCBGy4Bn9Gz7dvGQAAAJCRWwYHkVsG oops";
        assert_eq!(red.redact_str(text), "oops [secret:deploy] oops");
        // A body line pasted with CRLF and indent still matches (lines are trimmed).
        let text = "  AAAEC8KfV0e1t3aM\r\n";
        assert_eq!(red.redact_str(text), "  [secret:deploy]\r\n");
        // The armor lines and the short last line are not patterns.
        let armor = "-----BEGIN OPENSSH PRIVATE KEY-----";
        assert_eq!(red.redact_str(armor), armor);
        assert_eq!(red.redact_str("Bw=="), "Bw==");
    }

    #[test]
    fn json_escaped_forms_are_matched() {
        let red = r(&[("deploy", KEY)]);
        let json = serde_json::to_string(&serde_json::json!({ "text": KEY })).unwrap();
        assert!(json.contains("\\n"));
        assert_eq!(red.redact_str(&json), "{\"text\":\"[secret:deploy]\"}");

        let pw = "pa\"ss\\word-123";
        let red = r(&[("pw", pw)]);
        let json = serde_json::to_string(&serde_json::json!({ "p": pw })).unwrap();
        assert_eq!(red.redact_str(&json), "{\"p\":\"[secret:pw]\"}");
        // The raw form still matches too.
        assert_eq!(red.redact_str(pw), "[secret:pw]");
    }

    #[test]
    fn json_escaped_crlf_key() {
        let crlf = KEY.replace('\n', "\r\n");
        let red = r(&[("k", crlf.as_str())]);
        let json = serde_json::to_string(&crlf).unwrap();
        assert_eq!(red.redact_str(&json), "\"[secret:k]\"");
        // And line by line, the CR is trimmed from each line.
        assert_eq!(red.redact_str("AAAEC8KfV0e1t3aM"), "[secret:k]");
    }

    #[test]
    fn utf8_values_and_boundaries() {
        let red = r(&[("u", "pässwörd-ünïcode")]);
        assert_eq!(red.redact_str("→pässwörd-ünïcode←"), "→[secret:u]←");
        // A byte value that starts inside a UTF-8 character ("ä" is C3 A4):
        // the match widens to the whole character.
        let value: Vec<u8> = [&[0xA4u8][..], b"bcdefghij"].concat();
        let red = Redactor::new([("b".to_string(), value)]).unwrap();
        assert_eq!(red.redact_str("xäbcdefghijx"), "x[secret:b]x");
        // The byte API replaces the exact bytes.
        assert_eq!(
            red.redact_bytes("xäbcdefghijx".as_bytes()).as_ref(),
            b"x\xC3[secret:b]x"
        );
    }

    #[test]
    fn utf8_widening_of_adjacent_matches_keeps_valid_text() {
        // Two byte values that each end and start inside the same "é" (C3 A9).
        let a: Vec<u8> = [&b"aaaaaaaa"[..], &[0xC3u8][..]].concat();
        let b: Vec<u8> = [&[0xA9u8][..], b"bbbbbbbb"].concat();
        let red = Redactor::new([("a".to_string(), a), ("b".to_string(), b)]).unwrap();
        let out = red.redact_str("<aaaaaaaaébbbbbbbb>");
        assert_eq!(out, "<[secret:a][secret:b]>");
    }

    #[test]
    fn redact_bytes_on_binary_data() {
        let red = r(&[("t", "token-1234567")]);
        let data = b"\x00\xfftoken-1234567\x00";
        assert_eq!(red.redact_bytes(data).as_ref(), b"\x00\xff[secret:t]\x00");
    }

    #[test]
    fn debug_does_not_show_values() {
        let red = r(&[("t", "token-1234567")]);
        let shown = format!(
            "{red:?} {:?}",
            RedactorHandle::new(r(&[("t", "token-1234567")]))
        );
        assert!(!shown.contains("token-1234567"), "{shown}");
    }

    #[test]
    fn handle_swaps_atomically_and_old_snapshots_survive() {
        let handle = RedactorHandle::default();
        assert!(handle.get().is_empty());
        let clone = handle.clone();
        let before = handle.get();
        clone.replace(r(&[("n", "new-secret-value")]));
        // The old snapshot is unchanged; a new get sees the new matcher.
        assert!(before.is_empty());
        assert_eq!(handle.get().redact_str("new-secret-value"), "[secret:n]");
        let old = handle.replace(Redactor::empty());
        assert_eq!(old.pattern_count(), 1);
        assert!(clone.get().is_empty());
    }

    #[test]
    fn handle_is_shared_across_threads() {
        let handle = RedactorHandle::new(r(&[("a", "aaaaaaaaaa")]));
        let threads: Vec<_> = (0..4)
            .map(|i| {
                let h = handle.clone();
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        if i == 0 {
                            h.replace(r(&[("a", "aaaaaaaaaa")]));
                        } else {
                            assert_eq!(h.get().redact_str("aaaaaaaaaa"), "[secret:a]");
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }

    fn scoped() -> Redactor {
        Redactor::build(
            [
                Entry {
                    label: "gh".to_owned(),
                    scope: Some("global".to_owned()),
                    value: "ghp_globalvalue123",
                },
                Entry {
                    label: "p/db".to_owned(),
                    scope: Some("p".to_owned()),
                    value: "p-project-Value-77",
                },
                Entry {
                    label: "q/db".to_owned(),
                    scope: Some("q".to_owned()),
                    value: "q-project-Value-88",
                },
                Entry {
                    label: "open".to_owned(),
                    scope: None,
                    value: "public-label-Value-99",
                },
            ],
            |_| true,
        )
        .unwrap()
    }

    #[test]
    fn a_view_hides_the_names_of_other_scopes() {
        let red = scoped();
        let text = "ghp_globalvalue123 p-project-Value-77 q-project-Value-88 public-label-Value-99";
        let (all, n) = red.redact_str_as(text, &View::All);
        assert_eq!(all, "[secret:gh] [secret:p/db] [secret:q/db] [secret:open]");
        assert_eq!(n, 4);
        let (p, n) = red.redact_str_as(text, &View::Scope("p".to_owned()));
        assert_eq!(p, "[secret] [secret:p/db] [secret] [secret:open]");
        assert_eq!(n, 4);
        let (none, _) = red.redact_str_as(text, &View::Hidden);
        assert_eq!(none, "[secret] [secret] [secret] [secret:open]");
        assert_eq!(
            red.redact_bytes_as(b"x q-project-Value-88", &View::Scope("p".to_owned()))
                .as_ref(),
            b"x [secret]"
        );
        let (s, n) = red.redact_string_as("nothing".to_owned(), &View::Hidden);
        assert_eq!((s.as_str(), n), ("nothing", 0));
        // The default is the full view.
        assert_eq!(red.redact_str("q-project-Value-88"), "[secret:q/db]");
    }

    #[test]
    fn keep_leaves_patterns_out() {
        let value = "line-one-is-strong-XyZ\nweakline\n";
        let red = Redactor::build(
            [Entry {
                label: "k".to_owned(),
                scope: None,
                value,
            }],
            |p| judge(p) == Judgment::Ok,
        )
        .unwrap();
        assert_eq!(red.redact_str("weakline"), "weakline");
        assert_eq!(red.redact_str("line-one-is-strong-XyZ"), "[secret:k]");
    }

    #[test]
    fn judge_marks_identifiers_and_weak_values() {
        use Judgment::{Identifier, Ok as Fine, Weak};
        let ok = |s: &str| judge(s.as_bytes()) == Fine;
        let id = |s: &str| matches!(judge(s.as_bytes()), Identifier(_));
        let weak = |s: &str| matches!(judge(s.as_bytes()), Weak(_));
        // Identifiers.
        assert!(id("01K8AGENT0000000000000000A"));
        assert!(id("01k8agent0000000000000000a"));
        assert!(id("agent:01K8AGENT0000000000000000A"));
        assert!(id("operator"));
        assert!(id("Grantee_Label"));
        assert!(id("authorized_keys"));
        // Weak: short words, names, numbers, low entropy.
        assert!(weak("hostname"));
        assert!(weak("web-dev-01"));
        assert!(weak("12345678"));
        assert!(weak("agent:p/w"));
        assert!(weak("Hostname"));
        assert!(weak("aaaaBBBB1"));
        // Short values are not judged: the matcher skips them.
        assert!(ok("abc"));
        // Real tokens and passwords pass.
        assert!(ok("ghp_R3alLook1ngT0ken"));
        assert!(ok("Tr0ub4dor&3"));
        assert!(ok("x7#Kp2!qZ"));
        assert!(ok("correct horse battery staple"));
        assert!(ok("sk-PROJECT-redact-me"));
        // 16 bytes or more: only the identifier checks.
        assert!(ok("aaaaaaaaaaaaaaaa"));
        // A TOTP seed (base32, 32 characters) is not a ULID.
        assert!(ok("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP"));
        assert!(entropy_bits("") < 1.0);
    }

    /// Push `chunks` through a stream redactor; return each emitted part
    /// (the last one from `finish`), and check the held limit after each
    /// push.
    fn stream(red: &Arc<Redactor>, view: View, chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut s = StreamRedactor::new(red.clone(), view);
        let mut parts = Vec::new();
        for c in chunks {
            parts.push(s.push(c));
            assert!(
                s.held() <= s.hold_limit(),
                "held {} > limit {}",
                s.held(),
                s.hold_limit()
            );
        }
        parts.push(s.finish());
        assert_eq!(s.held(), 0);
        parts
    }

    /// TER-29 (GitHub #21, L2): a token split at every byte offset of the
    /// stream is redacted, and the result is the same as on the whole text.
    #[test]
    fn a_stream_finds_a_token_split_at_each_offset() {
        let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let red = Arc::new(r(&[("gh", token)]));
        let text = format!("export GITHUB_TOKEN={token}\nok done\n");
        let whole = red.redact_bytes(text.as_bytes()).into_owned();
        assert!(!String::from_utf8_lossy(&whole).contains(token));
        let b = text.as_bytes();
        for i in 0..=b.len() {
            let got: Vec<u8> = stream(&red, View::All, &[&b[..i], &b[i..]]).concat();
            assert_eq!(got, whole, "split at {i}");
        }
        // Three parts: every pair of offsets.
        for i in 0..=b.len() {
            for j in i..=b.len() {
                let got: Vec<u8> =
                    stream(&red, View::All, &[&b[..i], &b[i..j], &b[j..]]).concat();
                assert_eq!(got, whole, "split at {i} and {j}");
            }
        }
        // One byte at a time.
        let bytes: Vec<&[u8]> = b.chunks(1).collect();
        assert_eq!(stream(&red, View::All, &bytes).concat(), whole);
    }

    /// Two secrets back to back, a secret that is the start of a longer
    /// one, and a secret at the very end of the stream.
    #[test]
    fn a_stream_handles_adjacent_overlapping_and_final_secrets() {
        let red = Arc::new(r(&[
            ("a", "alpha-secret-1"),
            ("b", "bravo-secret-2"),
            ("short", "charlie-key"),
            ("long", "charlie-key-extended"),
        ]));
        for text in [
            "alpha-secret-1bravo-secret-2",
            "x charlie-key-extended y charlie-key z",
            "the end is alpha-secret-1",
            "charlie-key",
        ] {
            let b = text.as_bytes();
            let whole = red.redact_bytes(b).into_owned();
            for i in 0..=b.len() {
                let got = stream(&red, View::All, &[&b[..i], &b[i..]]).concat();
                assert_eq!(got, whole, "{text:?} split at {i}");
            }
        }
        assert_eq!(
            stream(&red, View::All, &[&b"alpha-secret-1bravo-"[..], &b"secret-2"[..]]).concat(),
            b"[secret:a][secret:b]"
        );
        assert_eq!(
            stream(&red, View::All, &[&b"x charlie-key"[..], &b"-extended"[..]]).concat(),
            b"x [secret:long]"
        );
        // The last part comes only from `finish`.
        let parts = stream(&red, View::All, &[&b"the end is alpha-secret-1"[..]]);
        assert_eq!(parts.concat(), b"the end is [secret:a]");
        assert!(!parts[0].ends_with(b"alpha-secret-1"));
    }

    /// Each part of a valid UTF-8 stream is valid UTF-8: the cut does not
    /// fall inside a character.
    #[test]
    fn a_stream_cuts_on_utf8_boundaries() {
        let token = "s3cr3t-T0KEN-value";
        let red = Arc::new(r(&[("t", token)]));
        let text = format!("é漢字🙂 {token} ünïcödé 🙂🙂🙂 end");
        let b = text.as_bytes();
        let whole = red.redact_bytes(b).into_owned();
        for i in 0..=b.len() {
            for j in i..=b.len() {
                let parts = stream(&red, View::All, &[&b[..i], &b[i..j], &b[j..]]);
                assert_eq!(parts.concat(), whole, "split at {i} and {j}");
                // The input parts may cut a character; the output parts
                // that follow a whole character do not.
                if text.is_char_boundary(i) && text.is_char_boundary(j) {
                    for p in &parts {
                        assert!(std::str::from_utf8(p).is_ok(), "part {p:?} at {i}/{j}");
                    }
                }
            }
        }
    }

    /// The held part stays under the limit also for long chunks with no
    /// secret, and a matcher with no pattern holds nothing.
    #[test]
    fn a_stream_holds_at_most_the_limit() {
        let red = Arc::new(r(&[("t", "0123456789abcdef0123")]));
        assert_eq!(red.max_pattern_len(), 20);
        let mut s = StreamRedactor::new(red.clone(), View::All);
        assert_eq!(s.hold_limit(), 19 + 3);
        let big = vec![b'x'; 100_000];
        let out = s.push(&big);
        assert_eq!(out.len() + s.held(), big.len());
        assert!(s.held() <= 19, "{}", s.held());
        for _ in 0..1000 {
            s.push(b"y");
            assert!(s.held() <= s.hold_limit());
        }
        // Multi-byte characters only: the cut moves back at most 3 bytes.
        let wide = "🙂".repeat(1000);
        s.push(wide.as_bytes());
        assert!(s.held() <= s.hold_limit(), "{}", s.held());

        let none = Arc::new(Redactor::empty());
        let mut s = StreamRedactor::new(none, View::All);
        assert_eq!(s.hold_limit(), 0);
        assert_eq!(s.push(b"abc"), b"abc");
        assert_eq!(s.held(), 0);
        assert!(s.finish().is_empty());
    }

    /// The view of the stream: a scoped label is hidden from another view.
    #[test]
    fn a_stream_uses_its_view() {
        let red = Arc::new(
            Redactor::build(
                [Entry {
                    label: "p/deploy".to_owned(),
                    scope: Some("p".to_owned()),
                    value: "deploy-token-abcdef123",
                }],
                |_| true,
            )
            .unwrap(),
        );
        let input: [&[u8]; 2] = [b"t=deploy-tok", b"en-abcdef123."];
        assert_eq!(
            stream(&red, View::Scope("p".into()), &input).concat(),
            b"t=[secret:p/deploy]."
        );
        assert_eq!(
            stream(&red, View::Scope("q".into()), &input).concat(),
            b"t=[secret]."
        );
    }
}
