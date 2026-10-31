//! The template files of the config repo (PLAN §5b, docs/api.md §21.5):
//! `templates/<name>.wcl` and `templates/<project>/<name>.wcl`.
//!
//! The files are the source of truth; the `agent_templates` table is
//! their index, so that the API, the fleets, and an agent create read one
//! store. [`load`] makes the table match the files:
//!
//! 1. A row with no file yet (`file_hash` NULL: a row from before the
//!    template files, or a write with no config repo) gets its file, once.
//!    The row wins over a file at that path: at the upgrade, that file is
//!    a packaged default that the same start wrote.
//! 2. Each file whose hash differs from its row is read
//!    (`terrarium_core::template_file::parse`) and written into the row. A
//!    new file makes a row (`source = config`). A file that does not read
//!    keeps the last good row, and its error goes to the log and the
//!    answer of the reload.
//! 3. A row with a file (`file_hash` set) whose file is gone is deleted.
//!
//! [`refresh`] runs the load when the daemon starts with its config repo
//! ([`configure`]), before each template read of the API, after a pull
//! or a sync of the config repo, at `POST /api/reload`, and on a timer
//! ([`spawn_watch`]): an edit of a file by hand shows within
//! [`WATCH_INTERVAL`]. A write through the API, a fleet, or a project
//! file writes the file in its own transaction (`store::templates`), so
//! the next load finds the row and the file equal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use terrarium_core::template_file;
use tracing::{info, warn};

use crate::api::Shared;
use crate::db::Tx;
use crate::error::ApiResult;
use crate::store::templates;

/// The time between two checks of the files for a change by hand.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(10);

/// The state of the loads: the errors of the last load, so that the log
/// shows each new error once.
#[derive(Debug, Default)]
pub struct Runtime {
    /// One load at a time.
    busy: tokio::sync::Mutex<Vec<String>>,
}

/// One template file on disk.
#[derive(Debug, Clone)]
pub struct OnDisk {
    pub rel: String,
    pub project: Option<String>,
    pub name: String,
    pub text: String,
    pub hash: String,
}

/// What a load did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    /// Rows that got their first file (step 1).
    pub written: Vec<String>,
    /// Rows that a file made or changed (step 2).
    pub loaded: Vec<String>,
    /// Rows whose file is gone (step 3).
    pub removed: Vec<String>,
    /// The files that do not read, one line each (`<file>:<line>:<col>:
    /// <message>`).
    pub errors: Vec<String>,
}

impl Report {
    pub fn changed(&self) -> bool {
        !self.written.is_empty() || !self.loaded.is_empty() || !self.removed.is_empty()
    }
}

/// The template files under `<repo>/templates/`, sorted by path, and the
/// paths that are not a template file (a bad name, a deeper dir).
pub fn scan(repo: &Path) -> (Vec<OnDisk>, Vec<String>) {
    let root = repo.join(template_file::DIR);
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(ty) = e.file_type() else { continue };
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            if ty.is_dir() {
                stack.push(path);
                continue;
            }
            if !ty.is_file() || !name.ends_with(&format!(".{}", template_file::EXTENSION)) {
                continue;
            }
            let rel = format!(
                "{}/{}",
                template_file::DIR,
                path.strip_prefix(&root).unwrap_or(&path).to_string_lossy()
            );
            let Some((project, name)) = template_file::key_of_rel(&rel) else {
                errors.push(format!(
                    "{rel}: not a template file path: use templates/<name>.wcl or \
                     templates/<project>/<name>.wcl"
                ));
                continue;
            };
            if let Some(bad) = project
                .iter()
                .chain(std::iter::once(&name))
                .find(|n| !crate::store::valid_name(n))
            {
                errors.push(format!(
                    "{rel}: bad name `{bad}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a \
                     letter or digit"
                ));
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => files.push(OnDisk {
                        hash: crate::store::artifacts::sha256_hex(text.as_bytes()),
                        rel,
                        project,
                        name,
                        text,
                    }),
                    Err(_) => errors.push(format!("{rel}: not UTF-8")),
                },
                Err(e) => errors.push(format!("{rel}: {e}")),
            }
        }
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    errors.sort();
    (files, errors)
}

/// Step 1: give each row with no file its file. The row wins.
pub fn write_missing(t: &mut Tx<'_>, repo: &Path) -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    for (id, _, _, hash) in templates::index(t)? {
        if hash.is_some() {
            continue;
        }
        let tpl = templates::get(t, &id)?;
        if tpl.source == crate::store::projects::SOURCE_API
            || tpl.source == crate::store::projects::SOURCE_FILE
        {
            t.execute(
                "UPDATE agent_templates SET source = ?2 WHERE id = ?1",
                rusqlite::params![id, templates::SOURCE_CONFIG],
            )?;
        }
        match templates::stage_file(t, Some(repo), &tpl) {
            Ok(()) => out.push(tpl.rel_path()),
            // The scan refused the old row: it stays in the store, with no
            // file, and the next load tries again.
            Err(e) => warn!(template = %tpl.key(), "cannot write the template file: {}", e.message),
        }
    }
    Ok(out)
}

/// Steps 2 and 3: make the rows match the files of `repo`.
pub fn sync(t: &mut Tx<'_>, repo: &Path) -> ApiResult<Report> {
    let (files, mut errors) = scan(repo);
    let mut report = Report::default();
    let rows: BTreeMap<(Option<String>, String), (String, Option<String>)> = templates::index(t)?
        .into_iter()
        .map(|(id, project, name, hash)| ((project, name), (id, hash)))
        .collect();
    let mut seen = BTreeSet::new();
    for f in &files {
        let key = (f.project.clone(), f.name.clone());
        seen.insert(key.clone());
        if rows.get(&key).and_then(|(_, h)| h.as_ref()) == Some(&f.hash) {
            continue;
        }
        let parsed = match template_file::parse(&f.text, &f.rel) {
            Ok(p) => p,
            Err(e) => {
                errors.extend(e.to_string().lines().map(str::to_owned));
                continue;
            }
        };
        if let Some(p) = &f.project
            && crate::store::projects::find(t, p)?.is_none()
        {
            errors.push(format!("{}: no project `{p}`", f.rel));
            continue;
        }
        let (project, name, description, spec, hash) = (
            f.project.clone(),
            f.name.clone(),
            parsed.description,
            parsed.spec,
            f.hash.clone(),
        );
        // The checks of a write first: a refusal (a bad value) leaves the
        // old row as it was.
        if let Err(e) = templates::checked(t, &spec) {
            errors.push(format!("{}: {}", f.rel, e.message));
            continue;
        }
        templates::apply_file(t, project.as_deref(), &name, description, &spec, &hash)?;
        report.loaded.push(f.rel.clone());
    }
    for ((project, name), (id, hash)) in rows {
        if hash.is_none() || seen.contains(&(project.clone(), name.clone())) {
            continue;
        }
        templates::delete(t, &id, None)?;
        report
            .removed
            .push(template_file::rel_path(project.as_deref(), &name));
    }
    report.errors = errors;
    Ok(report)
}

/// The config repo dir, when the daemon has one.
pub fn repo_dir(state: &Shared) -> Option<PathBuf> {
    state.config_repo.get().ok().map(|r| r.dir)
}

/// Is a load needed? True when a row has no file, or a file and its row
/// differ. A read only: most checks find nothing to do.
async fn stale(state: &Shared, repo: PathBuf) -> ApiResult<bool> {
    let (files, _) = tokio::task::spawn_blocking(move || scan(&repo))
        .await
        .map_err(|e| crate::error::ApiError::internal(format!("template scan task: {e}")))?;
    let rows = state.db.read(templates::index).await?;
    if rows.iter().any(|(_, _, _, h)| h.is_none()) {
        return Ok(true);
    }
    let on_disk: BTreeMap<(Option<String>, String), String> = files
        .into_iter()
        .map(|f| ((f.project, f.name), f.hash))
        .collect();
    let in_store: BTreeMap<(Option<String>, String), String> = rows
        .into_iter()
        .filter_map(|(_, p, n, h)| h.map(|h| ((p, n), h)))
        .collect();
    Ok(on_disk != in_store)
}

/// Load the files of the config repo into the store ([`sync`], after
/// [`write_missing`]), when a file or a row changed. With no config repo,
/// nothing. A new error goes to the log once.
pub async fn refresh(state: &Shared) -> ApiResult<Report> {
    let Some(repo) = repo_dir(state) else {
        return Ok(Report::default());
    };
    let mut last = state.config_repo.templates.busy.lock().await;
    if !stale(state, repo.clone()).await? {
        return Ok(Report {
            errors: last.clone(),
            ..Report::default()
        });
    }
    let dir = repo.clone();
    let written = state
        .db
        .write("system", move |t| write_missing(t, &dir))
        .await?;
    let mut report = state.db.write("system", move |t| sync(t, &repo)).await?;
    report.written = written;
    if report.changed() {
        info!(
            written = report.written.len(),
            loaded = report.loaded.len(),
            removed = report.removed.len(),
            "template files loaded"
        );
    }
    for e in report.errors.iter().filter(|e| !last.contains(e)) {
        warn!("template file: {e}");
    }
    last.clone_from(&report.errors);
    Ok(report)
}

/// [`refresh`] for a caller that goes on with the store as it is: an
/// error goes to the log.
pub async fn refresh_quiet(state: &Shared) {
    if let Err(e) = refresh(state).await {
        warn!("cannot load the template files: {}", e.message);
    }
}

/// Configure the config repo of the daemon, then load the template files
/// (the first load writes the files of the rows from before them).
pub async fn configure(state: &Shared, repo: crate::config_repo::ConfigRepo) {
    state.config_repo.configure(repo);
    match refresh(state).await {
        Ok(r) => {
            if !r.written.is_empty() {
                info!(files = ?r.written, "templates moved from the database to the config repo");
            }
        }
        Err(e) => warn!("cannot load the template files: {}", e.message),
    }
}

/// Check the files on a timer, so that an edit by hand reaches the store
/// without a read of the API.
pub fn spawn_watch(state: Shared) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            refresh_quiet(&state).await;
        }
    })
}
