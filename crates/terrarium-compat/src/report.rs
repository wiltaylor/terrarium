//! The result of one suite run: one row per step, printed as a Markdown
//! table and written to `target/compat-report.md`.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use terrarium_core::HarnessId;

/// The result of one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Skip,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "FAIL",
            Self::Skip => "skip",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone)]
pub struct Step {
    pub harness: HarnessId,
    pub name: String,
    pub outcome: Outcome,
    /// How long the step took, when the step measures time.
    pub time: Option<Duration>,
    pub evidence: String,
}

/// The whole run.
#[derive(Debug, Clone)]
pub struct Report {
    pub started: DateTime<Utc>,
    /// `dry run` or `real`.
    pub mode: &'static str,
    pub steps: Vec<Step>,
    /// The model turns that each harness got (typed turns).
    pub turns: Vec<(HarnessId, u32)>,
    /// Free text below the table: deviations from the deployment, and
    /// what the leftover check could not attribute.
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(mode: &'static str) -> Self {
        Self {
            started: Utc::now(),
            mode,
            steps: Vec::new(),
            turns: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn add(
        &mut self,
        harness: HarnessId,
        name: &str,
        outcome: Outcome,
        time: Option<Duration>,
        evidence: impl Into<String>,
    ) {
        let step = Step {
            harness,
            name: name.to_owned(),
            outcome,
            time,
            evidence: evidence.into(),
        };
        println!(
            "[compat] {} {} {}: {}",
            step.harness,
            step.name,
            step.outcome.as_str(),
            step.evidence
        );
        self.steps.push(step);
    }

    pub fn pass(&mut self, h: HarnessId, name: &str, evidence: impl Into<String>) {
        self.add(h, name, Outcome::Pass, None, evidence);
    }

    pub fn fail(&mut self, h: HarnessId, name: &str, evidence: impl Into<String>) {
        self.add(h, name, Outcome::Fail, None, evidence);
    }

    pub fn skip(&mut self, h: HarnessId, name: &str, evidence: impl Into<String>) {
        self.add(h, name, Outcome::Skip, None, evidence);
    }

    pub fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        println!("[compat] note: {text}");
        self.notes.push(text);
    }

    pub fn set_turns(&mut self, h: HarnessId, turns: u32) {
        self.turns.retain(|(x, _)| *x != h);
        self.turns.push((h, turns));
    }

    /// True when any step failed.
    pub fn failed(&self) -> bool {
        self.steps.iter().any(|s| s.outcome == Outcome::Fail)
    }

    pub fn count(&self, outcome: Outcome) -> usize {
        self.steps.iter().filter(|s| s.outcome == outcome).count()
    }

    /// The report as Markdown.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# Terrarium compatibility report\n");
        let _ = writeln!(
            out,
            "Run: {} ({}). Pass: {}. Fail: {}. Skip: {}.\n",
            self.started.format("%Y-%m-%d %H:%M:%S UTC"),
            self.mode,
            self.count(Outcome::Pass),
            self.count(Outcome::Fail),
            self.count(Outcome::Skip),
        );
        let _ = writeln!(out, "| Harness | Step | Result | Time | Evidence |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for s in &self.steps {
            let time = s
                .time
                .map(|t| format!("{:.1} s", t.as_secs_f64()))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                s.harness,
                s.name,
                s.outcome.as_str(),
                time,
                cell(&s.evidence)
            );
        }
        if !self.turns.is_empty() {
            let _ = writeln!(out, "\nModel turns typed:");
            for (h, n) in &self.turns {
                let _ = writeln!(out, "- {h}: {n}");
            }
        }
        if !self.notes.is_empty() {
            let _ = writeln!(out, "\nNotes:");
            for n in &self.notes {
                let _ = writeln!(out, "- {}", n.replace('\n', " "));
            }
        }
        out
    }

    /// Write the Markdown to `path`, and make the parent directory.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_markdown())
    }
}

/// Text that is safe in one table cell.
fn cell(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = one_line.replace('|', "\\|");
    const MAX: usize = 400;
    if s.chars().count() > MAX {
        s = s.chars().take(MAX).collect::<String>() + "…";
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_has_one_row_per_step_and_escapes_pipes() {
        let mut r = Report::new("dry run");
        r.pass(HarnessId::Claude, "version", "2.1.310 in >=2.1.310");
        r.add(
            HarnessId::Codex,
            "fresh_start",
            Outcome::Fail,
            Some(Duration::from_millis(1500)),
            "a | b\nc",
        );
        r.set_turns(HarnessId::Claude, 3);
        let md = r.to_markdown();
        assert!(md.contains("| claude | version | pass |  | 2.1.310 in >=2.1.310 |"));
        assert!(md.contains("| codex | fresh_start | FAIL | 1.5 s | a \\| b c |"));
        assert!(md.contains("- claude: 3"));
        assert!(r.failed());
    }
}
