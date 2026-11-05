//! The tools of an agent in its sandbox (GitHub #21, design D3, TER-34):
//! one dir for each agent, `<runtime>/agent-tools/<ulid>/`, with a symlink
//! `<tool> -> terrarium` for each tool granted to the agent or its project.
//! The sandbox binds the dir read-only at [`SANDBOX_DIR`], last on `PATH`,
//! so a tool never shadows a program (and `store::tools::shadows` refuses
//! the names of the sandbox `PATH`). The CLI sees its `argv[0]` and runs
//! `terrarium tool run <name> -- <args>`.
//!
//! The dir is bound, not each link: a grant or a revoke rewrites the links
//! ([`refresh`]) and a running agent sees the change at once.
//!
//! | Path | Mode | Holds |
//! |---|---|---|
//! | `<runtime>/agent-tools/` | 0755 | one dir for each agent |
//! | `<runtime>/agent-tools/<ulid>/` | 0755 | the links, bound read-only |

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use tracing::warn;

use crate::api::Shared;

/// Where the sandbox shows the agent's dir. Not `/opt/terrarium/tools`:
/// that is the harness tools dir (`TERRARIUM_TOOLS_DIR`).
pub const SANDBOX_DIR: &str = "/opt/terrarium/agent-tools/bin";

/// Where the dirs are, and what each link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `<runtime>/agent-tools`.
    pub root: PathBuf,
    /// The `terrarium` CLI as the agent sees it: `/opt/terrarium/bin/terrarium`
    /// in the sandbox, the host path with no sandbox.
    pub link: PathBuf,
}

/// The state of the agent tools (`AppState::agent_tools`). Unconfigured
/// (the in-memory supervisor of the tests, unless a test sets it): no dir
/// is written.
#[derive(Debug, Default)]
pub struct Runtime {
    cfg: RwLock<Option<Config>>,
}

impl Runtime {
    pub fn configure(&self, cfg: Config) {
        *self
            .cfg
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cfg);
    }

    pub fn config(&self) -> Option<Config> {
        self.cfg
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// `<root>/<agent>`: the dir of one agent.
pub fn dir(root: &Path, agent: &str) -> PathBuf {
    root.join(agent)
}

/// Make `dir` hold exactly one link `<name> -> link` for each of `names`.
/// A name that is not a valid tool name is skipped. Only links are
/// removed: anything else in the dir stays (nothing else should be there).
pub fn write(dir: &Path, names: &[String], link: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    let want: BTreeSet<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| terrarium_core::tool::check_name(n).is_ok())
        .collect();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name();
        let keep = name.to_str().is_some_and(|n| want.contains(n))
            && std::fs::read_link(e.path()).is_ok_and(|t| t == link);
        if !keep && e.file_type()?.is_symlink() {
            std::fs::remove_file(e.path())?;
        }
    }
    for n in want {
        let p = dir.join(n);
        if std::fs::symlink_metadata(&p).is_err() {
            std::os::unix::fs::symlink(link, &p)?;
        }
    }
    Ok(())
}

/// Write the dir of agent `id` of `project` from its grants now. Returns
/// the dir, or `None` when the agent tools are not configured.
pub async fn refresh_agent(state: &Shared, id: &str, project: &str) -> Option<PathBuf> {
    let cfg = state.agent_tools.config()?;
    let (key, proj) = (id.to_owned(), project.to_owned());
    let names = match state
        .db
        .read(move |c| crate::store::tools::granted_to(c, &key, &proj))
        .await
    {
        Ok(rows) => rows.into_iter().map(|r| r.def.name).collect::<Vec<_>>(),
        Err(e) => {
            warn!(agent = id, "cannot read the tool grants: {e}");
            Vec::new()
        }
    };
    let d = dir(&cfg.root, id);
    if let Err(e) = write(&d, &names, &cfg.link) {
        warn!(
            agent = id,
            "cannot write the agent tools dir {}: {e}",
            d.display()
        );
    }
    Some(d)
}

/// Write the dirs of every agent: after a grant, a revoke, a change or a
/// removal of a tool, and a reload of the tool files. A dir of an agent
/// that is gone is removed.
pub async fn refresh(state: &Shared) {
    let Some(cfg) = state.agent_tools.config() else {
        return;
    };
    let agents = match state
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT id, project FROM agents")?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    {
        Ok(a) => a,
        Err(e) => {
            warn!("cannot list the agents for the agent tools: {e}");
            return;
        }
    };
    for (id, project) in &agents {
        refresh_agent(state, id, project).await;
    }
    // The dirs of removed agents.
    if let Ok(entries) = std::fs::read_dir(&cfg.root) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !agents.iter().any(|(id, _)| *id == name)
                && e.file_type().is_ok_and(|t| t.is_dir())
                && let Err(err) = std::fs::remove_dir_all(e.path())
            {
                warn!(
                    "cannot remove the agent tools dir {}: {err}",
                    e.path().display()
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// The dir holds one link for each granted tool, to the CLI; a revoke
    /// removes the link; a bad name never becomes a file; other files stay.
    #[test]
    fn the_dir_holds_one_link_for_each_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir(tmp.path(), "01K8AGENT0000000000000000A");
        let link = Path::new("/opt/terrarium/bin/terrarium");
        let list = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        write(
            &d,
            &list(&["tracker-read", "deploy", "../evil", "Bad"]),
            link,
        )
        .unwrap();
        assert_eq!(names(&d), ["deploy", "tracker-read"]);
        assert_eq!(std::fs::read_link(d.join("deploy")).unwrap(), link);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // A revoke, and a link to something else is put right.
        std::fs::remove_file(d.join("deploy")).unwrap();
        std::os::unix::fs::symlink("/bin/sh", d.join("deploy")).unwrap();
        std::fs::write(d.join("notes"), "x").unwrap();
        write(&d, &list(&["deploy"]), link).unwrap();
        assert_eq!(names(&d), ["deploy", "notes"]);
        assert_eq!(std::fs::read_link(d.join("deploy")).unwrap(), link);
        // Again: no change.
        write(&d, &list(&["deploy"]), link).unwrap();
        assert_eq!(names(&d), ["deploy", "notes"]);
        write(&d, &[], link).unwrap();
        assert_eq!(names(&d), ["notes"]);
    }
}
