//! What every command gets: the API, the mode, the output, and who "me" is.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::Value;
use terrarium_core::paths;
use tokio::sync::OnceCell;

use crate::api::{Api, CliError};
use crate::output::Out;
use crate::transport::{HttpTransport, Transport};

/// The agent-mode env that the runner sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnv {
    /// `TERRARIUM_AGENT`: the agent ULID.
    pub agent: String,
    /// `TERRARIUM_SOCKET`: the runner socket.
    pub socket: PathBuf,
}

impl AgentEnv {
    /// Agent mode needs both variables.
    pub fn from_env() -> Option<Self> {
        let agent = std::env::var(paths::ENV_AGENT)
            .ok()
            .filter(|v| !v.is_empty())?;
        let socket = std::env::var_os(paths::ENV_AGENT_SOCKET).filter(|v| !v.is_empty())?;
        Some(Self {
            agent,
            socket: PathBuf::from(socket),
        })
    }
}

pub struct Ctx {
    pub api: Api,
    /// Set in operator mode: the streams (SSE, WebSocket) need it.
    pub http: Option<HttpTransport>,
    /// Set in agent mode.
    pub agent: Option<AgentEnv>,
    pub json: bool,
    /// The `--project` flag.
    pub project: Option<String>,
    pub out: Out,
    /// Stdin for `-` bodies; tests set it.
    stdin: Option<String>,
    identity: OnceCell<Value>,
    /// The shortest time between two polls of a wait loop.
    pub poll_floor: Duration,
    /// The stored sessions of `terrarium login`. `None` in agent mode.
    pub credentials: Option<crate::credentials::Store>,
    /// Where the bearer token came from.
    pub token_source: crate::credentials::TokenSource,
}

impl Ctx {
    pub fn new(
        transport: Arc<dyn Transport>,
        http: Option<HttpTransport>,
        agent: Option<AgentEnv>,
    ) -> Self {
        Self {
            api: Api::new(transport),
            http,
            agent,
            json: false,
            project: None,
            out: Out::Stdout,
            stdin: None,
            identity: OnceCell::new(),
            poll_floor: Duration::from_secs(2),
            credentials: None,
            token_source: crate::credentials::TokenSource::None,
        }
    }

    /// A context for tests: the output goes to a buffer.
    #[cfg(test)]
    pub fn for_test(transport: Arc<dyn Transport>, agent: Option<AgentEnv>) -> Self {
        let mut c = Self::new(transport, None, agent);
        c.out = Out::Buffer(Default::default());
        c.poll_floor = Duration::ZERO;
        c
    }

    #[cfg(test)]
    pub fn with_stdin(mut self, s: &str) -> Self {
        self.stdin = Some(s.to_owned());
        self
    }

    #[cfg(test)]
    pub fn with_identity(self, v: Value) -> Self {
        let _ = self.identity.set(v);
        self
    }

    pub fn is_agent(&self) -> bool {
        self.agent.is_some()
    }

    /// `whoami` from the runner socket. The runner answers it itself.
    pub async fn identity(&self) -> anyhow::Result<&Value> {
        self.identity
            .get_or_try_init(|| async {
                let Some(agent) = &self.agent else {
                    return Ok(serde_json::json!({"actor": "operator"}));
                };
                let socket = agent.socket.clone();
                tokio::task::spawn_blocking(move || {
                    crate::runner_socket::request(&socket, "whoami", Value::Null)
                })
                .await
                .context("the whoami task failed")?
            })
            .await
    }

    /// My mail address: the agent ULID, or `operator`.
    pub async fn me(&self) -> anyhow::Result<String> {
        let Some(agent) = &self.agent else {
            return Ok("operator".into());
        };
        let id = self.identity().await.ok().and_then(|v| {
            ["agent_id", "agent"]
                .iter()
                .find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_owned))
        });
        Ok(id.unwrap_or_else(|| agent.agent.clone()))
    }

    /// The project: an explicit name, else `--project`, else the agent's own.
    pub async fn project_or(&self, name: Option<&str>) -> anyhow::Result<String> {
        if let Some(n) = name.or(self.project.as_deref()) {
            return Ok(n.to_owned());
        }
        if self.is_agent()
            && let Some(p) = self
                .identity()
                .await?
                .get("project")
                .and_then(Value::as_str)
        {
            return Ok(p.to_owned());
        }
        Err(CliError::usage(
            "name a project with --project <name> (or as the argument)",
        ))
    }

    /// A text argument: `-` reads stdin.
    pub fn text(&self, arg: &str) -> anyhow::Result<String> {
        if arg != "-" {
            return Ok(arg.to_owned());
        }
        if let Some(s) = &self.stdin {
            return Ok(s.clone());
        }
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("cannot read stdin")?;
        Ok(s)
    }

    /// Lines of stdin up to the first blank line or the end (the git
    /// credential helper protocol).
    pub fn stdin_until_blank(&self) -> anyhow::Result<String> {
        if let Some(s) = &self.stdin {
            return Ok(s.clone());
        }
        let mut out = String::new();
        for line in std::io::stdin().lines() {
            let line = line.context("cannot read stdin")?;
            if line.is_empty() {
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn opt_text(&self, arg: Option<&str>) -> anyhow::Result<Option<String>> {
        arg.map(|a| self.text(a)).transpose()
    }

    /// Print `v` as JSON with `--json`, else run the text printer.
    pub fn emit(&self, v: &Value, text: impl FnOnce(&Self, &Value)) -> anyhow::Result<()> {
        if self.json {
            self.out.line(serde_json::to_string_pretty(v)?);
        } else {
            text(self, v);
        }
        Ok(())
    }

    pub fn line(&self, s: impl AsRef<str>) {
        self.out.line(s);
    }
}

/// The deadline of a `wait` command, and the `?wait=` value for each poll.
pub struct Waiter {
    deadline: Instant,
    floor: Duration,
}

/// The daemon holds a long-poll for at most this long.
pub const MAX_WAIT: u64 = 300;

impl Waiter {
    pub fn new(timeout_secs: u64, floor: Duration) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(timeout_secs),
            floor,
        }
    }

    /// The `?wait=` value for the next poll, or `None` when time is up.
    pub fn next(&self) -> Option<u64> {
        let left = self.deadline.checked_duration_since(Instant::now())?;
        if left.is_zero() {
            return None;
        }
        Some(left.as_secs_f64().ceil().clamp(1.0, MAX_WAIT as f64) as u64)
    }

    /// Call after an empty poll. A daemon without long-poll answers at
    /// once; this keeps the loop from spinning.
    pub async fn pause(&self, started: Instant) {
        let spent = started.elapsed();
        if spent < self.floor {
            let left = self.deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep((self.floor - spent).min(left)).await;
        }
    }
}
