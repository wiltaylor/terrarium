//! The adapter interface. One adapter per harness. See PLAN §7.

use std::path::Path;
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::config::SubscriptionDef;
use crate::error::Result;
use crate::event::Event;
use crate::harness::HarnessId;
use crate::hook::HookPayload;
use crate::ids::HarnessSessionId;
use crate::launch::{FileOverlay, LaunchSpec};
use crate::model::Agent;

/// The parsed terminal screen of a harness.
pub type Screen = vt100::Screen;

/// What an adapter can tell from the screen alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScreenHint {
    /// The adapter cannot tell.
    #[default]
    Unknown,
    /// The input prompt shows and accepts a turn.
    PromptReady,
    /// The harness is busy with a turn.
    Busy,
    /// The harness shows a question or a dialog to the user.
    Question,
    /// The adapter knows the prompt does not accept a turn now, but the
    /// screen is not a busy turn or a question either. Examples: a start
    /// screen, text left in the input box, the process has exited. Do not
    /// type, and do not fall back to a generic detector.
    NotReady,
}

/// An answer to a question that the harness shows (see `Event::Question`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Answer {
    /// Pick the option at this index, counted from 0 in screen order.
    Choice(usize),
    /// Allow the request (a permission prompt).
    Allow,
    /// Deny the request, or cancel the question.
    Deny,
}

/// Knowledge of one harness: launch flags, hook mapping, session capture, and
/// how to type a turn. The trait is object safe, so the daemon can hold a
/// `Box<dyn Adapter>` per harness. No method is async: each call is either
/// pure, or a short blocking call that the caller runs off the async runtime.
pub trait Adapter: Send + Sync {
    fn id(&self) -> HarnessId;

    /// Run the harness to read its installed version. This blocks.
    fn detect_version(&self) -> Result<Version>;

    /// Command line and env for a fresh start or a resume. Includes the yolo
    /// flags, the system prompt, the hook config, and the quiet-mode env.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec;

    /// [`Adapter::launch`] for an agent whose subscription the caller has
    /// resolved (`Agent::subscription` is only the name). A harness that
    /// can spend many providers (pi) picks its provider flags from it. The
    /// default ignores the subscription.
    fn launch_for(
        &self,
        agent: &Agent,
        resume: Option<&HarnessSessionId>,
        _subscription: Option<&SubscriptionDef>,
    ) -> LaunchSpec {
        self.launch(agent, resume)
    }

    /// Map a hook payload from the runner socket to events.
    fn hook_event(&self, hook: &HookPayload) -> Vec<Event>;

    /// True when `hook` is a prompt-submit hook for a prompt that the
    /// harness made itself, not one that was typed. Copilot sends a
    /// `UserPromptSubmit` with a `<system_notification>` prompt when a
    /// background shell command ends. The runner does not count such a hook
    /// as the delivery of a typed turn, and does not report it as a user
    /// turn (`Event::TurnStart`). The default is false.
    fn is_system_prompt(&self, _hook: &HookPayload) -> bool {
        false
    }

    /// True when the harness sends a hook at the end of its startup, and
    /// its prompt does not take a turn before that hook. The runner then
    /// keeps the agent `starting` until a hook matches
    /// [`Adapter::is_startup_hook`], also when the screen reads as ready.
    /// pi draws an empty editor before its startup is complete, and a
    /// submit in that time only shows "Startup is still in progress". The
    /// default is false: the screen alone decides.
    fn waits_for_startup_hook(&self) -> bool {
        false
    }

    /// True when `hook` tells that the startup of the harness is complete
    /// (see [`Adapter::waits_for_startup_hook`]). The default is false.
    fn is_startup_hook(&self, _hook: &HookPayload) -> bool {
        false
    }

    /// Heuristics from the screen, for `blocked` detection and for "is the
    /// prompt ready" before typing.
    fn screen_hint(&self, _screen: &Screen) -> ScreenHint {
        ScreenHint::Unknown
    }

    /// Where the harness writes its session id if no hook carries it.
    fn find_session_id(&self, _agent: &Agent) -> Option<HarnessSessionId> {
        None
    }

    /// Keystrokes that submit a user turn. The default is bracketed paste and
    /// then Enter. [`bracketed_paste`] removes the paste markers from the
    /// text first (TER-1289, TER-1325): a turn can carry untrusted mail.
    fn type_turn(&self, text: &str) -> Vec<u8> {
        bracketed_paste(text)
    }

    /// The pause between the text of a turn and its final CR. With a pause,
    /// the runner writes [`Adapter::type_turn`] without the CR, waits this
    /// long, and then writes the CR on its own ([`exit_key_writes`]). A
    /// harness that drops a CR that comes in the same burst as a long paste
    /// needs it: Claude Code keeps the paste as "[Pasted text #N +L lines]"
    /// in its input box and does not submit it (TER-194). The default is
    /// zero: one write.
    fn turn_submit_pause(&self) -> Duration {
        Duration::ZERO
    }

    /// True when the input box of the harness holds text, and a CR would
    /// submit it: not busy, and no dialog. The runner presses CR again
    /// when a typed turn still sits in the input box. An idle agent that
    /// keeps text in its input box takes no turn: the runner marks it
    /// `blocked` with [`INPUT_PENDING_REASON`], and the daemon mails its
    /// leads. The default is false: the adapter cannot tell.
    fn input_pending(&self, _screen: &Screen) -> bool {
        false
    }

    /// Keystrokes that type `text` as-is and press Enter, for a leader's
    /// raw send (`POST /api/agents/{id}/send` with `raw: true`): a slash
    /// command or a bang command runs. No bracketed paste and no escape
    /// ([`escape_turn`]). The caller splits the final CR off with
    /// [`exit_key_writes`] and [`Adapter::exit_key_pause`], as for the exit
    /// keys. The default is the text, then CR.
    fn raw_turn_keys(&self, text: &str) -> Vec<u8> {
        raw_text_keys(text)
    }

    /// Keystrokes that answer the question `request_id`. Empty when the
    /// adapter has no answer path.
    fn answer(&self, _request_id: &str, _answer: &Answer) -> Vec<u8> {
        Vec::new()
    }

    /// Keystrokes that empty the input line of the harness. The runner sends
    /// them before it types a turn again after a delivery that got no
    /// prompt-submit hook, so the old text does not stay in front of the new
    /// text. They must not interrupt a running turn or exit the harness. The
    /// default is Ctrl-U (kill to the start of the line).
    fn clear_input_keys(&self) -> Vec<u8> {
        vec![CTRL_U]
    }

    /// Keystrokes that interrupt the running turn of the harness, as the
    /// operator's Stop does (`POST /api/agents/{id}/interrupt`). Each item
    /// is one write; the caller waits [`INTERRUPT_GAP`] between two writes,
    /// so the harness does not read two Esc keys as one Alt chord. The keys
    /// must stop the turn and keep the harness running: never a key that
    /// can exit it (a second Ctrl-C quits most harnesses). Empty (the
    /// default) means "no known keys": the harness cannot be interrupted.
    fn interrupt_keys(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Keystrokes that make an idle harness exit cleanly, for a graceful
    /// stop before a kill. Empty (the default) means "no known keys": the
    /// caller kills the process.
    fn exit_keys(&self) -> Vec<u8> {
        Vec::new()
    }

    /// The pause between the exit text and its final CR. With a pause, the
    /// caller types the exit keys without the CR, waits this long, and then
    /// types the CR ([`exit_key_writes`]). A harness that takes a fast burst
    /// as a paste needs it: Codex keeps `/exit\r` in one write in the
    /// composer and does not submit it. The default is zero: one write.
    fn exit_key_pause(&self) -> Duration {
        Duration::ZERO
    }

    /// True when the harness keeps the system prompt of a session from the
    /// moment it made the session, so a resume with a changed prompt still
    /// sends the old one. The supervisor then starts a fresh session (not a
    /// resume) when the rendered prompt changed. The default is false.
    fn prompt_frozen_on_resume(&self) -> bool {
        false
    }

    /// Write the per-agent files that the harness reads from its home, and
    /// return the overlays that the sandbox must bind. `home` is the shared
    /// harness home on the host (for example `/auth/claude`). The default
    /// writes nothing. This blocks.
    fn materialize_config(&self, _agent: &Agent, _home: &Path) -> Result<Vec<FileOverlay>> {
        Ok(Vec::new())
    }

    /// [`Adapter::materialize_config`] for an agent whose subscription the
    /// caller has resolved. A harness whose per-agent files name the
    /// provider (pi's `models.json` for a self-hosted server) writes it
    /// there. The default ignores the subscription.
    fn materialize_for(
        &self,
        agent: &Agent,
        home: &Path,
        _subscription: Option<&SubscriptionDef>,
    ) -> Result<Vec<FileOverlay>> {
        self.materialize_config(agent, home)
    }
}

/// The writes of a graceful exit, from [`Adapter::exit_keys`] and
/// [`Adapter::exit_key_pause`]. With no pause, or keys that do not end in
/// a CR, it is one write of all the keys. Otherwise it is two writes: the
/// keys without the final CR, then the CR. The caller waits `pause` between
/// them.
pub fn exit_key_writes(keys: &[u8], pause: Duration) -> Vec<Vec<u8>> {
    match keys.split_last() {
        Some((b'\r', text)) if !pause.is_zero() && !text.is_empty() => {
            vec![text.to_vec(), b"\r".to_vec()]
        }
        _ if keys.is_empty() => Vec::new(),
        _ => vec![keys.to_vec()],
    }
}

/// Ctrl-U: kill the line before the cursor.
pub const CTRL_U: u8 = 0x15;

/// The reason of the `blocked` status that the runner sets when an idle
/// agent keeps unsubmitted text in its input box for the submit timeout
/// ([`Adapter::input_pending`], TER-194, TER-208). No turn can go out
/// then. The daemon mails the leads of the agent when a status change has
/// this reason.
pub const INPUT_PENDING_REASON: &str = "unsubmitted text in the input box";

/// Esc, the interrupt key of the harness TUIs.
pub const ESC: u8 = 0x1b;

/// The pause between two writes of [`Adapter::interrupt_keys`]. A TUI reads
/// an Esc that comes right before another key as Alt with that key; 150 ms
/// is longer than that wait, and still short enough for a double Esc
/// (Copilot CLI, "esc esc interrupt").
pub const INTERRUPT_GAP: Duration = Duration::from_millis(150);

/// The character that [`escape_turn`] puts in front of a turn: U+00A0
/// NO-BREAK SPACE. Verified live on 2026-10-28: with it, `/help ...` was a
/// normal turn in Claude Code 2.1.310, Codex 0.164.0, Copilot 1.0.103,
/// and pi 1.4.0. U+200B (zero width space) and a plain space did not work
/// in Claude Code: it ran the slash command. The words of the turn do not
/// change.
pub const TURN_ESCAPE: char = '\u{00A0}';

/// The first characters that make a harness run typed text as a command:
/// `/` (a slash command, for example `/plan`) and `!` (a shell command in
/// Claude Code, Codex, Copilot, and pi).
pub const COMMAND_PREFIXES: &[char] = &['/', '!'];

/// Escape a turn so that the harness takes it as plain text (D3,
/// `docs/research/harness-hardening.md`, finding 1). When the text, after
/// its leading white space, starts with one of `prefixes`, or is exactly one
/// of `lone` (Copilot opens a shell on a lone `$`), the result is
/// [`TURN_ESCAPE`] and the text. Other text does not change. The escape
/// does not change the words that the model reads.
pub fn escape_turn<'a>(
    text: &'a str,
    prefixes: &[char],
    lone: &[&str],
) -> std::borrow::Cow<'a, str> {
    let t = text.trim_start();
    let command = t.starts_with(prefixes) || lone.contains(&t.trim_end());
    if command {
        std::borrow::Cow::Owned(format!("{TURN_ESCAPE}{text}"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// The text with each leading [`TURN_ESCAPE`] removed: the turn as the
/// sender wrote it.
pub fn unescape_turn(text: &str) -> &str {
    text.trim_start_matches(TURN_ESCAPE)
}

/// The bracketed-paste start marker.
pub const PASTE_START: &str = "\x1b[200~";
/// The bracketed-paste end marker.
pub const PASTE_END: &str = "\x1b[201~";

/// The text with every bracketed-paste marker removed (TER-1289). One
/// pass of `replace` is not enough: the removal of a nested marker can
/// rebuild one (`"\x1b[20\x1b[201~1~"` becomes `"\x1b[201~"`), and a
/// rebuilt end marker ends the paste early, so the rest of an untrusted
/// text (task, mail, card) is typed as keys.
///
/// One scan in linear time: each char goes onto the output, and a marker
/// that the output then ends with is cut off. The output before the push
/// had no marker, so a new marker can only end at the last char. The
/// result has no marker, at any nesting depth. The markers are ASCII, so
/// a cut never splits a UTF-8 char.
pub fn strip_paste_markers(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(PASTE_START) && !text.contains(PASTE_END) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut clean = String::with_capacity(text.len());
    for c in text.chars() {
        clean.push(c);
        if c == '~' && (clean.ends_with(PASTE_START) || clean.ends_with(PASTE_END)) {
            clean.truncate(clean.len() - PASTE_END.len());
        }
    }
    std::borrow::Cow::Owned(clean)
}

/// The text as keystrokes, then CR. The bracketed-paste markers are
/// removed: a raw text is typed, not pasted.
pub fn raw_text_keys(text: &str) -> Vec<u8> {
    let clean = strip_paste_markers(text);
    let mut out = clean.into_owned().into_bytes();
    out.push(b'\r');
    out
}

/// Wrap `text` in bracketed-paste markers and add a carriage return. The
/// paste markers in `text` are removed first ([`strip_paste_markers`]): a
/// marker in the text would end the paste early, and the rest of the text
/// would be typed as keys (TER-1289). Every caller gets the strip, also an
/// adapter that keeps the default [`Adapter::type_turn`] (TER-1325).
pub fn bracketed_paste(text: &str) -> Vec<u8> {
    let clean = strip_paste_markers(text);
    let mut out = Vec::with_capacity(clean.len() + 13);
    out.extend_from_slice(PASTE_START.as_bytes());
    out.extend_from_slice(clean.as_bytes());
    out.extend_from_slice(PASTE_END.as_bytes());
    out.push(b'\r');
    out
}

/// Parse the first `x.y.z` in the output of `<harness> --version`. A full
/// stop or comma after the version is not a part of it ("GitHub Copilot
/// CLI 1.0.103.").
pub fn parse_version_output(output: &str) -> Result<Version> {
    output
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == 'v')
        .find_map(|word| {
            let w = word.trim();
            Version::parse(w)
                .or_else(|_| Version::parse(w.trim_end_matches(['.', ',', ';', ':'])))
                .ok()
        })
        .ok_or_else(|| crate::Error::Version(format!("no version in `{}`", output.trim())))
}

/// Run `program --version` and parse the result. This blocks.
pub fn run_version(program: &str) -> Result<Version> {
    let out = std::process::Command::new(program)
        .arg("--version")
        .output()
        .map_err(|e| crate::Error::Version(format!("cannot run {program}: {e}")))?;
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps_text() {
        assert_eq!(bracketed_paste("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
    }

    /// An adapter with only the required methods: it keeps every default.
    struct DefaultAdapter;

    impl Adapter for DefaultAdapter {
        fn id(&self) -> HarnessId {
            HarnessId::Claude
        }
        fn detect_version(&self) -> Result<Version> {
            Ok(Version::new(0, 0, 0))
        }
        fn launch(&self, _agent: &Agent, _resume: Option<&HarnessSessionId>) -> LaunchSpec {
            LaunchSpec::default()
        }
        fn hook_event(&self, _hook: &HookPayload) -> Vec<Event> {
            Vec::new()
        }
    }

    /// TER-1325: the default `type_turn` strips the paste markers, also
    /// nested ones, so a marker in the text cannot end the paste early.
    #[test]
    fn default_type_turn_strips_paste_markers() {
        let a = DefaultAdapter;
        assert_eq!(a.type_turn("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
        for text in [
            "a\x1b[201~/exit\rb",
            "a\x1b[200~b",
            "a\x1b[20\x1b[201~1~/exit\rb",
        ] {
            let keys = a.type_turn(text);
            let body = &keys[PASTE_START.len()..keys.len() - PASTE_END.len() - 1];
            let body = std::str::from_utf8(body).unwrap();
            assert!(keys.starts_with(PASTE_START.as_bytes()), "{text:?}");
            assert!(keys.ends_with(b"\x1b[201~\r"), "{text:?}");
            assert!(
                !body.contains(PASTE_START) && !body.contains(PASTE_END),
                "{text:?} -> {body:?}"
            );
        }
        assert_eq!(
            a.type_turn("a\x1b[20\x1b[201~1~b"),
            b"\x1b[200~ab\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn command_turns_get_the_escape() {
        let esc = |t| escape_turn(t, COMMAND_PREFIXES, &[]).into_owned();
        assert_eq!(esc("/plan now"), "\u{00A0}/plan now");
        assert_eq!(esc("  !rm -rf x"), "\u{00A0}  !rm -rf x");
        assert_eq!(esc("\n/compact"), "\u{00A0}\n/compact");
        // Other text does not change.
        for t in ["New mail from lead (thread T)", "a /b", "", "$", "x!"] {
            assert_eq!(esc(t), t);
        }
        // The lone `$` of Copilot.
        assert_eq!(escape_turn(" $ ", COMMAND_PREFIXES, &["$"]), "\u{00A0} $ ");
        assert_eq!(escape_turn("$HOME", COMMAND_PREFIXES, &["$"]), "$HOME");
        // The harnesses read a turn that starts with U+00A0 as text
        // (verified live, see TURN_ESCAPE); the escape is the first char.
        let e = esc("/plan");
        assert!(e.starts_with(TURN_ESCAPE));
        assert_eq!(unescape_turn(&e), "/plan");
        assert_eq!(unescape_turn("plain"), "plain");
    }

    #[test]
    fn raw_text_is_typed_not_pasted() {
        assert_eq!(raw_text_keys("/compact"), b"/compact\r".to_vec());
        assert_eq!(raw_text_keys("a\x1b[201~b"), b"ab\r".to_vec());
        // TER-1289: a nested marker does not come back.
        assert_eq!(raw_text_keys("a\x1b[20\x1b[201~1~b"), b"ab\r".to_vec());
    }

    /// TER-1289: the strip runs to a fixed point. Nested markers (one in
    /// the other, at any depth), markers that a removed start marker
    /// rebuilds, and split markers leave no marker.
    #[test]
    fn paste_marker_strip_leaves_no_marker() {
        let cases = [
            ("plain text", "plain text"),
            ("a\x1b[201~b", "ab"),
            ("a\x1b[200~b", "ab"),
            // Nested end marker in an end marker.
            ("\x1b[20\x1b[201~1~", ""),
            ("x\x1b[20\x1b[201~1~/exit\r", "x/exit\r"),
            // Two levels deep.
            ("\x1b[20\x1b[20\x1b[201~1~1~", ""),
            // A start marker that rebuilds an end marker, and the reverse.
            ("\x1b[20\x1b[200~1~", ""),
            ("\x1b[20\x1b[201~0~", ""),
            // Split across the ESC and the CSI.
            ("\x1b\x1b[201~[201~", ""),
            ("\x1b[\x1b[200~201~", ""),
            // Parts of a marker that are not a marker stay.
            ("\x1b[20", "\x1b[20"),
            ("1~ \x1b[201", "1~ \x1b[201"),
            // UTF-8 around and between the parts of a marker.
            ("é\x1b[20\x1b[201~1~ü", "éü"),
            ("日\x1b[20\x1b[201~本1~語", "日\x1b[20本1~語"),
            ("\x1b[2\u{00A0}01~", "\x1b[2\u{00A0}01~"),
        ];
        for (text, want) in cases {
            let got = strip_paste_markers(text);
            assert_eq!(got, want, "{text:?}");
            assert!(!got.contains(PASTE_START) && !got.contains(PASTE_END));
        }
        assert!(matches!(
            strip_paste_markers("no marker"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// TER-1289: deep nesting is one linear scan. 100 000 levels (about
    /// 800 KB) leave nothing. A strip that loops over the whole text for
    /// each level would make about 10^11 byte steps here.
    #[test]
    fn deeply_nested_paste_markers_strip_in_one_scan() {
        let depth = 100_000;
        let text = format!(
            "{}{PASTE_END}{}",
            "\x1b[20".repeat(depth),
            "1~".repeat(depth)
        );
        assert_eq!(strip_paste_markers(&format!("a{text}b")), "ab");
    }

    #[test]
    fn exit_key_writes_split_the_cr_only_with_a_pause() {
        let pause = Duration::from_millis(300);
        assert_eq!(
            exit_key_writes(b"/exit\r", Duration::ZERO),
            vec![b"/exit\r".to_vec()]
        );
        assert_eq!(
            exit_key_writes(b"/exit\r", pause),
            vec![b"/exit".to_vec(), b"\r".to_vec()]
        );
        assert_eq!(exit_key_writes(b"\x04", pause), vec![b"\x04".to_vec()]);
        assert_eq!(exit_key_writes(b"\r", pause), vec![b"\r".to_vec()]);
        assert!(exit_key_writes(b"", pause).is_empty());
    }

    #[test]
    fn version_output_parses() {
        assert_eq!(
            parse_version_output("2.1.310 (Claude Code)\n").expect("claude"),
            Version::new(2, 1, 310)
        );
        assert_eq!(
            parse_version_output("codex-cli 0.160.0").expect("codex"),
            Version::new(0, 160, 0)
        );
        assert_eq!(
            parse_version_output("GitHub Copilot CLI 1.0.103.\nRun 'copilot update'.")
                .expect("copilot"),
            Version::new(1, 0, 103)
        );
        assert!(parse_version_output("nothing here").is_err());
    }

    // The trait must stay object safe.
    #[allow(dead_code)]
    fn assert_object_safe(_: &dyn Adapter) {}
}
