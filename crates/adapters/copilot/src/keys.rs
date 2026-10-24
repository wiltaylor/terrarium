//! Keystrokes for the Copilot CLI TUI (`docs/compat.md`, "Typing into the
//! TUI").

use terrarium_core::Answer;
use terrarium_core::adapter::bracketed_paste;

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
/// Cursor down.
pub const DOWN: &[u8] = b"\x1b[B";
pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";
/// Ctrl-U: delete from the cursor to the start of the line.
pub const CTRL_U: u8 = 0x15;
/// Backspace (DEL).
pub const BACKSPACE: u8 = 0x7f;
/// The number of lines that [`clear_input`] removes.
pub const CLEAR_LINES: usize = 256;

/// Submit a turn: bracketed paste, then CR at once (verified; a bare LF
/// adds a new line and does not submit). Newlines in the paste stay. The
/// screen draws the paste late (more than 0.2 s), but the CR right after
/// it still submits the full text.
///
/// The paste markers are removed from `text` first. A turn can carry mail,
/// which is untrusted input, and a stray end marker would end the paste
/// early and turn the rest of the text into keystrokes.
pub fn type_turn(text: &str) -> Vec<u8> {
    let clean = text.replace(PASTE_START, "").replace(PASTE_END, "");
    bracketed_paste(&clean)
}

/// Answer the `ask_user` question ("↑/↓ select · enter accept · ctrl+d
/// decline · esc cancel"; the cursor starts on the first choice).
///
/// - `Choice(n)`: Down `n` times, then CR. Verified for `n` = 1.
/// - `Allow`: CR, which accepts the first choice. Not tested.
/// - `Deny`: Esc ("esc cancel"). Not tested.
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

/// Empty the input box: Ctrl-U then Backspace, [`CLEAR_LINES`] times.
///
/// Verified in a probe on 2026-10-24 (1.0.103, no turn sent): Ctrl-U
/// empties only the line of the cursor; Backspace then joins the line
/// above. After a paste of three or four lines (cursor at the end) the
/// pairs emptied the box, and the screen read as ready. On an empty box
/// both keys do nothing. If the cursor was moved up, the lines below it
/// stay. Ctrl-C is not used: it empties the box, but arms "ctrl+c again to
/// exit", and on a running turn it would interrupt.
pub fn clear_input() -> Vec<u8> {
    [CTRL_U, BACKSPACE].repeat(CLEAR_LINES)
}

/// The keys for a clean exit: `/exit` then CR. Copilot exits in 1.0 to
/// 1.5 s with `SessionEnd.reason` = `user_exit`.
pub fn exit() -> Vec<u8> {
    b"/exit\r".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_is_paste_then_cr() {
        assert_eq!(
            type_turn("First line.\nSecond line."),
            b"\x1b[200~First line.\nSecond line.\x1b[201~\r".to_vec()
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

    #[test]
    fn clear_is_kill_and_backspace_pairs() {
        let keys = clear_input();
        assert_eq!(keys.len(), 2 * CLEAR_LINES);
        assert!(keys.chunks(2).all(|c| c == [0x15, 0x7f]));
        // No key that interrupts, exits, or submits.
        assert!(
            !keys
                .iter()
                .any(|b| matches!(b, 0x03 | 0x04 | b'\r' | b'\n' | 0x1b))
        );
    }
}
