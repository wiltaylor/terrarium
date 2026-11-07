//! The screen detector: a port of `screen_state()` from
//! `spikes/codex/lib/codexlib.py` (`docs/compat.md`, "Codex 0.160.0",
//! "Typing into the TUI").
//!
//! The idle screen ends with the composer, a status line, and a footer:
//!
//! ```text
//! › Ask Codex to do anything          <- placeholder, drawn dim (SGR 2)
//!   GPT-6-Luna low · /path · <title>
//!   ? for shortcuts                                ⚠ 1 warning · f2 to view
//! ```
//!
//! While a turn runs, a line `• Working (1s • esc to interrupt)` is above
//! the composer. Earlier prompts also start with `›`, but Codex draws that
//! mark dim; the composer mark is not dim.
//!
//! # The rule
//!
//! 1. The composer is the last line that starts with `›` and is not a
//!    numbered option (`› 1. …`).
//! 2. The input text is the text after `›` with the dim characters blanked.
//!    The placeholder is dim, so an empty composer has no input text. Only
//!    the first line of a composer with more than one line is read.
//! 3. `busy`: "esc to interrupt" or "Working (<n>s" in the four lines above
//!    the composer or in the lines below it.
//! 4. `dialog`: one of the last six non-empty lines has a dialog key hint
//!    ([`dialog_hint`]). A dialog hides the composer.
//! 5. `ready`: a composer, no input text, not busy, no dialog.
//!
//! The spike also asks for the same `ready` for 1 s, because Codex draws
//! the composer for about 0.35 s before a start-up dialog replaces it. This
//! port is stateless (one adapter serves many agents), so it does not do
//! that. The launch flags remove every start-up dialog that the spikes saw:
//! trust, hook review, the resume cwd chooser, and the update box.

use terrarium_core::ScreenHint;
use terrarium_core::adapter::Screen;

/// The mark at the start of the composer and of earlier prompts.
pub const PROMPT_MARK: char = '›';

/// How many lines above the composer can hold the busy line.
const BUSY_LINES_ABOVE: usize = 4;
/// How many non-empty lines at the bottom can hold a dialog hint.
const DIALOG_TAIL: usize = 6;

/// What the detector reads from one screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenState {
    pub input_box: bool,
    /// The text in the composer with the placeholder removed, trimmed.
    /// `None` when there is no composer.
    pub input_text: Option<String>,
    pub busy: bool,
    /// The line with the dialog key hint, trimmed.
    pub dialog: Option<String>,
    /// The non-empty lines below the composer, joined with `\n`.
    pub footer: Option<String>,
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
    /// The composer holds text that a CR would submit: not busy, and no
    /// dialog. The placeholder does not count when the state comes from
    /// [`screen_state_of`] (the dim mask).
    pub fn input_pending(&self) -> bool {
        self.input_box
            && !self.busy
            && self.dialog.is_none()
            && self.input_text.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// [`ScreenState::input_pending`] for a parsed screen.
pub fn input_pending(screen: &Screen) -> bool {
    screen_state_of(screen).input_pending()
}

/// True when `rest` starts with `word` and the word ends there.
fn starts_with_word(rest: &str, word: &str) -> bool {
    rest.strip_prefix(word)
        .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
}

/// The dialog key hints of Codex 0.160.0: "enter continue · esc quit"
/// (trust), "enter confirm · esc skip" (hook review), "enter continue ·
/// esc use session · ctrl+c quit" (resume cwd), "r retry f fork
/// esc/ctrl+c/q exit" (open in another app).
///
/// And the footers that `docs/research/harness-hardening.md` (Codex, 5)
/// read in the 0.164.0 source and snapshots (B2): the model-migration
/// prompt at start ("enter/esc continue · ⌃c quit", or "enter/esc
/// confirm"), the rate-limit switch popup ("Press enter to confirm or esc
/// to go back"), and the Plan-mode question ("tab to add notes | enter to
/// submit answer | ←/→ to navigate questions | esc to interrupt", and
/// "enter to submit answer | esc to interrupt"). The question footer has
/// "esc to interrupt" too, so it must win over the busy line: a dialog
/// wins in [`ScreenState::hint`].
pub fn dialog_hint(line: &str) -> bool {
    let l = line.to_lowercase();
    let t = l.trim_start();
    [
        "enter continue",
        "enter confirm",
        "enter to select",
        "enter to submit",
        "enter/esc continue",
        "enter/esc confirm",
        "press enter to confirm",
    ]
    .iter()
    .any(|w| starts_with_word(t, w))
        || [
            "· esc quit",
            "· esc skip",
            "· esc cancel",
            "· esc use session",
            "· ⌃c quit",
            "enter to submit answer",
            "tab to add notes",
            "press enter to confirm or esc to go back",
        ]
        .iter()
        .any(|m| l.contains(m))
        || l.contains("esc/ctrl+c/q exit")
}

/// The busy line: "esc to interrupt", or "Working (<digits>s".
pub fn busy_text(text: &str) -> bool {
    if text.contains("esc to interrupt") {
        return true;
    }
    text.match_indices("Working (").any(|(i, m)| {
        let rest = &text[i + m.len()..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with('s')
    })
}

/// `› 1. …`: an option of a numbered list, not the composer.
fn numbered_option(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix(PROMPT_MARK) else {
        return false;
    };
    let rest = rest.trim_start();
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && rest[digits..]
            .strip_prefix('.')
            .is_some_and(|r| r.starts_with(char::is_whitespace))
}

/// Run the detector over screen lines, top to bottom. `dim` holds the same
/// lines with the dim characters blanked; without it the placeholder
/// counts as input text.
pub fn screen_state<S: AsRef<str>>(lines: &[S], dim: Option<&[S]>) -> ScreenState {
    let lines: Vec<&str> = lines.iter().map(AsRef::as_ref).collect();
    let mut st = ScreenState::default();

    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    st.dialog = non_empty[non_empty.len().saturating_sub(DIALOG_TAIL)..]
        .iter()
        .rfind(|l| dialog_hint(l))
        .map(|l| l.trim().to_owned());

    let composer = (0..lines.len())
        .rev()
        .find(|&i| lines[i].trim_start().starts_with(PROMPT_MARK) && !numbered_option(lines[i]));
    if let Some(idx) = composer
        && st.dialog.is_none()
    {
        st.input_box = true;
        let row = dim
            .and_then(|d| d.get(idx))
            .map(AsRef::as_ref)
            .filter(|r| r.contains(PROMPT_MARK))
            .unwrap_or(lines[idx]);
        st.input_text = row
            .split_once(PROMPT_MARK)
            .map(|(_, text)| text.trim().to_owned());
        let footer = lines[idx + 1..]
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let above = lines[idx.saturating_sub(BUSY_LINES_ABOVE)..idx].join("\n");
        st.busy = busy_text(&above) || busy_text(&footer);
        st.footer = Some(footer);
    }
    st.ready =
        st.input_box && !st.busy && st.dialog.is_none() && st.input_text.as_deref() == Some("");
    st
}

/// The rows of a parsed screen as text, and the same rows with the dim
/// cells blanked.
pub fn screen_rows(screen: &Screen) -> (Vec<String>, Vec<String>) {
    let (rows, cols) = screen.size();
    let mut plain = Vec::with_capacity(usize::from(rows));
    let mut dim = Vec::with_capacity(usize::from(rows));
    for r in 0..rows {
        let mut p = String::new();
        let mut d = String::new();
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            p.push_str(text);
            if cell.dim() {
                d.extend(std::iter::repeat_n(' ', text.chars().count()));
            } else {
                d.push_str(text);
            }
        }
        plain.push(p.trim_end().to_owned());
        dim.push(d.trim_end().to_owned());
    }
    (plain, dim)
}

/// The detector state for a parsed screen.
pub fn screen_state_of(screen: &Screen) -> ScreenState {
    let (plain, dim) = screen_rows(screen);
    screen_state(&plain, Some(&dim))
}

/// The hint for a parsed screen.
pub fn screen_hint(screen: &Screen) -> ScreenHint {
    screen_state_of(screen).hint()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/screens/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).expect("fixture")
    }

    /// Split a `===== name (t=…) =====` capture file into name → lines.
    fn sections(text: &str) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if let Some(head) = line
                .strip_prefix("===== ")
                .and_then(|l| l.strip_suffix(" ====="))
            {
                let name = head.split(" (t=").next().unwrap_or(head).to_owned();
                out.insert(name.clone(), Vec::new());
                current = Some(name);
            } else if let Some(name) = &current {
                out.get_mut(name).expect("section").push(line.to_owned());
            }
        }
        out
    }

    /// The labels of `spikes/codex/typing-into-tui/detector_check.py`:
    /// `idle-*` and `resume-*` ready, `busy-*` busy, `typed-*` input text.
    #[test]
    fn typing_screens_with_the_dim_mask() {
        let text = sections(&fixture("typing.txt"));
        let dim = sections(&fixture("typing-dim.txt"));
        assert_eq!(text.len(), 10);
        for (label, lines) in &text {
            let st = screen_state(lines, dim.get(label).map(Vec::as_slice));
            assert!(st.input_box && st.dialog.is_none(), "{label}: {st:?}");
            let want = if label.starts_with("idle") || label.starts_with("resume") {
                ScreenHint::PromptReady
            } else if label.starts_with("busy") {
                ScreenHint::Busy
            } else {
                assert!(st.input_text.as_deref().is_some_and(|t| !t.is_empty()));
                ScreenHint::NotReady
            };
            assert_eq!(st.hint(), want, "{label}: {st:?}");
        }
        let typed = screen_state(&text["typed-before-CR"], Some(&dim["typed-before-CR"]));
        assert_eq!(typed.input_text.as_deref(), Some("Reply with exactly:"));
        // Text typed while busy is busy, not ready.
        let queued = screen_state(&text["busy-typed-T4"], Some(&dim["busy-typed-T4"]));
        assert_eq!(
            queued.input_text.as_deref(),
            Some("Also add the word: queued")
        );
        assert_eq!(queued.hint(), ScreenHint::Busy);
    }

    /// TER-208: typed text that did not submit is pending input. An empty
    /// composer (the placeholder under the dim mask) and a busy screen are
    /// not.
    #[test]
    fn typed_text_is_pending_input() {
        let text = sections(&fixture("typing.txt"));
        let dim = sections(&fixture("typing-dim.txt"));
        let pending = |label: &str| screen_state(&text[label], Some(&dim[label])).input_pending();
        assert!(pending("typed-after-LF"));
        assert!(pending("typed-before-CR"));
        for label in ["idle-start", "idle-after-T1", "busy-T1", "busy-typed-T4"] {
            assert!(!pending(label), "{label}");
        }
    }

    #[test]
    fn without_the_dim_mask_the_placeholder_is_input() {
        let text = sections(&fixture("typing.txt"));
        let st = screen_state::<String>(&text["idle-start"], None);
        assert_eq!(st.input_text.as_deref(), Some("Ask Codex to do anything"));
        assert!(!st.ready);
        assert_eq!(st.hint(), ScreenHint::NotReady);
    }

    /// The start screens of `spikes/codex/yolo-and-trust`: a dialog, or a
    /// composer. The text before `===== after keys =====` is the first
    /// screen.
    #[test]
    fn start_screens() {
        for (file, dialog) in [
            ("screen-plain-nongit.txt", None),
            ("screen-plain-git.txt", Some("enter continue · esc quit")),
            ("screen-yolo-git.txt", Some("enter continue · esc quit")),
            ("screen-never-full.txt", None),
            ("screen-trust-c.txt", None),
            ("screen-hooks-notrust.txt", Some("enter confirm · esc skip")),
            ("screen-hooks-warning.txt", None),
            ("screen-update-banner.txt", None),
            ("screen-update-off.txt", None),
            ("screen-daemon.txt", None),
            ("screen-daemon-with-c.txt", None),
        ] {
            let text = fixture(file);
            let first = text.split("===== after keys =====").next().unwrap_or("");
            let lines: Vec<&str> = first.lines().collect();
            let st = screen_state(&lines, None);
            assert_eq!(st.dialog.as_deref(), dialog, "{file}");
            assert_eq!(st.input_box, dialog.is_none(), "{file}");
            assert!(!st.busy, "{file}");
            if dialog.is_some() {
                assert_eq!(st.hint(), ScreenHint::Question, "{file}");
            }
        }
    }

    #[test]
    fn resume_dialogs_are_questions() {
        let s = sections(&fixture("resume.txt"));
        let cwd = screen_state::<String>(&s["C-resume-other-cwd"], None);
        assert_eq!(
            cwd.dialog.as_deref(),
            Some("enter continue · esc use session · ctrl+c quit")
        );
        assert_eq!(cwd.hint(), ScreenHint::Question);
        let other = screen_state::<String>(&s["D-second"], None);
        assert!(
            other
                .dialog
                .is_some_and(|d| d.contains("esc/ctrl+c/q exit"))
        );
        for label in ["B-resumed-idle", "D-first"] {
            let st = screen_state::<String>(&s[label], None);
            assert!(st.input_box && !st.busy && st.dialog.is_none(), "{label}");
        }
        for label in ["A-after-exit", "B-after-exit", "E-bogus-uuid"] {
            let st = screen_state::<String>(&s[label], None);
            assert_eq!(st.hint(), ScreenHint::NotReady, "{label}");
            assert!(!st.input_box, "{label}");
        }
    }

    #[test]
    fn hook_spike_screens() {
        let s = sections(&fixture("hooks.txt"));
        // working-0 is just after the CR: the prompt is still in the
        // composer, and the busy line is not there yet.
        let w0 = screen_state::<String>(&s["working-0"], None);
        assert!(w0.input_box && !w0.busy && !w0.ready);
        for label in ["working-6", "working-12"] {
            let st = screen_state::<String>(&s[label], None);
            assert_eq!(st.hint(), ScreenHint::Busy, "{label}: {st:?}");
        }
        for label in ["idle-before-turn", "idle-after-stop", "after-idle-wait"] {
            let st = screen_state::<String>(&s[label], None);
            assert!(st.input_box && !st.busy && st.dialog.is_none(), "{label}");
        }
        let status = fixture("status.txt");
        let lines: Vec<&str> = status.lines().collect();
        let st = screen_state(&lines, None);
        assert!(st.input_box && !st.busy && st.dialog.is_none());
    }

    /// B2: the footers from the research, as the 0.164.0 snapshots draw
    /// them, are questions, also with "esc to interrupt" in them.
    #[test]
    fn research_dialogs_are_questions() {
        for footer in [
            "  enter/esc continue · ⌃c quit",
            "  enter/esc confirm",
            "  Press enter to confirm or esc to go back",
            "  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt",
            "  enter to submit answer | esc to interrupt",
        ] {
            assert!(dialog_hint(footer), "{footer}");
            let lines = [
                "  >_ OpenAI Codex (v0.164.0)",
                "",
                "  Try the new model?",
                "› 1. Use gpt-6-luna",
                "  2. Keep gpt-5.6-luna",
                "",
                footer,
            ];
            let st = screen_state(&lines, None);
            assert_eq!(st.hint(), ScreenHint::Question, "{footer}: {st:?}");
        }
        // The inline async question keeps the composer: not a dialog.
        assert!(!dialog_hint("  enter submit   ⌃] skip   ⇧→ main prompt"));
        assert!(!dialog_hint("  ? for shortcuts"));
    }

    #[test]
    fn markers() {
        assert!(busy_text("• Working (12s • esc to interrupt)"));
        assert!(busy_text("• Working (3s"));
        assert!(!busy_text("Worked for 2s • 5:04 PM"));
        assert!(!busy_text("Working (soon)"));
        assert!(dialog_hint("  Enter to select · Esc to cancel"));
        assert!(!dialog_hint("  enter continued"));
        assert!(!dialog_hint("  ? for shortcuts"));
        assert!(numbered_option("› 1. Trust and continue"));
        assert!(!numbered_option("› 1.5 is a number"));
        assert!(!numbered_option("› Ask Codex"));
    }

    /// Rebuild the real attributes from a capture pair: a character that
    /// the dim capture blanks is drawn with SGR 2. Then the vt100 path must
    /// give the same hint as the text path.
    fn ansi_from_pair(text: &[String], dim: &[String]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, line) in text.iter().enumerate() {
            let d: Vec<char> = dim.get(i).map(|l| l.chars().collect()).unwrap_or_default();
            let mut dimmed = false;
            for (j, ch) in line.chars().enumerate() {
                let want = ch != ' ' && d.get(j).is_none_or(|c| *c == ' ');
                if want != dimmed {
                    out.extend_from_slice(if want { b"\x1b[2m" } else { b"\x1b[22m" });
                    dimmed = want;
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            out.extend_from_slice(b"\x1b[0m\r\n");
        }
        out
    }

    #[test]
    fn vt100_screen_gives_the_same_hint() {
        let text = sections(&fixture("typing.txt"));
        let dim = sections(&fixture("typing-dim.txt"));
        for (label, want) in [
            ("idle-start", ScreenHint::PromptReady),
            ("idle-after-T1", ScreenHint::PromptReady),
            ("busy-T1", ScreenHint::Busy),
            ("busy-typed-T4", ScreenHint::Busy),
            ("typed-before-CR", ScreenHint::NotReady),
        ] {
            let lines = &text[label];
            let rows = u16::try_from(lines.len() + 1).expect("rows");
            let mut parser = vt100::Parser::new(rows, 200, 0);
            parser.process(&ansi_from_pair(lines, &dim[label]));
            assert_eq!(screen_hint(parser.screen()), want, "{label}");
        }
    }
}
