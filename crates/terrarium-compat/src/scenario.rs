//! The scenarios. [`run_suite`] runs each selected harness, in the dry
//! mode or for real, and gives the [`Report`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use terrarium_core::adapter::exit_key_writes;
use terrarium_core::config::AgentDef;
use terrarium_core::{
    Adapter, Agent, AgentStatus, Event, EventRecord, FileOverlay, HarnessId, HarnessSessionId,
    LaunchSpec, ScreenHint,
};
use terrarium_runner::{NoHandler, Runner, RunnerConfig};

use crate::Settings;
use crate::harness::{self, Claude, Codex, Copilot, Grok, Harness, Pi};
use crate::leftovers::{self, MountPoint, Snapshot};
use crate::recording::{Captured, RecordingAdapter};
use crate::report::{Outcome, Report};

/// The hard cap of model turns per harness per run.
pub const MAX_TURNS: u32 = 3;
/// The sentinel in the rendered prompt.
pub const SENTINEL: &str = "MARMALADE";
/// The rendered prompt of every agent in the suite.
pub const PROMPT: &str = "You are a Terrarium compatibility test agent. Keep every reply \
                          short, and do not use tools. The secret word is MARMALADE.";
const TURN_PONG: &str = "Reply with exactly: pong";
const TURN_SECRET: &str = "What is the secret word? Reply with the word only.";
const TURN_RESUME: &str = "Two questions. What is the secret word, and what single word did \
                           you reply with to my first message? Reply with just those two words.";
const TURN_RESUME_NO_PROMPT: &str =
    "What single word did you reply with to my first message? Reply with that word only.";

const START_LIMIT: Duration = Duration::from_secs(30);
const DELIVERY_LIMIT: Duration = Duration::from_secs(40);
const TURN_LIMIT: Duration = Duration::from_secs(150);
const IDLE_LIMIT: Duration = Duration::from_secs(20);
const EXIT_LIMIT: Duration = Duration::from_secs(15);

/// The env vars that pass from this process to the harness, as in the
/// daemon's `sandbox::child_env` for mode `none`.
const INHERIT: &[&str] = &["LANG", "LC_ALL", "LC_CTYPE", "TZ", "USER", "LOGNAME"];
/// Prefixes that never pass (the daemon's `SCRUB_PREFIXES`).
const SCRUB: &[&str] = &[
    "CLAUDE",
    "ANTHROPIC",
    "OPENAI",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "COPILOT",
    "PI_",
];

/// Run every selected harness and give the report.
pub async fn run_suite(settings: &Settings) -> Report {
    let mut report = Report::new(if settings.dry_run { "dry run" } else { "real" });
    if !settings.dry_run {
        report.note(
            "Sandbox mode `none` on the operator's real homes: the per-agent overlays that \
             bwrap binds are not applied, so the harness sees the shared files. The suite \
             copies only Claude's per-agent `.claude.json` over the shared one for the run \
             (and restores it), because the start needs its quiet-start keys.",
        );
        if !settings.cli.is_file() {
            for h in &settings.harnesses {
                report.fail(
                    *h,
                    "setup",
                    format!(
                        "the hook CLI {} is missing: run `cargo build -p terrarium`",
                        settings.cli.display()
                    ),
                );
            }
            return report;
        }
    }
    for id in settings.harnesses.clone() {
        let home = settings.home(id);
        match id {
            HarnessId::Claude => run_one(&Claude::new(settings, home), settings, &mut report).await,
            HarnessId::Codex => {
                let h = Codex::new(settings, home);
                run_one(&h, settings, &mut report).await;
                codex_empty_hooks(&h, settings, &mut report).await;
            }
            HarnessId::Copilot => {
                let h = Copilot::new(settings, home);
                run_one(&h, settings, &mut report).await;
            }
            HarnessId::Pi => {
                let h = Pi::new(settings, home);
                if !settings.dry_run {
                    if settings.pi.is_none() {
                        report.skip(id, "all", settings.pi_off_reason.clone());
                        continue;
                    }
                    if let Some(why) = h.refusal() {
                        report.skip(id, "all", why);
                        continue;
                    }
                }
                run_one(&h, settings, &mut report).await;
            }
            HarnessId::Grok => {
                let h = Grok::new(settings, home);
                if settings.dry_run {
                    run_one(&h, settings, &mut report).await;
                } else {
                    report.skip(
                        id,
                        "all",
                        "a real grok run is a signed-in, metered check: it needs the \
                         operator's OK through the lead (TER-1192 release gate)",
                    );
                }
            }
        }
    }
    report
}

async fn run_one(h: &dyn Harness, settings: &Settings, report: &mut Report) {
    if settings.dry_run {
        dry_run(h, report);
    } else {
        real_run(h, settings, report).await;
    }
}

// ------------------------------------------------------------- helpers

/// The temp root of one run. The name has no dot (Codex's trust key) and a
/// random part that is unique to the run: it is a leftover key.
struct RunDirs {
    root: tempfile::TempDir,
    runtime: tempfile::TempDir,
}

impl RunDirs {
    fn new() -> std::io::Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("tcompat-")
            .rand_bytes(10)
            .tempdir_in("/tmp")?;
        // Unix socket paths must stay short.
        let runtime = tempfile::Builder::new()
            .prefix("tcr-")
            .rand_bytes(6)
            .tempdir_in("/tmp")?;
        Ok(Self { root, runtime })
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn key(&self) -> String {
        self.root
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn make_agent(id: HarnessId, model: &str, state: &Path) -> Result<Agent, String> {
    let mut def =
        json!({"name": format!("compat-{id}"), "project": "compat", "harness": id.as_str()});
    if !model.is_empty() {
        def["model"] = json!(model);
    }
    let def: AgentDef = serde_json::from_value(def).map_err(|e| format!("agent def: {e}"))?;
    let agent = Agent::from_def(&def, state);
    std::fs::create_dir_all(&agent.workspace)
        .map_err(|e| format!("cannot create {}: {e}", agent.workspace.display()))?;
    Ok(agent)
}

/// The child env: a clean base as the daemon makes it for sandbox mode
/// `none`, the harness's extra env, then the spec's env on top.
fn finish_env(mut spec: LaunchSpec, cli: &Path, extra: &[(String, String)]) -> LaunchSpec {
    let mut env = BTreeMap::new();
    for key in INHERIT {
        if let Ok(v) = std::env::var(key) {
            env.insert((*key).to_owned(), v);
        }
    }
    env.entry("LANG".to_owned())
        .or_insert_with(|| "C.UTF-8".to_owned());
    env.insert("TERM".into(), "xterm-256color".into());
    if let Ok(home) = std::env::var("HOME") {
        env.insert("HOME".into(), home);
    }
    let base = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let path = match cli.parent() {
        Some(dir) => format!("{}:{base}", dir.display()),
        None => base,
    };
    env.insert("PATH".into(), path);
    for (k, v) in extra {
        if !SCRUB.iter().any(|p| k.starts_with(p)) {
            env.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in &spec.env {
        env.insert(k.clone(), v.clone());
    }
    spec.env = env;
    spec.clear_env = true;
    spec
}

fn read_events(path: &Path) -> Vec<EventRecord> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn count(events: &[EventRecord], f: impl Fn(&Event) -> bool) -> usize {
    events.iter().filter(|r| f(&r.event)).count()
}

fn sessions_started(
    events: &[EventRecord],
) -> Vec<(HarnessSessionId, Option<String>, Option<PathBuf>)> {
    events
        .iter()
        .filter_map(|r| match &r.event {
            Event::SessionStarted {
                harness_session,
                source,
                transcript_path,
            } => Some((
                harness_session.clone(),
                source.clone(),
                transcript_path.clone(),
            )),
            _ => None,
        })
        .collect()
}

async fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The last non-empty lines of the screen, for evidence.
fn screen_tail(runner: &Runner, n: usize) -> String {
    let lines: Vec<String> = runner
        .screen()
        .lines
        .iter()
        .map(|l| l.trim_end().to_owned())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" ⏎ ")
}

/// Screen lines that look like an error message.
fn error_lines(runner: &Runner) -> Vec<String> {
    runner
        .screen()
        .lines
        .iter()
        .map(|l| l.trim().to_owned())
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            low.contains("error:")
                || l.contains("ERROR")
                || low.contains("exception")
                || low.contains("failed to")
        })
        .collect()
}

fn secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

/// One harness process under a runner, with the recording adapter.
struct Live {
    runner: Runner,
    rec: Arc<RecordingAdapter>,
}

struct Ctx<'a> {
    h: &'a dyn Harness,
    settings: &'a Settings,
    agent: Agent,
    dirs: RunDirs,
    turns: u32,
}

impl Ctx<'_> {
    fn events_path(&self) -> PathBuf {
        terrarium_runner::private_dir(&self.dirs.state(), self.agent.id).join("events.jsonl")
    }

    fn events(&self) -> Vec<EventRecord> {
        read_events(&self.events_path())
    }

    async fn spawn(&self, resume: Option<&HarnessSessionId>) -> Result<Live, String> {
        let spec = self.h.launch(&self.agent, resume);
        let spec = finish_env(spec, &self.settings.cli, &self.h.extra_env());
        let mut rc = RunnerConfig::new(self.agent.id, self.dirs.state(), self.dirs.runtime.path());
        rc.name = self.agent.name.clone();
        rc.project = self.agent.project.clone();
        // One attempt only: a retry would type the turn twice and could
        // spend a second model turn.
        rc.submit_attempts = 1;
        rc.submit_timeout = Duration::from_secs(25);
        let rec = Arc::new(RecordingAdapter::new(self.h.adapter()));
        let runner = Runner::spawn(rc, rec.clone(), spec, Arc::new(NoHandler))
            .await
            .map_err(|e| format!("cannot spawn: {e}"))?;
        Ok(Live { runner, rec })
    }

    /// Wait for `idle`. `Err` carries the evidence.
    async fn wait_idle(&self, live: &Live, limit: Duration) -> Result<Duration, String> {
        let t = Instant::now();
        let ok = wait_until(limit, || {
            matches!(
                live.runner.status(),
                AgentStatus::Idle | AgentStatus::Error | AgentStatus::Stopped
            )
        })
        .await;
        let status = live.runner.status();
        if ok && status == AgentStatus::Idle {
            Ok(t.elapsed())
        } else {
            Err(format!(
                "status {status:?} after {}; screen: {}",
                secs(t.elapsed()),
                screen_tail(&live.runner, 8)
            ))
        }
    }

    /// Type one turn and wait for its end.
    async fn turn(&mut self, live: &Live, text: &str) -> Turn {
        let mut out = Turn::default();
        if self.turns >= MAX_TURNS {
            out.error = Some(format!("turn budget of {MAX_TURNS} is spent; not typed"));
            return out;
        }
        let before = self.events();
        let ends = count(&before, |e| matches!(e, Event::TurnEnd));
        let starts = count(&before, |e| matches!(e, Event::TurnStart { .. }));
        let t = Instant::now();
        self.turns += 1;
        let delivery = live.runner.type_turn(text);
        out.delivered = match tokio::time::timeout(DELIVERY_LIMIT, delivery.wait()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("no delivery result in {}", secs(DELIVERY_LIMIT))),
        };
        let path = self.events_path();
        let runner = live.runner.clone();
        out.ended = wait_until(TURN_LIMIT, || {
            count(&read_events(&path), |e| matches!(e, Event::TurnEnd)) > ends
                || matches!(runner.status(), AgentStatus::Error | AgentStatus::Stopped)
        })
        .await
            && count(&read_events(&path), |e| matches!(e, Event::TurnEnd)) > ends;
        out.time = t.elapsed();
        let after = self.events();
        out.started = count(&after, |e| matches!(e, Event::TurnStart { .. })) > starts;
        out.idle = self.wait_idle(live, IDLE_LIMIT).await.is_ok();
        out.hooks = live.rec.hooks_since(t);
        out.screen = live
            .runner
            .screen()
            .lines
            .iter()
            .filter(|l| !l.contains(text))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        out.reply = harness::reply_from_hooks(&out.hooks);
        out
    }

    /// Exit with the harness's exit keys, else kill. Gives what happened.
    async fn stop(&self, live: &Live) -> String {
        let t = Instant::now();
        let keys = self.h.exit_keys();
        let runner = &live.runner;
        if !keys.is_empty()
            && type_exit_keys(&keys, live.rec.exit_key_pause(), |w| async move {
                runner.write(&w).await.is_ok()
            })
            .await
            && let Ok(st) = tokio::time::timeout(EXIT_LIMIT, live.runner.wait()).await
        {
            return format!(
                "clean exit ({:?}) after {}, code {}",
                String::from_utf8_lossy(&keys).trim(),
                secs(t.elapsed()),
                st.code
            );
        }
        let _ = live.runner.kill();
        let st = tokio::time::timeout(EXIT_LIMIT, live.runner.wait()).await;
        format!(
            "no clean exit in {}; killed ({})",
            secs(EXIT_LIMIT),
            st.map(|s| format!("code {}, signal {:?}", s.code, s.signal))
                .unwrap_or_else(|_| "still running".into())
        )
    }
}

/// Type the exit keys as the runner's `stop` does: the writes of
/// [`exit_key_writes`], with `pause` between them. False when a write fails.
async fn type_exit_keys<F, Fut>(keys: &[u8], pause: Duration, mut write: F) -> bool
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = bool>,
{
    for (i, w) in exit_key_writes(keys, pause).into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(pause).await;
        }
        if !write(w).await {
            return false;
        }
    }
    true
}

struct Turn {
    delivered: Result<(), String>,
    started: bool,
    ended: bool,
    idle: bool,
    time: Duration,
    hooks: Vec<Captured>,
    reply: Option<String>,
    /// The screen without the lines that hold the typed text.
    screen: String,
    error: Option<String>,
}

impl Default for Turn {
    fn default() -> Self {
        Self {
            delivered: Err("not typed".into()),
            started: false,
            ended: false,
            idle: false,
            time: Duration::ZERO,
            hooks: Vec::new(),
            reply: None,
            screen: String::new(),
            error: None,
        }
    }
}

impl Turn {
    /// The reply text: from the hooks, else the screen.
    fn text(&self) -> &str {
        self.reply.as_deref().unwrap_or(&self.screen)
    }

    fn says(&self, word: &str) -> bool {
        self.text()
            .to_ascii_lowercase()
            .contains(&word.to_ascii_lowercase())
    }

    fn summary(&self) -> String {
        if let Some(e) = &self.error {
            return e.clone();
        }
        let reply = match &self.reply {
            Some(r) => format!("reply (hook) {:?}", r.trim()),
            None => "reply read from the screen".to_owned(),
        };
        format!(
            "delivery {}, TurnStart {}, TurnEnd {}, idle after {}, {} hook(s), {reply}",
            match &self.delivered {
                Ok(()) => "confirmed".to_owned(),
                Err(e) => format!("failed ({e})"),
            },
            yes(self.started),
            yes(self.ended),
            if self.idle { "yes" } else { "no" },
            self.hooks.len(),
        )
    }

    fn complete(&self) -> bool {
        self.error.is_none() && self.delivered.is_ok() && self.started && self.ended && self.idle
    }
}

fn yes(b: bool) -> &'static str {
    if b { "seen" } else { "missing" }
}

/// Keeps the original bytes of a shared file and puts them back on drop.
struct StateGuard {
    target: PathBuf,
    original: Option<Vec<u8>>,
    backup: PathBuf,
}

impl StateGuard {
    /// Copy `source` over `target` for the run. A copy of the original
    /// goes to `backup` too, in case the process dies before the restore.
    fn install(source: &Path, target: &Path, backup: &Path) -> std::io::Result<Self> {
        let original = match std::fs::read(target) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if let Some(b) = &original {
            if let Some(parent) = backup.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(backup, b)?;
        }
        std::fs::copy(source, target)?;
        Ok(Self {
            target: target.to_path_buf(),
            original,
            backup: backup.to_path_buf(),
        })
    }

    fn restore(&mut self) -> String {
        let r = match self.original.take() {
            Some(b) => std::fs::write(&self.target, &b).map(|()| b.len()),
            None => std::fs::remove_file(&self.target).map(|()| 0),
        };
        match r {
            Ok(n) => {
                let _ = std::fs::remove_file(&self.backup);
                format!("restored {} ({n} bytes)", self.target.display())
            }
            Err(e) => format!(
                "cannot restore {}: {e}; the original is in {}",
                self.target.display(),
                self.backup.display()
            ),
        }
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        if self.original.is_some() {
            let _ = self.restore();
        }
    }
}

fn blocking<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

/// Report the harnesses.toml pin. The suite, the image, and the daemon
/// read the same file.
fn pin_step(h: &dyn Harness, report: &mut Report) {
    let id = h.id();
    let pin = terrarium_core::harnesses::pin(id);
    report.pass(id, "pin", format!("harnesses.toml pins {}", pin.version));
}

/// Read the installed version. `false` when the binary is missing. Any
/// version passes: the other steps test it, so the suite is the way to
/// test a new release.
fn version_step(h: &dyn Harness, report: &mut Report) -> bool {
    let id = h.id();
    let adapter = h.adapter();
    let pin = terrarium_core::harnesses::pin(id).version;
    match blocking(|| adapter.detect_version()) {
        Ok(v) => {
            report.pass(id, "version", format!("{v} is installed (pin {pin})"));
            true
        }
        Err(e) => {
            report.skip(id, "version", format!("the binary did not answer: {e}"));
            false
        }
    }
}

// ------------------------------------------------------------- dry run

fn dry_run(h: &dyn Harness, report: &mut Report) {
    let id = h.id();
    pin_step(h, report);
    version_step(h, report);
    let check = || -> Result<String, String> {
        let tmp = tempfile::Builder::new()
            .prefix("tcompat-dry-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let model = match h.model() {
            "" => "test-model",
            m => m,
        };
        let agent = make_agent(id, model, &tmp.path().join("state"))?;
        let overlays = h.materialize(&agent, &home, Some(PROMPT))?;
        let prompt = std::fs::read_to_string(agent.workspace.with_file_name("harness/prompt.md"))
            .map_err(|e| format!("prompt.md: {e}"))?;
        if !prompt.contains(SENTINEL) {
            return Err("prompt.md has no sentinel".into());
        }
        let fresh = h.launch(&agent, None);
        let sid = HarnessSessionId("11111111-2222-4333-8444-555555555555".into());
        let resume = h.launch(&agent, Some(&sid));
        for (what, spec) in [("fresh", &fresh), ("resume", &resume)] {
            if spec.program != id.program() {
                return Err(format!("{what}: program {}", spec.program));
            }
            if spec.cwd != agent.workspace {
                return Err(format!("{what}: cwd {}", spec.cwd.display()));
            }
            if !spec.args.iter().any(|a| a.contains(model)) {
                return Err(format!("{what}: no model `{model}` in the args"));
            }
            let env = finish_env((*spec).clone(), Path::new("/x/terrarium"), &[]);
            if !env.clear_env || !env.env.contains_key("PATH") {
                return Err(format!("{what}: the child env is not clean"));
            }
        }
        if !resume.args.iter().any(|a| a.contains(sid.0.as_str())) {
            return Err("resume: the session ID is not in the args".into());
        }
        if id == HarnessId::Codex && !fresh.args.iter().any(|a| a == harness::CODEX_EFFORT) {
            return Err("codex: no low reasoning effort".into());
        }
        Ok(format!(
            "{} overlay(s); fresh: `{} {}` args; resume names the session",
            overlays.len(),
            fresh.program,
            fresh.args.len()
        ))
    };
    match blocking(check) {
        Ok(ev) => report.pass(id, "launch_spec", ev),
        Err(e) => report.fail(id, "launch_spec", e),
    }
    let user_hooks = h.user_hooks();
    if !user_hooks.is_empty() {
        report.skip(
            id,
            "user_hooks",
            format!(
                "a real run skips this harness unless TERRARIUM_COMPAT_ALLOW_USER_HOOKS=1: the \
                 shared home has the operator's own hooks: {}",
                user_hooks.join("; ")
            ),
        );
    }
    match id {
        HarnessId::Codex => codex_hooks_file_dry(h, report),
        HarnessId::Copilot => copilot_empty_prompts_dry(h, report),
        _ => {}
    }
    for step in [
        "fresh_start",
        "turn_with_hooks",
        "system_prompt_applied",
        "resume",
        "usage_parsed",
        "leftovers",
    ] {
        report.skip(id, step, "dry run");
    }
}

/// Codex "not verified": the empty `{"hooks": {}}` file. Dry part: the
/// per-agent file is that exact JSON.
fn codex_hooks_file_dry(h: &dyn Harness, report: &mut Report) {
    let r = blocking(|| -> Result<String, String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agent = make_agent(HarnessId::Codex, "m", &tmp.path().join("state"))?;
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        h.materialize(&agent, &home, None)?;
        let file = terrarium_adapter_codex::config::codex_dir(&agent).join("hooks.json");
        let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if v == json!({"hooks": {}}) {
            Ok("per-agent hooks.json is `{\"hooks\": {}}`".into())
        } else {
            Err(format!("per-agent hooks.json is {v}"))
        }
    });
    match r {
        Ok(ev) => report.pass(HarnessId::Codex, "empty_hooks_file (dry)", ev),
        Err(e) => report.fail(HarnessId::Codex, "empty_hooks_file (dry)", e),
    }
}

/// Copilot "not verified": both prompt files empty. Dry part: with no
/// prompt, `materialize` writes both files empty.
fn copilot_empty_prompts_dry(h: &dyn Harness, report: &mut Report) {
    let r = blocking(|| -> Result<String, String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agent = make_agent(HarnessId::Copilot, "m", &tmp.path().join("state"))?;
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        h.materialize(&agent, &home, None)?;
        let dir = terrarium_adapter_copilot::config::copilot_dir(&agent);
        let mut sizes = Vec::new();
        for name in ["SYSTEM.md", "APPEND_SYSTEM.md"] {
            let len = std::fs::metadata(dir.join(name))
                .map_err(|e| format!("{name}: {e}"))?
                .len();
            sizes.push(format!("{name} {len} bytes"));
            if len != 0 {
                return Err(sizes.join(", "));
            }
        }
        Ok(format!("no prompt: {}", sizes.join(", ")))
    });
    match r {
        Ok(ev) => report.pass(HarnessId::Copilot, "empty_prompt_files (dry)", ev),
        Err(e) => report.fail(HarnessId::Copilot, "empty_prompt_files (dry)", e),
    }
}

// ------------------------------------------------------------ real run

async fn real_run(h: &dyn Harness, settings: &Settings, report: &mut Report) {
    let id = h.id();
    let rest = [
        "fresh_start",
        "turn_with_hooks",
        "system_prompt_applied",
        "resume",
        "usage_parsed",
        "leftovers",
    ];
    let skip_rest = |report: &mut Report, from: usize, why: &str| {
        for step in &rest[from..] {
            report.skip(id, step, why.to_owned());
        }
    };
    // The snapshot comes first, so the leftover check also covers what the
    // version and auth probes write (`claude auth status` can migrate the
    // state file and leave a backup).
    let home = h.home().to_path_buf();
    if !home.is_dir() {
        report.skip(id, "version", format!("no home at {}", home.display()));
        skip_rest(report, 0, "no home");
        return;
    }
    let before = blocking(|| Snapshot::take(&home));
    if !version_step(h, report) {
        skip_rest(report, 0, "no binary");
        return;
    }
    match blocking(|| h.auth()) {
        Ok(ev) => report.pass(id, "auth", ev),
        Err(why) => {
            report.skip(id, "auth", format!("not logged in: {why}"));
            skip_rest(report, 0, "not logged in; the suite never runs a login");
            return;
        }
    }
    if h.model().is_empty() {
        report.fail(id, "setup", "no model");
        return;
    }
    let user_hooks = h.user_hooks();
    if !user_hooks.is_empty() {
        let list = user_hooks.join("; ");
        if settings.allow_user_hooks {
            report.note(format!(
                "{id}: the operator's own hooks fire in the test agent (sandbox none): {list}"
            ));
        } else {
            let why = format!(
                "the shared home has the operator's own hooks, which fire in the test agent \
                 in sandbox mode `none` and can act outside the home: {list}. Set \
                 TERRARIUM_COMPAT_ALLOW_USER_HOOKS=1 to run anyway"
            );
            report.skip(id, "user_hooks", why);
            skip_rest(
                report,
                0,
                "the operator's hooks are in the shared home (see user_hooks)",
            );
            return;
        }
    }
    let dirs = match RunDirs::new() {
        Ok(d) => d,
        Err(e) => {
            report.fail(id, "setup", format!("temp dirs: {e}"));
            return;
        }
    };
    let agent = match make_agent(id, h.model(), &dirs.state()) {
        Ok(a) => a,
        Err(e) => {
            report.fail(id, "setup", e);
            return;
        }
    };
    let mut ctx = Ctx {
        h,
        settings,
        agent,
        dirs,
        turns: 0,
    };

    // 2. fresh_start
    let t_mat = Instant::now();
    let overlays: Vec<FileOverlay> =
        match blocking(|| h.materialize(&ctx.agent, &home, Some(PROMPT))) {
            Ok(o) => o,
            Err(e) => {
                report.fail(id, "fresh_start", format!("materialize: {e}"));
                skip_rest(report, 1, "materialize failed");
                return;
            }
        };
    let mounts: Vec<MountPoint> = overlays
        .iter()
        .filter(|o| o.target.starts_with(&home) && !before.contains(&o.target))
        .map(|o| MountPoint {
            path: o.target.clone(),
            bytes: if o.target.is_dir() {
                None
            } else {
                std::fs::read(&o.target).ok()
            },
        })
        .collect();
    let mut guard = None;
    if let Some(name) = h.state_overlay() {
        let target = home.join(name);
        if let Some(o) = overlays.iter().find(|o| o.target == target) {
            let backup = settings
                .target_dir
                .join(format!("compat-{id}-state-backup.json"));
            match StateGuard::install(&o.source, &target, &backup) {
                Ok(g) => guard = Some(g),
                Err(e) => {
                    report.fail(id, "fresh_start", format!("cannot place {name}: {e}"));
                    skip_rest(report, 1, "setup failed");
                    return;
                }
            }
        }
    }
    let materialize_time = t_mat.elapsed();
    let t0 = Instant::now();
    let live = match ctx.spawn(None).await {
        Ok(l) => l,
        Err(e) => {
            report.fail(id, "fresh_start", e);
            finish(&mut ctx, report, &before, &mounts, guard, &[], 2).await;
            return;
        }
    };
    let started = ctx.wait_idle(&live, START_LIMIT).await;
    let hints = live.rec.hints_since(t0);
    let dialog = hints.contains(&ScreenHint::Question);
    let errors = error_lines(&live.runner);
    let login = live
        .runner
        .screen()
        .lines
        .iter()
        .any(|l| l.contains(terrarium_adapter_copilot::auth::LOGIN_REQUIRED));
    let fresh_ok = match &started {
        Ok(t) => {
            let ev = format!(
                "idle {} after spawn (materialize {}); screen hints {hints:?}; dialog: {}; \
                 error lines: {errors:?}",
                secs(*t),
                secs(materialize_time),
                if dialog { "YES" } else { "none" }
            );
            let ok = !dialog && errors.is_empty() && !login;
            report.add(
                id,
                "fresh_start",
                if ok { Outcome::Pass } else { Outcome::Fail },
                Some(*t),
                if login {
                    format!("logged out on screen; {ev}")
                } else {
                    ev
                },
            );
            ok
        }
        Err(e) => {
            report.fail(
                id,
                "fresh_start",
                format!("not idle in {}: {e}", secs(START_LIMIT)),
            );
            false
        }
    };
    if id == HarnessId::Copilot {
        copilot_empty_prompts_live(settings, &home, fresh_ok, &errors, report);
    }
    if !fresh_ok {
        let how = ctx.stop(&live).await;
        report.note(format!("{id}: stopped after the failed start: {how}"));
        finish(&mut ctx, report, &before, &mounts, guard, &[], 2).await;
        return;
    }

    // 3. turn_with_hooks
    let t1 = ctx.turn(&live, TURN_PONG).await;
    let events = ctx.events();
    let sid = sessions_started(&events)
        .last()
        .map(|(s, _, _)| s.clone())
        .or_else(|| blocking(|| h.adapter().find_session_id(&ctx.agent)));
    let [submit, stop] = h.turn_hooks();
    let ws = ctx.agent.workspace.display().to_string();
    let mut problems = Vec::new();
    for name in [submit, stop] {
        let Some(c) = t1.hooks.iter().find(|c| c.event() == name) else {
            problems.push(format!("no {name} hook"));
            continue;
        };
        for f in h.payload_fields() {
            if c.body
                .get(*f)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                problems.push(format!("{name} has no `{f}`"));
            }
        }
        if let Some(cwd) = c.body.get("cwd").and_then(Value::as_str)
            && cwd != ws
        {
            problems.push(format!("{name}.cwd is {cwd}"));
        }
        if let (Some(s), Some(sid)) = (c.body.get("session_id").and_then(Value::as_str), &sid)
            && s != sid.0
        {
            problems.push(format!("{name}.session_id {s} is not {}", sid.0));
        }
    }
    if sid.is_none() {
        problems.push("no session ID (no SessionStarted, no find_session_id)".into());
    }
    if !t1.says("pong") {
        problems.push(format!("the reply has no `pong`: {:?}", t1.text().trim()));
    }
    let hook_names: Vec<&str> = t1.hooks.iter().map(Captured::event).collect();
    let ev = format!(
        "{}; session {}; hooks {hook_names:?}; fields {:?} checked on {submit}/{stop}{}",
        t1.summary(),
        sid.as_ref().map(|s| s.0.as_str()).unwrap_or("?"),
        h.payload_fields(),
        if problems.is_empty() {
            String::new()
        } else {
            format!("; problems: {}", problems.join("; "))
        }
    );
    let turn_ok = t1.complete() && problems.is_empty();
    report.add(
        id,
        "turn_with_hooks",
        if turn_ok {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        Some(t1.time),
        ev,
    );

    // 4. system_prompt_applied
    let prompt_ok = match h.prompt_reaches_harness() {
        Err(why) => {
            report.skip(id, "system_prompt_applied", format!("sandbox none: {why}"));
            false
        }
        Ok(()) if !t1.complete() => {
            report.skip(
                id,
                "system_prompt_applied",
                "the first turn did not complete",
            );
            false
        }
        Ok(()) => {
            let t2 = ctx.turn(&live, TURN_SECRET).await;
            let ok = t2.complete() && t2.says(SENTINEL);
            report.add(
                id,
                "system_prompt_applied",
                if ok { Outcome::Pass } else { Outcome::Fail },
                Some(t2.time),
                format!(
                    "prompt has `the secret word is {SENTINEL}`; {}; {SENTINEL} in the reply: {}",
                    t2.summary(),
                    if t2.says(SENTINEL) { "yes" } else { "NO" }
                ),
            );
            ok
        }
    };

    // 5. resume
    let how = ctx.stop(&live).await;
    let ended = count(&ctx.events(), |e| matches!(e, Event::SessionEnded { .. }));
    let mut sids: Vec<String> = sid.iter().map(|s| s.0.clone()).collect();
    match &sid {
        None => report.skip(id, "resume", format!("no session ID; first runner: {how}")),
        Some(_) if !t1.complete() => report.skip(
            id,
            "resume",
            format!("the first turn did not complete; {how}"),
        ),
        Some(sid) => {
            let n_before = sessions_started(&ctx.events()).len();
            match ctx.spawn(Some(sid)).await {
                Err(e) => report.fail(id, "resume", format!("{how}; {e}")),
                Ok(live2) => {
                    let idle = ctx.wait_idle(&live2, START_LIMIT).await;
                    match idle {
                        Err(e) => {
                            report.fail(
                                id,
                                "resume",
                                format!("{how}; not idle after the resume: {e}"),
                            );
                        }
                        Ok(t_idle) => {
                            let text = if prompt_ok {
                                TURN_RESUME
                            } else {
                                TURN_RESUME_NO_PROMPT
                            };
                            let t3 = ctx.turn(&live2, text).await;
                            let new = sessions_started(&ctx.events());
                            let new = &new[n_before.min(new.len())..];
                            let source = new.last().and_then(|(_, s, _)| s.clone());
                            let same = new.iter().all(|(s, _, _)| s == sid);
                            for (s, _, _) in new {
                                if !sids.contains(&s.0) {
                                    sids.push(s.0.clone());
                                }
                            }
                            let mut problems = Vec::new();
                            if new.is_empty() {
                                problems.push("no SessionStarted after the resume".to_owned());
                            }
                            if let Some(want) = h.resume_source()
                                && source.as_deref() != Some(want)
                            {
                                problems.push(format!("source {source:?}, not {want:?}"));
                            }
                            if !same {
                                problems.push("the session ID changed".to_owned());
                            }
                            if prompt_ok && !t3.says(SENTINEL) {
                                problems.push(format!("no {SENTINEL} in the reply"));
                            }
                            if !t3.says("pong") {
                                problems.push("no `pong` (the history) in the reply".to_owned());
                            }
                            let ok = t3.complete() && problems.is_empty();
                            report.add(
                                id,
                                "resume",
                                if ok { Outcome::Pass } else { Outcome::Fail },
                                Some(t3.time),
                                format!(
                                    "first runner: {how} ({ended} SessionEnded); idle {} after \
                                     the resume spawn; SessionStarted source {source:?}; {}{}",
                                    secs(t_idle),
                                    t3.summary(),
                                    if problems.is_empty() {
                                        String::new()
                                    } else {
                                        format!("; problems: {}", problems.join("; "))
                                    }
                                ),
                            );
                        }
                    }
                    let how2 = ctx.stop(&live2).await;
                    report.note(format!("{id}: resumed runner: {how2}"));
                }
            }
        }
    }

    // 6. usage_parsed
    match &sid {
        None => report.skip(id, "usage_parsed", "no session ID"),
        Some(s) => {
            let from_event = sessions_started(&ctx.events())
                .iter()
                .rev()
                .find_map(|(_, _, p)| p.clone())
                .filter(|p| p.is_file());
            let path = blocking(|| h.transcript(&ctx.agent, s)).or(from_event);
            match path {
                None => report.fail(id, "usage_parsed", format!("no session file for {}", s.0)),
                Some(p) => match blocking(|| h.usage(&p)) {
                    Ok((n, model)) if n > 0 && !model.is_empty() => report.pass(
                        id,
                        "usage_parsed",
                        format!("{n} sample(s), model `{model}`, from {}", p.display()),
                    ),
                    Ok((n, model)) => report.fail(
                        id,
                        "usage_parsed",
                        format!("{n} sample(s), model {model:?}, from {}", p.display()),
                    ),
                    Err(e) => report.fail(id, "usage_parsed", e),
                },
            }
        }
    }

    finish(&mut ctx, report, &before, &mounts, guard, &sids, 99).await;
}

/// Restore the state file, clean the home, and record the turns. `from`
/// is the index of the first step in the `rest` list that must still be
/// skipped (99: none).
async fn finish(
    ctx: &mut Ctx<'_>,
    report: &mut Report,
    before: &Snapshot,
    mounts: &[MountPoint],
    guard: Option<StateGuard>,
    sids: &[String],
    from: usize,
) {
    let id = ctx.h.id();
    let skipped = [
        "turn_with_hooks",
        "system_prompt_applied",
        "resume",
        "usage_parsed",
    ];
    if from < 99 {
        for s in &skipped {
            report.skip(id, s, "the start failed");
        }
    }
    let restored = guard.map(|mut g| g.restore());
    let mut keys = vec![ctx.dirs.key(), ctx.agent.id.to_string()];
    keys.extend(sids.iter().cloned());
    let l = blocking(|| leftovers::clean(before, &keys, mounts));
    let mut ev = leftovers::summary(&l, &before.root);
    if let Some(r) = restored {
        ev = format!("{r}; {ev}");
    }
    let ok = l.errors.is_empty() && restored_ok(&ev);
    report.add(
        id,
        "leftovers",
        if ok { Outcome::Pass } else { Outcome::Fail },
        None,
        ev,
    );
    if !l.unattributed.is_empty() || !l.changed_other.is_empty() {
        report.note(format!(
            "{id}: {} new path(s) and {} changed file(s) in {} carry no run key. They are most \
             likely the operator's own sessions on the same home; they stay.",
            l.unattributed.len(),
            l.changed_other.len(),
            before.root.display()
        ));
    }
    report.set_turns(id, ctx.turns);
}

fn restored_ok(ev: &str) -> bool {
    !ev.contains("cannot restore")
}

// ------------------------------------------------------ the sub-checks

/// Copilot "not verified": both prompt files empty does not throw. The
/// real start ran with the shared `SYSTEM.md` and `APPEND_SYSTEM.md`,
/// which the adapter makes empty when they are missing.
fn copilot_empty_prompts_live(
    settings: &Settings,
    home: &Path,
    fresh_ok: bool,
    errors: &[String],
    report: &mut Report,
) {
    let id = HarnessId::Copilot;
    let sizes: Vec<(String, Option<u64>)> = ["SYSTEM.md", "APPEND_SYSTEM.md"]
        .iter()
        .map(|n| {
            (
                (*n).to_owned(),
                std::fs::metadata(home.join(n)).ok().map(|m| m.len()),
            )
        })
        .collect();
    let both_empty = sizes.iter().all(|(_, l)| *l == Some(0));
    let Some(ext) = &settings.copilot_prompt_ext else {
        report.skip(
            id,
            "empty_prompt_files",
            format!(
                "the prompt extension is not loaded (set TERRARIUM_COMPAT_COPILOT_PROMPT_EXT to \
                 the copilot-custom-system-prompt package dir), so no file was read; shared \
                 files: {sizes:?}"
            ),
        );
        return;
    };
    if !both_empty {
        report.skip(
            id,
            "empty_prompt_files",
            format!("the shared prompt files are not both empty: {sizes:?}"),
        );
        return;
    }
    let ok = fresh_ok && errors.is_empty();
    report.add(
        id,
        "empty_prompt_files",
        if ok { Outcome::Pass } else { Outcome::Fail },
        None,
        format!(
            "extension {} loaded with both prompt files empty: start {}; error lines {errors:?}",
            ext.display(),
            if fresh_ok { "reached idle" } else { "failed" }
        ),
    );
}

/// Codex "not verified": an empty `{"hooks": {}}` file is accepted at
/// start. Sandbox `none` cannot bind the per-agent file over the shared
/// one, so this check starts Codex twice on a throw-away `CODEX_HOME` with
/// no login: once with the empty file, once with a broken file as the
/// control. No turn is typed and no credential is read or copied.
async fn codex_empty_hooks(h: &Codex, settings: &Settings, report: &mut Report) {
    let id = HarnessId::Codex;
    let name = "empty_hooks_file";
    if settings.dry_run {
        return;
    }
    // Under the target dir, not /tmp: Codex warns about a home under a
    // temp dir, and that warning would hide the result.
    let base = settings.target_dir.join("compat-codex-hooks");
    let _ = std::fs::remove_dir_all(&base);
    let mut screens = Vec::new();
    for (label, body) in [
        ("empty", terrarium_adapter_codex::config::EMPTY_HOOKS),
        ("broken", "{ this is not json"),
    ] {
        let home = base.join(label);
        if let Err(e) = std::fs::create_dir_all(&home)
            .and_then(|()| std::fs::write(home.join("hooks.json"), body))
        {
            report.fail(id, name, format!("cannot make {}: {e}", home.display()));
            return;
        }
        let probe = h.with_home(settings, home.clone());
        let dirs = match RunDirs::new() {
            Ok(d) => d,
            Err(e) => {
                report.fail(id, name, e.to_string());
                return;
            }
        };
        let agent = match make_agent(id, h.model(), &dirs.state()) {
            Ok(a) => a,
            Err(e) => {
                report.fail(id, name, e);
                return;
            }
        };
        let ctx = Ctx {
            h: &probe,
            settings,
            agent,
            dirs,
            turns: 0,
        };
        let live = match ctx.spawn(None).await {
            Ok(l) => l,
            Err(e) => {
                report.fail(id, name, e);
                return;
            }
        };
        // Let the start screen settle.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let lines: Vec<String> = live
            .runner
            .screen()
            .lines
            .iter()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect();
        let _ = live.runner.kill();
        let _ = tokio::time::timeout(EXIT_LIMIT, live.runner.wait()).await;
        let hook_msgs: Vec<String> = lines
            .iter()
            .filter(|l| {
                let low = l.to_ascii_lowercase();
                low.contains("hook")
                    && ["error", "invalid", "parse", "failed", "expected"]
                        .iter()
                        .any(|w| low.contains(w))
            })
            .cloned()
            .collect();
        screens.push((label, hook_msgs, lines));
    }
    let _ = std::fs::remove_dir_all(&base);
    let (empty, broken) = (&screens[0], &screens[1]);
    let tail = |l: &[String]| {
        l.iter()
            .rev()
            .take(4)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ⏎ ")
    };
    if !empty.1.is_empty() {
        report.fail(
            id,
            name,
            format!("the empty file gives hook messages: {:?}", empty.1),
        );
    } else if !broken.1.is_empty() {
        report.pass(
            id,
            name,
            format!(
                "no hook message with `{{\"hooks\": {{}}}}`; the broken control file gives {:?}. \
                 Empty-home screen: {}",
                broken.1,
                tail(&empty.2)
            ),
        );
    } else {
        report.skip(
            id,
            name,
            format!(
                "inconclusive: neither file gives a hook message before login (Codex may read \
                 hooks.json only after login). Empty: {}; broken: {}",
                tail(&empty.2),
                tail(&broken.2)
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use semver::Version;
    use terrarium_core::HookPayload;

    use super::*;

    /// An adapter with `/exit` exit keys and a set exit-key pause.
    struct FakeAdapter {
        pause: Duration,
    }

    impl Adapter for FakeAdapter {
        fn id(&self) -> HarnessId {
            HarnessId::Codex
        }
        fn detect_version(&self) -> terrarium_core::Result<Version> {
            Ok(Version::new(1, 0, 0))
        }
        fn launch(&self, agent: &Agent, _resume: Option<&HarnessSessionId>) -> LaunchSpec {
            LaunchSpec::new("true", &agent.workspace)
        }
        fn hook_event(&self, _hook: &HookPayload) -> Vec<Event> {
            Vec::new()
        }
        fn exit_keys(&self) -> Vec<u8> {
            b"/exit\r".to_vec()
        }
        fn exit_key_pause(&self) -> Duration {
            self.pause
        }
    }

    /// The writes of the suite's stop through the recording adapter, with
    /// the time of each.
    async fn stop_writes(pause: Duration) -> Vec<(Instant, Vec<u8>)> {
        let rec = RecordingAdapter::new(Arc::new(FakeAdapter { pause }));
        let log = Mutex::new(Vec::new());
        let ok = type_exit_keys(&rec.exit_keys(), rec.exit_key_pause(), |w| {
            log.lock().unwrap().push((Instant::now(), w));
            async { true }
        })
        .await;
        assert!(ok);
        log.into_inner().unwrap()
    }

    #[tokio::test]
    async fn stop_splits_the_cr_after_the_exit_key_pause() {
        let pause = Duration::from_millis(300);
        let writes = stop_writes(pause).await;
        let bytes: Vec<&[u8]> = writes.iter().map(|(_, w)| w.as_slice()).collect();
        assert_eq!(bytes, vec![b"/exit".as_slice(), b"\r".as_slice()]);
        assert!(writes[1].0 - writes[0].0 >= pause);
    }

    #[tokio::test]
    async fn stop_with_no_pause_is_one_write() {
        let writes = stop_writes(Duration::ZERO).await;
        let bytes: Vec<&[u8]> = writes.iter().map(|(_, w)| w.as_slice()).collect();
        assert_eq!(bytes, vec![b"/exit\r".as_slice()]);
    }

    #[tokio::test]
    async fn stop_ends_at_a_failed_write() {
        let mut n = 0;
        let ok = type_exit_keys(b"/exit\r", Duration::from_millis(1), |_| {
            n += 1;
            async { false }
        })
        .await;
        assert!(!ok);
        assert_eq!(n, 1);
    }
}
