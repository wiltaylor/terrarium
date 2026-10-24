//! Keystrokes for the Claude Code TUI (`docs/compat.md`, "Typing into the
//! TUI").

use terrarium_core::Answer;
use terrarium_core::adapter::bracketed_paste;

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
/// Cursor down.
pub const DOWN: &[u8] = b"\x1b[B";
pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";

/// Submit a turn: bracketed paste, then CR (not LF; a bare LF adds a new
/// line and does not submit). Newlines inside the paste stay in the prompt.
///
/// The paste markers are removed from `text` first. A turn can carry mail,
/// which is untrusted input, and a stray end marker would end the paste
/// early and turn the rest of the text into keystrokes.
pub fn type_turn(text: &str) -> Vec<u8> {
    let clean = text.replace(PASTE_START, "").replace(PASTE_END, "");
    bracketed_paste(&clean)
}

/// Answer a question on the screen.
///
/// - `Choice(n)`: the cursor starts on option 1. Send Down `n` times, then
///   CR. Verified for `AskUserQuestion` (spike 5).
/// - `Allow`: CR. A permission prompt has "Yes" as option 1. Not tested: in
///   bypass mode, only `AskUserQuestion` asks.
/// - `Deny`: Esc, which the key hint names "Esc to cancel".
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

/// The keys for a clean exit: `/exit` then CR. The process exits in about
/// 0.5 s with `SessionEnd.reason` = `prompt_input_exit`.
pub fn exit() -> Vec<u8> {
    b"/exit\r".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_is_paste_then_cr() {
        assert_eq!(
            type_turn("line one\nline two"),
            b"\x1b[200~line one\nline two\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn paste_markers_in_text_are_removed() {
        assert_eq!(
            type_turn("a\x1b[201~/exit\rb"),
            b"\x1b[200~a/exit\rb\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn answers() {
        assert_eq!(answer(&Answer::Choice(0)), b"\r".to_vec());
        assert_eq!(answer(&Answer::Choice(1)), b"\x1b[B\r".to_vec());
        assert_eq!(answer(&Answer::Choice(2)), b"\x1b[B\x1b[B\r".to_vec());
        assert_eq!(answer(&Answer::Allow), b"\r".to_vec());
        assert_eq!(answer(&Answer::Deny), b"\x1b".to_vec());
        assert_eq!(exit(), b"/exit\r".to_vec());
    }
}
