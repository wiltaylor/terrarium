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

/// How a host path relates to the workspace base, when workspaces are on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// Inside one workspace: use the confined walk.
    Workspace(Location),
    /// Refused: under the base but not in one workspace, or a spelling that
    /// can name the base without matching it lexically (F1).
    Refused(String),
    /// Not the base by any spelling this check knows: TER-272 handling.
    Other,
}

/// Classify a path lexically. A path that could be an alias of the base is
/// refused, never sent to the TER-272 handling, which follows links:
/// - a verbatim or device prefix (`\\?\`, `\\.\`, `//?/`, `//./`);
/// - a component with `~` on a drive path (an 8.3 short name);
/// - a UNC path to an administrative drive share (`\\host\C$`).
///
/// [`canonical_alias`] covers links and other aliases on Windows.
pub fn classify(path: &str) -> Class {
    if let Some(found) = locate(path) {
        return match found {
            Ok(location) => Class::Workspace(location),
            Err(m) => Class::Refused(m),
        };
    }
    // F12: a path that the TER-969 rules reject (a trailing dot or space, a
    // `:` stream, a device name, an empty or `.`/`..` component, non-ASCII)
    // can name a path under the base without matching it lexically. Win32
    // strips or reinterprets those parts, so refuse it and never fall back.
    if PathKind::Windows.key(path).is_none() {
        return Class::Refused(format!(
            "{path}: invalid Windows path component (trailing dot or space, \
             stream, device name or non-ASCII) while workspaces are on"
        ));
    }
    let slashed = path.replace('\\', "/");
    if slashed.starts_with("//?/") || slashed.starts_with("//./") {
        return Class::Refused(format!("{path}: verbatim and device paths are refused"));
    }
    if let Some(unc) = slashed.strip_prefix("//") {
        let share = unc.split('/').nth(1).unwrap_or_default();
        let bytes = share.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'$' {
            return Class::Refused(format!("{path}: administrative drive shares are refused"));
        }
        return Class::Other;
    }
    let bytes = slashed.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && slashed.split('/').any(|part| part.contains('~'))
    {
        return Class::Refused(format!(
            "{path}: short (8.3) names are refused while workspaces are on"
        ));
    }
    Class::Other
}

/// [`locate`] for a file: the parent components and the file name.
pub fn split_file(location: &Location) -> Result<(Vec<&str>, &str), String> {
    match location.rest.split_last() {
        Some((name, dirs)) => Ok((dirs.iter().map(String::as_str).collect(), name)),
        None => Err("the workspace root is not a file".into()),
    }
}

/// The longest existing prefix of `path`, canonicalized, with the rest
/// appended, as a lexical Windows path without the `\\?\` prefix.
fn resolved(
    path: &str,
    canonicalize: impl Fn(&std::path::Path) -> Option<String>,
) -> Option<String> {
    // Split the text: on Linux test hosts a `PathBuf` does not split on `\\`.
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    for keep in (1..=parts.len()).rev() {
        let mut prefix = parts[..keep].join("\\");
        if keep == 1 {
            prefix.push('\\');
        }
        if let Some(real) = canonicalize(std::path::Path::new(&prefix)) {
            let real = real.strip_prefix(r"\\?\").unwrap_or(&real);
            let mut out = real.trim_end_matches('\\').to_owned();
            for part in &parts[keep..] {
                out.push('\\');
                out.push_str(part);
            }
            return Some(out);
        }
    }
    None
}

/// F1, after [`classify`] said `Other`: does the path reach the base
/// through a link, a junction or another alias? Then it is refused.
pub fn canonical_alias(
    path: &str,
    canonicalize: impl Fn(&std::path::Path) -> Option<String>,
) -> Option<String> {
    let real = resolved(path, canonicalize)?;
    PathKind::Windows
        .inside(BASE, &real, true)
        .then(|| format!("{path} leads into {BASE}: use the workspace path itself"))
}

/// The decision for a path while workspaces are on: the workspace, a
/// refusal, or `None` for the TER-272 handling. `canonicalize` resolves an
/// existing path (as the account on the host).
pub fn decide_with(
    path: &str,
    canonicalize: impl Fn(&std::path::Path) -> Option<String>,
) -> Result<Option<Location>, String> {
    match classify(path) {
        Class::Workspace(location) => Ok(Some(location)),
        Class::Refused(m) => Err(m),
        Class::Other => match canonical_alias(path, canonicalize) {
            Some(m) => Err(m),
            None => Ok(None),
        },
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use terrarium_winapi::{Dir, Identity, Workspace};

    fn canonical(path: &std::path::Path) -> Option<String> {
        std::fs::canonicalize(path)
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
    }

    /// The decision for a path while workspaces are on: the workspace, a
    /// refusal, or `None` for the TER-272 handling. Run it as the account,
    /// so that the canonical check sees what the account would open.
    pub fn decide(path: &str) -> Result<Option<Location>, String> {
        decide_with(path, canonical)
    }

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
        let decided = match identity {
            Some(identity) => identity
                .with_impersonation(|| Ok(decide(cwd)))
                .map_err(|_| "Windows path check impersonation failed".to_owned())?,
            None => decide(cwd),
        };
        let Some(location) = decided? else {
            return Ok(());
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
pub use native::{base_ready, check_cwd, decide, open_dir};

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

    /// F1: every alias spelling of the base that this check knows is refused,
    /// never handed to the TER-272 handling.
    #[test]
    fn alias_spellings_of_the_base_are_refused() {
        for path in [
            r"C:\TERRAR~1\work\p\a\f",
            r"C:\Terrarium\WORK~1\p\a",
            r"c:\terrar~1\work\p\a",
            r"C:\Users\RUNNER~1\x",
            r"\\?\C:\Terrarium\work\p\a\f",
            r"\\?\UNC\localhost\C$\Terrarium\work\p\a",
            r"\\.\C:\Terrarium\work\p\a",
            "//?/C:/Terrarium/work/p/a",
            r"\\localhost\C$\Terrarium\work\p\a",
            r"\\127.0.0.1\c$\Terrarium\work\p\a",
            r"C:\Terrarium\work",
            r"C:\Terrarium\work\p",
        ] {
            assert!(matches!(classify(path), Class::Refused(_)), "{path}");
        }
        assert!(matches!(
            classify(r"C:\Terrarium\work\p\a\f"),
            Class::Workspace(_)
        ));
        for path in [r"C:\Users\a\x", r"D:\data", r"\\server\share\dir"] {
            assert_eq!(classify(path), Class::Other, "{path}");
        }
    }

    /// F1: a link or junction that resolves into the base is refused, also
    /// when the path below it does not exist yet.
    #[test]
    fn links_into_the_base_are_refused() {
        let fake = |p: &std::path::Path| match p.to_str()? {
            r"C:\link" => Some(r"\\?\C:\Terrarium\work\p\a".to_owned()),
            r"C:\elsewhere" => Some(r"\\?\C:\elsewhere".to_owned()),
            r"C:\" => Some(r"\\?\C:\".to_owned()),
            _ => None,
        };
        assert!(canonical_alias(r"C:\link\new\file.txt", fake).is_some());
        assert!(canonical_alias(r"C:\link", fake).is_some());
        assert!(canonical_alias(r"C:\elsewhere\x", fake).is_none());
        assert!(canonical_alias(r"C:\missing\x", fake).is_none());
    }

    /// F12: trailing dots and spaces, `::$DATA` and `:name` streams, in an
    /// existing or a new component, anywhere on the path: always refused,
    /// never `Ok(None)` (the TER-272 fallback).
    #[test]
    fn invalid_components_never_fall_back() {
        // `C:\Terrarium\work\p\a` and its file `f` exist; `new` does not.
        let fake = |p: &std::path::Path| match p.to_str()? {
            r"C:\" | r"C:\Terrarium" | r"C:\Terrarium\work" | r"C:\Terrarium\work\p" => {
                Some(format!(r"\\?\{}", p.to_str()?))
            }
            r"C:\Terrarium\work\p\a" | r"C:\Terrarium\work\p\a\f" => {
                Some(format!(r"\\?\{}", p.to_str()?))
            }
            _ => None,
        };
        for tail in [
            r"f.",
            r"f ",
            r"f::$DATA",
            r"f:name",
            r"new.",
            r"new ",
            r"new::$DATA",
            r"new:s",
        ] {
            for path in [
                format!(r"C:\Terrarium\work\p\a\{tail}"),
                format!(r"C:\Terrarium\work\p\a\{tail}\below.txt"),
                format!(r"C:\Terrarium\work\p\a\new\{tail}"),
            ] {
                assert!(
                    matches!(classify(&path), Class::Refused(_)),
                    "classify {path}"
                );
                assert!(decide_with(&path, fake).is_err(), "decide {path}");
            }
        }
        for path in [
            r"C:\Terrarium.\work\p\a\f",
            r"C:\Terrarium\work.\p\a\f",
            r"C:\Terrarium\work \p\a",
            r"C:\Terrarium \work\p\a",
            r"C:\Terrarium\work\p.\a\f",
            r"C:\Terrarium\work\p\a \f",
            r"C:\Terrarium\work\p\a\AUX.txt",
            r"C:\Terrarium\work\p\a\x\..\f",
            r"C:\Terrarium\work\p\a\\f",
        ] {
            assert!(decide_with(path, fake).is_err(), "{path}");
        }
        // Valid workspace paths, existing or new, take the walk.
        for path in [
            r"C:\Terrarium\work\p\a\f",
            r"C:\Terrarium\work\p\a\new\g.txt",
        ] {
            assert!(matches!(decide_with(path, fake), Ok(Some(_))), "{path}");
        }
        // Other valid paths still use the TER-272 handling.
        assert_eq!(decide_with(r"D:\data\x", fake), Ok(None));
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
