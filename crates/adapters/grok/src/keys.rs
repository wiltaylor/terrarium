//! Keystrokes for the Grok Build TUI.
//!
//! Sources: docs.x.ai "Keyboard shortcuts" and "Modes and commands"
//! (res-1190 F2, F4). Enter submits; a busy Enter queues or steers.
//! Ctrl+C cancels the running turn, but first clears a focused input that
//! holds text. Esc does NOT cancel. `/quit` or `/exit` quits; Ctrl+Q and
//! Ctrl+D quit only on a second press. None of it is checked in a
//! signed-in TUI yet (TER-1192 release gate).

use std::time::Duration;

use terrarium_core::Answer;
use terrarium_core::adapter::{bracketed_paste, strip_paste_markers};

/// Cursor down.
pub const DOWN: &[u8] = b"\x1b[B";
pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";
/// Ctrl-C: cancel the running turn.
pub const CTRL_C: u8 = 0x03;

/// The pause between the paste of a turn and its CR, and between `/quit`
/// and its CR. Claude Code and Codex drop a CR that comes in the same
/// burst as a paste (TER-194, TER-208). Grok's handling is not captured,
/// so the CR goes in its own write.
pub const SUBMIT_PAUSE: Duration = Duration::from_millis(300);

/// The turn as plain text (D3): a text that starts with `/` is a slash
/// command, and `!` a shell command, in the harness TUIs. Such a turn gets
/// [`terrarium_core::adapter::TURN_ESCAPE`] in front.
pub fn escape(text: &str) -> std::borrow::Cow<'_, str> {
    terrarium_core::adapter::escape_turn(text, terrarium_core::adapter::COMMAND_PREFIXES, &[])
}

/// Submit a turn: bracketed paste, then CR. The paste markers are removed
/// from `text` first: a turn can carry mail, which is untrusted input, and
/// a stray end marker would turn the rest of the text into keystrokes.
pub fn type_turn(text: &str) -> Vec<u8> {
    let clean = strip_paste_markers(text);
    bracketed_paste(&escape(&clean))
}

/// Interrupt a turn: one Ctrl-C. Never two: a second press can quit.
pub fn interrupt() -> Vec<Vec<u8>> {
    vec![vec![CTRL_C]]
}

/// Quit an idle TUI: `/quit`, then CR.
pub fn exit() -> Vec<u8> {
    b"/quit\r".to_vec()
}

/// Answer a question on the screen. Not captured: the usual menu keys.
///
/// - `Choice(n)`: Down `n` times, then CR.
/// - `Allow`: CR, which takes the first (selected) option.
/// - `Deny`: Esc.
pub fn answer(answer: &Answer) -> Vec<u8> {
    match answer {
        Answer::Choice(n) => {
            let mut out = DOWN.repeat(*n);
            out.extend_from_slice(ENTER);
            out
        }
        Answer::Allow => ENTER.to_vec(),
        Answer::Deny => ESC.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TER-1289: nested and split markers cannot rebuild an end marker.
    /// The typed bytes hold exactly one start and one end marker, at the
    /// ends of the paste.
    #[test]
    fn nested_and_split_paste_markers_are_removed() {
        for (text, want) in [
            ("a\x1b[20\x1b[201~1~b", "ab"),
            ("a\x1b[20\x1b[20\x1b[201~1~1~b", "ab"),
            ("a\x1b[20\x1b[200~1~b", "ab"),
            ("a\x1b\x1b[201~[201~b", "ab"),
            ("a\x1b[\x1b[200~201~b", "ab"),
            // UTF-8 around and between the parts of a marker.
            ("é\x1b[20\x1b[201~1~ü", "éü"),
            ("日\x1b[20\x1b[201~本1~語", "日\x1b[20本1~語"),
        ] {
            let typed = type_turn(text);
            let mut expected = b"\x1b[200~".to_vec();
            expected.extend_from_slice(want.as_bytes());
            expected.extend_from_slice(b"\x1b[201~\r");
            assert_eq!(typed, expected, "{text:?}");
        }
    }

    #[test]
    fn a_turn_is_a_clean_bracketed_paste() {
        assert_eq!(type_turn("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
        // A stray end marker in mail cannot end the paste early.
        assert_eq!(
            type_turn("a\x1b[201~/quit\r"),
            b"\x1b[200~a/quit\r\x1b[201~\r".to_vec()
        );
        // A slash command stays text.
        assert_eq!(
            type_turn("/quit"),
            "\x1b[200~\u{a0}/quit\x1b[201~\r".as_bytes().to_vec()
        );
    }

    #[test]
    fn the_control_keys() {
        assert_eq!(interrupt(), vec![vec![0x03]]);
        assert_eq!(exit(), b"/quit\r".to_vec());
        assert_eq!(answer(&Answer::Choice(2)), b"\x1b[B\x1b[B\r".to_vec());
        assert_eq!(answer(&Answer::Allow), b"\r".to_vec());
        assert_eq!(answer(&Answer::Deny), b"\x1b".to_vec());
    }
}
