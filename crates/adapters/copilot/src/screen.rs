//! The screen detector: a port of `screen_state()` from
//! `spikes/copilot/lib/copilotlib.py` (`docs/compat.md`, "Typing into the
//! TUI").
//!
//! The idle screen ends with the input box and a footer:
//!
//! ```text
//!  /path [⎇ main]
//! ────────────────────────────────────────────────
//! ❯
//! ────────────────────────────────────────────────
//!  ← open sidebar · Interactive · Allow All · / commands · ? help · tab next tab     Claude Haiku 4.5
//! ```
//!
//! While a turn runs, the footer is ` ◉ Working esc esc edit prompt` or
//! ` ◎ Working · 256 B esc esc interrupt · ctrl+q enqueue`.
//!
//! # The rules
//!
//! - The input box: the last two rule lines (20 or more `─`) with a line
//!   that starts with `❯` between them. The text after `❯` is the input.
//!   Dim cells (a placeholder) do not count as input.
//! - Busy: a footer line below the box starts with `◉`/`◎`/`○`/`●` and
//!   then `Working`, or has "esc esc interrupt". (The spike also read the
//!   three lines above the box. Every capture has the marker in the footer,
//!   and above the box a reply bullet such as "● Working on it" would read
//!   as busy for good, so the port reads the footer only.)
//! - Dialog: one of the last six non-empty lines has a key hint:
//!   "↑/↓ (to) navigate/select" or "enter (to) select/accept/confirm". The
//!   trust dialog, the model picker, and the `ask_user` question all have
//!   one.
//! - Ready: an input box, empty, not busy, and no dialog.
//!
//! Text typed while busy steers the running turn and fires no prompt
//! hook, so the runner must never type into a busy screen. Busy is
//! reliable once Copilot draws it: every capture with the "Working" footer
//! reads as busy. One gap stays: Copilot draws a turn more than 0.2 s after
//! the CR, so a screen right after the CR still reads as ready (the
//! `working-0` and `lag-` captures). The prompt hook comes 0.07 to 0.95 s
//! after the CR.

use terrarium_core::ScreenHint;
use terrarium_core::adapter::Screen;

/// Key hints of dialogs, in lower case. A line matches when its lower-case
/// text contains one.
///
/// The last five come from `docs/research/harness-hardening.md` (Copilot
/// CLI, 1a and 5), read in the 1.0.103 bundle, not captured (B2): the tool
/// approval ("Approve once / Approve for this session"), the auto-mode
/// switch on a rate limit ("Needs approval"), the folder trust, and the
/// decline key of a form.
pub const DIALOG_MARKERS: [&str; 15] = [
    "↑/↓ to navigate",
    "↑/↓ navigate",
    "↑/↓ to select",
    "↑/↓ select",
    "enter to select",
    "enter select",
    "enter to accept",
    "enter accept",
    "enter to confirm",
    "enter confirm",
    "approve once",
    "approve for this session",
    "needs approval",
    "confirm folder trust",
    "ctrl+d decline",
];

/// The footer text that shows while a turn runs and a tool is active.
pub const BUSY_MARKER: &str = "esc esc interrupt";
/// The word after the spinner while a turn runs.
pub const WORKING: &str = "Working";
/// The spinner glyphs before "Working".
pub const SPINNERS: [char; 4] = ['◉', '◎', '○', '●'];
/// The number of last non-empty lines that the dialog check reads.
pub const DIALOG_TAIL: usize = 6;

const PROMPT_MARK: char = '❯';

/// What the detector reads from one screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenState {
    pub input_box: bool,
    /// The text in the input box, trimmed, without dim placeholder text.
    /// `None` when there is no box.
    pub input_text: Option<String>,
    pub busy: bool,
    /// The dialog key hint that was found.
    pub dialog: Option<&'static str>,
    /// The non-empty lines below the box, joined with `\n`.
    pub footer: Option<String>,
    /// The prompt-ready rule (see the module docs).
    pub ready: bool,
}

impl ScreenState {
    /// The hint for the runner. A dialog wins over busy, and busy wins over
    /// ready.
    pub fn hint(&self) -> ScreenHint {
        if self.dialog.is_some() {
            ScreenHint::Question
        } else if self.busy {
            ScreenHint::Busy
        } else if self.ready {
            ScreenHint::PromptReady
        } else {
            ScreenHint::NotReady
        }
    }
}

impl ScreenState {
    /// The input box holds text that a CR would submit: not busy, and no
    /// dialog. Dim placeholder text does not count when the state comes
    /// from [`state_of`].
    pub fn input_pending(&self) -> bool {
        self.input_box
            && !self.busy
            && self.dialog.is_none()
            && self.input_text.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// [`ScreenState::input_pending`] for a parsed screen.
pub fn input_pending(screen: &Screen) -> bool {
    state_of(screen).input_pending()
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 20 && t.chars().all(|c| c == '─')
}

/// True when `line` says that a turn runs.
pub fn is_busy_line(line: &str) -> bool {
    if line.contains(BUSY_MARKER) {
        return true;
    }
    let t = line.trim_start();
    let mut chars = t.chars();
    match chars.next() {
        Some(c) if SPINNERS.contains(&c) => {
            let rest = chars.as_str();
            rest.starts_with(char::is_whitespace) && rest.trim_start().starts_with(WORKING)
        }
        _ => false,
    }
}

/// The dialog key hint in `line`, if it has one.
pub fn dialog_marker(line: &str) -> Option<&'static str> {
    let lower = line.to_lowercase();
    DIALOG_MARKERS.into_iter().find(|m| lower.contains(m))
}

/// Run the detector over screen lines, top to bottom. `dim` is the same
/// screen with dim cells blanked; when given, the input text comes from it.
pub fn screen_state<S: AsRef<str>>(lines: &[S], dim: Option<&[S]>) -> ScreenState {
    let lines: Vec<&str> = lines.iter().map(AsRef::as_ref).collect();
    let dim: Option<Vec<&str>> = dim.map(|d| d.iter().map(AsRef::as_ref).collect());
    let mut st = ScreenState::default();

    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = &non_empty[non_empty.len().saturating_sub(DIALOG_TAIL)..];
    st.dialog = tail.iter().rev().find_map(|l| dialog_marker(l));

    let rules: Vec<usize> = (0..lines.len()).filter(|&i| is_rule(lines[i])).collect();
    for pair in rules.windows(2).rev() {
        let (a, b) = (pair[0], pair[1]);
        let inner = &lines[a + 1..b];
        let Some(first) = inner.first() else { continue };
        if !first.trim_start().starts_with(PROMPT_MARK) {
            continue;
        }
        st.input_box = true;
        let source: Vec<&str> = match &dim {
            Some(d) if d.len() >= b => d[a + 1..b].to_vec(),
            _ => inner.to_vec(),
        };
        let joined = source.join("\n");
        let text = match joined.find(PROMPT_MARK) {
            Some(i) => &joined[i + PROMPT_MARK.len_utf8()..],
            None => joined.as_str(),
        };
        st.input_text = Some(text.trim().to_owned());
        let below: Vec<&str> = lines[b + 1..]
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect();
        st.busy = below.iter().any(|l| is_busy_line(l));
        st.footer = Some(below.join("\n"));
        break;
    }

    st.ready =
        st.input_box && !st.busy && st.dialog.is_none() && st.input_text.as_deref() == Some("");
    st
}

/// The rows of a parsed screen, as text.
pub fn screen_lines(screen: &Screen) -> Vec<String> {
    let (_, cols) = screen.size();
    screen.rows(0, cols).collect()
}

/// The rows of a parsed screen with dim cells as spaces.
pub fn screen_lines_undimmed(screen: &Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    (0..rows)
        .map(|r| {
            let mut line = String::new();
            for c in 0..cols {
                let Some(cell) = screen.cell(r, c) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.dim() || !cell.has_contents() {
                    line.push(' ');
                } else {
                    line.push_str(cell.contents());
                }
            }
            line.trim_end().to_owned()
        })
        .collect()
}

/// The detector state for a parsed screen.
pub fn state_of(screen: &Screen) -> ScreenState {
    let lines = screen_lines(screen);
    let dim = screen_lines_undimmed(screen);
    screen_state(&lines, Some(&dim))
}

/// The hint for a parsed screen.
pub fn screen_hint(screen: &Screen) -> ScreenHint {
    state_of(screen).hint()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/screens/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).expect("fixture")
    }

    /// Split a `===== name (t=…) =====` capture file into (name, lines).
    pub(crate) fn sections(text: &str) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for line in text.lines() {
            if let Some(head) = line
                .strip_prefix("===== ")
                .and_then(|l| l.strip_suffix(" ====="))
            {
                let name = head.split(" (t=").next().unwrap_or(head);
                out.push((name.to_owned(), Vec::new()));
            } else if let Some(last) = out.last_mut() {
                last.1.push(line.to_owned());
            }
        }
        out
    }

    /// The hint that each captured screen must give, by label. The labels
    /// come from the spikes: `idle-` ready, `busy-`/`working-` busy,
    /// `typed-` text in the box, `dialog-` a question, `lag-` (the paste is
    /// not drawn yet) ready.
    const EXPECTED: &[(&str, ScreenHint)] = &[
        // typing-into-tui
        ("idle-start", ScreenHint::PromptReady),
        ("lag-paste-not-drawn-at-0.2s", ScreenHint::PromptReady),
        ("busy-T1", ScreenHint::Busy),
        ("idle-after-T1", ScreenHint::PromptReady),
        ("typed-after-LF", ScreenHint::NotReady),
        ("busy-T3-tool", ScreenHint::Busy),
        ("busy-typed-T4", ScreenHint::Busy),
        ("busy-after-T4-enter", ScreenHint::Busy),
        ("idle-after-T3-T4", ScreenHint::PromptReady),
        ("dialog-ask-user", ScreenHint::Question),
        ("dialog-ask-user-down", ScreenHint::Question),
        ("idle-after-T5", ScreenHint::PromptReady),
        // hooks-in-interactive. `working-0` was taken right after the CR,
        // before Copilot drew the turn: the screen is still the idle one.
        ("idle-before-turn", ScreenHint::PromptReady),
        ("working-0", ScreenHint::PromptReady),
        ("working-6", ScreenHint::Busy),
        ("working-12", ScreenHint::Busy),
        ("idle-after-stop", ScreenHint::PromptReady),
        ("after-idle-wait", ScreenHint::PromptReady),
        ("after-exit", ScreenHint::NotReady),
        // session-id-and-resume
        ("A-after-exit", ScreenHint::NotReady),
        ("B-resumed-idle", ScreenHint::PromptReady),
        ("B-after-exit", ScreenHint::NotReady),
        ("C-session-id-again", ScreenHint::PromptReady),
        ("E-bogus-resume", ScreenHint::NotReady),
        ("F-resume-other-cwd", ScreenHint::PromptReady),
        ("D-first", ScreenHint::PromptReady),
        ("D-second", ScreenHint::PromptReady),
    ];

    fn expected(name: &str) -> ScreenHint {
        EXPECTED
            .iter()
            .find(|e| e.0 == name)
            .unwrap_or_else(|| panic!("no expectation for {name}"))
            .1
    }

    #[test]
    fn captured_tui_screens_match_the_spike() {
        let text = sections(&fixture("typing.txt"));
        let dim = sections(&fixture("typing-dim.txt"));
        assert_eq!(text.len(), dim.len());
        let mut n = 0;
        for ((name, lines), (dname, dlines)) in text.iter().zip(&dim) {
            assert_eq!(name, dname);
            let st = screen_state(lines, Some(dlines.as_slice()));
            assert_eq!(st.hint(), expected(name), "{name}: {st:?}");
            // The text alone gives the same answer.
            assert_eq!(
                screen_state::<String>(lines, None).hint(),
                st.hint(),
                "{name}"
            );
            n += 1;
        }
        for file in ["hooks.txt", "resume.txt"] {
            for (name, lines) in sections(&fixture(file)) {
                let st = screen_state::<String>(&lines, None);
                assert_eq!(st.hint(), expected(&name), "{file} {name}: {st:?}");
                n += 1;
            }
        }
        assert_eq!(n, EXPECTED.len());
    }

    #[test]
    fn typed_text_shows_in_the_state() {
        let text = sections(&fixture("typing.txt"));
        let find = |n: &str| {
            let (_, lines) = text.iter().find(|(x, _)| x == n).expect(n);
            screen_state::<String>(lines, None)
        };
        assert_eq!(
            find("typed-after-LF").input_text.as_deref(),
            Some("Reply with exactly:")
        );
        let st = find("busy-typed-T4");
        assert!(st.busy && !st.ready);
        assert!(st.input_text.expect("text").contains("queued"));
        let st = find("dialog-ask-user");
        assert_eq!(st.dialog, Some("↑/↓ select"));
        assert!(!st.input_box);
    }

    /// TER-208: typed text that did not submit is pending input. An empty
    /// box, a busy screen, and a dialog are not.
    #[test]
    fn typed_text_is_pending_input() {
        let text = sections(&fixture("typing.txt"));
        let dim = sections(&fixture("typing-dim.txt"));
        let pending = |n: &str| {
            let (_, lines) = text.iter().find(|(x, _)| x == n).expect(n);
            let (_, dlines) = dim.iter().find(|(x, _)| x == n).expect(n);
            screen_state(lines, Some(dlines.as_slice())).input_pending()
        };
        assert!(pending("typed-after-LF"));
        for n in [
            "idle-start",
            "lag-paste-not-drawn-at-0.2s",
            "idle-after-T1",
            "busy-T1",
            "busy-typed-T4",
            "dialog-ask-user",
        ] {
            assert!(!pending(n), "{n}");
        }
    }

    #[test]
    fn captured_start_screens() {
        for (file, want) in [
            ("screen-ctrlc.txt", ScreenHint::PromptReady),
            ("screen-dialog-allowall-1.txt", ScreenHint::Question),
            ("screen-dialog-plain.txt", ScreenHint::Question),
            ("screen-dialog-yolo-only.txt", ScreenHint::Question),
            ("screen-idle-allowall-true.txt", ScreenHint::PromptReady),
            ("screen-idle-autoupdate-on.txt", ScreenHint::PromptReady),
            ("screen-idle-nongit.txt", ScreenHint::PromptReady),
            ("screen-idle-trusted-config.txt", ScreenHint::PromptReady),
            ("auth-signed-in.txt", ScreenHint::PromptReady),
            ("auth-no-keychain.txt", ScreenHint::PromptReady),
            ("usage.txt", ScreenHint::PromptReady),
        ] {
            let text = fixture(file);
            let lines: Vec<&str> = text.lines().collect();
            let st = screen_state(&lines, None);
            assert_eq!(st.hint(), want, "{file}: {st:?}");
        }
    }

    #[test]
    fn busy_lines() {
        assert!(is_busy_line(
            " ◉ Working esc esc edit prompt      Claude Haiku 4.5"
        ));
        assert!(is_busy_line(
            " ◎ Working · 256 B esc esc interrupt · ctrl+q enqueue"
        ));
        assert!(!is_busy_line(" ◎ Loading: 13 hooks, 2 skills"));
        assert!(!is_busy_line(" ○ Asking user Waiting for response"));
        assert!(!is_busy_line(" ← open sidebar · Interactive · Allow All"));
    }

    #[test]
    fn dim_placeholder_is_not_input() {
        let rule = "─".repeat(40);
        let lines = vec![rule.clone(), "❯ Ask anything".to_owned(), rule.clone()];
        let dim = vec![String::new(), "❯".to_owned(), String::new()];
        assert!(!screen_state(&lines, None).ready);
        // Rules are dim too; the structure comes from the text lines.
        let st = screen_state(&lines, Some(dim.as_slice()));
        assert!(st.ready, "{st:?}");
    }

    fn parse(lines: &[String], dim_rows: &[usize]) -> vt100::Parser {
        let rows = u16::try_from(lines.len()).expect("rows");
        let mut parser = vt100::Parser::new(rows, 120, 0);
        let mut bytes = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                bytes.extend_from_slice(b"\r\n");
            }
            if dim_rows.contains(&i) {
                bytes.extend_from_slice(b"\x1b[2m");
                bytes.extend_from_slice(l.as_bytes());
                bytes.extend_from_slice(b"\x1b[22m");
            } else {
                bytes.extend_from_slice(l.as_bytes());
            }
        }
        parser.process(&bytes);
        parser
    }

    #[test]
    fn vt100_screen_gives_the_same_hint() {
        let all = sections(&fixture("typing.txt"));
        for (name, lines) in &all {
            let parser = parse(lines, &[]);
            assert_eq!(screen_hint(parser.screen()), expected(name), "{name}");
        }
        // A dim placeholder in the box reads as empty.
        let rule = "─".repeat(40);
        let lines = vec![
            rule.clone(),
            "❯ Ask anything".to_owned(),
            rule,
            " ← open sidebar · Interactive · Allow All".to_owned(),
        ];
        assert_eq!(
            screen_hint(parse(&lines, &[]).screen()),
            ScreenHint::NotReady
        );
        assert_eq!(
            screen_hint(parse(&lines, &[1]).screen()),
            ScreenHint::PromptReady
        );
        let mut lines = lines;
        lines[1] = "❯".to_owned();
        let mut parser = parse(&lines, &[]);
        parser.process(b"\x1b[2;3H\x1b[2mAsk anything\x1b[22m");
        assert_eq!(screen_hint(parser.screen()), ScreenHint::PromptReady);
    }

    /// B2: the dialogs that the research found in the bundle are questions.
    #[test]
    fn research_dialogs_are_questions() {
        for line in [
            "  1. Approve once",
            "  2. Approve for this session",
            "  Rate limit reached · Needs approval to switch to Auto",
            "Confirm folder trust",
            " ↑/↓ select · enter accept · ctrl+d decline · esc cancel",
        ] {
            assert!(dialog_marker(line).is_some(), "{line}");
            let st = screen_state(&["Copilot", line], None);
            assert_eq!(st.hint(), ScreenHint::Question, "{line}");
        }
        assert!(dialog_marker("I approve of this plan").is_none());
    }
}
