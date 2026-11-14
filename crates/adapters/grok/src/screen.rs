//! The screen detector, from the signed-in grok 1.0.77 capture (TER-1312,
//! `tests/fixtures/screens/grok-1.0.77.txt`).
//!
//! The idle screen ends with the input box and a key hint line:
//!
//! ```text
//!   ╭──────────────────────────────────────────────╮
//!   │ ❯                                            │
//!   ╰──────────────────────────── Grok 4.7 (high) ─╯
//!
//!   Shift+Tab:mode  │  Ctrl+x:shortcuts
//! ```
//!
//! # Busy
//!
//! While grok works, a status line above the box holds a braille spinner
//! and ends with `[stop]`: `⠋ Waiting for response… 0.0s … [stop]`, also
//! while a Ctrl+C cancel runs (`⠼ Cancelling… 0.1s … [stop]`). The
//! four-line data-retention notice ("Help improve Grok") can sit between
//! that line and the box. The key hint line under the box has
//! `Ctrl+c:cancel` while a turn runs (not while it is cancelled).
//!
//! # Questions
//!
//! - The permission question takes the place of the box. Its options are
//!   `Yes, and don't ask again for anything (always-approve mode)`,
//!   `Yes, proceed`, `No, reject (type to add feedback)` and `Never
//!   allow: <cmd>`, and its key hint line has `Ctrl+o:always-approve`.
//! - The folder trust question on the first start in a folder: "Do you
//!   trust the contents of this directory?", `Yes, proceed  y`,
//!   `No, quit  n`.
//!
//! Both take the place of the input box, so their markers count only on
//! a screen with no box.
//!
//! # Ready
//!
//! The box is found, it is empty, grok is not busy, no question shows,
//! and grok has not exited (`Resume this session with:`). Text in the box
//! (a typed turn, the `/model` picker) is not ready. A screen with no box
//! (the logo while grok starts) is not ready.

use terrarium_core::ScreenHint;
use terrarium_core::adapter::Screen;

/// Text that shows only on a question: the permission question and the
/// folder trust question.
pub const QUESTION_MARKERS: [&str; 3] = [
    "Ctrl+o:always-approve",
    "No, reject",
    "Do you trust the contents of this directory?",
];

/// The end of the busy status line above the box.
pub const BUSY_MARKER: &str = "[stop]";

/// The key hint under the box while a turn runs.
pub const BUSY_HINT: &str = "Ctrl+c:cancel";

/// The line that grok prints after it exits.
pub const EXIT_MARKER: &str = "Resume this session with:";

/// The prompt mark at the start of the box.
const PROMPT_MARK: char = '❯';

/// How many non-empty lines above the box are searched for the status
/// line: the line itself, the four-line data-retention notice, and one
/// spare.
const STATUS_LINES_ABOVE: usize = 6;

/// What the detector reads from one screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenState {
    pub input_box: bool,
    /// The text in the box, trimmed. `None` when there is no box.
    pub input_text: Option<String>,
    pub busy: bool,
    /// The question marker that was found.
    pub question: Option<&'static str>,
    /// grok has exited (the resume line shows).
    pub exited: bool,
}

impl ScreenState {
    /// The hint for the runner. A question wins over busy, and busy wins
    /// over ready.
    pub fn hint(&self) -> ScreenHint {
        if self.question.is_some() {
            ScreenHint::Question
        } else if self.busy {
            ScreenHint::Busy
        } else if self.ready() {
            ScreenHint::PromptReady
        } else {
            ScreenHint::NotReady
        }
    }

    /// The prompt-ready rule (see the module docs).
    pub fn ready(&self) -> bool {
        self.input_box
            && !self.busy
            && !self.exited
            && self.question.is_none()
            && self.input_text.as_deref() == Some("")
    }
}

fn is_spinner(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
}

/// The text of one box row: between the side bars, without the prompt
/// mark.
fn box_row_text(line: &str) -> &str {
    let t = line.trim();
    let t = t.strip_prefix('│').unwrap_or(t);
    let t = t.strip_suffix('│').unwrap_or(t);
    let t = t.trim();
    t.strip_prefix(PROMPT_MARK).unwrap_or(t).trim()
}

/// Run the detector over screen lines, top to bottom.
pub fn screen_state<S: AsRef<str>>(lines: &[S]) -> ScreenState {
    let lines: Vec<&str> = lines.iter().map(AsRef::as_ref).collect();
    let mut st = ScreenState {
        exited: lines.iter().any(|l| l.contains(EXIT_MARKER)),
        ..ScreenState::default()
    };
    let bottom = lines.iter().rposition(|l| l.trim_start().starts_with('╰'));
    let top = bottom.and_then(|b| {
        lines[..b]
            .iter()
            .rposition(|l| l.trim_start().starts_with('╭'))
    });
    if let (Some(a), Some(b)) = (top, bottom) {
        let inner = &lines[a + 1..b];
        if inner
            .first()
            .is_some_and(|l| l.trim_start().starts_with('│') && l.contains(PROMPT_MARK))
        {
            st.input_box = true;
            st.input_text = Some(
                inner
                    .iter()
                    .map(|l| box_row_text(l))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_owned(),
            );
        }
        let status = lines[..a]
            .iter()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(STATUS_LINES_ABOVE)
            .any(|l| {
                l.trim_end().ends_with(BUSY_MARKER)
                    || l.trim_start().chars().next().is_some_and(is_spinner)
            });
        st.busy = status || lines[b + 1..].iter().any(|l| l.contains(BUSY_HINT));
    }
    // Both questions take the place of the box, so a marker counts only
    // when no box shows: a reply that quotes "No, reject" is not one.
    if !st.input_box {
        st.question = QUESTION_MARKERS
            .into_iter()
            .find(|m| lines.iter().any(|l| l.contains(m)));
    }
    st
}

/// The rows of a parsed screen, as text.
pub fn screen_lines(screen: &Screen) -> Vec<String> {
    let (_, cols) = screen.size();
    screen.rows(0, cols).collect()
}

/// The hint for a parsed screen.
pub fn screen_hint(screen: &Screen) -> ScreenHint {
    screen_state(&screen_lines(screen)).hint()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let path = format!(
            "{}/tests/fixtures/screens/grok-1.0.77.txt",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).expect("fixture")
    }

    /// Split a `===== name =====` capture file into (name, lines).
    fn sections(text: &str) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for line in text.lines() {
            if let Some(name) = line
                .strip_prefix("===== ")
                .and_then(|l| l.strip_suffix(" ====="))
            {
                out.push((name.to_owned(), Vec::new()));
            } else if let Some(last) = out.last_mut() {
                last.1.push(line.to_owned());
            }
        }
        out
    }

    use ScreenHint::{Busy, NotReady, PromptReady, Question};

    /// Every captured screen and its hint, in capture order.
    const EXPECTED: &[(&str, ScreenHint)] = &[
        ("trust", Question),
        ("idle-start", PromptReady),
        ("typed", NotReady),
        ("busy-waiting", Busy),
        ("busy-tool", Busy),
        ("question", Question),
        ("busy-cancelling", Busy),
        ("idle-after-cancel", PromptReady),
        ("model-picker", NotReady),
        ("idle-resumed", PromptReady),
        ("busy-resumed", Busy),
        ("idle-after-resumed", PromptReady),
        ("exited", NotReady),
    ];

    #[test]
    fn every_captured_screen_has_its_hint() {
        let all = sections(&fixture());
        let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        let want: Vec<&str> = EXPECTED.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, want);
        for ((name, lines), (_, hint)) in all.iter().zip(EXPECTED) {
            assert_eq!(screen_state(lines).hint(), *hint, "{name}");
        }
    }

    #[test]
    fn the_typed_turn_is_the_box_text() {
        let all = sections(&fixture());
        let (_, typed) = all.iter().find(|(n, _)| n == "typed").expect("typed");
        assert_eq!(
            screen_state(typed).input_text.as_deref(),
            Some("Run the shell command: wc -c note.txt   and tell me the number.")
        );
    }

    #[test]
    fn a_parsed_screen_gives_the_same_hint() {
        let all = sections(&fixture());
        for (name, lines) in &all {
            let mut parser = vt100::Parser::new(40, 120, 0);
            parser.process(lines.join("\r\n").as_bytes());
            let want = EXPECTED.iter().find(|(n, _)| n == name).expect("name").1;
            assert_eq!(screen_hint(parser.screen()), want, "{name}");
        }
    }
}
