//! The screen detector: a port of `screen_state()` from
//! `spikes/pi/lib/pilib.py` (`docs/compat.md`, "pi 0.85.1"), with the
//! busy and dialog rules made stricter.
//!
//! The idle screen ends with the editor between two rules, and a footer:
//!
//! ```text
//! ────────────────────────────────────────── (rule)
//! <editor text; empty when idle; no prompt mark>
//! ────────────────────────────────────────── (rule)
//! /path/to/cwd
//! ↑1.2k ↓10 … $0.000 (sub) 0.4%/272k (auto)     (openai-codex) gpt-5.6-luna • thinking off
//! ```
//!
//! # Busy
//!
//! While pi works, the top rule of the editor holds the spinner and the
//! working message: `── ⠸ Working ─────`. The runner accepts `Busy` as a
//! delivery confirmation (beside the `input` hook), so the rule must not
//! fire on an idle screen:
//!
//! 1. The top rule has a braille spinner frame (U+2800 to U+28FF) or the
//!    word "Working". pi draws only the working indicator in the rule. An
//!    editor that overflows draws ` ↑ N more ` in the rule; that is not
//!    busy.
//! 2. Or one of the few lines just above the editor starts with a spinner
//!    frame. pi draws the retry ("Retrying (1/3) in 2s…"), compaction, and
//!    branch-summary indicators there, not in the rule (read in the
//!    installed source, `status-indicator.js`; not captured).
//!
//! # Dialogs
//!
//! An extension UI prompt (`ctx.ui.select()`, `confirm()`) and the
//! project-trust dialog take the place of the editor, between the rules,
//! with a key hint: `↑↓ navigate  enter select  escape/ctrl+c cancel`.
//! The fork question of `--session <id>` from another cwd is plain text
//! that ends in `[y/N]`, with no rules. A marker counts only inside the box
//! (or anywhere in the tail when there is no box), so a model reply with
//! "[y/N]" just above the editor is not a dialog.
//!
//! # Ready
//!
//! The box is found, it is empty, pi is not busy, no dialog shows, at most
//! [`MAX_FOOTER_LINES`] non-empty lines follow the box, and no exit line
//! ("To resume this session") shows.
//!
//! # Startup
//!
//! pi 1.4.0 draws the empty editor and the footer before its startup is
//! complete, so the screen reads as ready too early. A submit then keeps
//! the text in the editor and adds "Startup is still in progress" to the
//! chat. The screen cannot tell that time apart: the runner waits for the
//! `session_start` hook ([`crate::hooks::is_startup_hook`]). The status
//! line stays in the chat after the startup, so it is not a not-ready
//! marker; the text in the editor already makes the screen not ready.

use terrarium_core::ScreenHint;
use terrarium_core::adapter::Screen;

/// Key hints of pi's dialogs and extension UI prompts.
///
/// The last three come from `docs/research/harness-hardening.md` (pi, 5),
/// read in the 1.4.0 source, not captured (B2): the model and session
/// selectors (`keyHint("tui.input.tab", "scope")`), the tree selector
/// (`keyHint("tui.select.confirm", "save")`), and the delete question of
/// the session selector.
pub const DIALOG_MARKERS: [&str; 10] = [
    "↑↓ navigate",
    "enter select",
    "enter submit",
    "escape cancel",
    "escape/ctrl+c cancel",
    "[y/N]",
    "[Y/n]",
    "tab scope",
    "enter save",
    "Delete session?",
];

/// The default working message in the top rule.
pub const BUSY_WORD: &str = "Working";

/// The line that pi prints after it exits (when the session has a file).
pub const EXIT_MARKER: &str = "To resume this session";

/// The most non-empty lines below the box on a ready screen: the cwd, the
/// token line, and room for extension status lines.
pub const MAX_FOOTER_LINES: usize = 4;

/// How many non-empty lines above the box can hold a status indicator.
const STATUS_LINES_ABOVE: usize = 3;

/// How many non-empty lines at the end of a screen with no box are
/// searched for a dialog marker.
const TAIL_LINES: usize = 6;

/// What the detector reads from one screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenState {
    pub input_box: bool,
    /// The text in the box, trimmed. `None` when there is no box.
    pub input_text: Option<String>,
    pub busy: bool,
    /// The dialog key hint that was found.
    pub dialog: Option<&'static str>,
    /// The number of non-empty lines below the box.
    pub footer_lines: usize,
    /// pi has exited (the exit line shows).
    pub exited: bool,
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
    /// The editor holds text that a CR would submit: not busy, no dialog,
    /// and pi has not exited.
    pub fn input_pending(&self) -> bool {
        self.input_box
            && !self.busy
            && !self.exited
            && self.dialog.is_none()
            && self.input_text.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// [`ScreenState::input_pending`] for a parsed screen.
pub fn input_pending(screen: &Screen) -> bool {
    screen_state(&screen_lines(screen)).input_pending()
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("──") && t.chars().filter(|&c| c == '─').count() >= 20
}

fn is_spinner(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
}

fn rule_is_busy(rule: &str) -> bool {
    rule.contains(BUSY_WORD) || rule.chars().any(is_spinner)
}

fn find_marker(lines: &[&str]) -> Option<&'static str> {
    DIALOG_MARKERS
        .into_iter()
        .find(|m| lines.iter().any(|l| l.contains(m)))
}

/// Run the detector over screen lines, top to bottom.
pub fn screen_state<S: AsRef<str>>(lines: &[S]) -> ScreenState {
    let lines: Vec<&str> = lines.iter().map(AsRef::as_ref).collect();
    let non_empty =
        |ls: &[&'_ str]| -> usize { ls.iter().filter(|l| !l.trim().is_empty()).count() };
    let mut st = ScreenState {
        exited: lines.iter().any(|l| l.contains(EXIT_MARKER)),
        ..ScreenState::default()
    };

    let rules: Vec<usize> = (0..lines.len()).filter(|&i| is_rule(lines[i])).collect();
    if let [.., a, b] = rules[..] {
        let inner = &lines[a + 1..b];
        st.input_box = true;
        st.input_text = Some(
            inner
                .iter()
                .map(|l| l.trim())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_owned(),
        );
        st.dialog = find_marker(inner);
        st.footer_lines = non_empty(&lines[b + 1..]);
        let above: Vec<&str> = lines[..a]
            .iter()
            .rev()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .take(STATUS_LINES_ABOVE)
            .collect();
        st.busy = rule_is_busy(lines[a])
            || above
                .iter()
                .any(|l| l.trim_start().chars().next().is_some_and(is_spinner));
    } else {
        let tail: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect();
        st.dialog = find_marker(&tail[tail.len().saturating_sub(TAIL_LINES)..]);
    }

    st.ready = st.input_box
        && !st.busy
        && !st.exited
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

    /// Split a `===== name (t=…) =====` capture file into (name, lines).
    fn sections(text: &str) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for line in text.lines() {
            if let Some(name) = line
                .strip_prefix("===== ")
                .and_then(|l| l.strip_suffix(" ====="))
            {
                let name = name.split(" (t=").next().unwrap_or(name);
                out.push((name.to_owned(), Vec::new()));
            } else if let Some(last) = out.last_mut() {
                last.1.push(line.to_owned());
            }
        }
        out
    }

    use ScreenHint::{Busy, NotReady, PromptReady, Question};

    /// Every captured TUI screen and its hint. The spike's labels say what
    /// the screen was: `idle-*` and `early-*` ready, `busy-*` and
    /// `working-*` (after the CR) busy, `typed-*` text in the box.
    const EXPECTED: &[(&str, &str, ScreenHint)] = &[
        ("typing.txt", "early-0.3s", PromptReady),
        ("typing.txt", "idle-start", PromptReady),
        ("typing.txt", "typed-after-LF", NotReady),
        ("typing.txt", "idle-after-ctrl-c-clear", PromptReady),
        ("typing.txt", "busy-T1", Busy),
        ("typing.txt", "idle-after-T1", PromptReady),
        ("typing.txt", "busy-T3-tool", Busy),
        ("typing.txt", "busy-typed-T4", Busy),
        ("typing.txt", "busy-after-T4-enter", Busy),
        ("typing.txt", "idle-after-T3-T4", PromptReady),
        ("hooks.txt", "idle-before-turn", PromptReady),
        // The paste is in the box; the CR is not processed yet.
        ("hooks.txt", "working-0", NotReady),
        ("hooks.txt", "working-8", Busy),
        ("hooks.txt", "idle-after-settled", PromptReady),
        // pi has exited and printed the resume line.
        ("hooks.txt", "after-ctrl-d", NotReady),
        ("session.txt", "B-session-id-again", PromptReady),
        ("session.txt", "E-after-one-ctrl-c", PromptReady),
        ("session.txt", "C-session-flag", PromptReady),
        ("session.txt", "C-session-id-other-cwd", PromptReady),
        ("session.txt", "C-session-flag-other-cwd", Question),
        ("session.txt", "D-second", PromptReady),
        ("ui-prompts.txt", "idle", PromptReady),
        ("ui-prompts.txt", "select-shown", Question),
        ("ui-prompts.txt", "select-answered", PromptReady),
        ("ui-prompts.txt", "confirm-shown", Question),
        ("ui-prompts.txt", "confirm-answered", PromptReady),
        ("ui-prompts.txt", "no-key-0.3s", PromptReady),
        ("ui-prompts.txt", "no-key-settled", PromptReady),
        // No session file, so no resume line: the last frame looks idle.
        // The runner knows from the exit that the process is gone.
        ("ui-prompts.txt", "after-ctrl-d", PromptReady),
    ];

    #[test]
    fn captured_tui_screens_match() {
        let mut seen = 0;
        for file in ["typing.txt", "hooks.txt", "session.txt", "ui-prompts.txt"] {
            for (name, lines) in sections(&fixture(file)) {
                let (_, _, want) = EXPECTED
                    .iter()
                    .find(|e| e.0 == file && e.1 == name)
                    .unwrap_or_else(|| panic!("no expectation for {file} {name}"));
                let st = screen_state(&lines);
                assert_eq!(st.hint(), *want, "{file} {name}: {st:?}");
                seen += 1;
            }
        }
        assert_eq!(seen, EXPECTED.len());
    }

    #[test]
    fn captured_start_screens_match() {
        for (file, want) in [
            ("screen-dialog-project-trust.txt", Question),
            ("screen-idle-approve.txt", PromptReady),
            ("screen-idle-ext-trust.txt", PromptReady),
            ("screen-idle-no-approve.txt", PromptReady),
            ("screen-idle-offline-bare.txt", PromptReady),
            ("screen-idle-offline-default.txt", PromptReady),
            // The update notice is a box above the editor. It does not block.
            ("screen-idle-online.txt", PromptReady),
            ("session-command.txt", PromptReady),
        ] {
            let text = fixture(file);
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(screen_state(&lines).hint(), want, "{file}");
        }
    }

    /// Rebuilt from the live screen of 2026-11-01 (pi 1.4.0, a self-hosted
    /// DeepSeek server; the rules are full width here). A turn typed before
    /// `session_start` stays in the editor. After the startup the status
    /// line stays above an empty editor, and the screen is ready.
    #[test]
    fn startup_in_progress_screens() {
        let text = fixture("screen-startup-in-progress.txt");
        let lines: Vec<&str> = text.lines().collect();
        let st = screen_state(&lines);
        assert_eq!(
            st.input_text.as_deref(),
            Some("[User] Reply with the single word: ok")
        );
        assert_eq!(st.hint(), NotReady);

        let text = fixture("screen-startup-done.txt");
        let lines: Vec<&str> = text.lines().collect();
        assert!(text.contains("Startup is still in progress"));
        assert_eq!(screen_state(&lines).hint(), PromptReady);
    }

    #[test]
    fn details_of_busy_and_typed_screens() {
        let all = sections(&fixture("typing.txt"));
        let get = |n: &str| screen_state(&all.iter().find(|(x, _)| x == n).expect(n).1);
        let typed = get("typed-after-LF");
        assert_eq!(typed.input_text.as_deref(), Some("abc"));
        let queued = get("busy-typed-T4");
        assert!(queued.busy);
        assert_eq!(
            queued.input_text.as_deref(),
            Some("Also add the word: queued")
        );
        let idle = get("idle-start");
        assert_eq!(idle.footer_lines, 2);
        assert!(idle.ready && !idle.exited);
    }

    /// TER-208: typed text that did not submit is pending input. An empty
    /// editor and a busy screen are not.
    #[test]
    fn typed_text_is_pending_input() {
        let all = sections(&fixture("typing.txt"));
        let pending =
            |n: &str| screen_state(&all.iter().find(|(x, _)| x == n).expect(n).1).input_pending();
        assert!(pending("typed-after-LF"));
        for n in [
            "idle-start",
            "idle-after-ctrl-c-clear",
            "idle-after-T1",
            "busy-T1",
            "busy-typed-T4",
        ] {
            assert!(!pending(n), "{n}");
        }
        let text = fixture("screen-startup-in-progress.txt");
        let lines: Vec<&str> = text.lines().collect();
        assert!(screen_state(&lines).input_pending());
    }

    fn screen(above: &[&str], top: &str, inner: &[&str]) -> Vec<String> {
        let rule = "─".repeat(60);
        let mut v: Vec<String> = above.iter().map(|s| s.to_string()).collect();
        v.push(top.to_owned());
        v.extend(inner.iter().map(|s| s.to_string()));
        v.push(rule);
        v.push("/w".into());
        v.push("$0.000 (sub) 0.0%/272k (auto)    (openai-codex) gpt-5.6-luna".into());
        v
    }

    #[test]
    fn synthetic_rules() {
        let rule = "─".repeat(60);
        assert_eq!(screen_state(&screen(&[], &rule, &[""])).hint(), PromptReady);
        // The retry indicator above the box is busy.
        let retry = screen(
            &[
                " Error: Connection error.",
                "",
                " ⠧ Retrying (1/3) in 2s... (escape to cancel)",
            ],
            &rule,
            &[""],
        );
        assert_eq!(screen_state(&retry).hint(), Busy);
        // An editor overflow label in the rule is not busy.
        let over = format!("{} ↑ 3 more {}", "─".repeat(25), "─".repeat(25));
        assert_eq!(screen_state(&screen(&[], &over, &["x"])).hint(), NotReady);
        // A custom working message still has the spinner.
        let custom = format!("── ⠋ Thinking hard {}", "─".repeat(40));
        assert_eq!(screen_state(&screen(&[], &custom, &[""])).hint(), Busy);
        // "[y/N]" in a model reply above the box is not a dialog.
        let reply = screen(&[" Continue? [y/N]"], &rule, &[""]);
        assert_eq!(screen_state(&reply).hint(), PromptReady);
        // Too many lines below the box: not at the bottom.
        let mut low = screen(&[], &rule, &[""]);
        low.extend(["a", "b", "c"].map(String::from));
        assert_eq!(screen_state(&low).hint(), NotReady);
        // No box at all.
        assert_eq!(screen_state(&["pi v0.85.1"]).hint(), NotReady);
    }

    #[test]
    fn vt100_screen_gives_the_same_hint() {
        let mut all = sections(&fixture("typing.txt"));
        all.extend(sections(&fixture("ui-prompts.txt")));
        for (name, want) in [
            ("idle-start", PromptReady),
            ("busy-T3-tool", Busy),
            ("select-shown", Question),
            ("typed-after-LF", NotReady),
        ] {
            let (_, lines) = all.iter().find(|(n, _)| n == name).expect(name);
            let rows = u16::try_from(lines.len()).expect("rows");
            let mut parser = vt100::Parser::new(rows, 120, 0);
            parser.process(lines.join("\r\n").as_bytes());
            assert_eq!(screen_hint(parser.screen()), want, "{name}");
        }
    }

    /// B2: the selectors that the research found are questions, in the box
    /// and with no box.
    #[test]
    fn research_selectors_are_questions() {
        let rule = "─".repeat(40);
        for line in [
            "tab scope (all/scoped)",
            "tab scope · re:<pattern> regex · \"phrase\" exact",
            "  enter save  escape/ctrl+c cancel",
            "Delete session? enter confirm · escape cancel",
        ] {
            let in_box = [rule.as_str(), line, rule.as_str(), "~/work"];
            assert_eq!(screen_state(&in_box).hint(), Question, "{line}");
            let no_box = ["pi", line];
            assert_eq!(screen_state(&no_box).hint(), Question, "{line}");
        }
    }
}
