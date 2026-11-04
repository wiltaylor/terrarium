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
    bracketed_paste(&escape(&clean))
}

/// The turn as plain text (D3): Claude Code runs a text that starts with `/` as a slash command (`/plan`
/// enters plan mode) and a text that starts with `!` in bash mode. A turn that starts with one of them
/// (after white space) gets [`terrarium_core::adapter::TURN_ESCAPE`] in
/// front, so it stays a prompt for the model. A leader runs a command on
/// purpose with a raw send (`terrarium agent send --raw`).
pub fn escape(text: &str) -> std::borrow::Cow<'_, str> {
    terrarium_core::adapter::escape_turn(text, terrarium_core::adapter::COMMAND_PREFIXES, &[])
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

/// Empty the input box: Ctrl-U. Verified on 2.1.310 inside bwrap
/// (`docs/compat.md`, "Claude inside bwrap"): "draft text" in the box went
/// away, the prompt read as ready again, and the agent stayed up. Ctrl-C
/// is not used: on an empty box it arms "Press Ctrl-C again to exit".
pub const CLEAR_INPUT: &[u8] = &[0x15];

/// The keys for a clean exit: `/exit` then CR. The process exits in about
/// 0.5 s with `SessionEnd.reason` = `prompt_input_exit`.
pub fn exit() -> Vec<u8> {
    b"/exit\r".to_vec()
}

/// The keys that interrupt a running turn: one Esc. The footer of a busy
/// turn says "esc to interrupt" (`crate::screen::BUSY_MARKER`). One Esc
/// only: a second Esc on an idle prompt opens the rewind menu. Ctrl-C is
/// not used: a second Ctrl-C exits Claude Code.
pub fn interrupt() -> Vec<Vec<u8>> {
    vec![ESC.to_vec()]
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
        assert_eq!(CLEAR_INPUT, b"\x15");
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

    #[test]
    fn interrupt_is_esc_and_never_ctrl_c() {
        let writes = interrupt();
        assert_eq!(writes.len(), 1);
        assert!(writes.iter().all(|w| w == b"\x1b"));
    }
}
