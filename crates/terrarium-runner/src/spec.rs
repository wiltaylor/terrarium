//! The spec file of a runner process (`terrarium-runner --spec <file>`): the
//! launch spec, the runner settings, and the adapter to use.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use terrarium_core::{AgentId, HarnessId, LaunchSpec};

use crate::config::RunnerConfig;
use crate::error::{Result, RunnerError};
use crate::ready::DefaultReadyDetector;

/// Which adapter the runner process uses for hooks, the screen, and keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    /// The adapter of `harness`.
    #[default]
    Harness,
    /// The test adapter: Claude Code hook names (`UserPromptSubmit`, `Stop`,
    /// and `Notification` as a question), no screen hints. For the fake TUI
    /// in the tests only.
    Fake,
}

/// The adapter settings that matter in the runner process. The daemon
/// builds the launch spec itself, so only the paths that the hook mapping
/// reads are here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterOptions {
    /// The harness home as the harness sees it (`CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`, `COPILOT_HOME`, the pi agent dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<PathBuf>,
    /// The same directory as the runner process sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_config_dir: Option<PathBuf>,
}

/// The settings of `DefaultReadyDetector`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyDetectorSpec {
    pub quiet_ms: u64,
    /// A regex for the last non-empty screen line. `None`: a prompt mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// The content of the spec file. Each field that is not given takes the
/// default of `RunnerConfig::new`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerSpec {
    pub harness: HarnessId,
    #[serde(default)]
    pub adapter: AdapterKind,
    #[serde(default)]
    pub adapter_options: AdapterOptions,
    /// The final command: for a sandbox, the full `bwrap` argv.
    pub launch: LaunchSpec,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default = "d_rows")]
    pub rows: u16,
    #[serde(default = "d_cols")]
    pub cols: u16,
    #[serde(default = "d_stall")]
    pub stall_timeout_ms: u64,
    #[serde(default = "d_submit")]
    pub submit_timeout_ms: u64,
    #[serde(default = "d_attempts")]
    pub submit_attempts: u32,
    #[serde(default = "d_log")]
    pub log_max_bytes: u64,
    #[serde(default = "d_tick")]
    pub tick_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_detector: Option<ReadyDetectorSpec>,
    /// How long an agent RPC request waits for a daemon connection before
    /// it gets the `unavailable` tunnel error.
    #[serde(default = "d_rpc_queue")]
    pub rpc_queue_timeout_ms: u64,
}

fn defaults() -> RunnerConfig {
    RunnerConfig::new(AgentId::default(), "/", "/")
}
fn d_rows() -> u16 {
    defaults().rows
}
fn d_cols() -> u16 {
    defaults().cols
}
fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
fn d_stall() -> u64 {
    ms(defaults().stall_timeout)
}
fn d_submit() -> u64 {
    ms(defaults().submit_timeout)
}
fn d_attempts() -> u32 {
    defaults().submit_attempts
}
fn d_log() -> u64 {
    defaults().log_max_bytes
}
fn d_tick() -> u64 {
    ms(defaults().tick)
}
/// The default of [`RunnerSpec::rpc_queue_timeout_ms`].
pub const DEFAULT_RPC_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
fn d_rpc_queue() -> u64 {
    ms(DEFAULT_RPC_QUEUE_TIMEOUT)
}

impl RunnerSpec {
    /// A spec from the settings of an in-process runner. The ready detector
    /// of `config` is code, so it does not carry over: set
    /// [`RunnerSpec::ready_detector`] for a `DefaultReadyDetector`.
    pub fn from_config(config: &RunnerConfig, harness: HarnessId, launch: LaunchSpec) -> Self {
        Self {
            harness,
            adapter: AdapterKind::Harness,
            adapter_options: AdapterOptions::default(),
            launch,
            name: config.name.clone(),
            project: config.project.clone(),
            roles: config.roles.clone(),
            rows: config.rows,
            cols: config.cols,
            stall_timeout_ms: ms(config.stall_timeout),
            submit_timeout_ms: ms(config.submit_timeout),
            submit_attempts: config.submit_attempts,
            log_max_bytes: config.log_max_bytes,
            tick_ms: ms(config.tick),
            ready_detector: None,
            rpc_queue_timeout_ms: d_rpc_queue(),
        }
    }

    /// The runner settings for `agent`.
    pub fn to_config(
        &self,
        agent: AgentId,
        state_dir: &Path,
        runtime_dir: &Path,
    ) -> Result<RunnerConfig> {
        let mut c = RunnerConfig::new(agent, state_dir, runtime_dir);
        c.name = self.name.clone();
        c.project = self.project.clone();
        c.roles = self.roles.clone();
        c.rows = self.rows;
        c.cols = self.cols;
        c.stall_timeout = Duration::from_millis(self.stall_timeout_ms);
        c.submit_timeout = Duration::from_millis(self.submit_timeout_ms);
        c.submit_attempts = self.submit_attempts;
        c.log_max_bytes = self.log_max_bytes;
        c.tick = Duration::from_millis(self.tick_ms.max(1));
        if let Some(d) = &self.ready_detector {
            let quiet = Duration::from_millis(d.quiet_ms);
            let det = match &d.pattern {
                Some(p) => DefaultReadyDetector::new(quiet, p).map_err(|e| {
                    RunnerError::Other(format!("ready_detector.pattern `{p}`: {e}"))
                })?,
                None => DefaultReadyDetector {
                    quiet,
                    pattern: None,
                },
            };
            c.ready_detector = Some(Arc::new(det));
        }
        Ok(c)
    }

    pub fn rpc_queue_timeout(&self) -> Duration {
        Duration::from_millis(self.rpc_queue_timeout_ms)
    }

    /// Read a spec file.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| RunnerError::io("read", path, e))?;
        serde_json::from_str(&text)
            .map_err(|e| RunnerError::Other(format!("{}: {e}", path.display())))
    }

    /// Write the spec file with mode `0600`: the launch env can carry
    /// settings that other users must not read.
    pub fn write(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|e| RunnerError::Other(e.to_string()))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| RunnerError::io("open", path, e))?;
        f.write_all(&bytes)
            .map_err(|e| RunnerError::io("write", path, e))
    }

    /// Replace the program and the arguments with `argv` (from
    /// `--sandbox-argv-file`).
    pub fn set_argv(&mut self, argv: Vec<String>) -> Result<()> {
        let mut it = argv.into_iter();
        let program = it
            .next()
            .ok_or_else(|| RunnerError::Other("the sandbox argv is empty".into()))?;
        self.launch.program = program;
        self.launch.args = it.collect();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_spec_takes_the_defaults() {
        let spec: RunnerSpec = serde_json::from_value(serde_json::json!({
            "harness": "claude",
            "launch": {"program": "claude", "cwd": "/tmp"}
        }))
        .expect("spec");
        assert_eq!(spec.adapter, AdapterKind::Harness);
        let c = spec
            .to_config(AgentId::new(), Path::new("/s"), Path::new("/r"))
            .expect("config");
        let d = defaults();
        assert_eq!((c.rows, c.cols), (d.rows, d.cols));
        assert_eq!(c.submit_timeout, d.submit_timeout);
        assert_eq!(spec.rpc_queue_timeout(), DEFAULT_RPC_QUEUE_TIMEOUT);
    }

    #[test]
    fn from_config_round_trips() {
        let mut cfg = RunnerConfig::new(AgentId::new(), "/s", "/r");
        cfg.name = "alice".into();
        cfg.submit_attempts = 3;
        let spec = RunnerSpec::from_config(&cfg, HarnessId::Codex, LaunchSpec::new("codex", "/"));
        let json = serde_json::to_string(&spec).expect("json");
        let back: RunnerSpec = serde_json::from_str(&json).expect("back");
        assert_eq!(back, spec);
        let c = back
            .to_config(cfg.agent, Path::new("/s"), Path::new("/r"))
            .expect("config");
        assert_eq!(c.name, "alice");
        assert_eq!(c.submit_attempts, 3);
    }
}
