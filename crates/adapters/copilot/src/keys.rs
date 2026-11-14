//! Keystrokes for the Copilot CLI TUI (`docs/compat.md`, "Typing into the
//! TUI").

use terrarium_core::Answer;
use terrarium_core::adapter::{bracketed_paste, strip_paste_markers};

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

/// Submit a turn: bracketed paste, then CR (a bare LF adds a new line and
/// does not submit). Newlines in the paste stay. The screen draws the
/// paste late (more than 0.2 s), but a CR right after a short paste still
/// submitted the full text. The runner writes the CR on its own,
/// [`SUBMIT_PAUSE`] after the paste.
///
/// The paste markers are removed from `text` first. A turn can carry mail,
/// which is untrusted input, and a stray end marker would end the paste
/// early and turn the rest of the text into keystrokes.
pub fn type_turn(text: &str) -> Vec<u8> {
    let clean = strip_paste_markers(text);
    bracketed_paste(&escape(&clean))
}

/// The pause between the paste of a turn and its CR (TER-208). The spikes
/// sent only short pastes. Claude Code loses a CR in the same write as a
/// long paste (TER-194), and Copilot draws a paste late, so the CR goes
/// in a later write. The runner presses CR again when the text stays in
/// the box ([`crate::screen::ScreenState::input_pending`]).
pub const SUBMIT_PAUSE: std::time::Duration = std::time::Duration::from_millis(300);

/// The turn as plain text (D3): Copilot runs a text that starts with `/` as a slash command (`/plan`,
/// `/autopilot`, `/delegate`), a text that starts with `!` as a shell
/// command, and opens a shell on a lone `$`. A turn that starts with one of them
/// (after white space) gets [`terrarium_core::adapter::TURN_ESCAPE`] in
/// front, so it stays a prompt for the model. A leader runs a command on
/// purpose with a raw send (`terrarium agent send --raw`).
pub fn escape(text: &str) -> std::borrow::Cow<'_, str> {
    terrarium_core::adapter::escape_turn(text, terrarium_core::adapter::COMMAND_PREFIXES, &["$"])
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

/// The keys that interrupt a running turn: Esc, then Esc again after
/// [`terrarium_core::adapter::INTERRUPT_GAP`]. The footer of a busy turn
/// says "esc esc interrupt" (`crate::screen::BUSY_MARKER`). Ctrl-C is not
/// used: it arms "ctrl+c again to exit".
pub fn interrupt() -> Vec<Vec<u8>> {
    vec![ESC.to_vec(), ESC.to_vec()]
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

    /// D3: a turn that starts with a command prefix stays plain text.
    #[test]
    fn command_prefixes_are_escaped() {
        for t in ["/plan now", "!ls", "  /exit"] {
            let typed = type_turn(t);
            let body = &typed[6..typed.len() - 7];
            let text = std::str::from_utf8(body).expect("utf8");
            assert!(text.starts_with('\u{00A0}'), "{t}: {text:?}");
            assert_eq!(&text['\u{00A0}'.len_utf8()..], t);
        }
        assert_eq!(
            type_turn("New mail"),
            b"\x1b[200~New mail\x1b[201~\r".to_vec()
        );
        assert_eq!(escape("a /b"), "a /b");
        assert_eq!(escape("$"), "\u{00A0}$");
        assert_eq!(escape("$x"), "$x");
    }

    #[test]
    fn interrupt_is_esc_and_never_ctrl_c() {
        let writes = interrupt();
        assert_eq!(writes.len(), 2);
        assert!(writes.iter().all(|w| w == b"\x1b"));
    }
}
