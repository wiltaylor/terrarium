//! Terrarium's adapter compatibility suite (PLAN §23.5, "Compatibility").
//!
//! The suite runs the real harnesses at their pinned versions through the
//! adapters and the in-process [`terrarium_runner::Runner`], with no daemon
//! and sandbox mode `none`, on the operator's real harness homes. One
//! scenario per harness:
//!
//! 1. `version`: the installed version (any version; the suite is the way
//!    to test a new release); `auth`: the harness is logged in (else the rest is skipped).
//! 2. `fresh_start`: materialize, launch, spawn, and reach `idle` with no
//!    dialog within 30 s.
//! 3. `turn_with_hooks`: one turn, the hooks, the session ID, the payload
//!    fields.
//! 4. `system_prompt_applied`: a sentinel in the rendered prompt comes back.
//! 5. `resume`: exit, start with `resume`, one more turn that needs the
//!    history.
//! 6. `usage_parsed`: the adapter's usage parser reads the session file.
//! 7. `leftovers`: the run's files in the shared home are removed; the
//!    rest is reported.
//!
//! At most [`scenario::MAX_TURNS`] model turns per harness per run. The
//! dry run (`TERRARIUM_COMPAT_DRY_RUN=1`, and the test that is not
//! ignored) builds every launch spec, materializes into a temp home, and
//! reads the versions; it starts no harness. See `docs/compat-suite.md`.

pub mod harness;
pub mod leftovers;
pub mod recording;
pub mod report;
pub mod scenario;

use std::path::{Path, PathBuf};

use terrarium_core::HarnessId;

pub use harness::PiSettings;
pub use report::{Outcome, Report};
pub use scenario::run_suite;

/// Every harness, in report order.
/// A real run skips grok: it needs a signed-in check (TER-1192).
pub const ALL: [HarnessId; 5] = [
    HarnessId::Claude,
    HarnessId::Codex,
    HarnessId::Copilot,
    HarnessId::Pi,
    HarnessId::Grok,
];

/// The settings of one run, from the env (see [`Settings::from_env`]).
#[derive(Debug, Clone)]
pub struct Settings {
    /// Build specs and materialize into a temp home only. No harness starts.
    pub dry_run: bool,
    pub harnesses: Vec<HarnessId>,
    /// The `terrarium` CLI that the hooks call.
    pub cli: PathBuf,
    /// The cargo target directory: the report goes here.
    pub target_dir: PathBuf,
    homes: Vec<(HarnessId, PathBuf)>,
    models: Vec<(HarnessId, String)>,
    /// `Some` only when `TERRARIUM_COMPAT_PI=1` and the provider and model
    /// are set.
    pub pi: Option<PiSettings>,
    /// Why pi does not run, when [`Settings::pi`] is `None`.
    pub pi_off_reason: String,
    /// The CopilotCustomSystemPrompt package directory, if any.
    pub copilot_prompt_ext: Option<PathBuf>,
    /// Run a harness whose shared home has the operator's own hooks
    /// (`TERRARIUM_COMPAT_ALLOW_USER_HOOKS=1`). Off by default: in sandbox
    /// mode `none` those hooks fire in the test agent.
    pub allow_user_hooks: bool,
}

/// The default model per harness: the cheapest one each spike used.
pub fn default_model(h: HarnessId) -> &'static str {
    match h {
        HarnessId::Claude => "haiku",
        HarnessId::Codex => "gpt-6-luna",
        HarnessId::Copilot => "claude-haiku-4.5",
        HarnessId::Pi | HarnessId::Grok => "",
    }
}

fn env_name(h: HarnessId) -> String {
    h.as_str().to_ascii_uppercase()
}

impl Settings {
    /// Read the settings from `env`:
    ///
    /// | Var | Default |
    /// |---|---|
    /// | `TERRARIUM_COMPAT_DRY_RUN` | unset (`1` = dry run) |
    /// | `TERRARIUM_COMPAT_HARNESSES` | `all` (a comma list: `claude,codex`) |
    /// | `TERRARIUM_COMPAT_CLI` | `<target>/debug/terrarium` |
    /// | `TERRARIUM_COMPAT_<H>_HOME` | `~/.claude`, `~/.codex`, `~/.copilot`, `~/.pi/agent` |
    /// | `TERRARIUM_COMPAT_<H>_MODEL` | [`default_model`] |
    /// | `TERRARIUM_COMPAT_PI` | unset (`1` = run pi) |
    /// | `TERRARIUM_COMPAT_PI_PROVIDER` | none (an API-key provider, for example `openai`) |
    /// | `TERRARIUM_COMPAT_COPILOT_PROMPT_EXT` | none |
    /// | `TERRARIUM_COMPAT_ALLOW_USER_HOOKS` | unset (`1` = run with the operator's hooks) |
    pub fn from_env(target_dir: PathBuf, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let var = |k: &str| env(k).filter(|v| !v.trim().is_empty());
        let home = var("HOME").map(PathBuf::from).unwrap_or_default();
        let harnesses = parse_harnesses(var("TERRARIUM_COMPAT_HARNESSES").as_deref());
        let homes = ALL
            .iter()
            .map(|h| {
                let default = match h {
                    HarnessId::Claude => home.join(".claude"),
                    HarnessId::Codex => home.join(".codex"),
                    HarnessId::Copilot => home.join(".copilot"),
                    HarnessId::Pi => home.join(".pi/agent"),
                    HarnessId::Grok => home.join(".grok"),
                };
                let path = var(&format!("TERRARIUM_COMPAT_{}_HOME", env_name(*h)))
                    .map(PathBuf::from)
                    .unwrap_or(default);
                (*h, path)
            })
            .collect();
        let models = ALL
            .iter()
            .map(|h| {
                let m = var(&format!("TERRARIUM_COMPAT_{}_MODEL", env_name(*h)))
                    .unwrap_or_else(|| default_model(*h).to_owned());
                (*h, m)
            })
            .collect();
        let pi_on = var("TERRARIUM_COMPAT_PI").as_deref() == Some("1");
        let provider = var("TERRARIUM_COMPAT_PI_PROVIDER");
        let pi_model = var("TERRARIUM_COMPAT_PI_MODEL");
        let (pi, pi_off_reason) = match (pi_on, provider, pi_model) {
            (false, _, _) => (
                None,
                "pi is off by default: this host has no pi login, and pi cannot spend the \
                 Claude plan (docs/compat.md). Set TERRARIUM_COMPAT_PI=1, \
                 TERRARIUM_COMPAT_PI_PROVIDER, TERRARIUM_COMPAT_PI_MODEL, and the API key"
                    .to_owned(),
            ),
            (true, Some(provider), Some(model)) => {
                (Some(PiSettings { provider, model }), String::new())
            }
            (true, _, _) => (
                None,
                "TERRARIUM_COMPAT_PI=1 needs TERRARIUM_COMPAT_PI_PROVIDER and \
                 TERRARIUM_COMPAT_PI_MODEL"
                    .to_owned(),
            ),
        };
        let cli = var("TERRARIUM_COMPAT_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| target_dir.join("debug/terrarium"));
        Self {
            dry_run: var("TERRARIUM_COMPAT_DRY_RUN").as_deref() == Some("1"),
            harnesses,
            cli,
            target_dir,
            homes,
            models,
            pi,
            pi_off_reason,
            copilot_prompt_ext: var("TERRARIUM_COMPAT_COPILOT_PROMPT_EXT").map(PathBuf::from),
            allow_user_hooks: var("TERRARIUM_COMPAT_ALLOW_USER_HOOKS").as_deref() == Some("1"),
        }
    }

    /// The shared home of `h`.
    pub fn home(&self, h: HarnessId) -> PathBuf {
        self.homes
            .iter()
            .find(|(x, _)| *x == h)
            .map(|(_, p)| p.clone())
            .unwrap_or_default()
    }

    /// The model of `h` (the pi model comes from [`PiSettings`]).
    pub fn model(&self, h: HarnessId) -> String {
        if h == HarnessId::Pi {
            return self
                .pi
                .as_ref()
                .map(|p| p.model.clone())
                .unwrap_or_default();
        }
        self.models
            .iter()
            .find(|(x, _)| *x == h)
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    }

    /// Where the report goes.
    pub fn report_path(&self) -> PathBuf {
        let name = if self.dry_run {
            "compat-report-dry.md"
        } else {
            "compat-report.md"
        };
        self.target_dir.join(name)
    }
}

/// `all`, or a comma list of harness names. Unknown names are ignored.
pub fn parse_harnesses(text: Option<&str>) -> Vec<HarnessId> {
    let text = text.unwrap_or("all").trim();
    if text.is_empty() || text == "all" {
        return ALL.to_vec();
    }
    ALL.iter()
        .copied()
        .filter(|h| text.split(',').any(|w| w.trim() == h.as_str()))
        .collect()
}

/// The cargo target directory of a test binary: the parent of
/// `CARGO_TARGET_TMPDIR`.
pub fn target_dir_of(tmpdir: &Path) -> PathBuf {
    tmpdir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| tmpdir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_list() {
        assert_eq!(parse_harnesses(None), ALL.to_vec());
        assert_eq!(parse_harnesses(Some("claude")), vec![HarnessId::Claude]);
        assert_eq!(
            parse_harnesses(Some("copilot, codex")),
            vec![HarnessId::Codex, HarnessId::Copilot]
        );
    }

    #[test]
    fn settings_defaults_and_pi_gate() {
        let env = |k: &str| match k {
            "HOME" => Some("/home/u".to_owned()),
            "TERRARIUM_COMPAT_PI" => Some("1".to_owned()),
            _ => None,
        };
        let s = Settings::from_env(PathBuf::from("/t"), &env);
        assert!(!s.dry_run);
        assert_eq!(s.home(HarnessId::Claude), PathBuf::from("/home/u/.claude"));
        assert_eq!(s.home(HarnessId::Pi), PathBuf::from("/home/u/.pi/agent"));
        assert_eq!(s.model(HarnessId::Claude), "haiku");
        assert_eq!(s.cli, PathBuf::from("/t/debug/terrarium"));
        assert!(s.pi.is_none(), "pi needs a provider and a model");
        assert!(s.pi_off_reason.contains("PROVIDER"));
        assert_eq!(s.report_path(), PathBuf::from("/t/compat-report.md"));
    }
}
