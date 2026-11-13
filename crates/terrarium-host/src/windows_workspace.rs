//! Windows account workspaces at `C:\Terrarium\work\<project>\<agent>`
//! (TER-274). The opt-in lives only in the host file. Off, Windows hosts
//! report `no_workspaces` and keep the TER-272 path handling.
//!
//! The lexical split below runs on every OS so it can be tested anywhere.
//! The Windows part opens the workspace through `terrarium_winapi::Workspace`:
//! the service provisions the root and its DACL, then each inner component
//! is opened as the run-as account, with no reparse-point following.

use serde::{Deserialize, Serialize};

use crate::protocol::PathKind;

/// The workspace base. Install creates it with a protected DACL.
pub const BASE: &str = r"C:\Terrarium\work";

/// `[windows_workspaces]` in the host file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Opt-in until native acceptance is done on the host. Only honoured
    /// from a protected host file (`Config::load`).
    #[serde(default)]
    pub enabled: bool,
}

/// A path inside one workspace: the workspace names and the components
/// below its root, in the spelling of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub project: String,
    pub agent: String,
    pub rest: Vec<String>,
}

/// `None`: the path is not under [`BASE`] (the caller keeps its other
/// checks). `Some(Err)`: it is under the base, but not inside one workspace.
/// Lexical only, with the TER-969 Windows path rules.
pub fn locate(path: &str) -> Option<Result<Location, String>> {
    if !PathKind::Windows.inside(BASE, path, true) {
        return None;
    }
    // inside() accepted it: a drive path, ASCII, no empty, `.`, `..` or
    // device components. Skip `C:`, `Terrarium`, `work`.
    let parts: Vec<&str> = path.split(['\\', '/']).skip(3).collect();
    let [project, agent, rest @ ..] = parts.as_slice() else {
        return Some(Err(format!(
            "{path} is not inside one workspace ({BASE}\\<project>\\<agent>)"
        )));
    };
    Some(Ok(Location {
        project: (*project).to_owned(),
        agent: (*agent).to_owned(),
        rest: rest.iter().map(|s| (*s).to_owned()).collect(),
    }))
}

/// [`locate`] for a file: the parent components and the file name.
pub fn split_file(location: &Location) -> Result<(Vec<&str>, &str), String> {
    match location.rest.split_last() {
        Some((name, dirs)) => Ok((dirs.iter().map(String::as_str).collect(), name)),
        None => Err("the workspace root is not a file".into()),
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use terrarium_winapi::{Dir, Identity, Workspace};

    /// Provision the workspace with service authority, then walk `dirs` as
    /// the account. Never call it while impersonating.
    pub fn open_dir(
        location: &Location,
        dirs: &[&str],
        identity: Option<&Identity>,
        mkdirs: bool,
    ) -> std::io::Result<Dir> {
        let workspace = Workspace::open(&location.project, &location.agent, identity, true)?;
        match identity {
            Some(identity) => identity.with_impersonation(|| workspace.into_dir(dirs, mkdirs)),
            None => workspace.into_dir(dirs, mkdirs),
        }
    }

    /// Before a spawn: provision the workspace of a cwd under the base and
    /// check its components. A command that runs as the account keeps its
    /// rights there. Only SYSTEM, Administrators and the account can change
    /// the workspace, so nobody else can swap the cwd before the spawn.
    pub fn check_cwd(
        policy: &Policy,
        identity: Option<&Identity>,
        cwd: &str,
    ) -> Result<(), String> {
        if !policy.enabled {
            return Ok(());
        }
        let location = match locate(cwd) {
            None => return Ok(()),
            Some(location) => location?,
        };
        let dirs: Vec<&str> = location.rest.iter().map(String::as_str).collect();
        open_dir(&location, &dirs, identity, false)
            .map(drop)
            .map_err(|e| format!("cwd {cwd}: {e}"))
    }

    /// Is the base present with a protected DACL on it and its ancestors?
    pub fn base_ready() -> bool {
        Workspace::base_ready()
    }
}
#[cfg(windows)]
pub use native::{base_ready, check_cwd, open_dir};

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(project: &str, agent: &str, rest: &[&str]) -> Location {
        Location {
            project: project.into(),
            agent: agent.into(),
            rest: rest.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn locates_paths_inside_one_workspace() {
        assert_eq!(
            locate(r"C:\Terrarium\work\proj\agent").unwrap().unwrap(),
            loc("proj", "agent", &[])
        );
        assert_eq!(
            locate(r"c:/terrarium/WORK/proj/agent/src/My File.txt")
                .unwrap()
                .unwrap(),
            loc("proj", "agent", &["src", "My File.txt"])
        );
    }

    #[test]
    fn base_and_project_level_paths_are_refused() {
        for path in [r"C:\Terrarium\work", r"C:\Terrarium\work\proj"] {
            assert!(locate(path).unwrap().is_err(), "{path}");
        }
    }

    #[test]
    fn other_paths_and_lexical_refusals_are_not_workspace_paths() {
        // Not under the base: the caller keeps its own checks (TER-969).
        for path in [
            r"C:\Users\a",
            r"D:\Terrarium\work\p\a",
            r"C:\Terrarium\workx\p\a",
            r"C:\Terrarium\work\p\a\..\b",
            r"C:\Terrarium\work\p\a\NUL",
            r"C:\Terrarium\work\p\a\x:stream",
            r"\\?\C:\Terrarium\work\p\a",
            "/work/p/a",
        ] {
            assert!(locate(path).is_none(), "{path}");
        }
    }

    #[test]
    fn files_need_a_name_below_the_root() {
        let l = loc("p", "a", &["d", "f.bin"]);
        assert_eq!(split_file(&l).unwrap(), (vec!["d"], "f.bin"));
        assert!(split_file(&loc("p", "a", &[])).is_err());
    }

    #[test]
    fn workspaces_are_off_by_default_and_strict() {
        let cfg = crate::config::Config::parse("", std::path::Path::new("fixture")).unwrap();
        assert!(!cfg.windows_workspaces.enabled);
        let on = crate::config::Config::parse(
            "[windows_workspaces]\nenabled = true",
            std::path::Path::new("fixture"),
        )
        .unwrap();
        assert!(on.windows_workspaces.enabled);
        assert!(toml::from_str::<Policy>("enabled = true\nbase = 'D:\\\\w'").is_err());
    }
}
