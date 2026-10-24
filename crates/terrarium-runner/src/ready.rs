//! Prompt-ready detection: may the runner type a turn now?

use std::time::Duration;

use regex::Regex;
use terrarium_core::adapter::Screen;

/// Tells the runner when the harness prompt accepts a turn. The runner asks
/// the adapter's `screen_hint` first, and asks the detector only when the
/// hint is `Unknown`.
pub trait ReadyDetector: Send + Sync {
    /// `quiet_for` is the time since the last PTY output.
    fn prompt_ready(&self, screen: &Screen, quiet_for: Duration) -> bool;
}

/// The default detector: the PTY was quiet for `quiet`, and the last
/// non-empty screen line matches `pattern`. Without a pattern, the line must
/// end in a prompt mark: `>`, `❯`, or `›`.
#[derive(Debug, Clone)]
pub struct DefaultReadyDetector {
    pub quiet: Duration,
    pub pattern: Option<Regex>,
}

/// The prompt marks that the default detector looks for.
pub const DEFAULT_PROMPT_MARKS: &[char] = &['>', '❯', '›'];

impl DefaultReadyDetector {
    pub fn new(quiet: Duration, pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            quiet,
            pattern: Some(Regex::new(pattern)?),
        })
    }

    fn line_matches(&self, line: &str) -> bool {
        let line = line.trim_end();
        match &self.pattern {
            Some(re) => re.is_match(line),
            None => line.ends_with(DEFAULT_PROMPT_MARKS),
        }
    }
}

impl Default for DefaultReadyDetector {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(300),
            pattern: None,
        }
    }
}

/// The last screen line that has text in it.
pub fn last_non_empty_line(screen: &Screen) -> Option<String> {
    let (_, cols) = screen.size();
    screen
        .rows(0, cols)
        .filter(|line| !line.trim().is_empty())
        .last()
}

impl ReadyDetector for DefaultReadyDetector {
    fn prompt_ready(&self, screen: &Screen, quiet_for: Duration) -> bool {
        quiet_for >= self.quiet
            && last_non_empty_line(screen).is_some_and(|line| self.line_matches(&line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(10, 40, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn custom_pattern_matches() {
        let d = DefaultReadyDetector::new(Duration::ZERO, r"^\$ $|^\$$").expect("regex");
        let p = screen(b"build ok\r\n$ ");
        assert!(d.prompt_ready(p.screen(), Duration::ZERO));
    }

    #[test]
    fn ready_needs_prompt_and_quiet() {
        let d = DefaultReadyDetector::default();
        let p = screen(b"hello\r\n> ");
        assert!(d.prompt_ready(p.screen(), Duration::from_secs(1)));
        assert!(!d.prompt_ready(p.screen(), Duration::from_millis(10)));
        let p = screen(b"> hi\r\nworking...\r\n");
        assert!(!d.prompt_ready(p.screen(), Duration::from_secs(1)));
    }
}
