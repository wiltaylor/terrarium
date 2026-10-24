//! The adapter interface. One adapter per harness. See PLAN §7.

use std::path::Path;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::config::SubscriptionDef;
use crate::error::Result;
use crate::event::Event;
use crate::harness::HarnessId;
use crate::hook::HookPayload;
use crate::ids::HarnessSessionId;
use crate::launch::{FileOverlay, LaunchSpec};
use crate::model::Agent;

/// The parsed terminal screen of a harness.
pub type Screen = vt100::Screen;

/// What an adapter can tell from the screen alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScreenHint {
    /// The adapter cannot tell.
    #[default]
    Unknown,
    /// The input prompt shows and accepts a turn.
    PromptReady,
    /// The harness is busy with a turn.
    Busy,
    /// The harness shows a question or a dialog to the user.
    Question,
    /// The adapter knows the prompt does not accept a turn now, but the
    /// screen is not a busy turn or a question either. Examples: a start
    /// screen, text left in the input box, the process has exited. Do not
    /// type, and do not fall back to a generic detector.
    NotReady,
}

/// An answer to a question that the harness shows (see `Event::Question`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Answer {
    /// Pick the option at this index, counted from 0 in screen order.
    Choice(usize),
    /// Allow the request (a permission prompt).
    Allow,
    /// Deny the request, or cancel the question.
    Deny,
}

/// Knowledge of one harness: launch flags, hook mapping, session capture, and
/// how to type a turn. The trait is object safe, so the daemon can hold a
/// `Box<dyn Adapter>` per harness. No method is async: each call is either
/// pure, or a short blocking call that the caller runs off the async runtime.
pub trait Adapter: Send + Sync {
    fn id(&self) -> HarnessId;

    /// The harness versions this adapter was tested against.
    fn tested_version(&self) -> VersionReq;

    /// Run the harness to read its installed version. This blocks.
    fn detect_version(&self) -> Result<Version>;

    /// Command line and env for a fresh start or a resume. Includes the yolo
    /// flags, the system prompt, the hook config, and the quiet-mode env.
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec;

    /// [`Adapter::launch`] for an agent whose subscription the caller has
    /// resolved (`Agent::subscription` is only the name). A harness that
    /// can spend many providers (pi) picks its provider flags from it. The
    /// default ignores the subscription.
    fn launch_for(
        &self,
        agent: &Agent,
        resume: Option<&HarnessSessionId>,
        _subscription: Option<&SubscriptionDef>,
    ) -> LaunchSpec {
        self.launch(agent, resume)
    }

    /// Map a hook payload from the runner socket to events.
    fn hook_event(&self, hook: &HookPayload) -> Vec<Event>;

    /// Heuristics from the screen, for `blocked` detection and for "is the
    /// prompt ready" before typing.
    fn screen_hint(&self, _screen: &Screen) -> ScreenHint {
        ScreenHint::Unknown
    }

    /// Where the harness writes its session id if no hook carries it.
    fn find_session_id(&self, _agent: &Agent) -> Option<HarnessSessionId> {
        None
    }

    /// Keystrokes that submit a user turn. The default is bracketed paste and
    /// then Enter.
    fn type_turn(&self, text: &str) -> Vec<u8> {
        bracketed_paste(text)
    }

    /// Keystrokes that answer the question `request_id`. Empty when the
    /// adapter has no answer path.
    fn answer(&self, _request_id: &str, _answer: &Answer) -> Vec<u8> {
        Vec::new()
    }

    /// Keystrokes that empty the input line of the harness. The runner sends
    /// them before it types a turn again after a delivery that got no
    /// prompt-submit hook, so the old text does not stay in front of the new
    /// text. They must not interrupt a running turn or exit the harness. The
    /// default is Ctrl-U (kill to the start of the line).
    fn clear_input_keys(&self) -> Vec<u8> {
        vec![CTRL_U]
    }

    /// Keystrokes that make an idle harness exit cleanly, for a graceful
    /// stop before a kill. Empty (the default) means "no known keys": the
    /// caller kills the process.
    fn exit_keys(&self) -> Vec<u8> {
        Vec::new()
    }

    /// True when the harness keeps the system prompt of a session from the
    /// moment it made the session, so a resume with a changed prompt still
    /// sends the old one. The supervisor then starts a fresh session (not a
    /// resume) when the rendered prompt changed. The default is false.
    fn prompt_frozen_on_resume(&self) -> bool {
        false
    }

    /// Write the per-agent files that the harness reads from its home, and
    /// return the overlays that the sandbox must bind. `home` is the shared
    /// harness home on the host (for example `/auth/claude`). The default
    /// writes nothing. This blocks.
    fn materialize_config(&self, _agent: &Agent, _home: &Path) -> Result<Vec<FileOverlay>> {
        Ok(Vec::new())
    }
}

/// Ctrl-U: kill the line before the cursor.
pub const CTRL_U: u8 = 0x15;

/// Wrap `text` in bracketed-paste markers and add a carriage return.
pub fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 13);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out.push(b'\r');
    out
}

/// Parse the first `x.y.z` in the output of `<harness> --version`.
pub fn parse_version_output(output: &str) -> Result<Version> {
    output
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == 'v')
        .find_map(|word| Version::parse(word.trim()).ok())
        .ok_or_else(|| crate::Error::Version(format!("no version in `{}`", output.trim())))
}

/// Run `program --version` and parse the result. This blocks.
pub fn run_version(program: &str) -> Result<Version> {
    let out = std::process::Command::new(program)
        .arg("--version")
        .output()
        .map_err(|e| crate::Error::Version(format!("cannot run {program}: {e}")))?;
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps_text() {
        assert_eq!(bracketed_paste("hi"), b"\x1b[200~hi\x1b[201~\r".to_vec());
    }

    #[test]
    fn version_output_parses() {
        assert_eq!(
            parse_version_output("2.1.310 (Claude Code)\n").expect("claude"),
            Version::new(2, 1, 310)
        );
        assert_eq!(
            parse_version_output("codex-cli 0.160.0").expect("codex"),
            Version::new(0, 160, 0)
        );
        assert!(parse_version_output("nothing here").is_err());
    }

    // The trait must stay object safe.
    #[allow(dead_code)]
    fn assert_object_safe(_: &dyn Adapter) {}
}
