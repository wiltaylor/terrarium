//! Sync file definitions (`projects/`, `agents/`) into the store.
//!
//! The file is the source of truth for an object with `source = file`:
//! the API refuses to edit it, and each sync writes the file values again.
//!
//! - A project or agent in a file is made, or updated, and marked `file`.
//!   An API object with the same name becomes `file`.
//! - A file project whose file is gone becomes an `api` project. Its board
//!   and cards stay.
//! - A file agent whose file is gone is deleted. The caller stops it first
//!   when it runs; its events stay in the log.

use std::path::Path;

use rusqlite::params;
use serde::Serialize;
use serde_json::json;
use terrarium_core::config::Definitions;

use crate::db::{Tx, now};
use crate::error::ApiResult;
use crate::store::projects::{self, NewProject, NewRepo, SOURCE_API, SOURCE_FILE};
use crate::store::{agents, check_name};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Changes {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SyncReport {
    pub projects: Changes,
    pub agents: Changes,
    /// Files that do not parse, and definitions the store refused.
    pub errors: Vec<String>,
    /// File agents that were removed and must be stopped.
    #[serde(skip)]
    pub removed_agent_ids: Vec<String>,
}

pub fn sync(t: &mut Tx<'_>, defs: &Definitions, state_dir: &Path) -> ApiResult<SyncReport> {
    let mut report = SyncReport::default();

    for def in &defs.projects {
        if let Err(e) = check_name("project", &def.name) {
            report
                .errors
                .push(format!("projects/{}.toml: {}", def.name, e.message));
            continue;
        }
        match projects::find(t, &def.name)? {
            None => {
                let created = projects::create(
                    t,
                    &NewProject {
                        name: def.name.clone(),
                        description: def.description.clone(),
                        card_prefix: def.prefix.clone(),
                    },
                    SOURCE_FILE,
                );
                match created {
                    Ok(_) => report.projects.added.push(def.name.clone()),
                    Err(e) => {
                        report
                            .errors
                            .push(format!("projects/{}.toml: {}", def.name, e.message));
                        continue;
                    }
                }
            }
            Some(p) => {
                let description = def.description.clone().unwrap_or_default();
                let prefix_change = def.prefix.as_ref().filter(|x| **x != p.card_prefix);
                if p.source != SOURCE_FILE
                    || p.description != description
                    || prefix_change.is_some()
                {
                    t.execute(
                        "UPDATE projects SET source = 'file', description = ?2, updated_at = ?3
                         WHERE name = ?1",
                        params![def.name, def.description, now()],
                    )?;
                    if prefix_change.is_some() {
                        let patch = projects::ProjectPatch {
                            description: None,
                            card_prefix: def.prefix.clone(),
                            idle_alert: None,
                            github_authors: None,
                        };
                        if let Err(e) = projects::update(t, &def.name, &patch) {
                            report
                                .errors
                                .push(format!("projects/{}.toml: {}", def.name, e.message));
                        }
                    }
                    report.projects.updated.push(def.name.clone());
                }
            }
        }
        // The GitHub author gate (TER-85): the file holds the allow list.
        let live = projects::get(t, &def.name)?.github_authors;
        match crate::store::github_gate::normalise_authors(&def.github_authors) {
            Ok(list) if list != live => {
                projects::set_github_authors(t, &def.name, &list)?;
                if !report.projects.added.contains(&def.name)
                    && !report.projects.updated.contains(&def.name)
                {
                    report.projects.updated.push(def.name.clone());
                }
            }
            Ok(_) => {}
            Err(e) => report
                .errors
                .push(format!("projects/{}.toml: {}", def.name, e.message)),
        }
        // Repos: the file list replaces the file repos, and takes over API
        // repos with the same name.
        t.execute(
            "DELETE FROM repos WHERE project = ?1 AND source = 'file'",
            [&def.name],
        )?;
        for r in &def.repos {
            t.execute(
                "DELETE FROM repos WHERE project = ?1 AND name = ?2",
                params![def.name, r.name],
            )?;
            let new = NewRepo {
                name: Some(r.name.clone()),
                url: r.url.clone(),
                default_branch: r.default_branch.clone(),
                credential: r.credential.clone(),
                path: r.path.clone(),
                hooks_dir: r.hooks_dir.clone(),
                push_deny: r.push_deny.clone(),
                push_via: r.push_via,
            };
            if let Err(e) = projects::attach_repo(t, &def.name, &new, SOURCE_FILE) {
                report.errors.push(format!(
                    "projects/{}.toml repo {}: {}",
                    def.name, r.name, e.message
                ));
            }
        }
    }
    let names: Vec<&str> = defs.projects.iter().map(|p| p.name.as_str()).collect();
    let mut stmt = t.prepare("SELECT name FROM projects WHERE source = 'file' ORDER BY name")?;
    let file_projects = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for name in file_projects
        .iter()
        .filter(|n| !names.contains(&n.as_str()))
    {
        t.execute(
            "UPDATE projects SET source = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, SOURCE_API, now()],
        )?;
        t.execute("UPDATE repos SET source = 'api' WHERE project = ?1", [name])?;
        t.audit(
            "project.detach",
            name,
            Some(name),
            json!({"reason": "file removed"}),
        )?;
        report.projects.removed.push(name.clone());
    }

    let mut kept = Vec::new();
    for def in &defs.agents {
        let label = format!("{}/{}", def.project, def.name);
        let existing = t
            .query_row(
                "SELECT id FROM agents WHERE project = ?1 AND name = ?2",
                params![def.project, def.name],
                |r| r.get::<_, String>(0),
            )
            .ok();
        let result = match existing {
            Some(id) => agents::sync_def(t, &id, def).map(|changed| {
                if changed {
                    report.agents.updated.push(label.clone());
                }
                id
            }),
            None => agents::create(t, def, state_dir, SOURCE_FILE).map(|a| {
                report.agents.added.push(label.clone());
                a.agent.id.to_string()
            }),
        };
        match result {
            Ok(id) => kept.push(id),
            Err(e) => report
                .errors
                .push(format!("agents/{}.toml: {}", def.name, e.message)),
        }
    }
    let mut stmt = t.prepare(
        "SELECT id, project || '/' || name FROM agents WHERE source = 'file' ORDER BY id",
    )?;
    let file_agents = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, label) in file_agents.into_iter().filter(|(id, _)| !kept.contains(id)) {
        agents::delete(t, &id)?;
        report.agents.removed.push(label);
        report.removed_agent_ids.push(id);
    }
    t.audit(
        "definitions.sync",
        "definitions",
        None,
        serde_json::to_value(&report)?,
    )?;
    Ok(report)
}
