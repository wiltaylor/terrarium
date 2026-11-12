//! The delivery of the card files into the workspace of the assignee
//! (docs/api.md §8.4, PLAN §13).
//!
//! The files of each card that is assigned to an agent (not archived, not
//! done) go to `<workspace>/.terrarium/cards/<card>/<name>`: the latest
//! version, read-only (mode 0444). Each card dir also has `index.md`: the
//! title, the links, and the file list. A card dir of a card that the agent
//! no longer has, and a file that the card no longer has, go away.
//!
//! When it happens:
//!
//! - At each start: `RunnerSupervisor` calls [`deliver`] before the launch,
//!   and the listener ([`spawn`]) calls it on the `starting` status (the
//!   in-memory supervisor of the tests too).
//! - While the agent is live: on `card.file_added`, `card.file_replaced`,
//!   and `card.file_removed`, and on a `card` event that assigns, claims,
//!   moves, or archives a card. The listener updates the dir at once, and
//!   mails the agent a notice from `system` (a `[Mailbox:system]` item)
//!   for a file change, or for a new assignment of a card with files. The
//!   agent's own change gets no notice.
//!
//! The agent owns its workspace, so the writes are fd-relative and follow
//! no symlink: `.terrarium` and `.terrarium/cards` must be real dirs (a
//! symlink there stops the delivery with an error); a symlink or a file in
//! the place of a card dir, or of a file in it, is removed, never followed.
//! Each file is a new temp file (`O_EXCL`, `O_NOFOLLOW`), then a
//! `renameat` over the old entry. `/.terrarium/` goes into
//! `.git/info/exclude` of a workspace that is a git repo
//! ([`crate::api::attachments::exclude`]), so an agent does not commit the
//! files.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, SFlag, fstatat};
use serde_json::Value;
use terrarium_core::{Agent, AgentId};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::api::Shared;
use crate::store::card_files::{self, DeliveredCard, INDEX_NAME};
use crate::store::mail::{self, Address, Outgoing};

/// The card dirs, relative to the workspace root.
pub const CARDS_DIR: &str = ".terrarium/cards";
/// The mode of a delivered file.
const FILE_MODE: u32 = 0o444;
/// The most levels of dirs that a removal goes down (the agent may make
/// dirs in a card dir).
const MAX_DEPTH: usize = 32;

const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DeliveryError {
    /// A symlink or a file in the place of a dir the daemon needs.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Io(String),
}

fn io(what: impl std::fmt::Display, e: impl std::fmt::Display) -> DeliveryError {
    DeliveryError::Io(format!("{what}: {e}"))
}

/// What one [`sync`] changed, as paths relative to [`CARDS_DIR`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub written: Vec<String>,
    pub removed: Vec<String>,
}

impl Report {
    pub fn changed(&self) -> bool {
        !self.written.is_empty() || !self.removed.is_empty()
    }
}

/// The text of `index.md` of one card. `inside` is the card dir as the
/// harness sees it; `link` the dashboard URL of the card, when known.
pub fn index_text(c: &DeliveredCard, inside: &Path, link: Option<&str>) -> String {
    let id = &c.card.id;
    let mut s = format!("# {id}: {}\n\n", one_line(&c.card.title));
    s.push_str(&format!(
        "- Card: {id} in project `{}`. Read it with `terrarium card show {id}`.\n",
        c.card.project
    ));
    if let Some(l) = link {
        s.push_str(&format!("- Dashboard: {l}\n"));
    }
    s.push_str(&format!("- This dir: `{}`\n", inside.display()));
    s.push_str(&format!(
        "\nThe daemon writes this dir: read-only copies of the latest version of \
         each file of the card. Edits here are lost. Add or replace a file with \
         `terrarium card attach {id} <path> [--replace]`; list the versions with \
         `terrarium card files versions {id} <name>`.\n\n"
    ));
    if c.files.is_empty() {
        s.push_str("The card has no files.\n");
        return s;
    }
    s.push_str("| File | Version | Bytes | Type | Added by | Added at |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for f in &c.files {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            f.name,
            f.version,
            f.size,
            f.media_type.split(';').next().unwrap_or(""),
            one_line(&f.created_by_name),
            f.created_at
        ));
    }
    s
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r', '|'], " ")
}

/// Open the dir `name` under `parent`, with no symlink followed. Make it
/// (mode 0755) when it is not there. `replace`: a symlink or a file in its
/// place is removed first; else it is [`DeliveryError::Refused`].
fn open_dir(
    parent: &OwnedFd,
    name: &OsStr,
    shown: &str,
    replace: bool,
) -> Result<OwnedFd, DeliveryError> {
    // A replace, a make, then the open.
    for _ in 0..3 {
        match openat(parent, name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => return Ok(fd),
            Err(Errno::ENOENT) => {
                match nix::sys::stat::mkdirat(parent, name, Mode::from_bits_truncate(0o755)) {
                    Ok(()) | Err(Errno::EEXIST) => continue,
                    Err(e) => return Err(io(shown, e)),
                }
            }
            Err(Errno::ELOOP | Errno::ENOTDIR) if replace => {
                remove_entry(parent, name, 0).map_err(|e| io(shown, e))?;
            }
            Err(Errno::ELOOP | Errno::ENOTDIR) => {
                return Err(DeliveryError::Refused(format!(
                    "{shown} in the workspace is a symlink or a file; the daemon writes the \
                     card files only into a real dir. Remove it, and the next delivery \
                     makes the dir."
                )));
            }
            Err(e) => return Err(io(shown, e)),
        }
    }
    Err(DeliveryError::Io(format!("{shown}: cannot make the dir")))
}

/// The names in the dir `fd`, with their kind (no symlink followed).
fn entries(fd: &OwnedFd) -> Result<Vec<(OsString, bool)>, Errno> {
    let mut out = Vec::new();
    let mut d = nix::dir::Dir::openat(fd, ".", DIR_FLAGS, Mode::empty())?;
    for e in d.iter() {
        let e = e?;
        let name = e.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let os = OsStr::from_bytes(name).to_owned();
        let is_dir = match e.file_type() {
            Some(t) => t == nix::dir::Type::Directory,
            None => {
                let st = fstatat(fd, os.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)?;
                SFlag::from_bits_truncate(st.st_mode & SFlag::S_IFMT.bits()) == SFlag::S_IFDIR
            }
        };
        out.push((os, is_dir));
    }
    Ok(out)
}

/// Remove the entry `name` of the dir `parent`: a file or a symlink with
/// `unlinkat`, a dir with all its content. No symlink is followed.
fn remove_entry(parent: &OwnedFd, name: &OsStr, depth: usize) -> Result<(), Errno> {
    use nix::unistd::{UnlinkatFlags, unlinkat};
    let st = match fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::ENOENT) => return Ok(()),
        Err(e) => return Err(e),
    };
    let is_dir = SFlag::from_bits_truncate(st.st_mode & SFlag::S_IFMT.bits()) == SFlag::S_IFDIR;
    if !is_dir {
        return match unlinkat(parent, name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(e) => Err(e),
        };
    }
    if depth > MAX_DEPTH {
        return Err(Errno::ELOOP);
    }
    let fd = openat(parent, name, DIR_FLAGS, Mode::empty())?;
    // A dir that the agent made read-only: the daemon may still empty it.
    let _ = nix::sys::stat::fchmod(&fd, Mode::from_bits_truncate(0o755));
    for (child, _) in entries(&fd)? {
        remove_entry(&fd, &child, depth + 1)?;
    }
    drop(fd);
    match unlinkat(parent, name, UnlinkatFlags::RemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Does the regular file `name` in `dir` hold exactly `data`? A symlink
/// or a special file does not.
fn same_file(dir: &OwnedFd, name: &str, data: &[u8]) -> bool {
    let Ok(fd) = openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) else {
        return false;
    };
    let mut f = std::fs::File::from(fd);
    let Ok(m) = f.metadata() else { return false };
    if !m.is_file() || m.len() != data.len() as u64 {
        return false;
    }
    use std::os::unix::fs::PermissionsExt;
    if m.permissions().mode() & 0o777 != FILE_MODE {
        return false;
    }
    let mut buf = Vec::with_capacity(data.len());
    f.read_to_end(&mut buf).is_ok() && buf == data
}

/// Write `data` to `name` in `dir` (mode 0444): a new temp file, then a
/// rename over the old entry. A dir in the place of the file is removed
/// first.
fn write_file(dir: &OwnedFd, name: &str, data: &[u8]) -> Result<(), Errno> {
    let tmp = format!(".{name}.{}.part", ulid::Ulid::new());
    let fd = openat(
        dir,
        tmp.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?;
    let mut f = std::fs::File::from(fd);
    let written = f
        .write_all(data)
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(FILE_MODE))
        })
        .map_err(|e| Errno::from_raw(e.raw_os_error().unwrap_or(nix::libc::EIO)));
    drop(f);
    let renamed = written.and_then(|()| {
        if let Ok(st) = fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW)
            && SFlag::from_bits_truncate(st.st_mode & SFlag::S_IFMT.bits()) == SFlag::S_IFDIR
        {
            remove_entry(dir, OsStr::new(name), 0)?;
        }
        nix::fcntl::renameat(dir, tmp.as_str(), dir, name)
    });
    if renamed.is_err() {
        let _ = nix::unistd::unlinkat(dir, tmp.as_str(), nix::unistd::UnlinkatFlags::NoRemoveDir);
    }
    renamed
}

/// Make `<workspace>/.terrarium/cards/` match `cards`. `inside` is the
/// workspace as the harness sees it, for the index; `link` makes the
/// dashboard URL of a card. Blocking.
pub fn sync(
    workspace: &Path,
    state_dir: &Path,
    cards: &[DeliveredCard],
    inside: &Path,
    link: &dyn Fn(&DeliveredCard) -> Option<String>,
) -> Result<Report, DeliveryError> {
    let mut report = Report::default();
    let ws = openat(
        nix::fcntl::AT_FDCWD,
        workspace,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io("the workspace", e))?;
    if cards.is_empty() {
        // Nothing to deliver: do not make the dirs, only empty them.
        let top = match openat(&ws, ".terrarium", DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(_) => return Ok(report),
        };
        let dir = match openat(&top, "cards", DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(_) => return Ok(report),
        };
        for (name, _) in entries(&dir).map_err(|e| io(CARDS_DIR, e))? {
            remove_entry(&dir, &name, 0).map_err(|e| io(CARDS_DIR, e))?;
            report.removed.push(name.to_string_lossy().into_owned());
        }
        return Ok(report);
    }
    let top = open_dir(&ws, OsStr::new(".terrarium"), ".terrarium", false)?;
    let dir = open_dir(&top, OsStr::new("cards"), CARDS_DIR, false)?;
    let wanted: BTreeSet<&str> = cards.iter().map(|c| c.card.id.as_str()).collect();
    for (name, _) in entries(&dir).map_err(|e| io(CARDS_DIR, e))? {
        if !name.to_str().is_some_and(|n| wanted.contains(n)) {
            remove_entry(&dir, &name, 0).map_err(|e| io(CARDS_DIR, e))?;
            report.removed.push(name.to_string_lossy().into_owned());
        }
    }
    for c in cards {
        let id = c.card.id.as_str();
        let shown = format!("{CARDS_DIR}/{id}");
        let card_dir = open_dir(&dir, OsStr::new(id), &shown, true)?;
        let names: BTreeSet<&str> = c
            .files
            .iter()
            .map(|f| f.name.as_str())
            .chain([INDEX_NAME])
            .collect();
        for (name, _) in entries(&card_dir).map_err(|e| io(&shown, e))? {
            if !name.to_str().is_some_and(|n| names.contains(n)) {
                remove_entry(&card_dir, &name, 0).map_err(|e| io(&shown, e))?;
                report
                    .removed
                    .push(format!("{id}/{}", name.to_string_lossy()));
            }
        }
        for f in &c.files {
            let data = card_files::read_blob(state_dir, &f.sha256)
                .map_err(|e| DeliveryError::Io(e.message))?;
            if !same_file(&card_dir, &f.name, &data) {
                write_file(&card_dir, &f.name, &data)
                    .map_err(|e| io(format!("{shown}/{}", f.name), e))?;
                report.written.push(format!("{id}/{}", f.name));
            }
        }
        let card_inside = inside.join(CARDS_DIR).join(id);
        let index = index_text(c, &card_inside, link(c).as_deref());
        if !same_file(&card_dir, INDEX_NAME, index.as_bytes()) {
            write_file(&card_dir, INDEX_NAME, index.as_bytes())
                .map_err(|e| io(format!("{shown}/{INDEX_NAME}"), e))?;
            report.written.push(format!("{id}/{INDEX_NAME}"));
        }
    }
    if let Err(e) = crate::api::attachments::exclude(workspace) {
        warn!(workspace = %workspace.display(), "cannot add the git exclude of the card files: {e}");
    }
    Ok(report)
}

/// One lock for each agent, so two deliveries do not run at once.
fn lock_of(agent: AgentId) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<AgentId, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut m = match LOCKS.get_or_init(Default::default).lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    m.entry(agent).or_default().clone()
}

/// The dashboard URL of a card, from `TERRARIUM_PUBLIC_URL`.
fn card_link(state: &Shared, c: &DeliveredCard) -> Option<String> {
    let base = state.web.public_url.as_deref()?.trim_end_matches('/');
    Some(format!(
        "{base}/p/{}/board?card={}",
        c.card.project, c.card.id
    ))
}

/// Deliver the card files of `agent` into its workspace. The workspace is
/// made when it is not there.
pub async fn deliver(state: &Shared, agent: &Agent) -> Result<Report, DeliveryError> {
    let lock = lock_of(agent.id);
    let _held = lock.lock().await;
    let Some(workspace) = crate::workspace::ensure_generic(state, agent, false)
        .await
        .map_err(|e| DeliveryError::Io(e.message))?
    else {
        return Err(DeliveryError::Io(format!(
            "cannot make the workspace {}",
            agent.workspace.display()
        )));
    };
    let id = agent.id.to_string();
    let cards = state
        .db
        .read(move |c| card_files::assigned(c, &id))
        .await
        .map_err(|e| DeliveryError::Io(e.message))?;
    let inside = state.supervisor.workspace_inside(agent);
    let state_dir = state.state_dir.clone();
    let st = state.clone();
    let report = tokio::task::spawn_blocking(move || {
        sync(&workspace, &state_dir, &cards, &inside, &|c| {
            card_link(&st, c)
        })
    })
    .await
    .map_err(|e| DeliveryError::Io(format!("the delivery task failed: {e}")))??;
    if report.changed() {
        debug!(agent = %agent.id, written = ?report.written, removed = ?report.removed, "card files delivered");
    }
    Ok(report)
}

/// Deliver at the start of `agent`; a failure is a warning, not a stop.
pub async fn deliver_at_start(state: &Shared, agent: &Agent) {
    if let Err(e) = deliver(state, agent).await {
        warn!(agent = %agent.id, "cannot deliver the card files: {e}");
    }
}

/// The agent `id` when it is live (a ULID of an agent that runs).
async fn live_agent(state: &Shared, id: &str) -> Option<Agent> {
    let id = id.parse::<AgentId>().ok()?.to_string();
    let view = state
        .db
        .read(move |c| crate::store::agents::get(c, &id))
        .await
        .ok()?;
    view.agent.status.is_live().then_some(view.agent)
}

/// What a hub event asks of the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Job {
    /// Deliver to these agents (ULIDs), when they are live.
    agents: Vec<String>,
    /// Mail this agent a notice with this subject and body.
    notice: Option<(String, String, String)>,
    /// Mail the notice only when the card has files (a new assignment).
    only_with_files: bool,
}

/// The job of one hub event, if any.
fn job_of(ev: &crate::events::StoredEvent) -> Option<Job> {
    let s = |k: &str| ev.data.get(k).and_then(Value::as_str);
    let self_made = |agent: &str| ev.actor.as_deref() == Some(format!("agent:{agent}").as_str());
    let card = ev.card.as_deref();
    match ev.kind.as_str() {
        "status" if s("status") == Some("starting") => Some(Job {
            only_with_files: false,
            agents: vec![ev.agent_id.clone()?],
            notice: None,
        }),
        k if k.starts_with("card.file_") => {
            let agent = ev.agent_id.clone()?;
            let card = card?;
            let name = s("name").unwrap_or("?");
            let path = format!("{CARDS_DIR}/{card}/{name}");
            let (subject, body) = match k {
                "card.file_added" => (
                    format!("{card}: file {name} added"),
                    format!(
                        "The file `{name}` (version {}) was added to card {card}. A read-only \
                         copy is in `{path}`; `{CARDS_DIR}/{card}/{INDEX_NAME}` lists the files.",
                        ev.data["version"]
                    ),
                ),
                "card.file_replaced" => (
                    format!("{card}: file {name} replaced"),
                    format!(
                        "The file `{name}` of card {card} has a new version ({}). `{path}` \
                         has it now. The old versions: `terrarium card files versions {card} {name}`.",
                        ev.data["version"]
                    ),
                ),
                "card.file_removed" => (
                    format!("{card}: file {name} removed"),
                    format!("The file `{name}` was removed from card {card}; `{path}` is gone."),
                ),
                _ => return None,
            };
            let notice = (!self_made(&agent)).then(|| (agent.clone(), subject, body));
            Some(Job {
                agents: vec![agent],
                notice,
                only_with_files: false,
            })
        }
        "card" => {
            let change = s("change")?;
            if !matches!(change, "assigned" | "claimed" | "moved" | "archived") {
                return None;
            }
            let mut agents: Vec<String> = ev.agent_id.iter().cloned().collect();
            if let Some(p) = s("previous").filter(|p| *p != "operator") {
                agents.push(p.to_owned());
            }
            agents.dedup();
            let notice = match (change, s("assignee"), card) {
                ("assigned", Some(a), Some(card)) if a != "operator" && !self_made(a) => Some((
                    a.to_owned(),
                    format!("{card}: card files"),
                    format!(
                        "Card {card} was assigned to you, and it has files. Read-only copies \
                         are in `{CARDS_DIR}/{card}/`; `{CARDS_DIR}/{card}/{INDEX_NAME}` lists them."
                    ),
                )),
                _ => None,
            };
            Some(Job {
                agents,
                notice,
                only_with_files: true,
            })
        }
        _ => None,
    }
}

/// Mail `agent` a notice from `system`.
async fn notify(state: &Shared, agent: &str, subject: String, body: String) {
    let id = agent.to_owned();
    let r = state
        .db
        .write("system", move |t| {
            let to = mail::resolve(t, &id, None)?;
            mail::send(
                t,
                Outgoing {
                    from: &Address::System,
                    to: &to,
                    subject: &subject,
                    body: &body,
                    thread: None,
                    in_reply_to: None,
                },
            )
            .map(|_| ())
        })
        .await;
    if let Err(e) = r {
        warn!(agent, "cannot mail the card file notice: {}", e.message);
    }
}

async fn run_job(state: &Shared, job: Job, card: Option<String>) {
    let mut delivered: Vec<String> = Vec::new();
    for a in &job.agents {
        let Some(agent) = live_agent(state, a).await else {
            continue;
        };
        match deliver(state, &agent).await {
            Ok(_) => delivered.push(a.clone()),
            Err(e) => warn!(agent = %agent.id, "cannot deliver the card files: {e}"),
        }
    }
    let Some((agent, subject, body)) = job.notice else {
        return;
    };
    if !delivered.contains(&agent) {
        return;
    }
    // A new assignment mails a notice only when the card has files.
    if job.only_with_files {
        let Some(card) = card else { return };
        let has = state
            .db
            .read(move |c| card_files::list(c, &card).map(|f| !f.is_empty()))
            .await
            .unwrap_or(false);
        if !has {
            return;
        }
    }
    notify(state, &agent, subject, body).await;
}

/// The listener: deliver on a start, on a file change, and on a change of
/// the assigned cards (module docs).
pub fn spawn(state: &Shared) -> JoinHandle<()> {
    let mut rx = state.db.hub().subscribe();
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    warn!("the card file delivery missed {n} events");
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            let Some(job) = job_of(&ev) else {
                continue;
            };
            let state = state.clone();
            let card = ev.card.clone();
            tokio::spawn(async move { run_job(&state, job, card).await });
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::store::board::CardRef;
    use crate::store::card_files::CardFile;
    use crate::store::card_files::{FileKind, Prepared, store_blob};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn file(state: &Path, card: &str, name: &str, data: &[u8]) -> CardFile {
        let sha = crate::store::artifacts::sha256_hex(data);
        store_blob(
            state,
            &Prepared {
                name: name.into(),
                kind: FileKind::Text,
                data: data.to_vec(),
                sha256: sha.clone(),
            },
        )
        .unwrap();
        CardFile {
            card: card.into(),
            name: name.into(),
            version: 1,
            size: data.len() as i64,
            sha256: sha,
            media_type: "text/plain; charset=utf-8".into(),
            created_by: "operator".into(),
            created_by_name: "operator".into(),
            created_at: "2026-11-04T00:00:00.000Z".into(),
            versions: 1,
        }
    }

    fn card(id: &str, files: Vec<CardFile>) -> DeliveredCard {
        DeliveredCard {
            card: CardRef {
                id: id.into(),
                project: "p".into(),
                title: "Do it".into(),
                assignee: None,
                archived: false,
            },
            files,
        }
    }

    fn no_link(_: &DeliveredCard) -> Option<String> {
        None
    }

    #[test]
    fn sync_writes_read_only_files_and_removes_stale_ones() {
        let t = tempfile::tempdir().unwrap();
        let (state, ws) = (t.path().join("state"), t.path().join("ws"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(ws.join(".git/info")).unwrap();
        let a = file(&state, "P-1", "spec.md", b"# spec");
        let b = file(&state, "P-1", "notes.txt", b"notes");
        let c = file(&state, "P-2", "x.txt", b"x");
        let inside = Path::new("/work/p/a");
        let r = sync(
            &ws,
            &state,
            &[card("P-1", vec![a.clone(), b]), card("P-2", vec![c])],
            inside,
            &no_link,
        )
        .unwrap();
        assert_eq!(r.written.len(), 5, "{r:?}");
        let dir = ws.join(CARDS_DIR).join("P-1");
        assert_eq!(std::fs::read(dir.join("spec.md")).unwrap(), b"# spec");
        let mode = std::fs::metadata(dir.join("spec.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o444);
        let index = std::fs::read_to_string(dir.join(INDEX_NAME)).unwrap();
        assert!(index.starts_with("# P-1: Do it"), "{index}");
        assert!(
            index.contains("| spec.md | 1 | 6 | text/plain |"),
            "{index}"
        );
        assert!(index.contains("/work/p/a/.terrarium/cards/P-1"), "{index}");
        let exclude = std::fs::read_to_string(ws.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("/.terrarium/"));

        // One file and one card fewer: they go, the indexes change.
        let r = sync(
            &ws,
            &state,
            &[card("P-1", vec![a.clone()]), card("P-2", vec![])],
            inside,
            &no_link,
        )
        .unwrap();
        assert_eq!(r.removed, vec!["P-1/notes.txt", "P-2/x.txt"], "{r:?}");
        assert_eq!(r.written, vec!["P-1/index.md", "P-2/index.md"]);
        // An agent edit of a delivered file is put back.
        std::fs::set_permissions(dir.join("spec.md"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::write(dir.join("spec.md"), b"changed").unwrap();
        std::fs::write(dir.join("extra.md"), b"mine").unwrap();
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("sub/deeper/f"), b"f").unwrap();
        let r = sync(&ws, &state, &[card("P-1", vec![a])], inside, &no_link).unwrap();
        assert_eq!(r.written, vec!["P-1/spec.md"]);
        let mut removed = r.removed.clone();
        removed.sort();
        assert_eq!(removed, vec!["P-1/extra.md", "P-1/sub", "P-2"]);
        assert_eq!(std::fs::read(dir.join("spec.md")).unwrap(), b"# spec");
        assert!(!dir.join("extra.md").exists());
        assert!(!dir.join("sub").exists());
        assert!(!ws.join(CARDS_DIR).join("P-2").exists());

        // No cards: the dirs are emptied.
        let r = sync(&ws, &state, &[], inside, &no_link).unwrap();
        assert_eq!(r.removed, vec!["P-1"]);
        assert_eq!(std::fs::read_dir(ws.join(CARDS_DIR)).unwrap().count(), 0);
    }

    #[test]
    fn sync_refuses_and_replaces_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let (state, ws, elsewhere) = (
            t.path().join("state"),
            t.path().join("ws"),
            t.path().join("elsewhere"),
        );
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let a = file(&state, "P-1", "spec.md", b"# spec");
        let cards = [card("P-1", vec![a])];
        let inside = Path::new("/w");

        // `.terrarium` is a symlink: refused, nothing written elsewhere.
        std::os::unix::fs::symlink(&elsewhere, ws.join(".terrarium")).unwrap();
        let e = sync(&ws, &state, &cards, inside, &no_link).unwrap_err();
        assert!(matches!(e, DeliveryError::Refused(_)), "{e:?}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        std::fs::remove_file(ws.join(".terrarium")).unwrap();

        // `.terrarium/cards` is a symlink: refused.
        std::fs::create_dir(ws.join(".terrarium")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, ws.join(CARDS_DIR)).unwrap();
        assert!(matches!(
            sync(&ws, &state, &cards, inside, &no_link),
            Err(DeliveryError::Refused(_))
        ));
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        std::fs::remove_file(ws.join(CARDS_DIR)).unwrap();

        // A card dir that is a symlink, and a file that is a symlink: the
        // links go, the files are written in the real dir.
        std::fs::create_dir(ws.join(CARDS_DIR)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, ws.join(CARDS_DIR).join("P-1")).unwrap();
        sync(&ws, &state, &cards, inside, &no_link).unwrap();
        assert!(
            ws.join(CARDS_DIR)
                .join("P-1")
                .symlink_metadata()
                .unwrap()
                .is_dir()
        );
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        let target = elsewhere.join("target");
        std::fs::write(&target, b"keep").unwrap();
        let f = ws.join(CARDS_DIR).join("P-1/spec.md");
        std::fs::remove_file(&f).unwrap();
        std::os::unix::fs::symlink(&target, &f).unwrap();
        let r = sync(&ws, &state, &cards, inside, &no_link).unwrap();
        assert_eq!(r.written, vec!["P-1/spec.md"]);
        assert!(f.symlink_metadata().unwrap().is_file());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        // A symlink to a dir planted as a stale entry: removed, the target
        // dir keeps its files.
        std::fs::write(elsewhere.join("keep.txt"), b"k").unwrap();
        std::os::unix::fs::symlink(&elsewhere, ws.join(CARDS_DIR).join("P-1/link")).unwrap();
        sync(&ws, &state, &cards, inside, &no_link).unwrap();
        assert!(!ws.join(CARDS_DIR).join("P-1/link").exists());
        assert!(elsewhere.join("keep.txt").exists());
    }

    fn ev(kind: &str, agent: Option<&str>, actor: &str, data: Value) -> crate::events::StoredEvent {
        serde_json::from_value(json!({
            "seq": 1, "id": "E", "at": "2026-11-04T00:00:00Z", "kind": kind,
            "agent_id": agent, "card": "P-1", "actor": actor, "summary": "s", "data": data
        }))
        .unwrap()
    }

    #[test]
    fn jobs() {
        let a = "01JAAAAAAAAAAAAAAAAAAAAAAA";
        let j = job_of(&ev(
            "card.file_added",
            Some(a),
            "operator",
            json!({"name": "s.md", "version": 1}),
        ))
        .unwrap();
        assert_eq!(j.agents, vec![a]);
        let (to, subject, body) = j.notice.unwrap();
        assert_eq!(to, a);
        assert_eq!(subject, "P-1: file s.md added");
        assert!(body.contains(".terrarium/cards/P-1/s.md"), "{body}");
        // The agent's own change: a delivery, no notice.
        let j = job_of(&ev(
            "card.file_replaced",
            Some(a),
            &format!("agent:{a}"),
            json!({"name": "s.md"}),
        ))
        .unwrap();
        assert!(j.notice.is_none());
        // No agent assignee: nothing.
        assert!(
            job_of(&ev(
                "card.file_removed",
                None,
                "operator",
                json!({"name": "s.md"})
            ))
            .is_none()
        );
        let j = job_of(&ev(
            "card",
            Some(a),
            "operator",
            json!({"change": "assigned", "assignee": a, "previous": "01JBBBBBBBBBBBBBBBBBBBBBBB"}),
        ))
        .unwrap();
        assert_eq!(j.agents.len(), 2);
        assert!(j.notice.is_some());
        assert!(
            job_of(&ev(
                "card",
                Some(a),
                "operator",
                json!({"change": "commented"})
            ))
            .is_none()
        );
        let j = job_of(&ev(
            "status",
            Some(a),
            "system",
            json!({"status": "starting"}),
        ))
        .unwrap();
        assert_eq!(j.agents, vec![a]);
        assert!(job_of(&ev("status", Some(a), "system", json!({"status": "idle"}))).is_none());
    }
}
