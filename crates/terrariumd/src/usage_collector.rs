//! Local usage accounting (PLAN §16.2), and the settings of the usage
//! subsystem.
//!
//! The collector subscribes to the event hub. When an agent's harness
//! reports its session (`session_started` with a `transcript_path`), the
//! collector opens a [`UsageSource`] for that harness and reads the new
//! usage records on each `turn_end`, at the end of the session, and on a
//! timer. Each record becomes one `usage_samples` row, tagged with the
//! agent's project, subscription, and model, and with the `seq` of the
//! agent's last `turn_start` event. The store drops a record whose
//! `(agent, message_id)` it has already (the dedupe rule of
//! `docs/compat.md`), so a tail that reads a resumed session file again
//! counts nothing twice.
//!
//! The collector emits a `usage.sample` event per agent at most once per
//! `sample_throttle`, with the totals since the last one, for the
//! dashboard's Usage strip.
//!
//! Only Claude has a source in this phase. A Codex, Copilot, or pi source
//! implements [`UsageSourceFactory`] and goes into [`default_sources`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use terrarium_adapter_claude::usage::{TranscriptTail, UsageRecord};
use terrarium_core::HarnessId;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::api::Shared;
use crate::events::{EventDraft, StoredEvent};
use crate::login_session::Logins;
use crate::probes::{HttpGet, ReqwestHttp};
use crate::sandbox::{SANDBOX_HOME, home_rel};
use crate::store::agents;
use crate::store::usage::{self as ustore, NewUsage, Totals};

/// The settings of usage accounting, quota probes, and login sessions.
#[derive(Clone)]
pub struct UsageConfig {
    /// The shared harness homes on this machine. A missing one is
    /// `/auth/<harness>`.
    pub homes: HashMap<HarnessId, PathBuf>,
    /// The program of a harness for status, login, and logout commands. A
    /// missing one is the harness name on `PATH`. Tests use fakes.
    pub programs: HashMap<HarnessId, String>,
    /// How often each subscription's probe runs.
    pub probe_interval: Duration,
    /// How often the scheduler looks for due probes and expired holds.
    pub probe_tick: Duration,
    /// How often the collector reads the session files of running agents.
    pub poll_interval: Duration,
    /// At most one `usage.sample` event per agent in this time.
    pub sample_throttle: Duration,
    /// A login session ends after this time (PLAN §16.1: ten minutes).
    pub login_timeout: Duration,
    /// Run the harness status commands once after the start, and record
    /// the logins found (PLAN §16.1).
    pub detect_on_start: bool,
    pub http: Arc<dyn HttpGet>,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            homes: HashMap::new(),
            programs: HashMap::new(),
            probe_interval: Duration::from_secs(300),
            probe_tick: Duration::from_secs(30),
            poll_interval: Duration::from_secs(15),
            sample_throttle: Duration::from_secs(10),
            login_timeout: Duration::from_secs(600),
            detect_on_start: false,
            http: Arc::new(ReqwestHttp::new()),
        }
    }
}

impl UsageConfig {
    /// The defaults, with the homes of `TERRARIUM_HARNESS_HOME`
    /// (`claude=/path,codex=/path`).
    pub fn from_env() -> Self {
        let homes = std::env::var("TERRARIUM_HARNESS_HOME")
            .ok()
            .map(|v| parse_homes(&v))
            .unwrap_or_default();
        Self {
            homes,
            ..Self::default()
        }
    }

    pub fn home(&self, h: HarnessId) -> PathBuf {
        self.homes
            .get(&h)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(h.auth_dir()))
    }

    pub fn program(&self, h: HarnessId) -> String {
        self.programs
            .get(&h)
            .cloned()
            .unwrap_or_else(|| h.program().to_owned())
    }
}

/// Parse `claude=/a,codex=/b`. Bad entries are skipped.
pub fn parse_homes(v: &str) -> HashMap<HarnessId, PathBuf> {
    v.split(',')
        .filter_map(|e| {
            let (h, dir) = e.split_once('=')?;
            Some((h.trim().parse().ok()?, PathBuf::from(dir.trim())))
        })
        .collect()
}

/// The usage subsystem's shared state, on `AppState::usage`.
pub struct UsageRuntime {
    pub config: UsageConfig,
    pub logins: Logins,
}

impl UsageRuntime {
    pub fn new(config: UsageConfig) -> Self {
        Self {
            config,
            logins: Logins::default(),
        }
    }
}

impl Default for UsageRuntime {
    fn default() -> Self {
        Self::new(UsageConfig::default())
    }
}

/// What the collector knows about one agent's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub agent: String,
    pub name: String,
    pub project: String,
    pub harness: HarnessId,
    pub subscription: Option<String>,
    pub model: Option<String>,
    /// The harness session id.
    pub session: String,
    /// The session file, as the hook reported it (a sandbox path).
    pub transcript: Option<PathBuf>,
}

/// One API response from a session file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceSample {
    pub message_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub at: Option<DateTime<Utc>>,
}

/// Reads one agent's usage as the harness writes it.
pub trait UsageSource: Send {
    /// The responses that are new since the last poll. A source may give a
    /// response again; the store drops repeats.
    fn poll(&mut self) -> Result<Vec<SourceSample>, String>;
}

/// Makes a [`UsageSource`] for the sessions of one harness.
pub trait UsageSourceFactory: Send + Sync {
    fn harness(&self) -> HarnessId;
    /// `home` is the shared harness home on this machine. `None` when the
    /// session has nothing to read.
    fn open(&self, session: &SessionInfo, home: &Path) -> Option<Box<dyn UsageSource>>;
}

/// The path of a session file on this machine. A path that the harness
/// reported from inside the sandbox (`/home/agent/.claude/...`) maps into
/// the shared home.
pub fn host_path(reported: &Path, harness: HarnessId, home: &Path) -> PathBuf {
    if reported.exists() {
        return reported.to_owned();
    }
    let inside = Path::new(SANDBOX_HOME).join(home_rel(harness));
    match reported.strip_prefix(&inside) {
        Ok(rest) => home.join(rest),
        Err(_) => reported.to_owned(),
    }
}

/// Claude Code: tail the session JSONL with the adapter's
/// [`TranscriptTail`]. Each `assistant` record with a new `message.id` is
/// one sample.
pub struct ClaudeSource {
    tail: TranscriptTail,
}

impl ClaudeSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            tail: TranscriptTail::new(path),
        }
    }
}

impl UsageSource for ClaudeSource {
    fn poll(&mut self) -> Result<Vec<SourceSample>, String> {
        let records = self.tail.poll().map_err(|e| e.to_string())?;
        Ok(records
            .into_iter()
            .filter_map(|r| match r {
                UsageRecord::Sample(s) => Some(SourceSample {
                    model: s.model.clone(),
                    input_tokens: s.input_tokens,
                    output_tokens: s.output_tokens,
                    cache_read: s.cache_read_input_tokens,
                    cache_write: s.cache_creation_input_tokens,
                    at: s.at,
                    message_id: s.message_id,
                }),
                UsageRecord::Cost { .. } => None,
            })
            .collect())
    }
}

pub struct ClaudeSourceFactory;

impl UsageSourceFactory for ClaudeSourceFactory {
    fn harness(&self) -> HarnessId {
        HarnessId::Claude
    }

    fn open(&self, session: &SessionInfo, home: &Path) -> Option<Box<dyn UsageSource>> {
        let reported = session.transcript.as_ref()?;
        let path = host_path(reported, HarnessId::Claude, home);
        debug!(agent = %session.agent, path = %path.display(), "tailing the Claude session file");
        Some(Box::new(ClaudeSource::new(path)))
    }
}

/// The usage sources of this phase.
pub fn default_sources() -> Vec<Arc<dyn UsageSourceFactory>> {
    vec![Arc::new(ClaudeSourceFactory)]
}

struct Tracked {
    info: SessionInfo,
    source: Box<dyn UsageSource>,
    turn_seq: Option<i64>,
    pending: Totals,
    last_emit: Option<Instant>,
}

/// The collector's state. The task in [`spawn`] drives it; tests call it
/// step by step.
pub struct Collector {
    state: Shared,
    sources: Vec<Arc<dyn UsageSourceFactory>>,
    agents: HashMap<String, Tracked>,
}

fn time_str(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Collector {
    pub fn new(state: Shared, sources: Vec<Arc<dyn UsageSourceFactory>>) -> Self {
        Self {
            state,
            sources,
            agents: HashMap::new(),
        }
    }

    /// The agents with an open source.
    pub fn tracked(&self) -> Vec<String> {
        let mut v: Vec<String> = self.agents.keys().cloned().collect();
        v.sort();
        v
    }

    /// Handle one event of the hub.
    pub async fn on_event(&mut self, e: &StoredEvent) {
        let Some(agent) = e.agent_id.clone() else {
            return;
        };
        match e.kind.as_str() {
            "session_started" => self.open(&agent, &e.data).await,
            "turn_start" => {
                if let Some(t) = self.agents.get_mut(&agent) {
                    t.turn_seq = Some(e.seq);
                }
            }
            "turn_end" => self.poll(&agent, false).await,
            "session_ended" | "exit" => self.close(&agent).await,
            "status" => {
                let s = e.data.get("status").and_then(Value::as_str);
                if matches!(s, Some("stopped" | "error")) {
                    self.close(&agent).await;
                }
            }
            _ => {}
        }
    }

    async fn open(&mut self, agent: &str, data: &Value) {
        if self.agents.contains_key(agent) {
            self.close(agent).await;
        }
        let key = agent.to_owned();
        let view = match self.state.db.read(move |c| agents::get(c, &key)).await {
            Ok(v) => v,
            Err(e) => {
                warn!(agent, "usage: cannot read the agent: {e}");
                return;
            }
        };
        let a = view.agent;
        let info = SessionInfo {
            agent: agent.to_owned(),
            name: a.name,
            project: a.project,
            harness: a.harness,
            subscription: a.subscription,
            model: a.model,
            session: data
                .get("harness_session")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            transcript: data
                .get("transcript_path")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        };
        let Some(factory) = self.sources.iter().find(|f| f.harness() == info.harness) else {
            debug!(agent, harness = %info.harness, "usage: no source for this harness");
            return;
        };
        let home = self.state.usage.config.home(info.harness);
        let Some(source) = factory.open(&info, &home) else {
            return;
        };
        let key = agent.to_owned();
        let turn_seq = self
            .state
            .db
            .read(move |c| ustore::last_turn_seq(c, &key))
            .await
            .unwrap_or(None);
        self.agents.insert(
            agent.to_owned(),
            Tracked {
                info,
                source,
                turn_seq,
                pending: Totals::default(),
                last_emit: None,
            },
        );
        // A resumed session file has history: read it now.
        self.poll(agent, false).await;
    }

    async fn close(&mut self, agent: &str) {
        self.poll(agent, true).await;
        self.agents.remove(agent);
    }

    /// Read every tracked agent's new usage.
    pub async fn poll_all(&mut self) {
        let keys: Vec<String> = self.agents.keys().cloned().collect();
        for k in keys {
            self.poll(&k, false).await;
        }
    }

    /// Read one agent's new usage and store it. `flush` emits the pending
    /// `usage.sample` event now.
    async fn poll(&mut self, agent: &str, flush: bool) {
        let throttle = self.state.usage.config.sample_throttle;
        let Some(t) = self.agents.get_mut(agent) else {
            return;
        };
        let samples = match t.source.poll() {
            Ok(s) => s,
            Err(e) => {
                warn!(agent, "usage: cannot read the session: {e}");
                Vec::new()
            }
        };
        let rows: Vec<NewUsage> = samples
            .into_iter()
            .map(|s| NewUsage {
                agent: t.info.agent.clone(),
                project: t.info.project.clone(),
                subscription: t.info.subscription.clone(),
                model: if s.model.is_empty() {
                    t.info.model.clone().unwrap_or_default()
                } else {
                    s.model
                },
                input_tokens: s.input_tokens,
                output_tokens: s.output_tokens,
                cache_read: s.cache_read,
                cache_write: s.cache_write,
                message_id: s.message_id,
                turn_seq: t.turn_seq,
                session: Some(t.info.session.clone()).filter(|s| !s.is_empty()),
                at: s.at.map(time_str),
            })
            .collect();
        if !rows.is_empty() {
            match self
                .state
                .db
                .write("system", move |tx| ustore::insert(tx, &rows))
                .await
            {
                Ok(added) => {
                    for s in &added {
                        t.pending.add(s);
                    }
                    if !added.is_empty() {
                        let project = t.info.project.clone();
                        if let Err(e) =
                            crate::quota_policy::check_budgets(&self.state, &project).await
                        {
                            warn!(project, "usage: budget check failed: {e}");
                        }
                    }
                }
                Err(e) => warn!(agent, "usage: cannot store samples: {e}"),
            }
        }
        let due = t.last_emit.is_none_or(|at| at.elapsed() >= throttle);
        if t.pending.messages > 0 && (flush || due) {
            let totals = std::mem::take(&mut t.pending);
            t.last_emit = Some(Instant::now());
            let info = t.info.clone();
            let result = self
                .state
                .db
                .write("system", move |tx| {
                    tx.emit(
                        EventDraft::new(
                            "usage.sample",
                            format!("{}: +{} tokens", info.name, totals.tokens()),
                            json!({
                                "subscription": info.subscription,
                                "harness": info.harness,
                                "model": info.model,
                                "input_tokens": totals.input_tokens,
                                "output_tokens": totals.output_tokens,
                                "cache_read": totals.cache_read,
                                "cache_write": totals.cache_write,
                                "messages": totals.messages,
                            }),
                        )
                        .agent(info.agent.clone(), info.name.clone())
                        .project(info.project.clone()),
                    )
                })
                .await;
            if let Err(e) = result {
                warn!(agent, "usage: cannot emit usage.sample: {e}");
            }
        }
    }
}

/// Start the collector: the event hub, and a timer.
pub fn spawn(state: Shared) -> JoinHandle<()> {
    let mut rx = state.db.hub().subscribe();
    let interval = state.usage.config.poll_interval;
    let mut c = Collector::new(state, default_sources());
    tokio::spawn(async move {
        info!("usage collector started");
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(e) => c.on_event(&e).await,
                    Err(RecvError::Lagged(n)) => warn!("the usage collector missed {n} events"),
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => c.poll_all().await,
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn homes_parse() {
        let h = parse_homes("claude=/a, codex=/b,bad,nope=/c");
        assert_eq!(h.len(), 2);
        assert_eq!(h[&HarnessId::Codex], PathBuf::from("/b"));
        let mut cfg = UsageConfig::default();
        assert_eq!(cfg.home(HarnessId::Pi), PathBuf::from("/auth/pi"));
        cfg.programs.insert(HarnessId::Claude, "/x/claude".into());
        assert_eq!(cfg.program(HarnessId::Claude), "/x/claude");
        assert_eq!(cfg.program(HarnessId::Codex), "codex");
    }

    #[test]
    fn sandbox_paths_map_into_the_home() {
        let p = host_path(
            Path::new("/home/agent/.claude/projects/-home-agent-work/s.jsonl"),
            HarnessId::Claude,
            Path::new("/auth/claude"),
        );
        assert_eq!(
            p,
            PathBuf::from("/auth/claude/projects/-home-agent-work/s.jsonl")
        );
        let other = Path::new("/elsewhere/s.jsonl");
        assert_eq!(
            host_path(other, HarnessId::Claude, Path::new("/auth/claude")),
            other
        );
    }

    #[test]
    fn claude_source_reads_new_samples_once() {
        let fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../adapters/claude/tests/fixtures/session.jsonl"
        ))
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut src = ClaudeSource::new(&path);
        assert!(src.poll().unwrap().is_empty());
        let (head, tail) = fixture.split_at(fixture.len() / 2);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(head.as_bytes()).unwrap();
        let a = src.poll().unwrap();
        f.write_all(tail.as_bytes()).unwrap();
        let b = src.poll().unwrap();
        let all: Vec<_> = a.into_iter().chain(b).collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].message_id, "msg_011CgLZ4D8Uzva9rUiX7ME9A");
        assert_eq!((all[0].input_tokens, all[0].output_tokens), (2, 45));
        assert_eq!((all[0].cache_write, all[0].cache_read), (36712, 0));
        assert_eq!(all[1].cache_read, 36712);
    }
}
