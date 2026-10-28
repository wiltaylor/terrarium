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
//! full mail body.
//!
//! Redaction is a guard against accidents. It does not find a value that is
//! changed (base64, reversed, split with spaces).

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use aho_corasick::{AhoCorasick, MatchKind};

/// The minimum length in bytes of a pattern. Shorter values are skipped.
pub const MIN_LEN: usize = 8;

/// An error from [`Redactor::new`].
#[derive(Debug, thiserror::Error)]
#[error("cannot build the redaction matcher: {0}")]
pub struct RedactorError(String);

/// A matcher of known secret values. Immutable; build a new one after a
/// change and swap it in a [`RedactorHandle`].
pub struct Redactor {
    /// `None` when there is no pattern: the fast path.
    matcher: Option<AhoCorasick>,
    /// The replacement text of each pattern, by pattern index.
    replacements: Vec<Arc<str>>,
}

impl Redactor {
    /// Builds a matcher from `(label, value)` pairs.
    ///
    /// A value shorter than [`MIN_LEN`] bytes is skipped. When two entries
    /// make the same pattern, the first entry gives the label.
    pub fn new<V: AsRef<[u8]>>(
        entries: impl IntoIterator<Item = (String, V)>,
    ) -> Result<Self, RedactorError> {
        let mut patterns: Vec<Vec<u8>> = Vec::new();
        let mut replacements: Vec<Arc<str>> = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();

        for (label, value) in entries {
            let value = value.as_ref();
            let replacement: Arc<str> = Arc::from(format!("[secret:{label}]"));
            let mut add = |pattern: Vec<u8>| {
                if pattern.len() >= MIN_LEN && seen.insert(pattern.clone()) {
                    patterns.push(pattern);
                    replacements.push(Arc::clone(&replacement));
                }
            };

            add(value.to_vec());
            if let Some(escaped) = json_escaped(value) {
                add(escaped);
            }
            if value.contains(&b'\n') {
                for line in value.split(|&b| b == b'\n') {
                    let line = line.trim_ascii();
                    if line.len() < MIN_LEN || is_armor(line) {
                        continue;
                    }
                    add(line.to_vec());
                    if let Some(escaped) = json_escaped(line) {
                        add(escaped);
                    }
                }
            }
        }

        if patterns.is_empty() {
            return Ok(Self::empty());
        }
        let matcher = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .map_err(|e| RedactorError(e.to_string()))?;
        Ok(Self {
            matcher: Some(matcher),
            replacements,
        })
    }

    /// A matcher with no pattern. Each call returns its input unchanged and
    /// does not allocate.
    pub fn empty() -> Self {
        Self {
            matcher: None,
            replacements: Vec::new(),
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

    /// Replaces each secret in `s` with `[secret:<label>]`.
    ///
    /// Returns the input borrowed (no allocation) when nothing matches. A
    /// match that starts or ends inside a UTF-8 character (possible only for
    /// a value that is not valid UTF-8) is widened to the whole character, so
    /// the result is always valid UTF-8 and no part of the secret stays.
    pub fn redact_str<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let Some(matcher) = &self.matcher else {
            return Cow::Borrowed(s);
        };
        let mut out: Option<String> = None;
        let mut last = 0;
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
            buf.push_str(&self.replacements[m.pattern().as_usize()]);
            last = end;
        }
        match out {
            None => Cow::Borrowed(s),
            Some(mut buf) => {
                buf.push_str(&s[last..]);
                Cow::Owned(buf)
            }
        }
    }

    /// Replaces each secret in `b` with `[secret:<label>]`. Returns the
    /// input borrowed (no allocation) when nothing matches.
    pub fn redact_bytes<'a>(&self, b: &'a [u8]) -> Cow<'a, [u8]> {
        let Some(matcher) = &self.matcher else {
            return Cow::Borrowed(b);
        };
        let mut out: Option<Vec<u8>> = None;
        let mut last = 0;
        for m in matcher.find_iter(b) {
            let buf = out.get_or_insert_with(|| Vec::with_capacity(b.len()));
            buf.extend_from_slice(&b[last..m.start()]);
            buf.extend_from_slice(self.replacements[m.pattern().as_usize()].as_bytes());
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
        match self.redact_str(&s) {
            Cow::Borrowed(_) => s,
            Cow::Owned(r) => r,
        }
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
}
