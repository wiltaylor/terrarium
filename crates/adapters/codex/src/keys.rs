//! Keystrokes for the Codex TUI (`docs/compat.md`, "Codex 0.160.0",
//! "Typing into the TUI").

use terrarium_core::Answer;
use terrarium_core::adapter::{CTRL_U, bracketed_paste};

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
/// Cursor down.
pub const DOWN: &[u8] = b"\x1b[B";
pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";
/// Backspace (DEL).
pub const BACKSPACE: u8 = 0x7f;
/// How many lines [`clear_input`] can empty.
pub const CLEAR_LINES: usize = 64;

/// Submit a turn: bracketed paste, then CR at once (verified). A bare LF
/// adds a new line and does not submit. Newlines inside the paste stay in
/// the prompt.
///
/// The paste markers are removed from `text` first. A turn can carry mail,
/// which is untrusted input, and a stray end marker would end the paste
/// early and turn the rest of the text into keystrokes.
pub fn type_turn(text: &str) -> Vec<u8> {
    let clean = text.replace(PASTE_START, "").replace(PASTE_END, "");
    bracketed_paste(&escape(&clean))
}

/// The turn as plain text (D3): Codex runs a text that starts with `/` as a slash command (`/plan`
/// enters Plan mode) and a text that starts with `!` as a shell command. A turn that starts with one of them
/// (after white space) gets [`terrarium_core::adapter::TURN_ESCAPE`] in
/// front, so it stays a prompt for the model. A leader runs a command on
/// purpose with a raw send (`terrarium agent send --raw`).
pub fn escape(text: &str) -> std::borrow::Cow<'_, str> {
    terrarium_core::adapter::escape_turn(text, terrarium_core::adapter::COMMAND_PREFIXES, &[])
}

/// Answer a dialog on the screen. Not tested on Codex: in yolo mode with
/// the launch flags, the spikes saw no dialog after the start. The keys
/// follow the dialogs that the spikes captured: a numbered list with the
/// `›` cursor on option 1, and an "enter … · esc …" hint.
///
/// - `Choice(n)`: Down `n` times, then CR.
/// - `Allow`: CR (option 1).
/// - `Deny`: Esc. On the start-up trust dialog, Esc is "quit".
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

/// Empty the composer before a retry: Ctrl-U (kill to the start of the
/// line) and Backspace (join the line above), [`CLEAR_LINES`] times. One
/// Ctrl-U empties only the current line of a turn with many lines. On an
/// empty composer both keys do nothing.
///
/// Not tested on Codex (`docs/compat.md` has no clear-key spike for it).
/// Esc and Ctrl-C are not used: on an empty composer, Esc can start the
/// edit of an earlier prompt, and Ctrl-C can arm the exit.
pub fn clear_input() -> Vec<u8> {
    [CTRL_U, BACKSPACE].repeat(CLEAR_LINES)
}

/// The keys for a clean exit: `/exit` then CR. The process exits after
/// 0.05 to 5.1 s, with `SessionEnd.reason` = `other`. The CR must come
/// [`EXIT_PAUSE`] after the text: in one write, Codex keeps `/exit` in the
/// composer and does not submit it.
pub fn exit() -> Vec<u8> {
    b"/exit\r".to_vec()
}

/// The pause between `/exit` and its CR. Every Codex spike waited 0.3 s
/// (`spikes/codex/*/run.py`).
pub const EXIT_PAUSE: std::time::Duration = std::time::Duration::from_millis(300);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_is_paste_then_cr() {
        assert_eq!(
            type_turn("First line.\nSecond line."),
            b"\x1b[200~First line.\nSecond line.\x1b[201~\r".to_vec()
        );
        assert_eq!(
            type_turn("a\x1b[201~/exit\rb"),
            b"\x1b[200~a/exit\rb\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn answers_and_clear() {
        assert_eq!(answer(&Answer::Choice(0)), b"\r".to_vec());
        assert_eq!(answer(&Answer::Choice(2)), b"\x1b[B\x1b[B\r".to_vec());
        assert_eq!(answer(&Answer::Allow), b"\r".to_vec());
        assert_eq!(answer(&Answer::Deny), b"\x1b".to_vec());
        let clear = clear_input();
        assert_eq!(clear.len(), 2 * CLEAR_LINES);
        assert_eq!(&clear[..2], &[0x15, 0x7f]);
        assert!(!clear.contains(&0x1b) && !clear.contains(&0x03));
        assert_eq!(exit(), b"/exit\r".to_vec());
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
    }
}
