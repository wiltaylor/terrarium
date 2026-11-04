//! The pkt-line part of a smart HTTP push (`git-receive-pack`), as the
//! git proxy reads it (`gitprotocol-pack(5)`, `gitprotocol-common(5)`).
//!
//! A push request body is:
//!
//! ```text
//! *shallow-line
//! command-line *command-line flush-pkt     ; or one push-cert block
//! [push-option-line ... flush-pkt]         ; with the push-options capability
//! [packfile]                               ; not for a push of deletes only
//! ```
//!
//! The first command line carries the capabilities after a NUL. The proxy
//! needs only the command list. [`parse_commands`] reads it from the first
//! bytes of the body; the rest of the body goes to the forge unchanged.
//! [`refusal`] makes the answer that git shows for a refused push.

/// The longest pkt-line (`LARGE_PACKET_MAX`).
pub const MAX_PKT: usize = 65520;

/// One ref update of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub old: String,
    pub new: String,
    /// The full ref name, `refs/heads/main`.
    pub refname: String,
}

impl Command {
    /// The branch of a `refs/heads/` ref.
    pub fn branch(&self) -> Option<&str> {
        self.refname.strip_prefix("refs/heads/")
    }

    /// A delete: the new ID is all zeros.
    pub fn is_delete(&self) -> bool {
        self.new.bytes().all(|b| b == b'0')
    }
}

/// The command list of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commands {
    pub commands: Vec<Command>,
    /// The capabilities of the first command line.
    pub caps: Vec<String>,
    /// The bytes of the body up to the end of the command list (the
    /// flush-pkt included).
    pub end: usize,
}

impl Commands {
    pub fn has_cap(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }
}

/// The result of [`parse_commands`] on the first bytes of a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parse {
    /// The command list is not complete yet: read more bytes.
    More,
    Done(Commands),
    /// Not a push request that the proxy takes. The text is for git.
    Bad(String),
}

/// An object ID: 40 (SHA-1) or 64 (SHA-256) lower-case hex digits.
fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A ref name that the proxy forwards: `refs/...`, with no control
/// character, space, or other byte that git refuses in a ref
/// (`git check-ref-format`). The forge checks the full rules again.
fn refname_ok(r: &str) -> bool {
    r.starts_with("refs/")
        && r.len() <= 1024
        && !r.contains("..")
        && !r.contains("//")
        && !r.contains("@{")
        && !r.ends_with('/')
        && !r.ends_with(".lock")
        && !r.ends_with('.')
        && !r.split('/').any(|p| p.starts_with('.'))
        && r.bytes().all(|b| {
            b > 0x20 && b != 0x7f && !matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

/// Read the 4 hex digits of a pkt-line length.
fn pkt_len(b: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(b).ok()?;
    if !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    usize::from_str_radix(s, 16).ok()
}

/// An incremental parser of the command list (review P2). Each
/// [`Parser::feed`] gets the whole buffer so far, but parses only the
/// pkt-lines that are new and complete, so a body that comes in many small
/// chunks costs time linear in its size.
#[derive(Debug, Default)]
pub struct Parser {
    /// The bytes parsed so far (whole pkt-lines).
    pos: usize,
    /// The buffer length that the next step needs: a smaller buffer is
    /// [`Parse::More`] with no work.
    need: usize,
    commands: Vec<Command>,
    caps: Vec<String>,
    /// The pkt-line headers read: the work done, for the tests.
    steps: usize,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse what `buf` (the first bytes of the body, a prefix of the next
    /// call's `buf`) adds.
    pub fn feed(&mut self, buf: &[u8]) -> Parse {
        if buf.len() < self.need {
            return Parse::More;
        }
        loop {
            let pos = self.pos;
            let Some(head) = buf.get(pos..pos + 4) else {
                self.need = pos + 4;
                return Parse::More;
            };
            self.steps += 1;
            let Some(len) = pkt_len(head) else {
                return Parse::Bad("the push request is not in pkt-line form".into());
            };
            if len == 0 {
                // The flush-pkt after the command list. A body of only a
                // flush-pkt is an empty push (git sends one to probe the
                // auth before a large push).
                return Parse::Done(Commands {
                    commands: std::mem::take(&mut self.commands),
                    caps: std::mem::take(&mut self.caps),
                    end: pos + 4,
                });
            }
            if !(4..=MAX_PKT).contains(&len) {
                return Parse::Bad(format!("bad pkt-line length {len} in the push request"));
            }
            let Some(raw) = buf.get(pos + 4..pos + len) else {
                self.need = pos + len;
                return Parse::More;
            };
            self.pos = pos + len;
            if let Err(m) = self.line(raw) {
                return Parse::Bad(m);
            }
        }
    }

    /// One complete pkt-line of the command list.
    fn line(&mut self, raw: &[u8]) -> Result<(), String> {
        let line = raw.strip_suffix(b"\n").unwrap_or(raw);
        let (body, cap_bytes) = match line.iter().position(|b| *b == 0) {
            Some(i) => (&line[..i], Some(&line[i + 1..])),
            None => (line, None),
        };
        let Ok(body) = std::str::from_utf8(body) else {
            return Err("a command of the push request is not UTF-8".into());
        };
        if self.commands.is_empty() && body.starts_with("shallow ") {
            if cap_bytes.is_some() || !is_oid(&body["shallow ".len()..]) {
                return Err("bad shallow line in the push request".into());
            }
            return Ok(());
        }
        if body == "push-cert" || body.starts_with("push-cert ") {
            return Err(
                "signed pushes (git push --signed) do not go through the Terrarium git proxy"
                    .into(),
            );
        }
        let mut parts = body.split(' ');
        let (Some(old), Some(new), Some(refname), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("bad command line in the push request".into());
        };
        if !is_oid(old) || !is_oid(new) || old.len() != new.len() {
            return Err("bad object ID in the push request".into());
        }
        if !refname_ok(refname) {
            return Err(format!(
                "the ref `{}` is not a ref name that the Terrarium git proxy forwards",
                refname.escape_default()
            ));
        }
        match (self.commands.is_empty(), cap_bytes) {
            (true, Some(c)) => {
                self.caps = String::from_utf8_lossy(c)
                    .split(' ')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                // Push options go to the forge after the command list,
                // where the proxy does not read them, and some forges act
                // on a branch from them (GitLab `merge_request.*`, Gerrit
                // `submit`). The proxy does not advertise the capability
                // ([`strip_push_options`]); a client that sends it anyway
                // is refused (review P3).
                if self.caps.iter().any(|c| c == "push-options") {
                    return Err(
                        "push options (git push -o) do not go through the Terrarium git proxy"
                            .into(),
                    );
                }
            }
            (true, None) => {}
            (false, Some(_)) => {
                return Err("capabilities after the first command of the push".into());
            }
            (false, None) => {}
        }
        if self.commands.len() >= 10_000 {
            return Err("a push of more than 10000 refs".into());
        }
        self.commands.push(Command {
            old: old.to_owned(),
            new: new.to_owned(),
            refname: refname.to_owned(),
        });
        Ok(())
    }
}

/// Parse the command list at the start of a push body in one go. `buf`
/// holds the first bytes; [`Parse::More`] asks for more.
pub fn parse_commands(buf: &[u8]) -> Parse {
    Parser::new().feed(buf)
}

/// What a pushed ref changes, for the `push_deny` check (review P3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target<'a> {
    /// A branch, `refs/heads/<b>`: the glob check applies.
    Branch(&'a str),
    /// A review ref that lands on a branch: Gerrit and AGit
    /// `refs/for/<b>` with `%<options>` (`refs/for/main%submit` submits
    /// into `main`). The glob check applies to `<b>`.
    ReviewOf(&'a str),
    /// A tag, `refs/tags/<t>`: allowed.
    Tag,
    /// Any other ref (`refs/meta/config`, `refs/drafts/...`,
    /// `refs/changes/...`, `refs/notes/...`): refused for a repo with
    /// `push_deny`, because a forge can give it a meaning that the proxy
    /// does not check.
    Other,
}

/// The [`Target`] of a ref name.
pub fn target(refname: &str) -> Target<'_> {
    if let Some(b) = refname.strip_prefix("refs/heads/") {
        return Target::Branch(b);
    }
    if refname.starts_with("refs/tags/") {
        return Target::Tag;
    }
    if let Some(rest) = refname.strip_prefix("refs/for/") {
        let b = rest.split('%').next().unwrap_or(rest);
        let b = b.strip_prefix("refs/heads/").unwrap_or(b);
        return Target::ReviewOf(b);
    }
    Target::Other
}

/// The advertisement of `git-receive-pack` with no `push-options`
/// capability (review P3). `head` is the start of the answer of
/// `info/refs`: the service line, a flush, an optional `version 1` line,
/// then the first ref line with the capabilities after a NUL. `None`:
/// `head` does not hold the first ref line yet (read more). `Some(Err)`:
/// not an advertisement that the proxy takes.
pub fn strip_push_options(head: &[u8]) -> Option<Result<(Vec<u8>, usize), String>> {
    let mut pos = 0usize;
    let mut flushed = false;
    loop {
        let h = head.get(pos..pos + 4)?;
        let Some(len) = pkt_len(h) else {
            return Some(Err(
                "the forge's ref advertisement is not in pkt-line form".into()
            ));
        };
        if len == 0 {
            if flushed {
                // No ref line (should not happen for receive-pack).
                return Some(Ok((head[..pos + 4].to_vec(), pos + 4)));
            }
            flushed = true;
            pos += 4;
            continue;
        }
        if !(4..=MAX_PKT).contains(&len) {
            return Some(Err("bad pkt-line in the forge's ref advertisement".into()));
        }
        let raw = head.get(pos + 4..pos + len)?;
        if !flushed || raw.starts_with(b"version ") {
            // `# service=...` before the flush; `version 1` after it.
            pos += len;
            continue;
        }
        // The first ref line: `<oid> <ref>\0<caps>\n`.
        let mut out = head[..pos].to_vec();
        let line = raw.strip_suffix(b"\n").unwrap_or(raw);
        match line.iter().position(|b| *b == 0) {
            None => out.extend_from_slice(&head[pos..pos + len]),
            Some(i) => {
                let caps = String::from_utf8_lossy(&line[i + 1..]);
                let kept: Vec<&str> = caps
                    .split(' ')
                    .filter(|c| !c.is_empty() && *c != "push-options")
                    .collect();
                let mut data = line[..i + 1].to_vec();
                data.extend_from_slice(kept.join(" ").as_bytes());
                data.push(b'\n');
                out.extend(pkt(&data));
            }
        }
        return Some(Ok((out, pos + len)));
    }
}

/// One pkt-line with `data`.
pub fn pkt(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

/// The flush-pkt.
pub const FLUSH: &[u8] = b"0000";

/// Split `data` into side-band packets of `band`, each at most
/// `max` bytes of data.
fn sideband(band: u8, data: &[u8], max: usize, out: &mut Vec<u8>) {
    for chunk in data.chunks(max - 1) {
        let mut d = Vec::with_capacity(chunk.len() + 1);
        d.push(band);
        d.extend_from_slice(chunk);
        out.extend(pkt(&d));
    }
}

/// The answer to a push that the proxy refuses as a whole: no ref changes.
/// `reasons` has one entry for each command, in order: the reason that
/// git shows after `[remote rejected]`. `message` goes to the side band
/// (git shows it as `remote: ...`). The form follows the capabilities of
/// the request: `report-status` (or `report-status-v2`), `side-band-64k`
/// (or `side-band`). Without `report-status`, the answer is `None`: the
/// caller answers with an HTTP error.
pub fn refusal(c: &Commands, reasons: &[String], message: &str) -> Option<Vec<u8>> {
    if !(c.has_cap("report-status") || c.has_cap("report-status-v2")) {
        return None;
    }
    let mut report = pkt(b"unpack ok\n");
    for (cmd, why) in c.commands.iter().zip(reasons) {
        // One line: git reads the reason to the end of the pkt-line.
        let why: String = why.chars().filter(|ch| !ch.is_control()).collect();
        report.extend(pkt(format!("ng {} {why}\n", cmd.refname).as_bytes()));
    }
    report.extend_from_slice(FLUSH);
    let max = if c.has_cap("side-band-64k") {
        MAX_PKT - 4
    } else if c.has_cap("side-band") {
        1000 - 4
    } else {
        return Some(report);
    };
    let mut out = Vec::new();
    if !c.has_cap("quiet") && !message.is_empty() {
        let mut m = message.replace('\n', " ");
        m.push('\n');
        sideband(2, m.as_bytes(), max, &mut out);
    }
    sideband(1, &report, max, &mut out);
    out.extend_from_slice(FLUSH);
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const Z: &str = "0000000000000000000000000000000000000000";

    fn body(lines: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for l in lines {
            out.extend(pkt(l.as_bytes()));
        }
        out.extend_from_slice(FLUSH);
        out.extend_from_slice(b"PACK....");
        out
    }

    #[test]
    fn parses_the_command_list() {
        let first = format!("{A} {B} refs/heads/main\0report-status side-band-64k agent=git/2\n");
        let second = format!("{Z} {B} refs/heads/op/x\n");
        let third = format!("{A} {Z} refs/tags/v1");
        let b = body(&[&first, &second, &third]);
        let Parse::Done(c) = parse_commands(&b) else {
            panic!("{:?}", parse_commands(&b));
        };
        assert_eq!(c.commands.len(), 3);
        assert_eq!(c.commands[0].branch(), Some("main"));
        assert_eq!(c.commands[1].refname, "refs/heads/op/x");
        assert!(c.commands[2].is_delete());
        assert_eq!(c.commands[2].branch(), None);
        assert!(c.has_cap("report-status") && c.has_cap("side-band-64k"));
        assert_eq!(&b[c.end..], b"PACK....");
        // Each cut of the body short of the flush asks for more.
        for cut in 0..c.end {
            assert_eq!(parse_commands(&b[..cut]), Parse::More, "cut {cut}");
        }
    }

    #[test]
    fn shallow_lines_and_the_probe() {
        let b = body(&[
            &format!("shallow {A}"),
            &format!("{A} {B} refs/heads/main\0report-status"),
        ]);
        let Parse::Done(c) = parse_commands(&b) else {
            panic!()
        };
        assert_eq!(c.commands.len(), 1);
        let Parse::Done(c) = parse_commands(b"0000") else {
            panic!()
        };
        assert!(c.commands.is_empty());
        assert_eq!(c.end, 4);
    }

    #[test]
    fn refuses_what_it_cannot_check() {
        for bad in [
            b"zzzz".to_vec(),
            b"0003".to_vec(),
            body(&["push-cert\0report-status"]),
            body(&[&format!("{A} {B}")]),
            body(&[&format!("{A} {B} refs/heads/a b")]),
            body(&[&format!("{A} xyz refs/heads/main")]),
            body(&[&format!("{A} {B} HEAD")]),
            body(&[&format!("{A} {B} main")]),
            body(&[&format!("{A} {B} refs/heads/../x")]),
            body(&[&format!("{A} {B} refs/heads/a\x01")]),
            body(&[
                &format!("{A} {B} refs/heads/a"),
                &format!("{A} {B} refs/heads/b\0caps"),
            ]),
            body(&[&format!("shallow {A}\0caps")]),
        ] {
            assert!(matches!(parse_commands(&bad), Parse::Bad(_)), "{bad:?}");
        }
    }

    #[test]
    fn one_byte_chunks_cost_linear_time() {
        // Review P2: a 4 MiB command list fed one byte at a time, as a
        // body of 1-byte HTTP chunks comes in.
        let mut b = Vec::new();
        let mut lines = 0;
        while b.len() < 4 << 20 {
            let caps = if lines == 0 { "\0report-status" } else { "" };
            let r = format!("{A} {B} refs/heads/{}{lines}{caps}\n", "x".repeat(900));
            b.extend(pkt(r.as_bytes()));
            lines += 1;
        }
        b.extend_from_slice(FLUSH);
        let t = std::time::Instant::now();
        let mut p = Parser::new();
        let mut done = None;
        for n in 1..=b.len() {
            match p.feed(&b[..n]) {
                Parse::More => {}
                other => {
                    done = Some(other);
                    break;
                }
            }
        }
        let took = t.elapsed();
        let Some(Parse::Done(c)) = done else {
            panic!("{done:?}")
        };
        assert_eq!(c.commands.len(), lines);
        // Each pkt-line header is read about once (twice when the header
        // came in before its line): linear, not one parse of the whole
        // buffer for each byte (about 4 million times 4500 lines).
        assert!(
            p.steps <= 2 * (lines + 1),
            "{} steps for {lines} lines",
            p.steps
        );
        // Well under 1 s of CPU in a release build; a debug build is
        // about 10 times slower.
        let bound = if cfg!(debug_assertions) { 3000 } else { 300 };
        assert!(took < std::time::Duration::from_millis(bound), "{took:?}");
    }

    #[test]
    fn push_options_and_targets() {
        let b = body(&[&format!("{A} {B} refs/heads/f\0report-status push-options")]);
        assert!(matches!(parse_commands(&b), Parse::Bad(m) if m.contains("push options")));
        assert_eq!(target("refs/heads/op/x"), Target::Branch("op/x"));
        assert_eq!(target("refs/for/main%submit"), Target::ReviewOf("main"));
        assert_eq!(
            target("refs/for/refs/heads/main%r=a"),
            Target::ReviewOf("main")
        );
        assert_eq!(target("refs/for/main"), Target::ReviewOf("main"));
        assert_eq!(target("refs/tags/v1"), Target::Tag);
        for o in [
            "refs/meta/config",
            "refs/drafts/main",
            "refs/changes/01/1/1",
            "refs/notes/x",
        ] {
            assert_eq!(target(o), Target::Other, "{o}");
        }
    }

    #[test]
    fn the_advertisement_loses_push_options() {
        let mut ad = pkt(b"# service=git-receive-pack\n");
        ad.extend_from_slice(FLUSH);
        let first = format!("{A} refs/heads/main\0report-status push-options atomic\n");
        ad.extend(pkt(first.as_bytes()));
        ad.extend(pkt(format!("{B} refs/heads/dev\n").as_bytes()));
        ad.extend_from_slice(FLUSH);
        for cut in 0..40 {
            assert!(strip_push_options(&ad[..cut]).is_none(), "{cut}");
        }
        let (out, used) = strip_push_options(&ad).unwrap().unwrap();
        let mut whole = out.clone();
        whole.extend_from_slice(&ad[used..]);
        let text = String::from_utf8_lossy(&whole);
        assert!(!text.contains("push-options"), "{text}");
        assert!(text.contains("report-status atomic\n"), "{text}");
        assert!(text.contains("refs/heads/dev"));
        // The new length is right: the stream parses as pkt-lines.
        let n = 4 + 27 + 4;
        let len =
            usize::from_str_radix(std::str::from_utf8(&whole[n..n + 4]).unwrap(), 16).unwrap();
        assert_eq!(&whole[n + len - 1..n + len], b"\n");
        // A `version 1` line before the refs is kept.
        let mut v1 = pkt(b"# service=git-receive-pack\n");
        v1.extend_from_slice(FLUSH);
        v1.extend(pkt(b"version 1\n"));
        v1.extend(pkt(first.as_bytes()));
        let (out, _) = strip_push_options(&v1).unwrap().unwrap();
        assert!(String::from_utf8_lossy(&out).contains("version 1"));
        assert!(!String::from_utf8_lossy(&out).contains("push-options"));
        assert!(strip_push_options(b"zzzz").unwrap().is_err());
    }

    #[test]
    fn the_refusal_follows_the_capabilities() {
        let c = |caps: &str| Commands {
            commands: vec![Command {
                old: A.into(),
                new: B.into(),
                refname: "refs/heads/op/x".into(),
            }],
            caps: caps.split(' ').map(str::to_owned).collect(),
            end: 0,
        };
        let why = vec!["denied by push_deny `op/*`".to_owned()];
        assert_eq!(refusal(&c("ofs-delta"), &why, "m"), None);
        let plain = refusal(&c("report-status"), &why, "m").unwrap();
        assert_eq!(
            String::from_utf8(plain).unwrap(),
            "000eunpack ok\n0032ng refs/heads/op/x denied by push_deny `op/*`\n0000"
        );
        let banded = refusal(&c("report-status side-band-64k"), &why, "no").unwrap();
        // The message on band 2, the report on band 1, then a flush.
        assert!(banded.starts_with(b"0008\x02no\n"));
        assert!(banded.ends_with(b"0000"));
        let inner = &banded[8..banded.len() - 4];
        assert_eq!(inner[4], 1);
        assert!(String::from_utf8_lossy(inner).contains("ng refs/heads/op/x"));
        let quiet = refusal(&c("report-status side-band-64k quiet"), &why, "no").unwrap();
        assert_eq!(quiet[4], 1);
    }
}
