//! An adapter wrapper that keeps a copy of each hook payload and each
//! change of the screen hint. The runner does not record the raw hook
//! JSON, and the scenarios must check the payload fields.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use semver::{Version, VersionReq};
use serde_json::Value;
use terrarium_core::adapter::Screen;
use terrarium_core::config::SubscriptionDef;
use terrarium_core::{
    Adapter, Agent, Answer, Event, FileOverlay, HarnessId, HarnessSessionId, HookPayload,
    LaunchSpec, Result, ScreenHint,
};

/// One hook message as the runner got it.
#[derive(Debug, Clone)]
pub struct Captured {
    pub at: Instant,
    /// The name from `terrarium hook <name>`.
    pub hook: String,
    pub body: Value,
}

impl Captured {
    /// `hook_event_name` from the payload, else the hook argument.
    pub fn event(&self) -> &str {
        self.body
            .get("hook_event_name")
            .and_then(Value::as_str)
            .unwrap_or(&self.hook)
    }
}

/// Delegates every call to `inner` and records hooks and screen hints.
pub struct RecordingAdapter {
    inner: Arc<dyn Adapter>,
    hooks: Mutex<Vec<Captured>>,
    hints: Mutex<Vec<(Instant, ScreenHint)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RecordingAdapter {
    pub fn new(inner: Arc<dyn Adapter>) -> Self {
        Self {
            inner,
            hooks: Mutex::new(Vec::new()),
            hints: Mutex::new(Vec::new()),
        }
    }

    /// Every hook so far.
    pub fn hooks(&self) -> Vec<Captured> {
        lock(&self.hooks).clone()
    }

    /// The hooks after `since`.
    pub fn hooks_since(&self, since: Instant) -> Vec<Captured> {
        lock(&self.hooks)
            .iter()
            .filter(|c| c.at >= since)
            .cloned()
            .collect()
    }

    /// The screen hints after `since`, one entry per change.
    pub fn hints_since(&self, since: Instant) -> Vec<ScreenHint> {
        lock(&self.hints)
            .iter()
            .filter(|(at, _)| *at >= since)
            .map(|(_, h)| *h)
            .collect()
    }
}

impl Adapter for RecordingAdapter {
    fn id(&self) -> HarnessId {
        self.inner.id()
    }

    fn tested_version(&self) -> VersionReq {
        self.inner.tested_version()
    }

    fn detect_version(&self) -> Result<Version> {
        self.inner.detect_version()
    }

    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        self.inner.launch(agent, resume)
    }

    fn launch_for(
        &self,
        agent: &Agent,
        resume: Option<&HarnessSessionId>,
        subscription: Option<&SubscriptionDef>,
    ) -> LaunchSpec {
        self.inner.launch_for(agent, resume, subscription)
    }

    fn is_system_prompt(&self, hook: &HookPayload) -> bool {
        self.inner.is_system_prompt(hook)
    }

    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        lock(&self.hooks).push(Captured {
            at: Instant::now(),
            hook: hook.hook.clone(),
            body: hook.body.clone(),
        });
        self.inner.hook_event(hook)
    }

    fn screen_hint(&self, screen: &Screen) -> ScreenHint {
        let hint = self.inner.screen_hint(screen);
        let mut hints = lock(&self.hints);
        if hints.last().map(|(_, h)| *h) != Some(hint) {
            hints.push((Instant::now(), hint));
        }
        hint
    }

    fn find_session_id(&self, agent: &Agent) -> Option<HarnessSessionId> {
        self.inner.find_session_id(agent)
    }

    fn type_turn(&self, text: &str) -> Vec<u8> {
        self.inner.type_turn(text)
    }

    fn answer(&self, request_id: &str, answer: &Answer) -> Vec<u8> {
        self.inner.answer(request_id, answer)
    }

    fn clear_input_keys(&self) -> Vec<u8> {
        self.inner.clear_input_keys()
    }

    fn exit_keys(&self) -> Vec<u8> {
        self.inner.exit_keys()
    }

    fn exit_key_pause(&self) -> Duration {
        self.inner.exit_key_pause()
    }

    fn prompt_frozen_on_resume(&self) -> bool {
        self.inner.prompt_frozen_on_resume()
    }

    fn materialize_config(&self, agent: &Agent, home: &Path) -> Result<Vec<FileOverlay>> {
        self.inner.materialize_config(agent, home)
    }
}
