//! The screen detector: a port of `screen_state()` from
//! `spikes/claude/lib/ptyharness.py` (`docs/compat.md`, "Typing into the
//! TUI").
//!
//! The idle screen ends with the input box and a footer:
//!
//! ```text
//! ──────────────────────────────────────────
//! ❯
//! ──────────────────────────────────────────
//!   ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
//! ```
//!
//! # The prompt-ready rule
//!
//! The runner types a turn only when [`ScreenState::ready`] is true, and it
//! checks again just before it writes. "The status is idle" is not enough:
//! Claude Code queues text that arrives while a turn runs, and the queued
//! text joins the running turn. Then two `UserPromptSubmit` hooks fire, but
//! only one `Stop`. So `ready` is strict. It is true only when all of these
//! are true:
//!
//! 1. The input box is found: the last two rule lines (20 or more `─`) with
//!    a line that starts with `❯` between them.
//! 2. The input box is empty.
//! 3. The footer below the box does not say "esc to interrupt" (busy).
//! 4. None of the last four non-empty lines has a dialog key hint.
//! 5. The box is at the bottom: at most [`MAX_FOOTER_LINES`] non-empty
//!    lines come after it.
//!
//! "No input box" is "not ready" (the theme picker has no key hint).

use terrarium_core::ScreenHint;
use terrarium_core::adapter::Screen;

/// Key hints that dialogs (trust, bypass warning, `AskUserQuestion`,
/// permission prompts) show in their last line, and the text of the
/// dialogs that `docs/research/harness-hardening.md` (Claude Code, 1a and
/// 5) found in the 2.1.310 bundle with no known key hint (B2): the
/// usage-limit menu, the spend-limit nudge, the `.mcp.json` approval, and
/// the Consumer Terms notice. Their look on screen is not captured.
pub const DIALOG_MARKERS: [&str; 9] = [
    "Enter to select",
    "Enter to confirm",
    "Esc to cancel",
    "to navigate",
    "Stop and wait for limit to reset",
    "Could not update your spend limit",
    "Press Enter to retry",
    "New MCP server found in this project",
    "An update to our Consumer Terms",
];

/// The footer text that shows while a turn runs.
pub const BUSY_MARKER: &str = "esc to interrupt";

/// The most non-empty lines below the input box on a ready screen.
pub const MAX_FOOTER_LINES: usize = 3;

const PROMPT_MARK: char = '❯';

/// What the detector reads from one screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenState {
    pub input_box: bool,
    /// The text in the input box, trimmed. `None` when there is no box.
    pub input_text: Option<String>,
    pub busy: bool,
    /// The dialog key hint that was found.
    pub dialog: Option<&'static str>,
    /// The non-empty lines below the box, joined with `\n`.
    pub footer: Option<String>,
    /// The number of non-empty lines below the box.
    pub footer_lines: usize,
    /// The strict prompt-ready rule (see the module docs).
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

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 20 && t.chars().all(|c| c == '─')
}

/// Run the detector over screen lines, top to bottom.
pub fn screen_state<S: AsRef<str>>(lines: &[S]) -> ScreenState {
    let lines: Vec<&str> = lines.iter().map(AsRef::as_ref).collect();
    let mut st = ScreenState::default();

    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = &non_empty[non_empty.len().saturating_sub(4)..];
    st.dialog = DIALOG_MARKERS
        .into_iter()
        .find(|m| tail.iter().any(|l| l.contains(m)));

    let rules: Vec<usize> = (0..lines.len()).filter(|&i| is_rule(lines[i])).collect();
    for pair in rules.windows(2).rev() {
        let (a, b) = (pair[0], pair[1]);
        let inner = &lines[a + 1..b];
        let Some(first) = inner.first() else { continue };
        if !first.trim_start().starts_with(PROMPT_MARK) {
            continue;
        }
        st.input_box = true;
        let joined = inner.join("\n");
        let start = joined
            .find(PROMPT_MARK)
            .map_or(0, |i| i + PROMPT_MARK.len_utf8());
        st.input_text = Some(joined[start..].trim().to_owned());
        let below: Vec<&str> = lines[b + 1..]
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect();
        st.footer_lines = below.len();
        let footer = below.join("\n");
        st.busy = footer.contains(BUSY_MARKER);
        st.footer = Some(footer);
        break;
    }

    st.ready = st.input_box
        && !st.busy
        && st.input_text.as_deref() == Some("")
        && st.dialog.is_none()
        && st.footer_lines <= MAX_FOOTER_LINES;
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

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/screens/{name}",
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

    /// (name prefix, input_box, busy, ready, dialog). The values come from
    /// `spikes/claude/typing-into-tui/detector_check.py` on the same files.
    const EXPECTED: &[(&str, bool, bool, bool, Option<&str>)] = &[
        ("T1 after startup", true, false, true, None),
        ("T2", true, false, true, None),
        ("T3", true, false, true, None),
        ("T4 big paste before CR", true, false, false, None),
        ("T4 (", true, false, true, None),
        ("T5 after plain text with LF", true, false, false, None),
        ("T5-submit", true, false, true, None),
        ("T6 second prompt typed while busy", true, true, false, None),
        ("T6 after", true, false, true, None),
        (
            "T7 question shown",
            false,
            false,
            false,
            Some("Enter to select"),
        ),
        (
            "T7 after Down",
            false,
            false,
            false,
            Some("Enter to select"),
        ),
        ("T7 after Enter", true, false, true, None),
        ("T7 after answer", true, false, true, None),
        ("idle-before-turn", true, false, true, None),
        ("working-0", true, false, false, None),
        ("working-5", true, true, false, None),
        ("working-10", true, true, false, None),
        ("working-15", true, true, false, None),
        ("idle-after-stop", true, false, true, None),
        ("after-idle-wait", true, false, true, None),
        ("after-exit", false, false, false, None),
    ];

    #[test]
    fn captured_tui_screens_match_the_spike() {
        let mut all = sections(&fixture("typing.txt"));
        all.extend(sections(&fixture("hooks.txt")));
        assert_eq!(all.len(), EXPECTED.len());
        for (name, lines) in &all {
            let (_, input_box, busy, ready, dialog) = EXPECTED
                .iter()
                .find(|e| name.starts_with(e.0))
                .unwrap_or_else(|| panic!("no expectation for {name}"));
            let st = screen_state(lines);
            assert_eq!(
                (st.input_box, st.busy, st.ready, st.dialog),
                (*input_box, *busy, *ready, *dialog),
                "{name}: {st:?}"
            );
        }
    }

    #[test]
    fn captured_start_screens_match_the_spike() {
        for (file, ready, dialog) in [
            ("screen-A-fresh.txt", false, None),
            ("screen-B-onboarded.txt", false, Some("Enter to confirm")),
            (
                "screen-C-onboarded-trust.txt",
                false,
                Some("Enter to confirm"),
            ),
            ("screen-D-all-seeded-skip.txt", true, None),
            ("screen-E-D-plus-quiet-env.txt", true, None),
            ("screen-F-trust-only-skip.txt", false, None),
            (
                "screen-G-untrusted-skip.txt",
                false,
                Some("Enter to confirm"),
            ),
            ("screen-H-minimal-seed.txt", true, None),
            ("screen-I-parent-trust.txt", true, None),
        ] {
            let text = fixture(file);
            let lines: Vec<&str> = text.lines().collect();
            let st = screen_state(&lines);
            assert_eq!((st.ready, st.dialog), (ready, dialog), "{file}");
            let want = match (ready, dialog) {
                (true, _) => ScreenHint::PromptReady,
                (_, Some(_)) => ScreenHint::Question,
                _ => ScreenHint::NotReady,
            };
            assert_eq!(st.hint(), want, "{file}");
        }
    }

    /// B2: the dialogs from the research, with the strings that it quotes
    /// from the bundle, are questions.
    #[test]
    fn research_dialogs_are_questions() {
        let rule = "─".repeat(40);
        for tail in [
            vec![
                "You've hit your limit · resets 5pm",
                "❯ 1. Stop and wait for limit to reset",
                "  2. Upgrade your plan",
            ],
            vec!["Could not update your spend limit. Press Enter to retry."],
            vec![
                "New MCP server found in this project: db",
                "❯ 1. Use this MCP server",
                "  2. Continue without using this MCP server",
            ],
            vec![
                "[ACTION REQUIRED] An update to our Consumer Terms and Privacy Policy",
                "You must run `claude` to review the updated terms",
            ],
        ] {
            let mut lines = vec!["✻ Welcome", rule.as_str(), "❯", rule.as_str(), ""];
            lines.extend(tail.iter().copied());
            let st = screen_state(&lines);
            assert_eq!(st.hint(), ScreenHint::Question, "{tail:?}: {st:?}");
        }
    }

    #[test]
    fn queued_text_is_busy_not_ready() {
        let all = sections(&fixture("typing.txt"));
        let (_, lines) = all
            .iter()
            .find(|(n, _)| n.starts_with("T6 second"))
            .expect("T6");
        let st = screen_state(lines);
        assert_eq!(st.hint(), ScreenHint::Busy);
        assert!(st.input_text.expect("text").contains("queued"));
    }

    #[test]
    fn box_not_at_bottom_is_not_ready() {
        let rule = "─".repeat(40);
        let mut lines = vec![rule.clone(), "❯".to_owned(), rule];
        assert!(screen_state(&lines).ready);
        lines.extend(["a", "b", "c", "d"].map(String::from));
        let st = screen_state(&lines);
        assert!(st.input_box && !st.ready);
        assert_eq!(st.hint(), ScreenHint::NotReady);
    }

    #[test]
    fn vt100_screen_gives_the_same_hint() {
        let all = sections(&fixture("typing.txt"));
        for (prefix, want) in [
            ("T1 after startup", ScreenHint::PromptReady),
            ("T6 second", ScreenHint::Busy),
            ("T7 question shown", ScreenHint::Question),
            ("T4 big paste", ScreenHint::NotReady),
        ] {
            let (_, lines) = all
                .iter()
                .find(|(n, _)| n.starts_with(prefix))
                .expect(prefix);
            let rows = u16::try_from(lines.len()).expect("rows");
            let mut parser = vt100::Parser::new(rows, 120, 0);
            parser.process(lines.join("\r\n").as_bytes());
            assert_eq!(screen_hint(parser.screen()), want, "{prefix}");
        }
    }
}
