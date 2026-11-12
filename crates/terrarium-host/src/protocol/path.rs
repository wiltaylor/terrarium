//! Lexical host paths. Never interpret a remote path with the daemon OS rules.
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    #[default]
    Posix,
    Windows,
}

impl PathKind {
    /// Validate an absolute path and return a comparison key. No normalization
    /// of parent components is allowed, even when it would stay inside a root.
    pub fn key(self, path: &str) -> Option<String> {
        if path.chars().any(char::is_control) {
            return None;
        }
        match self {
            Self::Posix => {
                if !path.starts_with('/') || path.split('/').any(|p| p == "..") {
                    return None;
                }
                Some(path.to_owned())
            }
            Self::Windows => {
                // ASCII-only avoids differing Unicode case folding between
                // Windows filesystems and the daemon. Device namespaces fail closed.
                if !path.is_ascii() {
                    return None;
                }
                let path = path.replace('\\', "/").to_ascii_lowercase();
                let rest = if let Some(rest) = path.strip_prefix("//") {
                    if rest.split('/').count() < 2 {
                        return None;
                    }
                    rest
                } else if path.len() >= 3
                    && path.as_bytes()[0].is_ascii_alphabetic()
                    && &path[1..3] == ":/"
                {
                    &path[3..]
                } else {
                    return None;
                };
                if !rest.is_empty() && rest.split('/').any(|p| !windows_component(p)) {
                    return None;
                }
                Some(path)
            }
        }
    }

    pub fn inside(self, root: &str, path: &str, allow_root: bool) -> bool {
        let (Some(root), Some(path)) = (self.key(root), self.key(path)) else {
            return false;
        };
        let strict = match self {
            Self::Posix => path
                .split('/')
                .skip(1)
                .all(|p| !matches!(p, "" | "." | "..")),
            Self::Windows => true,
        };
        strict
            && ((allow_root && path == root)
                || path.starts_with(&format!("{}/", root.trim_end_matches('/'))))
    }

    pub fn workspace(self, project: &str, agent: &str) -> Option<String> {
        if [project, agent]
            .iter()
            .any(|s| s.is_empty() || s.contains(['/', '\\']))
        {
            return None;
        }
        let path = match self {
            Self::Posix => format!("/work/{project}/{agent}"),
            Self::Windows => format!(r"C:\Terrarium\work\{project}\{agent}"),
        };
        self.key(&path)?;
        Some(path)
    }
}

fn windows_component(part: &str) -> bool {
    if part.is_empty() || part.ends_with(['.', ' ']) || part.chars().any(|c| "<>:\"|?*".contains(c))
    {
        return false;
    }
    let stem = part
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    !matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
        && !(stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

#[cfg(test)]
mod tests {
    use super::PathKind::*;

    #[test]
    fn windows_path_table() {
        for path in [
            r"C:\",
            r"C:\Terrarium\work\p\a",
            "d:/src/file.txt",
            r"\\server\share",
            r"\\server\share\dir",
        ] {
            assert!(Windows.key(path).is_some(), "{path}");
        }
        for path in [
            "C:relative",
            "/root",
            r"\root",
            r"C:\a\..\b",
            r"C:\a\.\b",
            r"C:\a\\b",
            r"C:\CON.txt",
            r"C:\nul",
            r"C:\LPT9.log",
            r"C:\bad.",
            r"C:\bad ",
            r"C:\x:stream",
            r"\\?\C:\x",
            r"\\.\pipe\x",
            r"\\server",
            "C:/x\n",
            "C:/é",
        ] {
            assert!(Windows.key(path).is_none(), "{path}");
        }
    }

    #[test]
    fn windows_root_boundaries() {
        let root = Windows.workspace("project", "agent").unwrap();
        assert!(Windows.inside(&root, "c:/TERRARIUM/work/PROJECT/AGENT/src", false));
        assert!(Windows.inside(&root, &root, true));
        assert!(!Windows.inside(&root, &root, false));
        for path in [
            "C:/Terrarium/work/project/agent2/src",
            "D:/Terrarium/work/project/agent/src",
            "C:/Terrarium/work/project/agent/../other",
            "C:/Terrarium/work/project/agent/NUL",
        ] {
            assert!(!Windows.inside(&root, path, true));
        }
        assert!(!Posix.inside("/work/p/a", "/work/p/a/../b", true));
        assert!(!Posix.inside("/work/p/a", "/work/p/a//b", true));
    }
}
