//! The system prompt library (PLAN §8.1, docs/api.md §28):
//! `<config repo>/system-prompts/<name>.md`.
//!
//! An agent or a template names a library entry with `persona = "<name>"`.
//! The packaged personas `leader` and `worker` are library entries too:
//! the defaults put them in `system-prompts/` (`crate::defaults` moves the
//! copies of an older start out of `prompts/`).
//!
//! The lookup of a name ([`resolve`]): `system-prompts/<name>.md`, then
//! `<prompts dir>/<name>.md`. The second step keeps an older config repo
//! working: a persona file in `prompts/` still starts the agent. The list
//! ([`list`]) shows the files of `system-prompts/`, and `leader` and
//! `worker` from `prompts/` when `system-prompts/` does not have them. It
//! never shows the protocol templates of `prompts/` (`default.md`,
//! `project-lead.md`, ...): they are not system prompts.
//!
//! A first line `<!-- description: <text> -->` gives the short
//! description of the selector. The daemon removes HTML comments from a
//! persona before the start (`crate::prompt::strip_comments`), so the line
//! never reaches the model.
//!
//! The files are the source of truth, with no index in the database: each
//! read goes to the disk, so an edit by hand shows at the next read and
//! at the next start of each agent. A write through the API goes to the
//! work tree of the config repo after the secret scan, and stays
//! uncommitted, as an edit of a template file does: the Config page (or
//! `terrarium admin config commit`) commits it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::config::{PERSONA_LEADER, PERSONA_NONE, PERSONA_WORKER, SYSTEM_PROMPT_MAX};

use crate::error::{ApiError, ApiResult};

/// The library dir in the config repo.
pub const DIR: &str = "system-prompts";
/// The extension of a library file.
pub const EXTENSION: &str = "md";
/// The library entries that every daemon has: the personas by role.
pub const BUILTIN: [&str; 2] = [PERSONA_LEADER, PERSONA_WORKER];
/// `source` of an entry that is an unedited packaged default.
pub const SOURCE_DEFAULT: &str = "default";
/// `source` of every other entry.
pub const SOURCE_CONFIG: &str = "config";
/// The start of the description line.
const DESCRIPTION_START: &str = "<!-- description:";

/// The library dir of the config repo at `repo`.
pub fn library_dir(repo: &Path) -> PathBuf {
    repo.join(DIR)
}

/// The dirs of a lookup: the library, and the prompts dir for an older
/// config repo.
#[derive(Debug, Clone)]
pub struct Dirs {
    /// `<config repo>/system-prompts`. `None`: no config repo; only the
    /// prompts dir counts.
    pub library: Option<PathBuf>,
    /// The prompts dir (`TERRARIUM_PROMPTS_DIR`, else `<config
    /// repo>/prompts`).
    pub prompts: PathBuf,
}

impl Dirs {
    /// The dirs of the config repo at `repo`, with its own `prompts/`.
    pub fn of_repo(repo: &Path) -> Self {
        Self {
            library: Some(library_dir(repo)),
            prompts: repo.join("prompts"),
        }
    }
}

/// Is `name` a name for a new library entry? The rules of a template
/// name: 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or a digit.
/// `none` is not a name: it means "the harness's default prompt".
pub fn check_name(name: &str) -> ApiResult<()> {
    if name == PERSONA_NONE {
        return Err(ApiError::invalid(
            "`none` is not a system prompt name: it means the harness's default prompt",
        ));
    }
    if crate::store::valid_name(name) {
        Ok(())
    } else {
        Err(ApiError::invalid(format!(
            "the system prompt name `{name}` is not valid: use 1-63 of a-z, 0-9, `-`, `_`, \
             starting with a letter or a digit"
        )))
    }
}

/// The file of `name`, and the dir it is in: `system-prompts/<name>.md`,
/// else `<prompts dir>/<name>.md`. `None` when neither is a file. The
/// caller checks the name (`terrarium_core::config::check_persona`).
pub fn resolve(dirs: &Dirs, name: &str) -> Option<PathBuf> {
    let file = format!("{name}.{EXTENSION}");
    dirs.library
        .iter()
        .map(|d| d.join(&file))
        .chain(std::iter::once(dirs.prompts.join(&file)))
        .find(|p| p.is_file())
}

/// The description of a library file: the text of a first line
/// `<!-- description: <text> -->`, else empty.
pub fn description(text: &str) -> String {
    split_description(text).0.unwrap_or_default()
}

/// The description line of `text` and the text after it.
fn split_description(text: &str) -> (Option<String>, &str) {
    let t = text.trim_start();
    let Some(rest) = t.strip_prefix(DESCRIPTION_START) else {
        return (None, text);
    };
    let Some(end) = rest.find("-->") else {
        return (None, text);
    };
    let d = rest[..end].split_whitespace().collect::<Vec<_>>().join(" ");
    (Some(d), rest[end + 3..].trim_start_matches(['\r', '\n']))
}

/// The prompt text of a library file, without its description line.
pub fn body(text: &str) -> &str {
    split_description(text).1
}

/// The file text of a body and a description: the description line first
/// when the description is not empty.
pub fn compose(description: &str, body: &str) -> String {
    let body = split_description(body).1;
    let d = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = if d.is_empty() {
        String::new()
    } else {
        format!("{DESCRIPTION_START} {d} -->\n\n")
    };
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

/// One entry of `GET /api/system-prompts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub name: String,
    /// From the description line. Empty when the file has none.
    pub description: String,
    /// The bytes of the file.
    pub size: u64,
    /// `default`: an unedited packaged default. `config`: any other file.
    pub source: String,
    /// The file, relative to the config repo: `system-prompts/<name>.md`,
    /// or `prompts/<name>.md` in an older config repo.
    pub path: String,
    /// `leader` or `worker`: the persona by role. It cannot be deleted.
    pub builtin: bool,
    /// The time of the last change of the file (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

/// One user of a library entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// `agent` or `template`.
    pub kind: String,
    pub id: String,
    /// `project/name` for an agent; the name, or `project/name` for a
    /// project template.
    pub name: String,
    /// The project. `None`: a global template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The agent has no `persona` and gets the entry by its roles.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_role: bool,
}

/// `GET /api/system-prompts/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detail {
    #[serde(flatten)]
    pub summary: Summary,
    /// The prompt text, without the description line.
    pub text: String,
    /// The agents and templates that name the entry.
    pub users: Vec<User>,
}

/// The rel path of `file` in the config repo, for the answer.
fn rel_of(file: &Path, dirs: &Dirs) -> String {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let in_library = dirs.library.as_deref().is_some_and(|l| file.starts_with(l));
    if in_library {
        format!("{DIR}/{name}")
    } else {
        format!("prompts/{name}")
    }
}

fn summary_of(dirs: &Dirs, repo: Option<&Path>, name: &str, file: &Path) -> Option<Summary> {
    let text = std::fs::read_to_string(file).ok()?;
    let meta = std::fs::metadata(file).ok();
    let rel = rel_of(file, dirs);
    let source = match repo {
        Some(r) if crate::defaults::is_unedited_default(r, &rel, text.as_bytes()) => SOURCE_DEFAULT,
        _ => SOURCE_CONFIG,
    };
    Some(Summary {
        name: name.to_owned(),
        description: description(&text),
        size: text.len() as u64,
        source: source.to_owned(),
        path: rel,
        builtin: BUILTIN.contains(&name),
        updated: meta.and_then(|m| m.modified().ok()).map(|t| {
            chrono::DateTime::<chrono::Utc>::from(t)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }),
    })
}

/// The names of the `.md` files of `dir` that are valid names, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(&format!(".{EXTENSION}")).map(str::to_owned)
        })
        .filter(|n| check_name(n).is_ok())
        .collect();
    out.sort();
    out
}

/// Every library entry, sorted by name: the files of `system-prompts/`,
/// and `leader` and `worker` from the prompts dir when the library does
/// not have them. `repo` is the config repo, for `source`.
pub fn list(dirs: &Dirs, repo: Option<&Path>) -> Vec<Summary> {
    let mut names = dirs.library.as_deref().map(names_in).unwrap_or_default();
    for b in BUILTIN {
        if !names.iter().any(|n| n == b) && dirs.prompts.join(format!("{b}.{EXTENSION}")).is_file()
        {
            names.push(b.to_owned());
        }
    }
    names.sort();
    names
        .iter()
        .filter_map(|n| summary_of(dirs, repo, n, &resolve(dirs, n)?))
        .collect()
}

fn not_found(name: &str) -> ApiError {
    ApiError::not_found(format!("no system prompt `{name}` in the library ({DIR}/)"))
}

/// One entry with its text. `users` is empty: the caller adds them.
pub fn get(dirs: &Dirs, repo: Option<&Path>, name: &str) -> ApiResult<Detail> {
    terrarium_core::config::check_persona(name).map_err(ApiError::invalid)?;
    let file = resolve(dirs, name).ok_or_else(|| not_found(name))?;
    // Only an entry of the list: not a protocol template of `prompts/`.
    let listed =
        dirs.library.as_deref().is_some_and(|l| file.starts_with(l)) || BUILTIN.contains(&name);
    if !listed {
        return Err(not_found(name));
    }
    let summary = summary_of(dirs, repo, name, &file).ok_or_else(|| not_found(name))?;
    let text = std::fs::read_to_string(&file)
        .map_err(|e| ApiError::internal(format!("{}: {e}", file.display())))?;
    Ok(Detail {
        summary,
        text: body(&text).to_owned(),
        users: Vec::new(),
    })
}

/// The refusal of a library file by the secret scan: `422 invalid` with
/// the findings in `details.findings`. The answer never has the value.
fn scan_refusal(findings: &[terrarium_core::secret_scan::Finding]) -> ApiError {
    let list: Vec<String> = findings.iter().map(ToString::to_string).collect();
    ApiError::invalid(format!(
        "the secret scan refused the system prompt ({} finding(s)): {}. A system prompt holds \
         no secret: give the agent a secret grant (PLAN §16b), or for a false hit add `{}` to \
         the line",
        findings.len(),
        list.join("; "),
        terrarium_core::secret_scan::ALLOW_MARKER
    ))
    .with_details(json!({ "findings": findings }))
}

/// The checked file of a write: the text with its description line, at
/// most [`SYSTEM_PROMPT_MAX`] bytes, not empty, and clean of secrets.
/// Returns the rel path and the bytes.
pub fn checked_file(name: &str, description: &str, text: &str) -> ApiResult<(String, Vec<u8>)> {
    check_name(name)?;
    if description.contains("-->") {
        return Err(ApiError::invalid("the description cannot hold `-->`"));
    }
    if body(text).trim().is_empty() {
        return Err(ApiError::invalid("a system prompt needs `text`"));
    }
    let file = compose(description, text);
    if file.len() > SYSTEM_PROMPT_MAX {
        return Err(ApiError::invalid(format!(
            "the system prompt has {} bytes; the limit is {SYSTEM_PROMPT_MAX}",
            file.len()
        )));
    }
    let rel = format!("{DIR}/{name}.{EXTENSION}");
    let findings = terrarium_core::secret_scan::scan(&rel, file.as_bytes());
    if !findings.is_empty() {
        return Err(scan_refusal(&findings));
    }
    Ok((rel, file.into_bytes()))
}

/// The users of `name` in the store: the agents whose persona is `name`
/// (and for `leader` and `worker`, the agents with no persona and that
/// role), and the templates whose spec names it.
pub fn users(conn: &rusqlite::Connection, name: &str) -> ApiResult<Vec<User>> {
    let mut out = Vec::new();
    let mut st = conn
        .prepare("SELECT id, project, name, persona, roles FROM agents ORDER BY project, name")?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (id, project, agent, persona, roles) = row?;
        let persona = persona
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty());
        let (effective, by_role) = match persona {
            Some(p) => (p, false),
            None => {
                let roles: Vec<String> = serde_json::from_str(&roles).unwrap_or_default();
                let lead = roles
                    .iter()
                    .any(|r| r == "project_admin" || r == "sysadmin");
                let p = if lead { PERSONA_LEADER } else { PERSONA_WORKER };
                (p.to_owned(), true)
            }
        };
        if effective == name {
            out.push(User {
                kind: "agent".into(),
                id,
                name: format!("{project}/{agent}"),
                project: Some(project),
                by_role,
            });
        }
    }
    let mut st =
        conn.prepare("SELECT id, project, name, spec FROM agent_templates ORDER BY scope, name")?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (id, project, tpl, spec) = row?;
        let spec: serde_json::Value = serde_json::from_str(&spec).unwrap_or_default();
        if spec.get("persona").and_then(|v| v.as_str()).map(str::trim) == Some(name) {
            out.push(User {
                kind: "template".into(),
                id,
                name: match &project {
                    Some(p) => format!("{p}/{tpl}"),
                    None => tpl,
                },
                project,
                by_role: false,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> Dirs {
        Dirs {
            library: Some(root.join(DIR)),
            prompts: root.join("prompts"),
        }
    }

    fn put(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// The library first, then `prompts/` for an older config repo. The
    /// list has the library files and the built-in personas, never the
    /// protocol templates.
    #[test]
    fn lookup_order_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let d = dirs(r);
        put(r, "prompts/leader.md", "old leader");
        put(r, "prompts/worker.md", "old worker");
        put(r, "prompts/default.md", "protocol");
        put(r, "prompts/reviewer.md", "an old named persona");
        put(r, "system-prompts/worker.md", "new worker");
        put(
            r,
            "system-prompts/terse.md",
            "<!-- description: Short answers -->\n\nBe terse.\n",
        );
        put(r, "system-prompts/Bad Name.md", "x");
        put(r, "system-prompts/notes.txt", "x");

        assert_eq!(
            resolve(&d, "worker").unwrap(),
            r.join("system-prompts/worker.md")
        );
        assert_eq!(resolve(&d, "leader").unwrap(), r.join("prompts/leader.md"));
        assert_eq!(
            resolve(&d, "reviewer").unwrap(),
            r.join("prompts/reviewer.md")
        );
        assert!(resolve(&d, "missing").is_none());
        // With no library: the prompts dir only.
        let old = Dirs {
            library: None,
            prompts: r.join("prompts"),
        };
        assert_eq!(
            resolve(&old, "worker").unwrap(),
            r.join("prompts/worker.md")
        );

        let l = list(&d, None);
        let names: Vec<&str> = l.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["leader", "terse", "worker"]);
        let terse = l.iter().find(|s| s.name == "terse").unwrap();
        assert_eq!(terse.description, "Short answers");
        assert_eq!(terse.path, "system-prompts/terse.md");
        assert!(!terse.builtin);
        let leader = l.iter().find(|s| s.name == "leader").unwrap();
        assert_eq!(leader.path, "prompts/leader.md");
        assert!(leader.builtin);

        let g = get(&d, None, "terse").unwrap();
        assert_eq!(g.text, "Be terse.\n");
        assert_eq!(get(&d, None, "leader").unwrap().text, "old leader");
        // A protocol template or an old named persona is not an entry.
        assert_eq!(
            get(&d, None, "default").unwrap_err().code,
            crate::error::Code::NotFound
        );
        assert_eq!(
            get(&d, None, "reviewer").unwrap_err().code,
            crate::error::Code::NotFound
        );
    }

    #[test]
    fn description_line_round_trip() {
        let f = compose("  Two\n words ", "Body text.\n\n");
        assert_eq!(f, "<!-- description: Two words -->\n\nBody text.\n");
        assert_eq!(description(&f), "Two words");
        assert_eq!(body(&f), "Body text.\n");
        // A new description replaces the old line; empty drops it.
        assert_eq!(
            compose("New", &f),
            "<!-- description: New -->\n\nBody text.\n"
        );
        assert_eq!(compose("", &f), "Body text.\n");
        assert_eq!(description("No line\n"), "");
        // Stripped from the persona at the start.
        assert_eq!(crate::prompt::strip_comments(&f).trim(), "Body text.");
    }

    #[test]
    fn checks_of_a_write() {
        assert!(checked_file("ok-name", "", "text").is_ok());
        for bad in ["none", "Upper", "-x", "a/b", ""] {
            assert!(checked_file(bad, "", "text").is_err(), "{bad}");
        }
        assert!(checked_file("x", "", "  \n").is_err());
        assert!(checked_file("x", "a --> b", "text").is_err());
        assert!(checked_file("x", "", &"y".repeat(SYSTEM_PROMPT_MAX + 1)).is_err());
        let e =
            checked_file("x", "", &format!("token ghp_{}", "a1B2c3D4e5".repeat(4))).unwrap_err();
        assert!(e.message.contains("secret scan"), "{}", e.message);
        assert!(!e.message.contains("ghp_a1"), "never the value");
    }
}
