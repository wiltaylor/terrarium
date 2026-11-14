//! Keystrokes for the Grok Build TUI.
//!
//! Sources: docs.x.ai "Keyboard shortcuts" and "Modes and commands"
//! (res-1190 F2, F4), checked in the signed-in grok 1.0.77 TUI (TER-1312,
//! `tests/fixtures/screens/grok-1.0.77.txt`): a pasted turn and a CR in
//! its own write submit; one Ctrl+C cancels the running turn ("Turn
//! cancelled by user"); `/quit` then CR exits with code 0 and prints
//! `grok --resume <id>`.
//!
//! The permission question lists four options with the second selected:
//! `1 Yes, and don't ask again for anything (always-approve mode)`,
//! `2 Yes, proceed`, `3 No, reject`, `4 Never allow: <cmd>`. Its footer
//! says `Ctrl+o:always-approve` and `Esc:scrollback`: Esc does NOT deny.
//! No answer here moves up, so no answer can reach option 1, and no key
//! here is Ctrl+O.

use std::time::Duration;

use terrarium_core::Answer;
use terrarium_core::adapter::bracketed_paste;

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
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
    let clean = text.replace(PASTE_START, "").replace(PASTE_END, "");
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

/// Answer a question on the screen.
///
/// - `Choice(n)`: Down `n` times from the selected option, then CR. On
///   the permission question the selection starts on option 2, so
///   `Choice(0)` is `Yes, proceed` and option 1 is never reached.
/// - `Allow`: CR, which takes the selected option. On the permission
///   question that is `Yes, proceed`, a one-time allow.
/// - `Deny`: Down, then CR: `No, reject`, the option under the selected
///   one. Esc only goes to the scrollback.
pub fn answer(answer: &Answer) -> Vec<u8> {
    match answer {
        Answer::Choice(n) => {
            let mut out = DOWN.repeat(*n);
            out.extend_from_slice(ENTER);
            out
        }
        Answer::Allow => ENTER.to_vec(),
        Answer::Deny => {
            let mut out = DOWN.to_vec();
            out.extend_from_slice(ENTER);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(answer(&Answer::Deny), b"\x1b[B\r".to_vec());
        // No answer moves up (to `always-approve mode`) or sends Ctrl+O.
        for a in [Answer::Allow, Answer::Deny, Answer::Choice(3)] {
            let keys = answer(&a);
            assert!(!keys.windows(3).any(|w| w == b"\x1b[A"), "{a:?}");
            assert!(!keys.contains(&0x0f), "{a:?}");
        }
    }
}
