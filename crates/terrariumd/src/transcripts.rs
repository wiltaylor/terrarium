//! The unified session transcript (`docs/api.md` §6.6): find the session
//! file of an agent session, parse it with the adapter's
//! [`Transcript`] parser, and keep the parsed blocks.
//!
//! The file of a session is the `transcript_path` of its `session_started`
//! event, else the adapter's lookup by the harness session ID. Either way
//! it goes through the rule of the usage collector
//! ([`usage_collector::check_transcript`]): only a regular file in the
//! agent's own session area (`docs/security-review.md` F12), with the
//! session ID in its name, and no other agent's harness session. Nothing
//! outside the agent's own dirs is read.
//!
//! [`TranscriptCache`] keeps the parser state and the blocks of the newest
//! files, keyed by path. A read checks the file length: when it grew, only
//! the new bytes are parsed; when it got shorter, the file is parsed again
//! from the top. A file that drops out of the cache keeps a small summary
//! (block count and title) for the sessions list, keyed by its length.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use terrarium_core::tail::LineTail;
use terrarium_core::transcript::{Transcript, TranscriptLog};
use terrarium_core::{Agent, AgentId, HarnessId};

use crate::api::Shared;
use crate::store::agents;
use crate::store::transcripts as tstore;
use crate::usage_collector::{check_transcript, host_path, transcript_roots};

/// The files whose blocks stay in memory.
pub const CACHED_FILES: usize = 16;

/// The length of a session title, in characters.
pub const TITLE_CHARS: usize = 80;

/// A new parser for the session files of `harness`.
pub fn parser_for(harness: HarnessId) -> Box<dyn Transcript> {
    match harness {
        HarnessId::Claude => {
            Box::new(terrarium_adapter_claude::transcript::ClaudeTranscript::new())
        }
        HarnessId::Codex => Box::new(terrarium_adapter_codex::transcript::CodexTranscript::new()),
        HarnessId::Copilot => {
            Box::new(terrarium_adapter_copilot::transcript::CopilotTranscript::new())
        }
        HarnessId::Pi => Box::new(terrarium_adapter_pi::transcript::PiTranscript::new()),
        HarnessId::Grok => Box::new(NoTranscript(harness)),
    }
}

/// A harness with no transcript parser yet (grok: its session files
/// need the signed-in capture, a TER-1192 release gate): no blocks.
struct NoTranscript(HarnessId);

impl Transcript for NoTranscript {
    fn harness(&self) -> HarnessId {
        self.0
    }
    fn parse_lines(&mut self, _bytes: &[u8]) -> Vec<terrarium_core::Block> {
        Vec::new()
    }
}

/// The harness that wrote a session file, from its path: the layout of
/// each harness differs. `None` when the path does not tell.
pub fn harness_of_path(path: &Path) -> Option<HarnessId> {
    let s = path.to_string_lossy();
    let name = path.file_name()?.to_string_lossy();
    if name == "events.jsonl" && s.contains("/session-state/") {
        Some(HarnessId::Copilot)
    } else if name.starts_with("rollout-") {
        Some(HarnessId::Codex)
    } else if s.contains("/projects/") && (s.contains("/.claude/") || s.contains("/claude/")) {
        Some(HarnessId::Claude)
    } else if s.contains("/pi/") || s.contains("/.pi/") {
        Some(HarnessId::Pi)
    } else {
        None
    }
}

/// A checked session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The file on this machine, with every symlink resolved. It may not
    /// exist yet (Codex and pi write it at the first message).
    pub path: PathBuf,
    pub harness: HarnessId,
}

/// Why a session has no transcript to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateError {
    /// No harness session, or no file for it.
    Missing(String),
    /// A file outside the agent's own dirs, or of another agent's session.
    Refused(String),
}

impl LocateError {
    pub fn message(&self) -> &str {
        match self {
            Self::Missing(m) | Self::Refused(m) => m,
        }
    }
}

/// The files of `harness` that the adapter's lookup finds for the harness
/// session `sid` in `roots` (the session areas of
/// [`transcript_roots`]).
fn find_in_roots(harness: HarnessId, roots: &[PathBuf], sid: &str) -> Option<PathBuf> {
    use terrarium_core::HarnessSessionId;
    let id = HarnessSessionId(sid.to_owned());
    roots.iter().find_map(|root| match harness {
        // `projects/<encoded cwd>/<id>.jsonl`, in any cwd dir.
        HarnessId::Claude => std::fs::read_dir(root)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path().join(format!("{sid}.jsonl")))
            .find(|p| p.is_file()),
        // `sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`; the lookup takes the
        // home that holds `sessions/`.
        HarnessId::Codex => {
            terrarium_adapter_codex::session::find_session_file(root.parent()?, &id)
        }
        HarnessId::Copilot => Some(root.join(sid).join("events.jsonl")).filter(|p| p.is_file()),
        HarnessId::Pi => terrarium_adapter_pi::session::find_session_file(root, &id),
        HarnessId::Grok => None,
    })
}

/// Find and check the session file of `agent`'s harness session `sid`.
/// `reported` is the `transcript_path` of its `session_started` event, as
/// the harness reported it (a sandbox path), if one came.
pub async fn locate(
    state: &Shared,
    agent: &Agent,
    sid: &str,
    reported: Option<&str>,
) -> Result<Located, LocateError> {
    locate_for(state, agent.id, agent.harness, sid, reported).await
}

/// [`locate`] by the agent ID and harness only: the archive
/// (`crate::session_archive`) uses it for an agent that the store
/// deleted already.
pub async fn locate_for(
    state: &Shared,
    agent_id: AgentId,
    agent_harness: HarnessId,
    sid: &str,
    reported: Option<&str>,
) -> Result<Located, LocateError> {
    if sid.is_empty() {
        return Err(LocateError::Missing(
            "the harness has not reported its session yet".into(),
        ));
    }
    let harness = reported
        .and_then(|r| harness_of_path(Path::new(r)))
        .unwrap_or(agent_harness);
    let home = state.usage.config.home(harness);
    let harness_dir = terrarium_runner::harness_dir(&state.state_dir, agent_id);
    let roots = transcript_roots(
        harness,
        &home,
        &harness_dir,
        state.usage.config.per_agent_sessions,
    );
    let candidate = match reported {
        Some(r) => host_path(Path::new(r), harness, &home, &harness_dir),
        None => find_in_roots(harness, &roots, sid).ok_or_else(|| {
            LocateError::Missing(format!("no session file for the harness session {sid}"))
        })?,
    };
    let path = check_transcript(&candidate, harness, sid, &roots)
        .map_err(|e| LocateError::Refused(format!("{}: {e}", candidate.display())))?;
    if harness != HarnessId::Pi {
        let (me, s) = (agent_id.to_string(), sid.to_owned());
        let owner = state
            .db
            .read(move |c| agents::harness_session_owner(c, &s, &me))
            .await
            .map_err(|e| LocateError::Missing(e.to_string()))?;
        if let Some(other) = owner {
            return Err(LocateError::Refused(format!(
                "the harness session {sid} belongs to the agent {other}"
            )));
        }
    }
    Ok(Located { path, harness })
}

/// Locate the file of one session row: its reported path, else the
/// adapter's lookup.
pub async fn locate_session(
    state: &Shared,
    agent: &Agent,
    session: &agents::Session,
) -> Result<Located, LocateError> {
    locate_row(
        state,
        agent.id,
        agent.harness,
        &session.id,
        session.harness_session_id.as_deref(),
    )
    .await
}

/// [`locate_session`] by the parts of the row: the agent may be deleted.
pub async fn locate_row(
    state: &Shared,
    agent_id: AgentId,
    harness: HarnessId,
    session: &str,
    harness_session_id: Option<&str>,
) -> Result<Located, LocateError> {
    let id = session.to_owned();
    let reported = state
        .db
        .read(move |c| tstore::reported_transcript(c, &id))
        .await
        .map_err(|e| LocateError::Missing(e.to_string()))?;
    let (path, reported_sid) = reported.unwrap_or_default();
    let sid = harness_session_id
        .map(str::to_owned)
        .or(reported_sid)
        .unwrap_or_default();
    locate_for(state, agent_id, harness, &sid, path.as_deref()).await
}

/// The parse state of one file.
pub struct Entry {
    pub harness: HarnessId,
    tail: LineTail,
    parser: Box<dyn Transcript>,
    pub log: TranscriptLog,
    /// The file name, for `source.file`.
    pub file: String,
}

impl Entry {
    fn new(path: &Path, harness: HarnessId) -> Self {
        Self {
            harness,
            tail: LineTail::new(path),
            parser: parser_for(harness),
            log: TranscriptLog::new(),
            file: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// Parse the bytes that the file got since the last read. This blocks.
    pub fn refresh(&mut self) -> std::io::Result<usize> {
        let chunk = self.tail.poll()?;
        if chunk.reset {
            self.parser = parser_for(self.harness);
            self.log = TranscriptLog::new();
        }
        if chunk.lines.is_empty() {
            return Ok(0);
        }
        let mut blocks = self.parser.parse_lines(&chunk.lines);
        for b in &mut blocks {
            b.source.file.clone_from(&self.file);
        }
        Ok(self.log.apply(blocks))
    }

    /// The bytes read so far.
    pub fn offset(&self) -> u64 {
        self.tail.offset()
    }
}

/// The block count and the title of a file of a given length.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    pub blocks: usize,
    pub title: Option<String>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<PathBuf, Arc<Mutex<Entry>>>,
    /// Least recently used first.
    order: Vec<PathBuf>,
    summaries: HashMap<PathBuf, (u64, Summary)>,
}

/// The parsed transcripts of the newest [`CACHED_FILES`] files.
#[derive(Default)]
pub struct TranscriptCache {
    inner: Mutex<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Lock one entry.
pub fn lock_entry(e: &Mutex<Entry>) -> MutexGuard<'_, Entry> {
    lock(e)
}

impl TranscriptCache {
    /// The entry of `path`, made when it is not cached. The newest
    /// [`CACHED_FILES`] stay.
    pub fn entry(&self, loc: &Located) -> Arc<Mutex<Entry>> {
        let mut inner = lock(&self.inner);
        inner.order.retain(|p| p != &loc.path);
        inner.order.push(loc.path.clone());
        if let Some(e) = inner.entries.get(&loc.path)
            && lock(e).harness == loc.harness
        {
            return e.clone();
        }
        let e = Arc::new(Mutex::new(Entry::new(&loc.path, loc.harness)));
        inner.entries.insert(loc.path.clone(), e.clone());
        while inner.order.len() > CACHED_FILES {
            let old = inner.order.remove(0);
            inner.entries.remove(&old);
        }
        e
    }

    /// Read the new bytes of `loc` and give its entry. This blocks: call it
    /// on the blocking pool.
    pub fn refreshed(&self, loc: &Located) -> std::io::Result<Arc<Mutex<Entry>>> {
        let e = self.entry(loc);
        let summary = {
            let mut g = lock(&e);
            g.refresh()?;
            (
                g.offset(),
                Summary {
                    blocks: g.log.len(),
                    title: g.log.title(TITLE_CHARS),
                },
            )
        };
        lock(&self.inner)
            .summaries
            .insert(loc.path.clone(), summary);
        Ok(e)
    }

    /// The block count and title of `loc`. A cached entry, or a summary of
    /// a file whose length did not change, answers without a parse; else
    /// the file is parsed once (and not kept). This blocks.
    pub fn summary(&self, loc: &Located) -> std::io::Result<Summary> {
        let len = match std::fs::metadata(&loc.path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Summary::default()),
            Err(e) => return Err(e),
        };
        let cached = lock(&self.inner).entries.get(&loc.path).cloned();
        if cached.is_some() {
            let e = self.refreshed(loc)?;
            let g = lock(&e);
            return Ok(Summary {
                blocks: g.log.len(),
                title: g.log.title(TITLE_CHARS),
            });
        }
        if let Some((at, s)) = lock(&self.inner).summaries.get(&loc.path)
            && *at == len
        {
            return Ok(s.clone());
        }
        let mut e = Entry::new(&loc.path, loc.harness);
        e.refresh()?;
        let s = Summary {
            blocks: e.log.len(),
            title: e.log.title(TITLE_CHARS),
        };
        lock(&self.inner)
            .summaries
            .insert(loc.path.clone(), (e.offset(), s.clone()));
        Ok(s)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn harness_from_the_path_layout() {
        let cases = [
            (
                "/home/agent/.claude/projects/-home-agent-work/01a1.jsonl",
                Some(HarnessId::Claude),
            ),
            (
                "/home/agent/.codex/sessions/2026/10/25/rollout-2026-10-25T20-42-36-01a1.jsonl",
                Some(HarnessId::Codex),
            ),
            (
                "/home/agent/.copilot/session-state/01a1/events.jsonl",
                Some(HarnessId::Copilot),
            ),
            (
                "/state/agents/X/harness/pi/sessions/2026-10-25T22-19-29-913Z_01a1.jsonl",
                Some(HarnessId::Pi),
            ),
            ("/tmp/x.jsonl", None),
        ];
        for (p, want) in cases {
            assert_eq!(harness_of_path(Path::new(p)), want, "{p}");
        }
    }

    #[test]
    fn cache_reads_only_new_bytes_and_keeps_a_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let line = |id: &str, t: &str| {
            format!(
                "{{\"type\":\"message\",\"id\":\"{id}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{t}\"}}]}}}}\n"
            )
        };
        std::fs::write(&path, line("a", "first")).unwrap();
        let loc = Located {
            path: path.clone(),
            harness: HarnessId::Pi,
        };
        let cache = TranscriptCache::default();
        assert_eq!(cache.summary(&loc).unwrap().blocks, 1);
        let e = cache.refreshed(&loc).unwrap();
        assert_eq!(lock_entry(&e).log.len(), 1);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, line("b", "second").as_bytes()).unwrap();
        let e = cache.refreshed(&loc).unwrap();
        let g = lock_entry(&e);
        assert_eq!(g.log.len(), 2);
        assert_eq!(g.log.blocks()[1].seq, 2);
        assert_eq!(g.log.blocks()[1].source.file, "s.jsonl");
        drop(g);
        let s = cache.summary(&loc).unwrap();
        assert_eq!((s.blocks, s.title.as_deref()), (2, Some("first")));
    }
}
