//! Keystrokes for the pi TUI (`docs/compat.md`, "pi 0.85.1").

use terrarium_core::Answer;
use terrarium_core::adapter::bracketed_paste;

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
/// Cursor down.
pub const DOWN: &[u8] = b"\x1b[B";
pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";

/// Submit a turn: bracketed paste, then CR. Verified: newlines in the
/// paste stay in the prompt; a bare LF adds a line and does not submit.
/// Text sent while a turn runs steers that turn (one more `input`).
///
/// The paste markers are removed from `text` first. A turn can carry mail,
/// which is untrusted input, and a stray end marker would end the paste
/// early and turn the rest of the text into keystrokes.
pub fn type_turn(text: &str) -> Vec<u8> {
    let clean = text.replace(PASTE_START, "").replace(PASTE_END, "");
    bracketed_paste(&escape(&clean))
}

/// The turn as plain text (D3): pi trims the text, then runs `!cmd` as a user shell command and an
/// exact `/quit`, `/new`, or `/login` as a command
/// (`interactive-mode.js`, `setupEditorSubmitHandler`). A turn that starts with one of them
/// (after white space) gets [`terrarium_core::adapter::TURN_ESCAPE`] in
/// front, so it stays a prompt for the model. A leader runs a command on
/// purpose with a raw send (`terrarium agent send --raw`).
pub fn escape(text: &str) -> std::borrow::Cow<'_, str> {
    terrarium_core::adapter::escape_turn(text, terrarium_core::adapter::COMMAND_PREFIXES, &[])
}

/// Answer an extension UI prompt (`ui_prompt_start`).
///
/// - `Choice(n)`: the cursor (`→`) starts on the first option. Down `n`
///   times, then CR. Verified on a `ctx.ui.select()` prompt: Down, CR
///   picked the second option.
/// - `Allow`: CR. A `ctx.ui.confirm()` prompt lists "Yes" first. Verified:
///   CR gave `true`.
/// - `Deny`: Esc. The key hint says "escape/ctrl+c cancel". Not run.
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

/// Empty the input box: Ctrl-C. Verified: Ctrl-C clears the whole editor
/// (also a multi-line paste) and pi stays up. Ctrl-U deletes only to the
/// start of the current line, which leaves the other lines of a multi-line
/// turn. pi does not interrupt a run on Ctrl-C (Esc does that).
///
/// Do not send this twice within 0.5 s: a second Ctrl-C within 500 ms
/// exits pi (read in the installed source, `handleCtrlC`).
pub const CLEAR_INPUT: &[u8] = &[0x03];

/// The keys for a clean exit: `/quit`, then CR. Verified: pi exits in
/// 0.1 s, and `session_shutdown.reason` = `quit`. Send them only when the
/// input box is empty. Ctrl-D also exits, but only on an empty editor.
/// Ctrl-C does not exit.
pub const EXIT: &[u8] = b"/quit\r";

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
            type_turn("a\x1b[201~/quit\rb"),
            b"\x1b[200~a/quit\rb\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn answers_and_keys() {
        assert_eq!(answer(&Answer::Choice(0)), b"\r".to_vec());
        assert_eq!(answer(&Answer::Choice(1)), b"\x1b[B\r".to_vec());
        assert_eq!(answer(&Answer::Allow), b"\r".to_vec());
        assert_eq!(answer(&Answer::Deny), b"\x1b".to_vec());
        assert_eq!(CLEAR_INPUT, b"\x03");
        assert_eq!(EXIT, b"/quit\r");
    }

    /// D3: a turn that starts with a command prefix stays plain text.
    #[test]
    fn command_prefixes_are_escaped() {
        for t in ["/plan now", "!ls", "  /exit"] {
            let typed = type_turn(t);
            let body = &typed[6..typed.len() - 7];
            let text = std::str::from_utf8(body).expect("utf8");
            assert!(text.starts_with('\u{200B}'), "{t}: {text:?}");
            assert_eq!(&text['\u{200B}'.len_utf8()..], t);
        }
        assert_eq!(
            type_turn("New mail"),
            b"\x1b[200~New mail\x1b[201~\r".to_vec()
        );
        assert_eq!(escape("a /b"), "a /b");
    }
}
